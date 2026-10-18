// The fixture's processor component. See fixture_ids.h for what it records
// and why.

#include "fixture_ids.h"

#include "public.sdk/source/vst/vstaudioeffect.h"
#include "pluginterfaces/base/ibstream.h"
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
				else if (id == kSendFromProcess && value > 0.5)
					send_message = true;
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
		std::array<uint64, 3 + kParamCount> words {};
		words[0] = process_calls.load (std::memory_order_relaxed);
		words[1] = set_processing_false_calls.load (std::memory_order_relaxed);
		words[2] = messages_received.load (std::memory_order_relaxed);
		for (int32 id = 0; id < kParamCount; ++id)
			words[3 + id] = points_received[id].load (std::memory_order_relaxed);
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
		return kResultOk;
	}

private:
	ParamValue gain {1.0};
	std::atomic<uint64> process_calls {0};
	std::atomic<uint64> set_processing_false_calls {0};
	std::atomic<uint64> messages_received {0};
	std::array<std::atomic<uint64>, kParamCount> points_received {};
};

FUnknown* createFixtureComponent (void* context)
{
	return FixtureComponent::createInstance (context);
}

} // namespace RackFixture
