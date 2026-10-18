//! Works the processor/controller split against real installed plugins, one
//! at a time: each named plugin is initialized, split, processed for 400
//! blocks on a spawned thread (a note every 50) while this thread sweeps the
//! controller (parameter writes and reads, display text), then read for state
//! without stopping it, joined and dropped. Prints per plugin: the controller
//! calls made meanwhile, the output peak, the messages the connection proxy
//! discarded, the restart flags raised while it was configured and while it
//! played, and the state read's size; and at the end the live and leaked
//! instance counts, which should both be 0.
//!
//! It calls a real plugin's controller beside its own processing, which is
//! the case a plugin may not be ready for, so run it alone:
//!
//!   cargo run --example split_probe --features vst3 -- "Surge XT" "Addictive Drums 2"

use rack::prelude::*;
use rack::vst3::{leaked_instance_count, live_instance_count, Vst3Plugin, Vst3Processor, Vst3Scanner};
use std::time::Instant;

const BLOCK: usize = 512;

/// Processes 400 blocks, a note every 50, and returns the half and the peak.
fn play(mut processor: Vst3Processor) -> (Vst3Processor, f32) {
    let inputs: Vec<Vec<f32>> = (0..processor.input_channels()).map(|_| vec![0.0; BLOCK]).collect();
    let mut outputs: Vec<Vec<f32>> = (0..processor.output_channels()).map(|_| vec![0.0; BLOCK]).collect();
    let mut peak = 0.0f32;
    for block in 0..400 {
        if block % 50 == 0 {
            let note = MidiEvent { sample_offset: 0, kind: MidiEventKind::NoteOn { note: 38, velocity: 110, channel: 0 } };
            let _ = processor.send_midi(&[note]);
        }
        let input_slices: Vec<&[f32]> = inputs.iter().map(Vec::as_slice).collect();
        let mut output_slices: Vec<&mut [f32]> = outputs.iter_mut().map(Vec::as_mut_slice).collect();
        processor.process(&input_slices, &mut output_slices, BLOCK).expect("process");
        for channel in &outputs {
            for sample in channel {
                peak = peak.max(sample.abs());
            }
        }
    }
    (processor, peak)
}

fn main() {
    let names: Vec<String> = std::env::args().skip(1).collect();
    let scanner = Vst3Scanner::new().expect("scanner");
    let plugins = scanner.scan().expect("scan");
    for name in names {
        let Some(info) = plugins.iter().find(|plugin| plugin.name == name) else {
            println!("{name}: not installed");
            continue;
        };
        let mut plugin = scanner.load(info).expect("load");
        plugin.initialize(48000.0, BLOCK, ProcessMode::Realtime).expect("initialize");
        let configuration_flags = plugin.take_restart_flags();
        let parameters = plugin.parameter_count();
        let (processor, mut controller) = plugin.split().expect("split");
        let audio = std::thread::spawn(move || play(processor));

        let start = Instant::now();
        let mut sweeps = 0usize;
        while !audio.is_finished() {
            if parameters > 0 {
                let index = sweeps % parameters;
                if controller.parameter_info(index).is_ok_and(|parameter| !parameter.is_read_only) {
                    let before = controller.get_parameter(index).unwrap_or(0.0);
                    let _ = controller.set_parameter(index, if before < 0.5 { 0.6 } else { 0.4 });
                    let _ = controller.get_parameter(index);
                    let _ = controller.set_parameter(index, before);
                }
            }
            let _ = controller.parameter_value_string(0, 0.5);
            sweeps += 1;
        }
        let (processor, peak) = audio.join().expect("the processing thread");
        let state = controller.read_state_while_processing().map(|bytes| bytes.len());
        let plugin = Vst3Plugin::join(processor, controller).expect("join");
        println!(
            "{name}: {parameters} parameters, {sweeps} controller sweeps in {:?}, peak {peak:.3}, \
             discarded {:?}, restart flags while configured {:#x} and while playing {:#x}, \
             state read while processing {state:?}",
            start.elapsed(),
            plugin.discarded_messages(),
            configuration_flags.bits(),
            plugin.take_restart_flags().bits(),
        );
    }
    println!("live instances {}, leaked {}", live_instance_count(), leaked_instance_count());
}
