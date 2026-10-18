//! Tests of the split into a processor half and a controller half, against
//! the fixture plugin: what each half's calls really reach, how the halves
//! are dropped, and that the two run at the same time.

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::{PluginInstance, PluginScanner};

use super::test_support::{
    fixture_info, load_fixture, param, parse_fixture_state, serial_plugin_test, Buffers,
    FixtureCounters, FIXTURE_BLOCK,
};
use super::{leaked_instance_count, live_instance_count, Vst3Plugin, Vst3Processor};

/// One block through a processor half.
fn process_one_block(processor: &mut Vst3Processor) {
    let mut buffers = Buffers::new(processor.input_channels(), processor.output_channels());
    buffers
        .with(|inputs, outputs| processor.process(inputs, outputs, FIXTURE_BLOCK))
        .expect("the fixture should process");
}

/// What the fixture has recorded so far, read through the whole instance.
fn counters(plugin: &Vst3Plugin) -> FixtureCounters {
    parse_fixture_state(&plugin.get_state().expect("the fixture's state should read"))
}

#[test]
fn split_is_refused_before_initialize_and_gives_the_instance_back() {
    let _serial = serial_plugin_test();
    let Some((scanner, info)) = fixture_info() else { return };
    let plugin = scanner.load(&info).expect("the fixture should load");

    let refused = plugin.split().expect_err("an uninitialized instance must not split");
    assert!(!refused.is_initialized(), "the instance comes back as it was");
}

#[test]
fn the_processor_half_processes_on_another_thread_while_the_controller_is_used_here() {
    let _serial = serial_plugin_test();
    let live_before = live_instance_count();
    let leaked_before = leaked_instance_count();
    let Some(plugin) = load_fixture() else { return };
    assert_eq!(live_instance_count(), live_before + 1);

    let (mut processor, mut controller) = plugin.split().expect("an initialized instance splits");
    let audio = std::thread::spawn(move || {
        for block in 0..1000u32 {
            processor
                .queue_parameter_change(param::GAIN, (block % 100) as f32 / 100.0)
                .expect("the processor queues a parameter point");
            process_one_block(&mut processor);
        }
        processor
    });

    let mut sweeps = 0u32;
    while !audio.is_finished() || sweeps == 0 {
        let value = (sweeps % 10) as f32 / 10.0;
        controller.set_parameter(param::GAIN, value).expect("the controller writes");
        controller.get_parameter(param::GAIN).expect("the controller reads");
        controller.parameter_info(param::GAIN).expect("the controller describes");
        controller.parameter_value_string(param::GAIN, value).expect("the controller formats");
        controller.output_bus_info(1).expect("the controller reports a bus");
        assert_eq!(controller.parameter_count(), param::COUNT);
        sweeps += 1;
    }

    let processor = audio.join().expect("the processing thread should finish");
    let plugin = Vst3Plugin::join(processor, controller).expect("the halves of one instance join");
    let recorded = counters(&plugin);
    assert_eq!(recorded.process_calls, 1000, "every block on the other thread reached the plugin");
    assert_eq!(recorded.points[param::GAIN], 1000, "every queued point reached the processor");

    drop(plugin);
    assert_eq!(live_instance_count(), live_before, "dropping the joined whole frees the instance");
    assert_eq!(leaked_instance_count(), leaked_before, "nothing leaked");
}

#[test]
fn dropping_the_processor_half_then_the_controller_half_frees_the_instance() {
    let _serial = serial_plugin_test();
    let live_before = live_instance_count();
    let leaked_before = leaked_instance_count();
    let Some(plugin) = load_fixture() else { return };

    let (processor, controller) = plugin.split().expect("an initialized instance splits");
    drop(processor);
    assert_eq!(live_instance_count(), live_before + 1, "the processor half alone frees nothing");
    drop(controller);
    assert_eq!(live_instance_count(), live_before, "the controller half frees it after");
    assert_eq!(leaked_instance_count(), leaked_before, "nothing leaked");
}

#[test]
fn dropping_the_controller_half_first_leaks_and_counts_the_instance() {
    let _serial = serial_plugin_test();
    let live_before = live_instance_count();
    let leaked_before = leaked_instance_count();
    let Some(plugin) = load_fixture() else { return };

    let (processor, controller) = plugin.split().expect("an initialized instance splits");
    // A debug build asserts against this drop order; a release build only
    // counts it. Either way the instance must not be freed under the
    // processor half.
    let outcome = catch_unwind(AssertUnwindSafe(move || drop(controller)));
    assert_eq!(outcome.is_err(), cfg!(debug_assertions), "only a debug build asserts");
    assert_eq!(leaked_instance_count(), leaked_before + 1, "the leak is counted");

    drop(processor);
    assert_eq!(
        live_instance_count(),
        live_before + 1,
        "the leaked instance is never freed, not even by the processor half's drop"
    );
}

#[test]
fn join_refuses_halves_of_two_instances_and_gives_both_back() {
    let _serial = serial_plugin_test();
    let live_before = live_instance_count();
    let (Some(first), Some(second)) = (load_fixture(), load_fixture()) else { return };
    let (first_processor, first_controller) = first.split().expect("the first splits");
    let (second_processor, second_controller) = second.split().expect("the second splits");

    let mismatch = Vst3Plugin::join(first_processor, second_controller)
        .expect_err("halves of two instances must not join");
    let first = Vst3Plugin::join(mismatch.processor, first_controller)
        .expect("the first processor comes back and joins its own controller");
    let second = Vst3Plugin::join(second_processor, mismatch.controller)
        .expect("the second controller comes back and joins its own processor");

    drop(first);
    drop(second);
    assert_eq!(live_instance_count(), live_before, "both instances are freed");
}

#[test]
fn the_controller_halfs_write_reaches_the_controller_and_not_the_processor() {
    let _serial = serial_plugin_test();
    let Some(plugin) = load_fixture() else { return };

    let (mut processor, mut controller) = plugin.split().expect("an initialized instance splits");
    controller.set_parameter(param::GAIN, 0.25).expect("the controller writes");
    process_one_block(&mut processor);
    let plugin = Vst3Plugin::join(processor, controller).expect("the halves join");

    assert_eq!(counters(&plugin).points[param::GAIN], 0, "the processor was not told");
    assert_eq!(plugin.get_parameter(param::GAIN).expect("read back"), 0.25, "the controller was");
}

#[test]
fn the_whole_instances_write_reaches_both_halves() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };

    plugin.set_parameter(param::GAIN, 0.75).expect("the whole writes");
    process_one_block(plugin.processor_mut());

    assert_eq!(counters(&plugin).points[param::GAIN], 1, "the processor received one point");
    assert_eq!(plugin.get_parameter(param::GAIN).expect("read back"), 0.75, "the controller holds it");
}

#[test]
fn a_queued_parameter_change_reaches_the_processor_and_not_the_controller() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let before = plugin.get_parameter(param::GAIN).expect("read");

    plugin.queue_parameter_change(param::GAIN, 0.5).expect("queue");
    process_one_block(plugin.processor_mut());

    assert_eq!(counters(&plugin).points[param::GAIN], 1, "the processor received one point");
    assert_eq!(plugin.get_parameter(param::GAIN).expect("read"), before, "the controller was not told");
}
