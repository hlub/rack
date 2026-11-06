// The fixture's processor component. See fixture_ids.h for what it records
// and why.

#include "fixture_ids.h"

#include "public.sdk/source/vst/vstaudioeffect.h"
#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"

#include <array>
#include <atomic>
#include <cstring>

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace RackFixture {

class FixtureComponent : public AudioEffect {
public:
	FixtureComponent () { setControllerClass (kControllerUID); }

	static FUnknown* createInstance (void*)
	{
		return static_cast<IAudioProcessor*> (new FixtureComponent ());
	}

	tresult PLUGIN_API initialize (FUnknown* context) SMTG_OVERRIDE
	{
		tresult result = AudioEffect::initialize (context);
		if (result != kResultOk)
			return result;
		addAudioInput (STR16 ("Main In"), SpeakerArr::kStereo);
		addAudioOutput (STR16 ("Main Out"), SpeakerArr::kStereo);
		addAudioOutput (STR16 ("Aux Out"), SpeakerArr::kMono, kAux);
		addEventInput (STR16 ("MIDI In"), 16);
		addEventOutput (STR16 ("MIDI Out"), 16);
		return kResultOk;
	}

	tresult PLUGIN_API setProcessing (TBool state) SMTG_OVERRIDE
	{
		if (!state)
			set_processing_false_calls.fetch_add (1, std::memory_order_relaxed);
		return kResultOk;
	}

	tresult PLUGIN_API process (ProcessData& data) SMTG_OVERRIDE
	{
		process_calls.fetch_add (1, std::memory_order_relaxed);
		bool send_message = false;
		bool report_mode = false;
		if (IEventList* events = data.inputEvents)
		{
			int32 event_count = events->getEventCount ();
			for (int32 event_index = 0; event_index < event_count; ++event_index)
			{
				Event event {};
				if (events->getEvent (event_index, event) != kResultOk)
					continue;
				record_event (event);
				// A legacy MIDI controller event is handed back on the output bus
				// unchanged, so a test can check the host's decode of what the
				// host itself encoded.
				if (event.type == Event::kLegacyMIDICCOutEvent && data.outputEvents)
					data.outputEvents->addEvent (event);
			}
		}
		if (IParameterChanges* changes = data.inputParameterChanges)
		{
			int32 queue_count = changes->getParameterCount ();
			for (int32 queue_index = 0; queue_index < queue_count; ++queue_index)
			{
				IParamValueQueue* queue = changes->getParameterData (queue_index);
				if (!queue)
					continue;
				ParamID id = queue->getParameterId ();
				int32 point_count = queue->getPointCount ();
				if (id >= kParamCount || point_count <= 0)
					continue;
				points_received[id].fetch_add (static_cast<uint64> (point_count),
				                               std::memory_order_relaxed);
				int32 offset = 0;
				ParamValue value = 0.0;
				if (queue->getPoint (point_count - 1, offset, value) != kResultOk)
					continue;
				if (id == kGain)
					gain = value;
				else if (id == kMode)
					mode = value;
				else if (id == kSendFromProcess && value > 0.5)
					send_message = true;
				else if (id == kReportFromProcess && value > 0.5)
					report_mode = true;
				else if (id == kEcho)
					echo = value > 0.5;
			}
		}

		if (report_mode)
		{
			mode = kReportedMode;
			if (data.outputParameterChanges)
			{
				int32 queue_index = 0;
				if (IParamValueQueue* queue =
				        data.outputParameterChanges->addParameterData (kMode, queue_index))
				{
					int32 point_index = 0;
					queue->addPoint (0, kReportedMode, point_index);
				}
			}
		}

		if (echo && data.outputParameterChanges)
		{
			// Says what it holds, whether or not it was just given.
			int32 queue_index = 0;
			if (IParamValueQueue* queue = data.outputParameterChanges->addParameterData (kGain, queue_index))
			{
				int32 point_index = 0;
				queue->addPoint (0, gain, point_index);
			}
		}

		if (data.outputParameterChanges)
		{
			// The meter moves every block.
			int32 queue_index = 0;
			if (IParamValueQueue* queue = data.outputParameterChanges->addParameterData (kMeter, queue_index))
			{
				int32 point_index = 0;
				ParamValue level =
				    static_cast<double> (process_calls.load (std::memory_order_relaxed) % 100) / 100.0;
				queue->addPoint (0, level, point_index);
			}
		}

		if (send_message)
		{
			if (IMessage* message = allocateMessage ())
			{
				message->setMessageID (kProcessMessageId);
				sendMessage (message);
				message->release ();
			}
		}

		for (int32 bus = 0; bus < data.numOutputs; ++bus)
		{
			AudioBusBuffers& output = data.outputs[bus];
			for (int32 channel = 0; channel < output.numChannels; ++channel)
			{
				float* out = output.channelBuffers32 ? output.channelBuffers32[channel] : nullptr;
				if (!out)
					continue;
				const float* in = nullptr;
				if (bus == 0 && data.numInputs > 0 && channel < data.inputs[0].numChannels &&
				    data.inputs[0].channelBuffers32)
					in = data.inputs[0].channelBuffers32[channel];
				for (int32 frame = 0; frame < data.numSamples; ++frame)
					out[frame] = in ? static_cast<float> (in[frame] * gain) : 0.0f;
			}
		}
		return kResultOk;
	}

	tresult PLUGIN_API notify (IMessage* message) SMTG_OVERRIDE
	{
		if (!message)
			return kInvalidArgument;
		messages_received.fetch_add (1, std::memory_order_relaxed);
		return kResultOk;
	}

	tresult PLUGIN_API getState (IBStream* state) SMTG_OVERRIDE
	{
		if (!state)
			return kInvalidArgument;
		uint32 magic = kComponentStateMagic;
		if (state->write (&magic, sizeof (magic), nullptr) != kResultOk)
			return kResultFalse;
		std::array<uint64, kComponentStateWords> words {};
		words[0] = process_calls.load (std::memory_order_relaxed);
		words[1] = set_processing_false_calls.load (std::memory_order_relaxed);
		words[2] = messages_received.load (std::memory_order_relaxed);
		for (int32 id = 0; id < kParamCount; ++id)
			words[3 + id] = points_received[id].load (std::memory_order_relaxed);
		words[3 + kParamCount] = events_received.load (std::memory_order_relaxed);
		for (int32 slot = 0; slot < kEventLogSize; ++slot)
			words[4 + kParamCount + slot] = event_log[slot].load (std::memory_order_relaxed);
		static_assert (sizeof (double) == sizeof (uint64), "a double fills one state word");
		std::memcpy (&words[kComponentStateWords - 1], &mode, sizeof (mode));
		return state->write (words.data (), static_cast<int32> (sizeof (words)), nullptr);
	}

	tresult PLUGIN_API setState (IBStream* state) SMTG_OVERRIDE
	{
		if (!state)
			return kInvalidArgument;
		uint32 magic = 0;
		int32 read = 0;
		if (state->read (&magic, sizeof (magic), &read) != kResultOk || read != sizeof (magic) ||
		    magic != kComponentStateMagic)
			return kResultFalse;
		std::array<uint64, kComponentStateWords> words {};
		if (state->read (words.data (), static_cast<int32> (sizeof (words)), &read) != kResultOk ||
		    read != static_cast<int32> (sizeof (words)))
			return kResultFalse;
		std::memcpy (&mode, &words[kComponentStateWords - 1], sizeof (mode));
		return kResultOk;
	}

private:
	// Appends an input event to the log: its type in the low 16 bits and, for a
	// legacy MIDI controller event, its control number, channel, value and
	// value2 in the next four bytes (see fixture_ids.h).
	void record_event (const Event& event)
	{
		uint64 word = event.type;
		if (event.type == Event::kLegacyMIDICCOutEvent)
		{
			word |= static_cast<uint64> (event.midiCCOut.controlNumber) << 16;
			word |= static_cast<uint64> (static_cast<uint8> (event.midiCCOut.channel)) << 24;
			word |= static_cast<uint64> (static_cast<uint8> (event.midiCCOut.value)) << 32;
			word |= static_cast<uint64> (static_cast<uint8> (event.midiCCOut.value2)) << 40;
		}
		uint64 count = events_received.load (std::memory_order_relaxed);
		event_log[count % kEventLogSize].store (word, std::memory_order_relaxed);
		events_received.store (count + 1, std::memory_order_relaxed);
	}

	ParamValue gain {1.0};
	bool echo {false};
	ParamValue mode {kModeStart};
	std::atomic<uint64> process_calls {0};
	std::atomic<uint64> set_processing_false_calls {0};
	std::atomic<uint64> messages_received {0};
	std::array<std::atomic<uint64>, kParamCount> points_received {};
	std::atomic<uint64> events_received {0};
	std::array<std::atomic<uint64>, kEventLogSize> event_log {};
};

FUnknown* createFixtureComponent (void* context)
{
	return FixtureComponent::createInstance (context);
}

} // namespace RackFixture
