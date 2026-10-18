//! Tests against the fixture plugin (`rack-sys/test/fixture_plugin`), which
//! records what the host did to it in its own state, so these tests can
//! assert on what reached the plugin rather than only on what rack returned.

use crate::{PluginInstance, PluginScanner, ProcessMode};

use super::test_support::{
    fixture_info, load_fixture, parse_fixture_state, serial_plugin_test, Buffers, FIXTURE_BLOCK,
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
