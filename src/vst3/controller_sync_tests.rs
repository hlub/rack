//! Tests that the values the controller reports are the values the component
//! uses, against the fixture plugin: whose component starts in a mode the
//! controller does not declare as its default, so a host that never tells the
//! controller the component's state reads the declared default back.

use crate::{PluginInstance, PluginScanner, ProcessMode};

use super::test_support::{
    fixture_info, fixture_state_with_mode, load_fixture, param, serial_plugin_test, FIXTURE_BLOCK,
};

#[test]
fn a_fresh_instance_reports_the_state_its_component_started_in() {
    let _serial = serial_plugin_test();
    let Some(plugin) = load_fixture() else { return };

    assert_eq!(
        plugin.get_parameter(param::MODE).expect("read"),
        param::MODE_START,
        "the controller was not told what the component started as"
    );
}

#[test]
fn an_instance_initialized_with_a_state_reports_that_states_values() {
    let _serial = serial_plugin_test();
    let Some(donor) = load_fixture() else { return };
    let state = fixture_state_with_mode(&donor.get_state().expect("read"), 0.25);
    drop(donor);
    let Some((scanner, info)) = fixture_info() else { return };

    let mut plugin = scanner.load(&info).expect("the fixture should load");
    plugin
        .initialize_with_state(48000.0, FIXTURE_BLOCK, ProcessMode::Realtime, &state)
        .expect("the fixture should initialize with a state");

    assert_eq!(plugin.get_parameter(param::MODE).expect("read"), 0.25);
}

#[test]
fn a_state_written_later_is_reported_too() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let state = fixture_state_with_mode(&plugin.get_state().expect("read"), 0.75);

    plugin.set_state(&state).expect("the fixture accepts its own state");

    assert_eq!(plugin.get_parameter(param::MODE).expect("read"), 0.75);
}
