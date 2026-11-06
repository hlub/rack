//! Tests of what a plugin does to its own parameter values after initialization and how the
//! host learns of it, against the fixture plugin: an edit its own interface makes, a request to
//! read its values again, and values its processor reports while processing.

use crate::PluginInstance;

use super::test_support::{load_fixture, param, parse_fixture_state, serial_plugin_test, Buffers, FIXTURE_BLOCK};
use super::{Vst3Plugin, Vst3Processor};

fn process_one(processor: &mut Vst3Processor) {
    let mut buffers = Buffers::new(processor.input_channels(), processor.output_channels());
    buffers
        .with(|inputs, outputs| processor.process(inputs, outputs, FIXTURE_BLOCK))
        .expect("process");
}

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

    let _ = plugin.begin_host_write(param::MODE);
    plugin.halves_mut().1.set_parameter(param::MODE, 0.1).expect("controller write");
    let _ = plugin.take_value_changes();

    assert!(
        (controller_value(&plugin, param::MODE) - 0.1).abs() < 1e-6,
        "the host's newer value stands"
    );
}

#[test]
fn a_meter_that_reports_every_block_is_never_a_change_but_is_forwarded_to_the_controller() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let before = controller_value(&plugin, param::METER);
    for _ in 0..50 {
        process_block(&mut plugin);
    }
    assert!(!plugin.controller().value_changes_pending(), "a read-only parameter raises nothing");
    assert!(
        (controller_value(&plugin, param::METER) - before).abs() < 1e-6,
        "nothing reaches the controller until the host takes"
    );
    assert!(plugin.take_value_changes().is_empty(), "forwarding a meter is not a change of values");
    assert!(
        (controller_value(&plugin, param::METER) - before).abs() > 1e-6,
        "the meter's value is on the controller, for a window of the plugin's own to show"
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
    assert!(
        plugin.controller().value_changes_pending(),
        "the edit's value is still waiting to be handed to the processor"
    );
    let _ = plugin.take_edits();
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
fn a_report_made_before_the_plugin_was_given_the_hosts_write_is_dropped_and_one_after_is_taken() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);

    let generation = plugin.begin_host_write(param::MODE);
    plugin.halves_mut().1.set_parameter(param::MODE, 0.1).expect("controller write");
    let _ = plugin.take_value_changes();
    assert!((controller_value(&plugin, param::MODE) - 0.1).abs() < 1e-6, "the older report is dropped");

    // A report made after the write was delivered says what the plugin holds after it.
    plugin
        .halves_mut()
        .0
        .queue_parameter_change_stamped(param::MODE, 0.1, generation)
        .expect("queue");
    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);
    let changes = plugin.take_value_changes();
    assert!(changes.reported_by_processor());
    assert!((controller_value(&plugin, param::MODE) - param::REPORTED_MODE).abs() < 1e-6);
}

#[test]
fn a_write_not_yet_delivered_keeps_every_report_invalid() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let _ = plugin.begin_host_write(param::MODE);
    plugin.halves_mut().1.set_parameter(param::MODE, 0.1).expect("controller write");
    // Several blocks pass, as when the host's per-block delivery cap leaves the write waiting.
    for _ in 0..5 {
        plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
        process_block(&mut plugin);
        let _ = plugin.take_value_changes();
        assert!(
            (controller_value(&plugin, param::MODE) - 0.1).abs() < 1e-6,
            "a plugin not yet given the write does not overrule it"
        );
    }
}

#[test]
fn a_value_the_plugin_edited_itself_is_handed_to_the_processor_and_the_view_is_not_ahead_of_the_sound() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 1.0).expect("controller write");
    let edits = plugin.take_edits();
    assert_eq!(edits.len(), 1, "one edit: {edits:?}");
    assert_eq!(edits[0].0, param::GAIN);
    assert!((edits[0].1 - param::SELF_EDIT_GAIN).abs() < 1e-6);
    assert!(plugin.take_edits().is_empty(), "taken once");

    let points_before = parse_fixture_state(&plugin.get_state().expect("state")).points[param::GAIN];
    for (index, value) in edits {
        let generation = plugin.begin_host_write(index);
        plugin
            .halves_mut()
            .0
            .queue_parameter_change_stamped(index, value, generation)
            .expect("queue");
    }
    process_block(&mut plugin);
    let points_after = parse_fixture_state(&plugin.get_state().expect("state")).points[param::GAIN];
    assert_eq!(points_after, points_before + 1, "the processor heard the edit");
}

#[test]
fn the_controller_never_holds_a_value_older_than_the_hosts_last_write_with_the_plugin_reporting_on_another_thread() {
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;

    const WRITES: u32 = 4000;
    let _serial = serial_plugin_test();
    let Some(plugin) = load_fixture() else { return };
    let (mut processor, mut controller) = plugin.split().expect("an initialized instance splits");
    processor.queue_parameter_change(param::ECHO, 1.0).expect("queue");

    // The host's slot for the gain: the latest (generation, value) the host set, which the audio
    // thread delivers at the top of some block, as the engine's parameter slots do.
    let slot = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let processed = Arc::new(AtomicU32::new(0));
    let audio = {
        let (slot, stop, processed) = (Arc::clone(&slot), Arc::clone(&stop), Arc::clone(&processed));
        std::thread::spawn(move || {
            let mut delivered = 0_u32;
            let mut block = 0_u32;
            while !stop.load(Ordering::Acquire) {
                block += 1;
                let latest = slot.load(Ordering::Acquire);
                let generation = (latest >> 32) as u32;
                // A block does not always have room for the waiting write (the host's cap).
                if generation != delivered && generation != 0 && block % 3 != 0 {
                    let value = f32::from_bits(latest as u32);
                    processor
                        .queue_parameter_change_stamped(param::GAIN, value, generation)
                        .expect("queue");
                    delivered = generation;
                }
                process_one(&mut processor);
                processed.store(delivered, Ordering::Release);
            }
            processor
        })
    };

    let mut last_generation = 0;
    let mut stale = 0;
    for write in 1..=WRITES {
        let value = write as f32 / WRITES as f32;
        // The order of a host write: the generation first, then the slot, then the controller.
        last_generation = controller.begin_host_write(param::GAIN);
        slot.store((u64::from(last_generation) << 32) | u64::from(value.to_bits()), Ordering::Release);
        controller.set_parameter(param::GAIN, value).expect("controller write");
        if write % 2 == 0 {
            let _ = controller.take_value_changes();
        }
        for _ in 0..(write % 7) {
            std::hint::spin_loop();
        }
        if controller.get_parameter(param::GAIN).expect("read") < value - 1e-6 {
            stale += 1;
        }
    }
    assert_eq!(stale, 0, "the controller held a value older than the host's last write {stale} times");

    // Once the plugin has been given the last write and has processed, what it reports is it.
    // Bounded: a processing thread that died would otherwise leave this waiting for ever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while processed.load(Ordering::Acquire) < last_generation && std::time::Instant::now() < deadline {
        std::hint::spin_loop();
    }
    let given = processed.load(Ordering::Acquire) >= last_generation;
    std::thread::sleep(std::time::Duration::from_millis(20));
    let reported_at_the_end = controller.take_value_changes().reported_by_processor();
    stop.store(true, Ordering::Release);
    let processor = audio.join().expect("the processing thread finishes");
    assert!(given, "the plugin was given the last write within ten seconds");
    assert!(
        reported_at_the_end,
        "the echoing plugin's report of the last write was applied by the final take, so the test exercised the path it checks"
    );
    assert!((controller.get_parameter(param::GAIN).expect("read") - 1.0).abs() < 1e-6);
    drop(Vst3Plugin::join(processor, controller).expect("the halves join"));
}

#[test]
fn an_interface_edit_the_host_wrote_over_afterwards_is_not_handed_to_the_processor() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    // The plugin edits Gain, then the host writes Gain: the host's value is the newer.
    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 1.0).expect("controller write");
    let _ = plugin.begin_host_write(param::GAIN);
    plugin.halves_mut().1.set_parameter(param::GAIN, 0.6).expect("controller write");
    assert!(plugin.take_edits().is_empty(), "the edit is older than the host's write");

    // The host writes Gain, then the plugin edits it: the edit is the newer.
    let _ = plugin.begin_host_write(param::GAIN);
    plugin.halves_mut().1.set_parameter(param::GAIN, 0.6).expect("controller write");
    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 0.0).expect("controller write");
    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 1.0).expect("controller write");
    let edits = plugin.take_edits();
    assert_eq!(edits.len(), 1, "{edits:?}");
}

#[test]
fn interface_edits_made_before_a_state_write_or_a_reset_are_not_handed_on_after_it() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let state = plugin.get_state().expect("state");

    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 1.0).expect("controller write");
    plugin.set_state(&state).expect("state write");
    assert!(plugin.take_edits().is_empty(), "a state write drops earlier edits");
    assert!(!plugin.controller().value_changes_pending(), "and the flag that says there are some");

    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 0.0).expect("controller write");
    plugin.halves_mut().1.set_parameter(param::SELF_EDIT, 1.0).expect("controller write");
    plugin.reset().expect("reset");
    assert!(plugin.take_edits().is_empty(), "a reset drops earlier edits");
}

#[test]
fn the_values_a_take_applied_are_listed_so_a_host_can_read_just_those_entries() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    plugin.queue_parameter_change(param::REPORT_FROM_PROCESS, 1.0).expect("queue");
    process_block(&mut plugin);
    let changes = plugin.take_value_changes();
    assert_eq!(changes.reported_indices(), &[param::MODE]);
}
