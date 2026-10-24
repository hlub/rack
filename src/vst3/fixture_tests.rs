//! Tests against the fixture plugin (`rack-sys/test/fixture_plugin`), which
//! records what the host did to it in its own state, so these tests can
//! assert on what reached the plugin rather than only on what rack returned.

use crate::{MidiEvent, MidiEventKind, PluginInstance, PluginScanner, ProcessMode};

use super::test_support::{
    control_number, fixture_info, load_fixture, parse_fixture_state, serial_plugin_test, Buffers,
    ReceivedEvent, FIXTURE_BLOCK,
};

/// Every bus fact the controller half answers without a plugin call.
fn bus_facts(plugin: &super::Vst3Plugin) -> (Vec<(usize, String)>, Vec<(usize, String)>, bool) {
    let outputs = (0..2).map(|bus| plugin.output_bus_info(bus).expect("output bus info")).collect();
    let inputs = (0..1).map(|bus| plugin.input_bus_info(bus).expect("input bus info")).collect();
    (outputs, inputs, plugin.emits_midi())
}

#[test]
fn bus_facts_recorded_at_initialize_match_the_live_answers() {
    let _serial = serial_plugin_test();
    let Some((scanner, info)) = fixture_info() else { return };
    let mut plugin = scanner.load(&info).expect("the fixture should load");

    let live = bus_facts(&plugin);
    plugin.initialize(48000.0, FIXTURE_BLOCK, ProcessMode::Realtime).expect("initialize");
    let recorded = bus_facts(&plugin);

    assert_eq!(recorded, live, "what initialize recorded must equal the live answers");
    assert_eq!(
        recorded,
        (
            vec![(2, "Main Out".to_string()), (1, "Aux Out".to_string())],
            vec![(2, "Main In".to_string())],
            true,
        ),
        "the fixture declares a stereo main output, a mono aux, a stereo input and MIDI out"
    );
    assert!(plugin.output_bus_info(2).is_err(), "a bus past the last is refused");
}

#[test]
fn fixture_loads_processes_and_reports_its_counters() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };

    let mut buffers = Buffers::new(plugin.input_channels(), plugin.output_channels());
    for _ in 0..3 {
        buffers
            .with(|inputs, outputs| plugin.process(inputs, outputs, FIXTURE_BLOCK))
            .expect("the fixture should process");
    }

    let counters = parse_fixture_state(&plugin.get_state().expect("the fixture's state should read"));
    assert_eq!(counters.process_calls, 3, "every process call should reach the fixture");
    assert_eq!(counters.points, [0; super::test_support::param::COUNT]);
}

/// One message of each kind that has no VST3 event of its own, on distinct
/// channels, with a pitch bend whose LSB (0x34) and MSB (0x24) differ.
fn controller_messages() -> [MidiEventKind; 4] {
    [
        MidiEventKind::ControlChange { controller: 7, value: 99, channel: 2 },
        MidiEventKind::ProgramChange { program: 42, channel: 3 },
        MidiEventKind::ChannelAftertouch { pressure: 77, channel: 4 },
        MidiEventKind::PitchBend { value: 0x1234, channel: 5 },
    ]
}

fn at_block_start(kind: MidiEventKind) -> MidiEvent {
    MidiEvent { sample_offset: 0, kind }
}

#[test]
fn control_change_program_change_channel_pressure_and_pitch_bend_reach_the_plugin_under_the_sdks_numbers() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };

    let events: Vec<MidiEvent> = controller_messages().into_iter().map(at_block_start).collect();
    plugin.processor_mut().send_midi(&events).expect("the fixture should accept the events");
    let mut buffers = Buffers::new(plugin.input_channels(), plugin.output_channels());
    buffers
        .with(|inputs, outputs| plugin.process(inputs, outputs, FIXTURE_BLOCK))
        .expect("the fixture should process");

    let counters = parse_fixture_state(&plugin.get_state().expect("the fixture's state should read"));
    assert_eq!(
        counters.events,
        vec![
            ReceivedEvent::legacy(7, 2, 99, 0),
            ReceivedEvent::legacy(control_number::PROGRAM_CHANGE, 3, 42, 0),
            ReceivedEvent::legacy(control_number::AFTER_TOUCH, 4, 77, 0),
            ReceivedEvent::legacy(control_number::PITCH_BEND, 5, 0x34, 0x24),
        ],
        "a control change keeps its controller number, and the other three use the SDK's \
         kCtrlProgramChange, kAfterTouch and kPitchBend with the value (and for a pitch bend \
         the LSB then the MSB) in the event's value fields"
    );
}

#[test]
fn a_controller_event_the_plugin_emits_decodes_to_the_message_that_was_sent() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };

    let sent: Vec<MidiEvent> = controller_messages().into_iter().map(at_block_start).collect();
    plugin.processor_mut().send_midi(&sent).expect("the fixture should accept the events");
    let mut buffers = Buffers::new(plugin.input_channels(), plugin.output_channels());
    let mut emitted = Vec::new();
    buffers
        .with(|inputs, outputs| {
            plugin.process_multi_bus(inputs, &[], outputs, &mut [], FIXTURE_BLOCK, Some(&mut emitted))
        })
        .expect("the fixture should process");

    assert_eq!(emitted, sent, "the fixture hands each controller event back, and rack decodes it");
}
