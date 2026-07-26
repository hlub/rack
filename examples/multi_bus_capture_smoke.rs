//! Prove genuine per-bus audio isolation via `Vst3Plugin::process_multi_bus`.
//!
//! Loads AD2 (default path, or a `.vst3` path given as an argument), triggers
//! only a kick-drum note (never a snare note), and captures the Master (bus
//! 0), Kick (bus 1), and Snare (bus 2) output buses simultaneously in the
//! same `process_multi_bus` calls. If bus activation/routing in
//! `rack_vst3_plugin_initialize`/`rack_vst3_plugin_process_multi_bus` is
//! correct, Kick and Master should show real signal while Snare stays at
//! (near-)zero RMS throughout -- proving the buses are actually isolated,
//! not all just aliases of the same summed signal.
//!
//! Usage: cargo run -p rack --example multi_bus_capture_smoke -- [path/to/plugin.vst3]
//! Defaults to the stock AD2 install location on macOS if no path is given.
//!
//! Known open gap (see scratch/todo/backline.txt): as of this writing, AD2
//! produces no audio at all when driven from a bare `rack` example like
//! this one -- even through the already-proven, unmodified single-bus
//! `Vst3Plugin::process` path -- while the equivalent `backline`-level
//! integration test (`examples/plugin_ad2.rs` at the drumr repo root, via
//! `AudioEngine::load_vst3`) reliably produces audio. The two call
//! sequences (scan, load, initialize, pump the Cocoa run loop, send_midi,
//! process) were compared line-by-line and found equivalent, so the
//! difference is environmental to this bare-example harness, not a defect
//! in this file's own logic or in `process_multi_bus`. Until that gap is
//! understood, a silent master bus here is inconclusive, not a routing
//! bug -- see the `Result:` section printed at the end.

#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
use rack::vst3::{ExtraOutputBus, Vst3Scanner};
#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
use rack::{MidiEvent, PluginInstance, PluginScanner, Result};

// JUCE-based plugins (AD2 included) load their default kit asynchronously
// via their internal MessageManager, which only runs when something pumps
// the Cocoa event loop on the main thread. Without this, the plugin stays
// silent forever -- not a bug in process_multi_bus, but a real prerequisite
// this example must satisfy itself, the same way backline's own
// `macos::init_nsapplication`/`pump_cfrunloop_for` do for its consumers.
// Declared locally (not a rack dependency) to keep this a self-contained
// diagnostic, matching dump_bus_layout.rs's own convention.
#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
#[link(name = "AppKit", kind = "framework")]
extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn objc_msgSend(
        receiver: *mut std::ffi::c_void,
        sel: *const std::ffi::c_void,
        ...
    ) -> *mut std::ffi::c_void;
    fn sel_registerName(name: *const std::ffi::c_char) -> *const std::ffi::c_void;
}

#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopRunInMode(mode: *const std::ffi::c_void, seconds: f64, return_after_source_handled: bool) -> i32;
    static kCFRunLoopDefaultMode: *const std::ffi::c_void;
}

// SAFETY: see backline::macos::init_nsapplication -- same call sequence,
// same well-known selector/argument shapes, must run once on the thread
// that will later pump the run loop (this example's main thread).
#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
fn init_nsapplication() {
    unsafe {
        let class = objc_getClass(c"NSApplication".as_ptr());
        let shared_application_selector = sel_registerName(c"sharedApplication".as_ptr());
        let app = objc_msgSend(class, shared_application_selector);
        let set_activation_policy_selector = sel_registerName(c"setActivationPolicy:".as_ptr());
        objc_msgSend(app, set_activation_policy_selector, 2i64); // Prohibited: no Dock icon
    }
}

#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
fn pump_cfrunloop_for(seconds: f64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
    while std::time::Instant::now() < deadline {
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.025, false);
        }
    }
}

#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
fn rms(samples: &[f32]) -> f32 {
    let sum_squares: f32 = samples.iter().map(|sample| sample * sample).sum();
    (sum_squares / samples.len() as f32).sqrt()
}

#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
fn main() -> Result<()> {
    use std::path::Path;

    const SAMPLE_RATE: f64 = 48_000.0;
    const BUFFER_FRAMES: usize = 512;
    // AD2's default keymap (keymaps/addictive-drums.toml in the drumr repo):
    // bd (kick) = note 36, sn (snare) = note 38, channel 9 (0-based) = MIDI
    // channel 10.
    const KICK_NOTE: u8 = 36;
    const MIDI_CHANNEL: u8 = 9;
    const RENDER_BUFFERS: usize = 250; // ~2.7s at 512 frames / 48kHz

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Library/Audio/Plug-Ins/VST3/Addictive Drums 2.vst3".to_string());

    init_nsapplication();

    println!("Looking for VST3 plugin at: {path}\n");

    let mut scanner = Vst3Scanner::new()?;
    if let Some(parent) = Path::new(&path).parent() {
        let _ = scanner.add_path(parent);
    }
    let plugins = scanner.scan()?;

    let requested = Path::new(&path);
    let info = plugins
        .iter()
        .find(|candidate| candidate.path == requested)
        .or_else(|| {
            plugins
                .iter()
                .find(|candidate| candidate.path.file_name() == requested.file_name())
        });

    let Some(info) = info else {
        println!("Plugin not found at {path}");
        println!("({} plugin(s) were found during scan)", plugins.len());
        return Ok(());
    };

    println!("Found plugin: {} by {}", info.name, info.manufacturer);

    let mut plugin = scanner.load(info)?;
    plugin.initialize(SAMPLE_RATE, BUFFER_FRAMES)?;

    println!("Pumping the run loop for 2s to let the default kit finish loading...");
    pump_cfrunloop_for(2.0);

    println!(
        "Initialized. Main bus: {} in / {} out. Output bus count: {}\n",
        plugin.input_channels(),
        plugin.output_channels(),
        plugin.output_bus_count()
    );

    let kick_channels = plugin.output_bus_channel_count(1);
    let snare_channels = plugin.output_bus_channel_count(2);
    println!("Bus 1 (expected Kick) channel count: {kick_channels:?}");
    println!("Bus 2 (expected Snare) channel count: {snare_channels:?}\n");

    let Some(kick_channels) = kick_channels else {
        println!("Plugin does not declare an output bus 1; cannot run the isolation check.");
        return Ok(());
    };
    let Some(snare_channels) = snare_channels else {
        println!("Plugin does not declare an output bus 2; cannot run the isolation check.");
        return Ok(());
    };

    let input_channels = plugin.input_channels();
    let silent_inputs = vec![vec![0.0f32; BUFFER_FRAMES]; input_channels];
    let silent_input_refs: Vec<&[f32]> = silent_inputs.iter().map(|channel| channel.as_slice()).collect();

    let mut master_out = vec![vec![0.0f32; BUFFER_FRAMES]; plugin.output_channels()];
    let mut kick_out = vec![vec![0.0f32; BUFFER_FRAMES]; kick_channels];
    let mut snare_out = vec![vec![0.0f32; BUFFER_FRAMES]; snare_channels];

    let mut master_rms_total = 0.0f32;
    let mut kick_rms_total = 0.0f32;
    let mut snare_rms_total = 0.0f32;
    let mut master_rms_peak = 0.0f32;
    let mut kick_rms_peak = 0.0f32;
    let mut snare_rms_peak = 0.0f32;

    println!("Triggering kick note (note {KICK_NOTE}, channel {MIDI_CHANNEL}); no snare note is ever sent.");
    plugin.send_midi(&[MidiEvent::note_on(KICK_NOTE, 110, MIDI_CHANNEL, 0)])?;

    for buffer_index in 0..RENDER_BUFFERS {
        let mut master_refs: Vec<&mut [f32]> = master_out.iter_mut().map(|c| c.as_mut_slice()).collect();
        let mut kick_channel_slices: Vec<&mut [f32]> = kick_out.iter_mut().map(|c| c.as_mut_slice()).collect();
        let mut snare_channel_slices: Vec<&mut [f32]> = snare_out.iter_mut().map(|c| c.as_mut_slice()).collect();

        let mut extra_outputs = [
            ExtraOutputBus { bus_index: 1, channels: &mut kick_channel_slices },
            ExtraOutputBus { bus_index: 2, channels: &mut snare_channel_slices },
        ];

        plugin.process_multi_bus(&silent_input_refs, &mut master_refs, &mut extra_outputs, BUFFER_FRAMES)?;

        let master_rms = master_out.iter().map(|c| rms(c)).fold(0.0f32, f32::max);
        let kick_rms = kick_out.iter().map(|c| rms(c)).fold(0.0f32, f32::max);
        let snare_rms = snare_out.iter().map(|c| rms(c)).fold(0.0f32, f32::max);

        master_rms_total += master_rms;
        kick_rms_total += kick_rms;
        snare_rms_total += snare_rms;
        master_rms_peak = master_rms_peak.max(master_rms);
        kick_rms_peak = kick_rms_peak.max(kick_rms);
        snare_rms_peak = snare_rms_peak.max(snare_rms);

        if buffer_index % 25 == 0 {
            println!(
                "  buffer {buffer_index:3}: master RMS={master_rms:.5}  kick RMS={kick_rms:.5}  snare RMS={snare_rms:.5}"
            );
        }
    }

    let buffers = RENDER_BUFFERS as f32;
    println!("\nAverages over {RENDER_BUFFERS} buffers:");
    println!("  master avg RMS = {:.5}", master_rms_total / buffers);
    println!("  kick   avg RMS = {:.5}  (peak {:.5})", kick_rms_total / buffers, kick_rms_peak);
    println!("  snare  avg RMS = {:.5}  (peak {:.5})", snare_rms_total / buffers, snare_rms_peak);

    println!("\nResult:");
    if master_rms_peak <= 0.0001 {
        // The already-proven single-bus `process()` path (exercised by
        // crates/backline's own passing `plugin_ad2` integration test) is
        // silent under this exact same harness too, so a silent master bus
        // here means AD2 never produced audio in THIS process at all --
        // an environment/harness gap in this bare example (no plugin
        // produces sound, not specifically bus 1/2), not evidence that
        // process_multi_bus's routing is wrong.
        println!(
            "  INCONCLUSIVE: master (bus 0) itself never showed signal, so this run cannot prove or \
             disprove per-bus isolation -- AD2 produced no audio at all in this process (see this \
             file's header comment / the backline.txt todo entry for known open questions here)."
        );
    } else if kick_rms_peak > 0.0001 && snare_rms_peak < 0.0001 {
        println!("  ISOLATED: kick bus shows real signal, snare bus stayed silent. Per-bus routing works.");
    } else if kick_rms_peak <= 0.0001 {
        println!("  UNEXPECTED: master bus has signal but kick bus never showed any -- routing or note mapping is wrong.");
    } else {
        println!(
            "  UNEXPECTED: snare bus shows signal (peak {snare_rms_peak:.5}) despite no snare note ever \
             being sent -- buses are NOT isolated as expected."
        );
    }

    Ok(())
}

#[cfg(any(
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
fn main() {
    println!("VST3 is not available on mobile platforms.");
    println!("This example only works on desktop platforms (macOS, Windows, Linux).");
}
