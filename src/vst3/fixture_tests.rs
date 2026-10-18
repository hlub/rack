//! Tests against the fixture plugin (`rack-sys/test/fixture_plugin`), which
//! records what the host did to it in its own state, so these tests can
//! assert on what reached the plugin rather than only on what rack returned.

use crate::PluginInstance;

use super::test_support::{load_fixture, parse_fixture_state, serial_plugin_test, Buffers};

#[test]
fn fixture_loads_processes_and_reports_its_counters() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };

    let mut buffers = Buffers::new(plugin.input_channels(), plugin.output_channels());
    for _ in 0..3 {
        buffers
            .with(|inputs, outputs| plugin.process(inputs, outputs, super::test_support::FIXTURE_BLOCK))
            .expect("the fixture should process");
    }

    let counters = parse_fixture_state(&plugin.get_state().expect("the fixture's state should read"));
    assert_eq!(counters.process_calls, 3, "every process call should reach the fixture");
    assert_eq!(counters.points, [0; super::test_support::param::COUNT]);
}
