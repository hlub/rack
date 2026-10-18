//! Tests of the two state reads against the fixture plugin: the controller
//! half's read that leaves the plugin processing, and the whole instance's
//! read that stops it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::PluginInstance;

use super::test_support::{load_fixture, parse_fixture_state, serial_plugin_test, Buffers, FIXTURE_BLOCK};
use super::Vst3Plugin;

#[test]
fn the_read_while_processing_has_the_suspending_reads_format_and_restores() {
    let _serial = serial_plugin_test();
    let Some(mut plugin) = load_fixture() else { return };

    let concurrent = plugin.controller().read_state_while_processing().expect("read");
    let suspending = plugin.get_state().expect("read");
    let concurrent_counters = parse_fixture_state(&concurrent);
    let suspending_counters = parse_fixture_state(&suspending);

    assert_eq!(concurrent.len(), suspending.len(), "the two reads write one format");
    assert_eq!(
        suspending_counters.set_processing_false_calls,
        concurrent_counters.set_processing_false_calls + 1,
        "only the suspending read leaves the Processing state"
    );
    plugin.set_state(&concurrent).expect("the read restores cleanly");
}

#[test]
fn reads_while_the_processor_processes_never_stop_it() {
    let _serial = serial_plugin_test();
    let Some(plugin) = load_fixture() else { return };

    let (mut processor, controller) = plugin.split().expect("an initialized instance splits");
    let stop = Arc::new(AtomicBool::new(false));
    let audio = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            let mut buffers = Buffers::new(processor.input_channels(), processor.output_channels());
            while !stop.load(Ordering::Relaxed) {
                buffers
                    .with(|inputs, outputs| processor.process(inputs, outputs, FIXTURE_BLOCK))
                    .expect("the fixture should process");
            }
            processor
        }
    });

    let reads: Vec<_> = (0..200)
        .map(|_| parse_fixture_state(&controller.read_state_while_processing().expect("read")))
        .collect();
    stop.store(true, Ordering::Relaxed);
    let processor = audio.join().expect("the processing thread should finish");

    let first = &reads[0];
    let last = &reads[reads.len() - 1];
    assert!(
        last.process_calls > first.process_calls,
        "processing went on across the reads: {} then {}",
        first.process_calls,
        last.process_calls
    );
    assert!(
        reads.iter().all(|read| read.set_processing_false_calls == first.set_processing_false_calls),
        "no read stopped processing"
    );

    let plugin = Vst3Plugin::join(processor, controller).expect("the halves join");
    let control = parse_fixture_state(&plugin.get_state().expect("read"));
    assert_eq!(
        control.set_processing_false_calls,
        first.set_processing_false_calls + 1,
        "the suspending read, as a control, does stop processing"
    );
}
