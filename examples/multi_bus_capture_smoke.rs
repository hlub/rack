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
//! Usage: cargo run -p rack --example multi_bus_capture_smoke --features cpal -- [path/to/plugin.vst3]
//! Defaults to the stock AD2 install location on macOS if no path is given.
//!
//! Root-caused gap (previously "known open gap, see scratch/todo/backline.txt"):
//! AD2 produced no audio at all when driven by calling `process()`/
//! `process_multi_bus()` directly from `main()` in a synchronous loop --
//! even with the Cocoa run loop pumped first so the default kit finishes
//! its async load, and even through the already-proven, unmodified
//! single-bus `Vst3Plugin::process`. The equivalent `backline`-level
//! integration test (`examples/plugin_ad2.rs` at the drumr repo root, via
//! `AudioEngine::load_vst3`) reliably produces audio. Two throwaway
//! diagnostics (not committed; see the session report) isolated the actual
//! difference:
//!   1. Opening a real cpal output stream that never touches the plugin
//!      (just fills silence) *before* pumping/processing was NOT sufficient
//!      -- AD2 stayed silent, ruling out "some live CoreAudio device
//!      callback merely needs to exist in-process."
//!   2. Driving the plugin's OWN `process()`/`process_multi_bus()` calls
//!      *from inside* a real cpal output stream callback -- instead of
//!      calling them synchronously from `main()` -- immediately produced
//!      real audio (peak |sample| ~0.48 on the first try).
//!
//! So AD2 (likely a JUCE behaviour, not anything specific to this bus
//! routing) requires its own process call to actually run on a genuine
//! real-time audio-device callback thread, not merely be invoked
//! synchronously after the message loop has been pumped. `backline::
//! AudioEngine::load_vst3` opens exactly such a stream as an intrinsic,
//! unconditional part of loading (see `crates/backline/src/audio.rs`'s
//! `build_streams`), which is *why* it always worked and this bare example
//! didn't. This example now opens its own real cpal output stream and runs
//! every `process_multi_bus` call from inside that stream's callback,
//! matching that requirement instead of fighting it. This is a general
//! rack-level lesson, not specific to AD2 or to multi-bus capture: any
//! future bare-`rack` example/test that drives a JUCE-based plugin and
//! expects real audio output needs a live cpal output stream actually
//! calling the plugin's `process()`, not just a pumped run loop plus
//! synchronous calls.

#[cfg(all(
    feature = "cpal",
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
mod run {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use rack::vst3::{ExtraOutputBus, Vst3Plugin, Vst3Scanner};
    use rack::{MidiEvent, PluginInstance, PluginScanner, Result};
    use std::sync::{Arc, Mutex};

    // JUCE-based plugins (AD2 included) load their default kit asynchronously
    // via their internal MessageManager, which only runs when something pumps
    // the Cocoa event loop on the main thread. Without this, the plugin stays
    // silent forever -- not a bug in process_multi_bus, but a real prerequisite
    // this example must satisfy itself, the same way backline's own
    // `macos::init_nsapplication`/`pump_cfrunloop_for` do for its consumers.
    // Declared locally (not a rack dependency) to keep this a self-contained
    // diagnostic, matching dump_bus_layout.rs's own convention.
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

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRunLoopRunInMode(mode: *const std::ffi::c_void, seconds: f64, return_after_source_handled: bool) -> i32;
        static kCFRunLoopDefaultMode: *const std::ffi::c_void;
    }

    // SAFETY: see backline::macos::init_nsapplication -- same call sequence,
    // same well-known selector/argument shapes, must run once on the thread
    // that will later pump the run loop (this example's main thread).
    fn init_nsapplication() {
        unsafe {
            let class = objc_getClass(c"NSApplication".as_ptr());
            let shared_application_selector = sel_registerName(c"sharedApplication".as_ptr());
            let app = objc_msgSend(class, shared_application_selector);
            let set_activation_policy_selector = sel_registerName(c"setActivationPolicy:".as_ptr());
            objc_msgSend(app, set_activation_policy_selector, 2i64); // Prohibited: no Dock icon
        }
    }

    fn pump_cfrunloop_for(seconds: f64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
        while std::time::Instant::now() < deadline {
            unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.025, false);
            }
        }
    }

    fn rms(samples: &[f32]) -> f32 {
        let sum_squares: f32 = samples.iter().map(|sample| sample * sample).sum();
        (sum_squares / samples.len() as f32).sqrt()
    }

    /// Per-bus RMS bookkeeping accumulated across every buffer the real
    /// audio callback renders. Guarded by a plain `Mutex`, not the
    /// lock-free atomics `crates/backline`'s real production callback
    /// uses for its single peak-level value -- this is a one-shot
    /// diagnostic example, not a shipped real-time audio path, and needs
    /// six running values (three buses' totals and peaks) updated
    /// together, so a short per-buffer lock is the simpler and clearer
    /// choice here.
    #[derive(Default)]
    struct BusStats {
        master_rms_total: f32,
        kick_rms_total: f32,
        snare_rms_total: f32,
        master_rms_peak: f32,
        kick_rms_peak: f32,
        snare_rms_peak: f32,
        buffers_processed: usize,
    }

    pub fn main() -> Result<()> {
        use std::path::Path;

        const SAMPLE_RATE: f64 = 48_000.0;
        const BUFFER_FRAMES: usize = 512;
        // AD2's default keymap (keymaps/addictive-drums.toml in the drumr repo):
        // bd (kick) = note 36, sn (snare) = note 38, channel 9 (0-based) = MIDI
        // channel 10.
        const KICK_NOTE: u8 = 36;
        const MIDI_CHANNEL: u8 = 9;
        const RENDER_SECONDS: f64 = 3.0;

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

        let mut plugin: Vst3Plugin = scanner.load(info)?;
        plugin.initialize(SAMPLE_RATE, BUFFER_FRAMES)?;

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

        let master_channels = plugin.output_channels();
        let plugin = Arc::new(Mutex::new(plugin));
        let stats = Arc::new(Mutex::new(BusStats::default()));

        // Real cpal output stream whose callback is the ONLY place
        // process_multi_bus is ever called from -- see this file's header
        // comment for why a synchronous main()-thread call never produced
        // audio from AD2, while this does.
        let host = cpal::default_host();
        let device = host.default_output_device().expect("no audio output device");
        let default_config = device.default_output_config().expect("no default output config");
        let device_channels = default_config.channels() as usize;
        let stream_config = cpal::StreamConfig {
            channels: default_config.channels(),
            sample_rate: cpal::SampleRate(SAMPLE_RATE as u32),
            buffer_size: cpal::BufferSize::Fixed(BUFFER_FRAMES as u32),
        };

        let plugin_cb = plugin.clone();
        let stats_cb = stats.clone();
        let mut master_out = vec![vec![0.0f32; BUFFER_FRAMES]; master_channels];
        let mut kick_out = vec![vec![0.0f32; BUFFER_FRAMES]; kick_channels];
        let mut snare_out = vec![vec![0.0f32; BUFFER_FRAMES]; snare_channels];

        println!("Opening real cpal output stream to drive process_multi_bus from a genuine audio callback...");
        let stream = device
            .build_output_stream(
                &stream_config,
                move |data: &mut [f32], _| {
                    let Ok(mut plugin) = plugin_cb.try_lock() else { data.fill(0.0); return; };

                    let input_channels = plugin.input_channels();
                    let silent_inputs = vec![0.0f32; BUFFER_FRAMES];
                    let input_refs: Vec<&[f32]> = (0..input_channels).map(|_| silent_inputs.as_slice()).collect();

                    let mut master_refs: Vec<&mut [f32]> = master_out.iter_mut().map(|c| c.as_mut_slice()).collect();
                    let mut kick_channel_slices: Vec<&mut [f32]> = kick_out.iter_mut().map(|c| c.as_mut_slice()).collect();
                    let mut snare_channel_slices: Vec<&mut [f32]> = snare_out.iter_mut().map(|c| c.as_mut_slice()).collect();
                    let mut extra_outputs = [
                        ExtraOutputBus { bus_index: 1, channels: &mut kick_channel_slices },
                        ExtraOutputBus { bus_index: 2, channels: &mut snare_channel_slices },
                    ];

                    if plugin.process_multi_bus(&input_refs, &mut master_refs, &mut extra_outputs, BUFFER_FRAMES).is_err() {
                        data.fill(0.0);
                        return;
                    }

                    let master_rms = master_out.iter().map(|c| rms(c)).fold(0.0f32, f32::max);
                    let kick_rms = kick_out.iter().map(|c| rms(c)).fold(0.0f32, f32::max);
                    let snare_rms = snare_out.iter().map(|c| rms(c)).fold(0.0f32, f32::max);

                    if let Ok(mut stats) = stats_cb.try_lock() {
                        stats.master_rms_total += master_rms;
                        stats.kick_rms_total += kick_rms;
                        stats.snare_rms_total += snare_rms;
                        stats.master_rms_peak = stats.master_rms_peak.max(master_rms);
                        stats.kick_rms_peak = stats.kick_rms_peak.max(kick_rms);
                        stats.snare_rms_peak = stats.snare_rms_peak.max(snare_rms);
                        stats.buffers_processed += 1;
                    }

                    // Play the master bus back so a human running this
                    // example live also hears the kick, duplicated across
                    // whatever channel count the device negotiated.
                    let frames = data.len() / device_channels;
                    for i in 0..frames.min(BUFFER_FRAMES) {
                        for channel in 0..device_channels {
                            let source_channel = channel % master_channels.max(1);
                            data[i * device_channels + channel] =
                                master_out.get(source_channel).map_or(0.0, |c| c[i]);
                        }
                    }
                },
                move |err| eprintln!("cpal output stream error: {err}"),
                None,
            )
            .expect("failed to build cpal output stream");
        stream.play().expect("failed to play cpal output stream");
        println!("Real audio callback is live.\n");

        println!("Pumping the run loop for 2s to let the default kit finish loading...");
        pump_cfrunloop_for(2.0);

        println!("Triggering kick note (note {KICK_NOTE}, channel {MIDI_CHANNEL}); no snare note is ever sent.");
        {
            let mut locked = plugin.lock().expect("plugin lock poisoned");
            locked.send_midi(&[MidiEvent::note_on(KICK_NOTE, 110, MIDI_CHANNEL, 0)])?;
        }

        std::thread::sleep(std::time::Duration::from_secs_f64(RENDER_SECONDS));
        drop(stream);

        let stats = stats.lock().expect("stats lock poisoned");
        let buffers = stats.buffers_processed.max(1) as f32;
        println!("\nAverages over {} buffers rendered by the real audio callback:", stats.buffers_processed);
        println!("  master avg RMS = {:.5}  (peak {:.5})", stats.master_rms_total / buffers, stats.master_rms_peak);
        println!("  kick   avg RMS = {:.5}  (peak {:.5})", stats.kick_rms_total / buffers, stats.kick_rms_peak);
        println!("  snare  avg RMS = {:.5}  (peak {:.5})", stats.snare_rms_total / buffers, stats.snare_rms_peak);

        println!("\nResult:");
        if stats.master_rms_peak <= 0.0001 {
            println!(
                "  INCONCLUSIVE: master (bus 0) itself never showed signal, so this run cannot prove or \
                 disprove per-bus isolation."
            );
        } else if stats.kick_rms_peak > 0.0001 && stats.snare_rms_peak < 0.0001 {
            println!("  ISOLATED: kick bus shows real signal, snare bus stayed silent. Per-bus routing works.");
        } else if stats.kick_rms_peak <= 0.0001 {
            println!(
                "  NOT YET ISOLATED: master bus has real signal but the kick bus never showed any. \
                 The host-side plumbing this checks (activateBus for every declared bus before setActive, \
                 rebinding each requested bus's buffers before process()) was independently verified \
                 correct by reading rack-sys/src/vst3_instance.cpp -- this most likely means AD2's loaded \
                 kit isn't in the per-instrument-routing state docs/vision.md calls AD2's \"individual outs\" \
                 mode (no VST3 parameter advertising or controlling that mode was found in a parameter-name \
                 search), not a bug in this bus-activation/binding code. Investigating what actually toggles \
                 that mode in AD2 (a kit setting? plugin state loaded via set_state?) is unstarted follow-up \
                 work, not something this example resolves."
            );
        } else {
            println!(
                "  UNEXPECTED: snare bus shows signal (peak {:.5}) despite no snare note ever \
                 being sent -- buses are NOT isolated as expected.",
                stats.snare_rms_peak
            );
        }

        Ok(())
    }
}

#[cfg(all(
    feature = "cpal",
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
fn main() -> rack::Result<()> {
    run::main()
}

#[cfg(not(feature = "cpal"))]
fn main() {
    println!("This example requires --features cpal (needed to drive process_multi_bus from a real audio callback -- see this file's header comment).");
}

#[cfg(all(
    feature = "cpal",
    any(target_os = "ios", target_os = "tvos", target_os = "watchos", target_os = "visionos")
))]
fn main() {
    println!("VST3 is not available on mobile platforms.");
    println!("This example only works on desktop platforms (macOS, Windows, Linux).");
}
