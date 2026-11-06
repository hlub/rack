// The fixture's edit controller. See fixture_ids.h for what it records and
// why.

#include "fixture_ids.h"

#include "public.sdk/source/vst/vsteditcontroller.h"
#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/base/ustring.h"

#include <array>
#include <atomic>
#include <cmath>
#include <cstring>

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace RackFixture {

class FixtureController : public EditController {
public:
	static FUnknown* createInstance (void*)
	{
		return static_cast<IEditController*> (new FixtureController ());
	}

	tresult PLUGIN_API initialize (FUnknown* context) SMTG_OVERRIDE
	{
		tresult result = EditController::initialize (context);
		if (result != kResultOk)
			return result;
		parameters.addParameter (STR16 ("Gain"), STR16 ("x"), 0, 1.0,
		                         ParameterInfo::kCanAutomate, kGain);
		parameters.addParameter (STR16 ("Request Restart"), nullptr, kRestartFlagSteps, 0.0,
		                         ParameterInfo::kNoFlags, kRequestRestart);
		parameters.addParameter (STR16 ("Rename Titles"), nullptr, 1, 0.0,
		                         ParameterInfo::kNoFlags, kRenameTitles);
		parameters.addParameter (STR16 ("Change Ids"), nullptr, 1, 0.0,
		                         ParameterInfo::kNoFlags, kChangeIds);
		parameters.addParameter (STR16 ("Send From Process"), nullptr, 1, 0.0,
		                         ParameterInfo::kCanAutomate, kSendFromProcess);
		parameters.addParameter (STR16 ("Mode"), nullptr, 0, 0.0, ParameterInfo::kCanAutomate,
		                         kMode);
		parameters.addParameter (STR16 ("Self Edit"), nullptr, 1, 0.0, ParameterInfo::kNoFlags,
		                         kSelfEdit);
		parameters.addParameter (STR16 ("Report From Process"), nullptr, 1, 0.0,
		                         ParameterInfo::kCanAutomate, kReportFromProcess);
		return kResultOk;
	}

	tresult PLUGIN_API connect (IConnectionPoint* other) SMTG_OVERRIDE
	{
		tresult result = EditController::connect (other);
		if (result != kResultTrue)
			return result;
		if (IMessage* message = allocateMessage ())
		{
			message->setMessageID (kConnectMessageId);
			sendMessage (message);
			message->release ();
		}
		return result;
	}

	tresult PLUGIN_API setParamNormalized (ParamID tag, ParamValue value) SMTG_OVERRIDE
	{
		tresult result = EditController::setParamNormalized (tag, value);
		if (result != kResultTrue && result != kResultOk)
			return result;
		IComponentHandler* handler = getComponentHandler ();
		if (tag == kRequestRestart)
		{
			int32 flags = static_cast<int32> (std::lround (value * kRestartFlagSteps));
			if (flags != 0 && handler)
				handler->restartComponent (flags);
		}
		else if (tag == kRenameTitles && value > 0.5)
		{
			if (Parameter* gain = parameters.getParameter (kGain))
			{
				UString (gain->getInfo ().title, str16BufferSize (String128))
				    .assign (STR16 ("Gain Renamed"));
				if (handler)
					handler->restartComponent (kParamTitlesChanged);
			}
		}
		else if (tag == kSelfEdit && value > 0.5)
		{
			EditController::setParamNormalized (kGain, kSelfEditGain);
			if (handler)
				handler->performEdit (kGain, kSelfEditGain);
		}
		else if (tag == kChangeIds && value > 0.5 && !parameters.getParameter (kExtra))
		{
			parameters.addParameter (STR16 ("Extra"), nullptr, 0, 0.0, ParameterInfo::kNoFlags,
			                         kExtra);
			if (handler)
				handler->restartComponent (kParamTitlesChanged);
		}
		return result;
	}

	tresult PLUGIN_API notify (IMessage* message) SMTG_OVERRIDE
	{
		if (!message)
			return kInvalidArgument;
		messages_received.fetch_add (1, std::memory_order_relaxed);
		return kResultOk;
	}

	// Reads the component's kMode out of its state, the one value of the
	// component's that the controller reports.
	tresult PLUGIN_API setComponentState (IBStream* state) SMTG_OVERRIDE
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
		double mode = 0.0;
		std::memcpy (&mode, &words[kComponentStateWords - 1], sizeof (mode));
		return setParamNormalized (kMode, mode);
	}

	tresult PLUGIN_API getState (IBStream* state) SMTG_OVERRIDE
	{
		if (!state)
			return kInvalidArgument;
		uint32 magic = kControllerStateMagic;
		if (state->write (&magic, sizeof (magic), nullptr) != kResultOk)
			return kResultFalse;
		uint64 received = messages_received.load (std::memory_order_relaxed);
		return state->write (&received, sizeof (received), nullptr);
	}

	tresult PLUGIN_API setState (IBStream*) SMTG_OVERRIDE { return kResultOk; }

private:
	std::atomic<uint64> messages_received {0};
};

FUnknown* createFixtureController (void* context)
{
	return FixtureController::createInstance (context);
}

} // namespace RackFixture
