// The rack test fixture's class ids, parameter ids and state layout.
//
// The fixture is a VST3 effect built from the SDK rack already vendors. It
// exists so rack's own tests can observe, from outside, what a host did to a
// plugin: how many times it processed, how often it left the Processing
// state, how many parameter points reached the processor, and which
// component/controller messages arrived. Every observation is written into
// the component's and the controller's state, which a test reads back with an
// ordinary state read, so nothing here needs an interface of its own.
//
// The layout below is read by rack's Rust tests (src/vst3/fixture_tests.rs),
// which mirror these numbers; a change here is a change there.

#pragma once

#include "pluginterfaces/base/funknown.h"
#include "pluginterfaces/vst/vsttypes.h"

namespace RackFixture {

static const Steinberg::FUID kComponentUID (0x5241434B, 0x46495854, 0x55524543, 0x4F4D5031);
static const Steinberg::FUID kControllerUID (0x5241434B, 0x46495854, 0x55524543, 0x54524C31);

// Parameter ids, which are also the parameters' indices in the controller's
// list until "change ids" adds one.
enum ParamIds : Steinberg::Vst::ParamID {
	// An ordinary continuous parameter, applied to the audio as a gain.
	kGain = 0,
	// Written on the controller, asks the host for a restart: the value times
	// kRestartFlagSteps, rounded, is the flag word passed to restartComponent.
	kRequestRestart = 1,
	// Written above one half on the controller, renames kGain and asks for
	// kParamTitlesChanged.
	kRenameTitles = 2,
	// Written above one half on the controller, adds parameter kExtra, so the
	// parameter count changes.
	kChangeIds = 3,
	// Delivered above one half to the processor, makes the component send one
	// message to the controller from inside process().
	kSendFromProcess = 4,
	// A value the component starts with that is not the controller's declared
	// default: the component is in kModeStart, the controller declares 0. Only
	// a host that tells the controller the component's state (setComponentState)
	// sees the controller report the value the component uses. Writes to it
	// reach the component as parameter points and set the value there.
	kMode = 5,
	// Written above one half on the controller, the controller changes kGain to
	// kSelfEditGain by itself and tells the host with performEdit, as a plugin's
	// own interface does.
	kSelfEdit = 6,
	// Delivered above one half to the processor, makes the component change kMode
	// to kReportedMode and report it to the host in the block's output parameter
	// changes, which the controller was never told.
	kReportFromProcess = 7,
	kParamCount = 8,
	// Added by kChangeIds; never counted by the component.
	kExtra = 100,
};

// kRequestRestart's step count: every flag of RestartFlags fits in 12 bits.
static constexpr Steinberg::int32 kRestartFlagSteps = 4095;

// The value the component's kMode starts with.
static constexpr double kModeStart = 0.5;

// What kSelfEdit makes the controller's kGain, and what kReportFromProcess makes kMode.
static constexpr double kSelfEditGain = 0.25;
static constexpr double kReportedMode = 0.9;

// How many of the most recent input events the component keeps.
static constexpr int kEventLogSize = 8;
// How many uint64 words follow the component state's magic.
static constexpr int kComponentStateWords = 3 + kParamCount + 1 + kEventLogSize + 1;

// First word of the component's state, then (each a little-endian uint64):
// process calls, setProcessing(false) calls, messages the component received,
// one count of parameter points received per id in 0..kParamCount, the number
// of input events received, and the last kEventLogSize of them, the event
// number n in slot n modulo kEventLogSize. An event is its type in bits 0-15
// and, when it is a legacy MIDI controller event, its controlNumber in bits
// 16-23, channel in 24-31, value in 32-39 and value2 in 40-47. The component
// also hands every legacy MIDI controller event back on its MIDI output bus.
// The last word is the component's kMode value as the bits of a double.
static constexpr Steinberg::uint32 kComponentStateMagic = 0x52465843; // "RFXC"
// First word of the controller's state, then one uint64: messages the
// controller received.
static constexpr Steinberg::uint32 kControllerStateMagic = 0x52465852; // "RFXR"

// The message id the controller sends on connect and the one the component
// sends from process().
static constexpr const char* kConnectMessageId = "rack.fixture.connect";
static constexpr const char* kProcessMessageId = "rack.fixture.process";

} // namespace RackFixture
