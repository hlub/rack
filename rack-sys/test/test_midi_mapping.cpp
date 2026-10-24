// Unit tests for the two pure functions that map between rack's MIDI 1.0
// events and VST3 events: rack_midi_to_vst3_event (host to plugin) and
// vst3_event_to_rack_midi (plugin to host). Both are given hand-built input and
// need no plugin. The control numbers are asserted against the literal values
// the VST3 SDK fixes in ivstmidicontrollers.h, so a change in either function
// cannot move them without these tests failing.

#include "rack_vst3.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstmidicontrollers.h"

#include <cstdint>
#include <cstring>
#include <iostream>

using Steinberg::Vst::Event;

bool rack_midi_to_vst3_event(const RackVST3MidiEvent& midi_event, Event* out);
bool vst3_event_to_rack_midi(const Event& event, RackVST3MidiEvent* out);

static_assert(Steinberg::Vst::kAfterTouch == 128, "SDK numbering of channel pressure");
static_assert(Steinberg::Vst::kPitchBend == 129, "SDK numbering of pitch bend");
static_assert(Steinberg::Vst::kCtrlProgramChange == 130, "SDK numbering of program change");

static int failures = 0;

#define CHECK(condition)                                                              \
    do {                                                                              \
        if (!(condition)) {                                                           \
            std::cerr << "FAIL line " << __LINE__ << ": " #condition "\n";            \
            ++failures;                                                               \
        }                                                                             \
    } while (0)

static RackVST3MidiEvent midi(uint8_t status, uint8_t data1, uint8_t data2, uint8_t channel) {
    RackVST3MidiEvent event;
    std::memset(&event, 0, sizeof(event));
    event.sample_offset = 17;
    event.status = status;
    event.data1 = data1;
    event.data2 = data2;
    event.channel = channel;
    return event;
}

static void check_legacy(const Event& event, uint8_t control_number, int channel, int value, int value2) {
    CHECK(event.type == Event::kLegacyMIDICCOutEvent);
    CHECK(event.midiCCOut.controlNumber == control_number);
    CHECK(event.midiCCOut.channel == channel);
    CHECK(event.midiCCOut.value == value);
    CHECK(event.midiCCOut.value2 == value2);
    CHECK(event.sampleOffset == 17);
}

static void test_encode() {
    Event event;

    CHECK(rack_midi_to_vst3_event(midi(0xB0, 7, 99, 2), &event));
    check_legacy(event, 7, 2, 99, 0);

    CHECK(rack_midi_to_vst3_event(midi(0xC0, 42, 0, 3), &event));
    check_legacy(event, 130, 3, 42, 0);

    CHECK(rack_midi_to_vst3_event(midi(0xD0, 77, 0, 4), &event));
    check_legacy(event, 128, 4, 77, 0);

    CHECK(rack_midi_to_vst3_event(midi(0xE0, 0x34, 0x24, 5), &event));
    check_legacy(event, 129, 5, 0x34, 0x24);

    CHECK(rack_midi_to_vst3_event(midi(0x90, 60, 127, 1), &event));
    CHECK(event.type == Event::kNoteOnEvent);
    CHECK(event.noteOn.pitch == 60 && event.noteOn.channel == 1 && event.noteOn.velocity == 1.0f);

    CHECK(rack_midi_to_vst3_event(midi(0x80, 61, 0, 1), &event));
    CHECK(event.type == Event::kNoteOffEvent);
    CHECK(event.noteOff.pitch == 61 && event.noteOff.channel == 1);

    CHECK(rack_midi_to_vst3_event(midi(0xA0, 62, 127, 1), &event));
    CHECK(event.type == Event::kPolyPressureEvent);
    CHECK(event.polyPressure.pitch == 62 && event.polyPressure.pressure == 1.0f);

    CHECK(!rack_midi_to_vst3_event(midi(0xF0, 0, 0, 0), &event));
    CHECK(!rack_midi_to_vst3_event(midi(0x90, 60, 100, 0), nullptr));
}

static void test_decode_uses_the_sdk_numbers() {
    RackVST3MidiEvent decoded;
    Event event;
    std::memset(&event, 0, sizeof(event));
    event.type = Event::kLegacyMIDICCOutEvent;
    event.midiCCOut.channel = 6;

    event.midiCCOut.controlNumber = 130;
    event.midiCCOut.value = 9;
    CHECK(vst3_event_to_rack_midi(event, &decoded));
    CHECK(decoded.status == 0xC0 && decoded.data1 == 9 && decoded.channel == 6);

    event.midiCCOut.controlNumber = 128;
    event.midiCCOut.value = 33;
    CHECK(vst3_event_to_rack_midi(event, &decoded));
    CHECK(decoded.status == 0xD0 && decoded.data1 == 33 && decoded.channel == 6);

    event.midiCCOut.controlNumber = 129;
    event.midiCCOut.value = 0x11;
    event.midiCCOut.value2 = 0x22;
    CHECK(vst3_event_to_rack_midi(event, &decoded));
    CHECK(decoded.status == 0xE0 && decoded.data1 == 0x11 && decoded.data2 == 0x22);

    event.midiCCOut.controlNumber = 64;
    event.midiCCOut.value = 127;
    CHECK(vst3_event_to_rack_midi(event, &decoded));
    CHECK(decoded.status == 0xB0 && decoded.data1 == 64 && decoded.data2 == 127);

    event.midiCCOut.controlNumber = 200;
    CHECK(!vst3_event_to_rack_midi(event, &decoded));
}

static void test_round_trip() {
    const RackVST3MidiEvent sent[] = {
        midi(0xB0, 1, 64, 0), midi(0xC0, 5, 0, 1), midi(0xD0, 100, 0, 2), midi(0xE0, 0x7F, 0x7F, 3),
        midi(0x90, 60, 100, 4), midi(0x80, 60, 40, 5), midi(0xA0, 60, 90, 6),
    };
    for (const RackVST3MidiEvent& original : sent) {
        Event event;
        RackVST3MidiEvent back;
        CHECK(rack_midi_to_vst3_event(original, &event));
        CHECK(vst3_event_to_rack_midi(event, &back));
        CHECK(back.status == original.status && back.channel == original.channel);
        CHECK(back.data1 == original.data1);
        // Program Change and Channel Pressure carry no second data byte.
        if (original.status != 0xC0 && original.status != 0xD0) {
            CHECK(back.data2 == original.data2);
        }
    }
}

int main() {
    test_encode();
    test_decode_uses_the_sdk_numbers();
    test_round_trip();
    if (failures != 0) {
        std::cerr << failures << " check(s) failed\n";
        return 1;
    }
    std::cout << "midi mapping: all checks passed\n";
    return 0;
}
