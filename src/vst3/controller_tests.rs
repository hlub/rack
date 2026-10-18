//! Tests of what the controller half learns from the plugin after
//! initialization, against the fixture plugin: restart requests, refreshed
//! parameter descriptions, and the messages between component and controller.

use crate::{ParameterInfo, PluginInstance};

use super::controller::changed_parameters;
use super::test_support::{load_fixture, param, parse_fixture_state, serial_plugin_test, Buffers, FIXTURE_BLOCK};
use super::{DiscardedMessages, TitleRefresh, Vst3Plugin, Vst3Processor};

/// Asks the fixture's controller to raise exactly `flags`.
fn request_restart(plugin: &mut Vst3Plugin, flags: u32) {
    let value = flags as f32 / param::RESTART_FLAG_STEPS;
    plugin.halves_mut().1.set_parameter(param::REQUEST_RESTART, value).expect("controller write");
}

#[test]
fn every_restart_flag_the_plugin_raises_is_taken_once() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    assert!(plugin.take_restart_flags().is_empty(), "initialization raised nothing");

    for bit in 0..12 {
        let flags = 1u32 << bit;
        request_restart(&mut plugin, flags);
        assert_eq!(plugin.controller().take_restart_flags().bits(), flags, "flag {flags:#x}");
        assert!(plugin.controller().take_restart_flags().is_empty(), "a second take is empty");
    }

    request_restart(&mut plugin, 0b1001);
    request_restart(&mut plugin, 0b0110);
    assert_eq!(plugin.take_restart_flags().bits(), 0b1111, "requests between takes are ORed");
}

/// Processes one block with a queued gain point, and returns how many gain
/// points the fixture has received in all.
fn queued_gain_points(plugin: &mut Vst3Plugin) -> u64 {
    plugin.queue_parameter_change(param::GAIN, 0.5).expect("queue");
    let mut buffers = Buffers::new(plugin.input_channels(), plugin.output_channels());
    buffers
        .with(|inputs, outputs| plugin.process(inputs, outputs, FIXTURE_BLOCK))
        .expect("process");
    parse_fixture_state(&plugin.get_state().expect("state")).points[param::GAIN]
}

#[test]
fn a_renamed_title_is_refreshed_into_the_controller_half() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    assert_eq!(plugin.parameter_info(param::GAIN).expect("info").name, "Gain");

    plugin.halves_mut().1.set_parameter(param::RENAME_TITLES, 1.0).expect("controller write");
    assert!(plugin.take_restart_flags().param_titles_changed(), "the plugin asked for a title refresh");
    assert_eq!(
        plugin.parameter_info(param::GAIN).expect("info").name,
        "Gain",
        "nothing changes until the host refreshes"
    );

    let refreshed = plugin.halves_mut().1.refresh_parameter_titles().expect("refresh");
    assert_eq!(refreshed, TitleRefresh::Changed(vec![param::GAIN]));
    assert_eq!(plugin.parameter_info(param::GAIN).expect("info").name, "Gain Renamed");
    assert_eq!(
        plugin.halves_mut().1.refresh_parameter_titles().expect("refresh"),
        TitleRefresh::Unchanged,
        "a second refresh finds nothing new"
    );
    assert_eq!(queued_gain_points(&mut plugin), 1, "the processor still resolves the index");
}

#[test]
fn changed_parameter_ids_refresh_nothing() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };
    let before: Vec<ParameterInfo> =
        (0..plugin.parameter_count()).map(|index| plugin.parameter_info(index).expect("info")).collect();

    plugin.halves_mut().1.set_parameter(param::CHANGE_IDS, 1.0).expect("controller write");
    let refreshed = plugin.halves_mut().1.refresh_parameter_titles().expect("refresh");
    assert_eq!(refreshed, TitleRefresh::IdsChanged);

    let after: Vec<ParameterInfo> =
        (0..plugin.parameter_count()).map(|index| plugin.parameter_info(index).expect("info")).collect();
    assert_eq!(after, before, "the instance keeps the list it had");
    assert_eq!(queued_gain_points(&mut plugin), 1, "the processor still resolves the index");
}

/// Has the fixture's component send one message from inside its next
/// `process`, on the thread that processes.
fn send_from_process(processor: &mut Vst3Processor) {
    processor.queue_parameter_change(param::SEND_FROM_PROCESS, 1.0).expect("queue");
    let mut buffers = Buffers::new(processor.input_channels(), processor.output_channels());
    buffers
        .with(|inputs, outputs| processor.process(inputs, outputs, FIXTURE_BLOCK))
        .expect("process");
}

#[test]
fn the_message_sent_while_connecting_is_delivered() {
    let _serial = serial_plugin_test();
    let Some(plugin) = load_fixture() else { return };

    let recorded = parse_fixture_state(&plugin.get_state().expect("state"));
    assert_eq!(recorded.component_messages, 1, "the controller's connect-time message arrived");
    assert_eq!(plugin.discarded_messages(), DiscardedMessages::default());
}

#[test]
fn a_message_sent_from_process_on_the_audio_thread_is_discarded_and_counted() {
    let _serial = serial_plugin_test();
    let Some(plugin) = load_fixture() else { return };

    let (processor, controller) = plugin.split().expect("an initialized instance splits");
    let audio = std::thread::spawn(move || {
        let mut processor = processor;
        send_from_process(&mut processor);
        processor
    });
    let processor = audio.join().expect("the processing thread should finish");
    let plugin = Vst3Plugin::join(processor, controller).expect("the halves join");

    assert_eq!(
        plugin.discarded_messages(),
        DiscardedMessages { component_to_controller: 1, controller_to_component: 0 }
    );
    assert_eq!(
        parse_fixture_state(&plugin.get_state().expect("state")).controller_messages,
        0,
        "nothing reached the controller"
    );
}

#[test]
fn a_message_sent_from_process_on_the_constructing_thread_is_delivered() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };

    send_from_process(plugin.processor_mut());

    assert_eq!(plugin.discarded_messages(), DiscardedMessages::default());
    assert_eq!(
        parse_fixture_state(&plugin.get_state().expect("state")).controller_messages,
        1,
        "the same message, sent on the thread the proxy belongs to, is delivered"
    );
}

fn described(index: usize, name: &str) -> ParameterInfo {
    ParameterInfo {
        index,
        name: name.to_string(),
        min: 0.0,
        max: 1.0,
        default: 0.0,
        unit: String::new(),
        step_count: 0,
        can_automate: true,
        is_read_only: false,
        is_list: false,
        is_hidden: false,
    }
}

#[test]
fn changed_parameters_names_each_index_whose_description_differs() {
    let before = vec![described(0, "Gain"), described(1, "Pan"), described(2, "Mix")];
    assert!(changed_parameters(&before, &before).is_empty(), "identical lists change nothing");

    let mut after = before.clone();
    after[0].name = "Level".to_string();
    after[2].step_count = 4;
    assert_eq!(changed_parameters(&before, &after), vec![0, 2]);
}
