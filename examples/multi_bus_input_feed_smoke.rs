//! Prove that `Vst3Plugin::process_multi_bus`'s new `extra_inputs` parameter
//! actually reaches a real plugin's processing, not just the Rust-side
//! validation.
//!
//! # Why this doesn't test XLN Audio Drum Butter
//!
//! This example was originally scoped to feed one plugin's isolated output
//! bus (e.g. AD2's "Kick" bus) into a second plugin's matching-named input
//! bus (e.g. Drum Butter's "Kick" input), per `docs/vision.md`'s description
//! of Drum Butter as having "per-voice inputs". A `dump_bus_layout` run
//! against the installed `DB-30 Drum Butter.vst3` found only ONE input bus
//! ("In", 2 channels) and ONE output bus ("Out", 2 channels) -- no per-voice
//! buses at all. That assumption does not hold for the copy of Drum Butter
//! installed on this machine; this example does not use it.
//!
//! Scanning every other installed VST3 plugin for a genuine multi-input-bus
//! effect (via the same `dump_bus_layout` tool) found `soothe2.vst3`
//! (Oeksound Soothe2), which declares two INPUT buses: bus 0 "Main Input"
//! and bus 1 "Sidechain Input" (2 channels each). This example drives
//! Soothe2 and binds real audio to its Sidechain Input via `extra_inputs`.
//!
//! # Test design (and why it differs from the original phrasing)
//!
//! The original test design (written with Drum Butter's assumed
//! per-voice-passthrough architecture in mind) called for leaving the main
//! bus silent throughout and only ever toggling the extra bus, on the
//! premise that "the effect processes whatever comes in on that channel and
//! presumably outputs *something* audible in response, even if just passed
//! through." That premise does not transfer to Soothe2 (or to any generic
//! dynamics/EQ effect): a spectral shaper has nothing to shape when its main
//! input is silent, so feeding its sidechain input alone -- with main
//! silent -- would produce silent output in master regardless of whether
//! the sidechain plumbing works at all. That comparison would be
//! uninformative for this class of plugin, not just for Soothe2.
//!
//! So this example runs BOTH comparisons and reports both honestly:
//!
//! - Phases 1-2 ("literal" design): main bus silent throughout; sidechain
//!   fed a test tone in phase 1, silent in phase 2. Included for
//!   completeness against the original spec, but expected to be
//!   uninformative for exactly the reason above.
//! - Phases 3-4 ("adapted" design, the real proof): main bus fed a
//!   continuous 200 Hz test tone throughout so Soothe2 has real audio to
//!   process; sidechain silent in phase 3, fed the SAME 200 Hz tone (at
//!   higher amplitude) in phase 4. Soothe2 is a resonance suppressor: if its
//!   sidechain input is used for detection (a documented feature of this
//!   plugin) and the extra-input plumbing reaches it, driving the sidechain
//!   with energy at the same frequency as the main signal should trigger
//!   measurably different processing of the main signal between phase 3 and
//!   phase 4. If the two phases come out statistically indistinguishable,
//!   that is evidence the extra input isn't reaching the plugin's
//!   processing (or that this particular plugin's sidechain doesn't affect
//!   output the way assumed) -- reported as such, not spun as success.
//!
//! Before any of the above, this example also runs a handful of fast,
//! real-plugin-independent validation checks (bad bus_index, wrong
//! channel_count, duplicate bus_index) directly against
//! `process_multi_bus`'s Rust-side validation, which rejects all of these
//! before ever reaching the FFI boundary. This gives concrete evidence the
//! mechanism is at least correctly *validated*, independent of whatever the
//! real-audio phases show.
//!
//! Usage: cargo run -p rack --example multi_bus_input_feed_smoke --features cpal -- [path/to/plugin.vst3]
//! Defaults to the stock Soothe2 install location on macOS if no path is given.

#[cfg(all(
    feature = "cpal",
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
mod run {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use rack::vst3::{ExtraInputBus, ExtraOutputBus, Vst3Plugin, Vst3Scanner};
    use rack::{PluginInstance, PluginScanner, Result};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // JUCE-based plugins load asynchronously via their internal
    // MessageManager, which only runs when something pumps the Cocoa event
    // loop on the main thread; `multi_bus_capture_smoke.rs` root-caused this
    // as a general rack-level lesson (not specific to AD2), so this example
    // follows the same self-contained pattern rather than assuming Soothe2
    // doesn't need it.
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

    // SAFETY: see backline::macos::init_nsapplication / multi_bus_capture_smoke.rs
    // -- same call sequence, same well-known selector/argument shapes, must
    // run once on the thread that will later pump the run loop (this
    // example's main thread).
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

    /// Per-phase RMS bookkeeping for the master (bus 0) output, accumulated
    /// across every buffer the real audio callback renders during that
    /// phase. Guarded by a plain `Mutex` -- a one-shot diagnostic example,
    /// not a shipped real-time path, so a short per-buffer lock is fine.
    #[derive(Default, Clone, Copy)]
    struct PhaseStats {
        rms_total: f32,
        rms_peak: f32,
        buffers_processed: usize,
    }

    impl PhaseStats {
        fn record(&mut self, master_rms: f32) {
            self.rms_total += master_rms;
            self.rms_peak = self.rms_peak.max(master_rms);
            self.buffers_processed += 1;
        }

        fn avg(&self) -> f32 {
            self.rms_total / (self.buffers_processed.max(1) as f32)
        }
    }

    /// Which buses get fed a test tone during a given phase; `false` means
    /// that bus is fed complete silence during the phase.
    #[derive(Clone, Copy)]
    struct PhaseConfig {
        label: &'static str,
        main_active: bool,
        sidechain_active: bool,
    }

    const PHASES: [PhaseConfig; 4] = [
        PhaseConfig { label: "1: literal design, sidechain=tone, main=silent", main_active: false, sidechain_active: true },
        PhaseConfig { label: "2: literal design, sidechain=silent, main=silent", main_active: false, sidechain_active: false },
        PhaseConfig { label: "3: adapted design, main=tone, sidechain=silent", main_active: true, sidechain_active: false },
        PhaseConfig { label: "4: adapted design, main=tone, sidechain=tone", main_active: true, sidechain_active: true },
    ];

    /// Fast, real-plugin-independent validation checks: these hit
    /// `process_multi_bus`'s Rust-side validation (bad bus_index, wrong
    /// channel_count, duplicate bus_index), which rejects the request
    /// before ever touching the FFI boundary -- safe to call synchronously
    /// outside a real audio callback, unlike an actual processing call.
    fn run_validation_checks(plugin: &mut Vst3Plugin, main_channels: usize, master_channels: usize) {
        println!("Running fast, real-plugin-independent validation checks...");
        let frames = 64usize;
        let silent = vec![0f32; frames];
        let input_refs: Vec<&[f32]> = (0..main_channels).map(|_| silent.as_slice()).collect();
        let mut master_out = vec![vec![0f32; frames]; master_channels];

        // Case 1: bus_index 0 (main bus) rejected -- must go through `inputs`.
        {
            let mut master_refs: Vec<&mut [f32]> = master_out.iter_mut().map(|c| c.as_mut_slice()).collect();
            let bogus_channels: Vec<&[f32]> = vec![silent.as_slice(); 2];
            let extra_inputs = [ExtraInputBus { bus_index: 0, channels: &bogus_channels }];
            let mut extra_outputs: [ExtraOutputBus<'_>; 0] = [];
            let result = plugin.process_multi_bus(&input_refs, &extra_inputs, &mut master_refs, &mut extra_outputs, frames, None);
            println!("  [{}] bus_index=0 rejected", if result.is_err() { "PASS" } else { "FAIL" });
        }

        // Case 2: out-of-range bus_index rejected.
        {
            let mut master_refs: Vec<&mut [f32]> = master_out.iter_mut().map(|c| c.as_mut_slice()).collect();
            let bogus_channels: Vec<&[f32]> = vec![silent.as_slice(); 2];
            let extra_inputs = [ExtraInputBus { bus_index: 99, channels: &bogus_channels }];
            let mut extra_outputs: [ExtraOutputBus<'_>; 0] = [];
            let result = plugin.process_multi_bus(&input_refs, &extra_inputs, &mut master_refs, &mut extra_outputs, frames, None);
            println!("  [{}] out-of-range bus_index=99 rejected", if result.is_err() { "PASS" } else { "FAIL" });
        }

        // Case 3: wrong channel_count rejected (bus 1 declares 2 channels; try 1).
        {
            let mut master_refs: Vec<&mut [f32]> = master_out.iter_mut().map(|c| c.as_mut_slice()).collect();
            let bogus_channels: Vec<&[f32]> = vec![silent.as_slice(); 1];
            let extra_inputs = [ExtraInputBus { bus_index: 1, channels: &bogus_channels }];
            let mut extra_outputs: [ExtraOutputBus<'_>; 0] = [];
            let result = plugin.process_multi_bus(&input_refs, &extra_inputs, &mut master_refs, &mut extra_outputs, frames, None);
            println!("  [{}] wrong channel_count (1 instead of 2) rejected", if result.is_err() { "PASS" } else { "FAIL" });
        }

        // Case 4: duplicate bus_index rejected.
        {
            let mut master_refs: Vec<&mut [f32]> = master_out.iter_mut().map(|c| c.as_mut_slice()).collect();
            let channels_a: Vec<&[f32]> = vec![silent.as_slice(); 2];
            let channels_b: Vec<&[f32]> = vec![silent.as_slice(); 2];
            let extra_inputs = [
                ExtraInputBus { bus_index: 1, channels: &channels_a },
                ExtraInputBus { bus_index: 1, channels: &channels_b },
            ];
            let mut extra_outputs: [ExtraOutputBus<'_>; 0] = [];
            let result = plugin.process_multi_bus(&input_refs, &extra_inputs, &mut master_refs, &mut extra_outputs, frames, None);
            println!("  [{}] duplicate bus_index=1 rejected", if result.is_err() { "PASS" } else { "FAIL" });
        }

        // Case 5: a well-formed request (correct bus_index, correct
        // channel_count) is accepted, so the above rejections are really
        // validation catching bad input, not the call always failing.
        {
            let mut master_refs: Vec<&mut [f32]> = master_out.iter_mut().map(|c| c.as_mut_slice()).collect();
            let good_channels: Vec<&[f32]> = vec![silent.as_slice(); 2];
            let extra_inputs = [ExtraInputBus { bus_index: 1, channels: &good_channels }];
            let mut extra_outputs: [ExtraOutputBus<'_>; 0] = [];
            let result = plugin.process_multi_bus(&input_refs, &extra_inputs, &mut master_refs, &mut extra_outputs, frames, None);
            println!("  [{}] well-formed extra_inputs request accepted", if result.is_ok() { "PASS" } else { "FAIL" });
        }
        println!();
    }

    pub fn main() -> Result<()> {
        use std::path::Path;

        const SAMPLE_RATE: f64 = 48_000.0;
        const BUFFER_FRAMES: usize = 512;
        const TONE_HZ: f64 = 200.0;
        const MAIN_AMPLITUDE: f32 = 0.3;
        const SIDECHAIN_AMPLITUDE: f32 = 0.6;
        const PHASE_SECONDS: f64 = 1.5;

        let path = std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/Library/Audio/Plug-Ins/VST3/soothe2.vst3".to_string());

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
            "Initialized. Main bus: {} in / {} out. Input bus count: {}\n",
            plugin.input_channels(),
            plugin.output_channels(),
            plugin.input_bus_count()
        );

        let sidechain_channels = plugin.input_bus_channel_count(1);
        println!("Bus 1 (expected Sidechain Input) channel count: {sidechain_channels:?}");

        let Some(sidechain_channels) = sidechain_channels else {
            println!("Plugin does not declare an input bus 1; cannot run the extra-input feed check.");
            return Ok(());
        };

        let main_channels = plugin.input_channels();
        let master_channels = plugin.output_channels();

        run_validation_checks(&mut plugin, main_channels, master_channels);

        let plugin = Arc::new(Mutex::new(plugin));
        let stats = Arc::new(Mutex::new([PhaseStats::default(); 4]));
        let buffer_counter = Arc::new(AtomicUsize::new(0));
        let buffers_per_phase = ((SAMPLE_RATE / BUFFER_FRAMES as f64) * PHASE_SECONDS) as usize;

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
        let buffer_counter_cb = buffer_counter.clone();
        let mut master_out = vec![vec![0.0f32; BUFFER_FRAMES]; master_channels];
        let mut main_tone_buf = vec![0.0f32; BUFFER_FRAMES];
        let mut sidechain_tone_buf = vec![0.0f32; BUFFER_FRAMES];
        let mut sample_position: u64 = 0;

        println!("Opening real cpal output stream to drive process_multi_bus from a genuine audio callback...");
        let stream = device
            .build_output_stream(
                &stream_config,
                move |data: &mut [f32], _| {
                    let Ok(mut plugin) = plugin_cb.try_lock() else { data.fill(0.0); return; };

                    let buffer_index = buffer_counter_cb.fetch_add(1, Ordering::SeqCst);
                    let phase_index = (buffer_index / buffers_per_phase.max(1)).min(PHASES.len() - 1);
                    let phase = PHASES[phase_index];

                    // Continuous-phase 200 Hz tone, computed from an
                    // absolute sample counter so there's no discontinuity
                    // at phase or buffer boundaries.
                    let angular_step = 2.0 * std::f64::consts::PI * TONE_HZ / SAMPLE_RATE;
                    for frame in 0..BUFFER_FRAMES {
                        let phase_angle = angular_step * (sample_position + frame as u64) as f64;
                        let tone = phase_angle.sin() as f32;
                        main_tone_buf[frame] = if phase.main_active { tone * MAIN_AMPLITUDE } else { 0.0 };
                        sidechain_tone_buf[frame] = if phase.sidechain_active { tone * SIDECHAIN_AMPLITUDE } else { 0.0 };
                    }
                    sample_position += BUFFER_FRAMES as u64;

                    let input_refs: Vec<&[f32]> = (0..main_channels).map(|_| main_tone_buf.as_slice()).collect();
                    let sidechain_refs: Vec<&[f32]> = (0..sidechain_channels).map(|_| sidechain_tone_buf.as_slice()).collect();
                    let extra_inputs = [ExtraInputBus { bus_index: 1, channels: &sidechain_refs }];
                    let mut extra_outputs: [ExtraOutputBus<'_>; 0] = [];

                    let mut master_refs: Vec<&mut [f32]> = master_out.iter_mut().map(|c| c.as_mut_slice()).collect();

                    if plugin
                        .process_multi_bus(&input_refs, &extra_inputs, &mut master_refs, &mut extra_outputs, BUFFER_FRAMES, None)
                        .is_err()
                    {
                        data.fill(0.0);
                        return;
                    }

                    let master_rms = master_out.iter().map(|c| rms(c)).fold(0.0f32, f32::max);

                    if let Ok(mut stats) = stats_cb.try_lock() {
                        stats[phase_index].record(master_rms);
                    }

                    // Play the master bus back so a human running this
                    // example live also hears it.
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

        println!("Pumping the run loop for 1s to let the plugin finish any async setup...");
        pump_cfrunloop_for(1.0);

        let total_seconds = PHASE_SECONDS * PHASES.len() as f64 + 0.5;
        println!("Rendering {:.1}s across {} phases ({:.1}s each)...", total_seconds, PHASES.len(), PHASE_SECONDS);
        std::thread::sleep(std::time::Duration::from_secs_f64(total_seconds));
        drop(stream);

        let stats = stats.lock().expect("stats lock poisoned");
        println!("\nPer-phase master (bus 0) output RMS:");
        for (index, phase) in PHASES.iter().enumerate() {
            let phase_stats = stats[index];
            println!(
                "  phase {}: {} buffers, avg RMS = {:.5}, peak RMS = {:.5}",
                phase.label, phase_stats.buffers_processed, phase_stats.avg(), phase_stats.rms_peak
            );
        }

        println!("\nComparison 1 (literal design: main silent throughout, sidechain toggled):");
        let literal_signal = stats[0];
        let literal_silent = stats[1];
        let literal_diff = (literal_signal.avg() - literal_silent.avg()).abs();
        println!(
            "  phase 1 (sidechain=tone) avg RMS = {:.5} vs phase 2 (sidechain=silent) avg RMS = {:.5}, diff = {:.5}",
            literal_signal.avg(), literal_silent.avg(), literal_diff
        );
        if literal_signal.rms_peak <= 0.0001 && literal_silent.rms_peak <= 0.0001 {
            println!(
                "  INCONCLUSIVE, as expected: with the main bus silent, Soothe2 (a resonance shaper) has \
                 nothing to process, so master output stays silent regardless of the sidechain bus's \
                 content. This comparison cannot prove or disprove whether extra_inputs reaches the plugin."
            );
        } else if literal_diff > 0.001 {
            println!("  MEASURABLE DIFFERENCE found even with main silent -- unexpected but noted honestly.");
        } else {
            println!("  No measurable difference (both phases produced some signal, but it didn't change).");
        }

        println!("\nComparison 2 (adapted design, the real proof: main driven throughout, sidechain toggled):");
        let adapted_baseline = stats[2];
        let adapted_sidechain = stats[3];
        let adapted_diff = (adapted_sidechain.avg() - adapted_baseline.avg()).abs();
        let relative_diff = if adapted_baseline.avg() > 0.0001 { adapted_diff / adapted_baseline.avg() } else { 0.0 };
        println!(
            "  phase 3 (sidechain=silent) avg RMS = {:.5} vs phase 4 (sidechain=tone) avg RMS = {:.5}, \
             abs diff = {:.5}, relative diff = {:.1}%",
            adapted_baseline.avg(), adapted_sidechain.avg(), adapted_diff, relative_diff * 100.0
        );
        if adapted_baseline.rms_peak <= 0.0001 {
            println!(
                "  INCONCLUSIVE: master output never showed signal even with the main bus driven -- \
                 something upstream of this check (plugin bypass state, gain staging) is preventing this \
                 run from proving anything either way."
            );
        } else if relative_diff > 0.05 {
            println!(
                "  MEASURABLE DIFFERENCE ({:.1}% relative change in master RMS) between sidechain-silent and \
                 sidechain-driven phases, with the same main-bus signal in both. This is real evidence that \
                 the extra_inputs mechanism is reaching Soothe2's own processing and influencing its output \
                 -- not just being accepted and ignored.",
                relative_diff * 100.0
            );
        } else {
            println!(
                "  NOT MEASURABLE: master RMS barely changed ({:.1}% relative) between sidechain-silent and \
                 sidechain-driven phases despite the main bus being actively driven in both. This is evidence \
                 that either the extra input isn't reaching Soothe2's processing, or Soothe2's sidechain input \
                 doesn't influence its output the way assumed -- reported honestly, not spun as success.",
                relative_diff * 100.0
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
