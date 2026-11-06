//! Tests of what a plugin does to its own parameter values after initialization and how the
//! host learns of it, against the fixture plugin: an edit its own interface makes, a request to
//! read its values again, and values its processor reports while processing.

use crate::PluginInstance;

use super::test_support::{load_fixture, param, serial_plugin_test, Buffers, FIXTURE_BLOCK};
use super::Vst3Plugin;

fn process_block(plugin: &mut Vst3Plugin) {
    let mut buffers = Buffers::new(plugin.input_channels(), plugin.output_channels());
    buffers
        .with(|inputs, outputs| plugin.process(inputs, outputs, FIXTURE_BLOCK))
        .expect("process");
}

fn controller_value(plugin: &Vst3Plugin, index: usize) -> f32 {
    plugin.controller().get_parameter(index).expect("controller read")
}

#[test]
fn a_plugin_that_does_nothing_reports_no_changes() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    assert!(plugin.take_value_changes().is_empty(), "initialization changed nothing the host has to read");
    process_block(&mut plugin);
    assert!(plugin.take_value_changes().is_empty(), "a block that reported nothing changes nothing");
}

#[test]
fn an_edit_the_plugin_makes_itself_is_reported_once_and_the_controller_holds_the_value() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    assert!((controller_value(&plugin, param::GAIN) - 1.0).abs() < 1e-6);

    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 1.0).expect("controller write");

    let changes = plugin.take_value_changes();
    assert!(changes.edited_by_plugin(), "the plugin passed performEdit");
    assert!(!changes.reported_by_processor());
    assert!((controller_value(&plugin, param::GAIN) - param::SELF_EDIT_GAIN).abs() < 1e-6);
    assert!(plugin.take_value_changes().is_empty(), "a second take is empty");
}

#[test]
fn a_request_to_read_the_values_again_is_reported_and_leaves_other_restart_flags() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let values_changed = 1u32 << 2;
    let titles_changed = 1u32 << 4;
    let flags = (values_changed | titles_changed) as f32 / param::RESTART_FLAG_STEPS;
    plugin.halves_mut().1.set_parameter(param::REQUEST_RESTART, flags).expect("controller write");

    assert!(plugin.take_value_changes().reread_requested());
    assert!(plugin.take_value_changes().is_empty(), "the request is taken once");
    assert_eq!(
        plugin.take_restart_flags().bits(),
        titles_changed,
        "the title request is still there for whoever acts on it"
    );
}

#[test]
fn a_value_the_processor_reports_reaches_the_controller_when_taken() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let before = controller_value(&plugin, param::MODE);
    assert!((before - param::MODE_START).abs() < 1e-6, "the controller starts in the component's mode");

    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);
    assert!(
        (controller_value(&plugin, param::MODE) - before).abs() < 1e-6,
        "nothing tells the controller until the host takes the changes"
    );

    let changes = plugin.take_value_changes();
    assert!(changes.reported_by_processor());
    assert!((controller_value(&plugin, param::MODE) - param::REPORTED_MODE).abs() < 1e-6);
    assert!(plugin.take_value_changes().is_empty());
}

#[test]
fn a_value_the_host_wrote_after_a_report_is_not_overwritten_by_it() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);

    plugin.halves_mut().1.set_parameter(param::MODE, 0.1).expect("controller write");
    let _ = plugin.take_value_changes();

    assert!(
        (controller_value(&plugin, param::MODE) - 0.1).abs() < 1e-6,
        "the host's newer value stands"
    );
}

#[test]
fn a_meter_that_reports_every_block_is_never_a_change_and_never_reaches_the_controller() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let before = controller_value(&plugin, param::METER);
    for _ in 0..50 {
        process_block(&mut plugin);
    }
    assert!(!plugin.controller().value_changes_pending(), "a read-only parameter raises nothing");
    assert!(plugin.take_value_changes().is_empty());
    assert!(
        (controller_value(&plugin, param::METER) - before).abs() < 1e-6,
        "the controller is not written for a read-only parameter"
    );
}

#[test]
fn pending_is_answered_without_taking_and_clears_when_taken() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    assert!(!plugin.controller().value_changes_pending());

    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 1.0).expect("controller write");
    assert!(plugin.controller().value_changes_pending(), "an edit is pending");
    assert!(plugin.controller().value_changes_pending(), "asking does not take it");
    let _ = plugin.take_value_changes();
    assert!(!plugin.controller().value_changes_pending());

    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);
    assert!(plugin.controller().value_changes_pending(), "a report is pending");
    let _ = plugin.take_value_changes();
    assert!(!plugin.controller().value_changes_pending());
}

#[test]
fn reports_made_before_a_state_write_or_a_reset_are_not_taken_after_it() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let state = plugin.get_state().expect("state");

    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);
    plugin.set_state(&state).expect("state write");
    assert!(plugin.take_value_changes().is_empty(), "a state write drops earlier reports");

    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);
    plugin.reset().expect("reset");
    assert!(plugin.take_value_changes().is_empty(), "a reset drops earlier reports");
}

#[test]
fn a_report_from_a_block_after_the_hosts_write_is_taken_and_one_before_it_is_not() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);
    plugin.halves_mut().1.set_parameter(param::MODE, 0.1).expect("controller write");
    let _ = plugin.take_value_changes();
    assert!((controller_value(&plugin, param::MODE) - 0.1).abs() < 1e-6, "the older report is dropped");

    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);
    let changes = plugin.take_value_changes();
    assert!(changes.reported_by_processor());
    assert!(
        (controller_value(&plugin, param::MODE) - param::REPORTED_MODE).abs() < 1e-6,
        "a report from a block that began after the write is the plugin's newer say"
    );
}
