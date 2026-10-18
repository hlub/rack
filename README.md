# Rack

**A modern Rust library for hosting audio plugins**

> **Status:** AudioUnit support is **production-ready** on macOS (Phases 1-8 complete, thoroughly tested).
> VST3 support is **working on macOS**, untested on Windows/Linux (no CI yet).
> iOS and visionOS are supported but untested. The API is stabilizing. CLAP support is planned.

[![Crates.io](https://img.shields.io/crates/v/rack.svg)](https://crates.io/crates/rack)
[![Documentation](https://docs.rs/rack/badge.svg)](https://docs.rs/rack)
[![License](https://img.shields.io/crates/l/rack.svg)](https://github.com/sinkingsugar/rack#license)

Rack is a cross-platform library for discovering, loading, and processing audio through VST3, AudioUnit, CLAP, and other plugin formats in Rust applications.

## Features

- ✅ **AudioUnit support** (macOS, iOS, visionOS) - production-ready with scanning, loading, processing, parameters, MIDI, presets, and GUI
- 🚧 **VST3 support** (macOS tested, Windows/Linux untested) - scanning, loading, processing, parameters, MIDI, and presets
- ⚡ **Zero-copy audio processing** - planar format with pointer assignment (no memcpy in hot path)
- 🎵 **SIMD-optimized** - ARM NEON and x86_64 SSE2 for 4x performance (AudioUnit)
- 🎹 **Zero-allocation MIDI** - SmallVec-based MIDI for real-time performance
- 🎛️ **GUI support** - AudioUnit: AUv3, AUv2, and generic fallback UI (VST3 GUI coming soon)
- 🎚️ **Clean, safe API** - minimal unsafe code, comprehensive error handling
- 🎼 **CLAP support** - planned
- 🔄 **cpal integration** - optional audio I/O helpers
- 🚀 **Zero-cost abstractions** - trait-based design

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
rack = "0.3"
```

### List available plugins

```rust
use rack::prelude::*;

fn main() -> Result<()> {
    let scanner = Scanner::new()?;
    let plugins = scanner.scan()?;

    for plugin in plugins {
        println!("{}", plugin);
    }

    Ok(())
}
```

### Load and process audio

```rust
use rack::prelude::*;

fn main() -> Result<()> {
    let scanner = Scanner::new()?;
    let plugins = scanner.scan()?;

    // Load first plugin
    let mut plugin = scanner.load(&plugins[0])?;
    plugin.initialize(48000.0, 512)?;

    // Process audio with planar buffers (zero-copy)
    let left_in = vec![0.0f32; 512];
    let right_in = vec![0.0f32; 512];
    let mut left_out = vec![0.0f32; 512];
    let mut right_out = vec![0.0f32; 512];

    plugin.process(
        &[&left_in, &right_in],
        &mut [&mut left_out, &mut right_out],
        512
    )?;

    Ok(())
}
```

### MIDI Synthesis

See [examples/simple_synth.rs](examples/simple_synth.rs) for a complete MIDI synthesis example.

## Platform Support

| Platform | AudioUnit | VST3 | CLAP | LV2 | Notes |
|----------|-----------|------|------|-----|-------|
| macOS    | ✅        | 🧪   | 🚧   | ❌  | AudioUnit production-ready, VST3 tested & working |
| iOS      | 🧪        | ❌   | ❌   | ❌  | AudioUnit compiles, untested |
| visionOS | 🧪        | ❌   | ❌   | ❌  | AudioUnit compiles, untested |
| Windows  | ❌        | 🧪   | 🚧   | ❌  | VST3 compiles, untested (no CI) |
| Linux    | ❌        | 🧪   | 🚧   | 🧪  | VST3 compiles, untested (no CI) |

- ✅ Production-ready (tested)
- 🧪 Experimental (compiles, may work, untested)
- 🚧 Planned
- ❌ Not applicable

**Platform-Specific Notes:**
- **Apple platforms (macOS):** AudioUnit is the default and recommended format
  - Production-ready: thoroughly tested, SIMD-optimized, with GUI support
  - Discovers and loads AUv3 app extensions or AudioUnit plugins
  - GUI support: AppKit-based (AUv3/AUv2/generic fallback)
  - VST3 also available on macOS (tested & working, but no GUI yet)
- **Windows/Linux:** VST3 support is experimental
  - Code compiles and follows VST3 SDK patterns
  - ⚠️ NOT tested on Windows or Linux (no CI infrastructure yet)
  - Standard VST3 plugin paths are scanned automatically
  - Help wanted: testing, CI setup, bug reports

## Examples

Run the examples:

```bash
# List all available plugins (AudioUnit on macOS, VST3 on Windows/Linux)
cargo run --example list_plugins

# List VST3 plugins specifically
cargo run --example list_vst3_plugins

# Process audio with VST3
cargo run --example vst3_processor

# Control parameters
cargo run --example control_parameters

# MIDI synthesis
cargo run --example simple_synth

# Browse and load presets
cargo run --example preset_browser

# Plugin GUI (AudioUnit on macOS - shows native plugin UI)
cargo run --example plugin_gui

# Real-time audio host with CPAL (requires 'cpal' feature)
cargo run --example cpal_host --features cpal
```

### Display Plugin GUI

```rust
use rack::prelude::*;

fn main() -> Result<()> {
    let scanner = Scanner::new()?;
    let plugins = scanner.scan()?;
    let mut plugin = scanner.load(&plugins[0])?;
    plugin.initialize(48000.0, 512)?;

    // Create GUI asynchronously
    plugin.create_gui(|result| {
        match result {
            Ok(gui) => {
                gui.show_window(Some("My Plugin"))?;
                // GUI is now visible!
            }
            Err(e) => eprintln!("GUI creation failed: {}", e),
        }
        Ok(())
    });

    // Keep program running...
    Ok(())
}
```

See [examples/plugin_gui.rs](examples/plugin_gui.rs) for a complete GUI example.

## Architecture

Rack uses a trait-based design for maximum flexibility:

```rust
pub trait PluginScanner {
    type Plugin: PluginInstance;
    fn scan(&self) -> Result<Vec<PluginInfo>>;
    fn load(&self, info: &PluginInfo) -> Result<Self::Plugin>;
}

pub trait PluginInstance: Send {
    fn initialize(&mut self, sample_rate: f64, max_block_size: usize) -> Result<()>;
    fn reset(&mut self) -> Result<()>;
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], num_frames: usize) -> Result<()>;
    fn get_parameter(&self, index: usize) -> Result<f32>;
    fn set_parameter(&mut self, index: usize, value: f32) -> Result<()>;
    // ... more methods
}
```

This allows different plugin formats to implement the same interface, making your code portable across formats.

### VST3: a processor half and a controller half

A VST3 plugin is two objects with two thread contracts: its audio processor
runs on the audio thread, and its edit controller is a UI-thread object. An
initialized `Vst3Plugin` splits into a `Vst3Processor` and a `Vst3Controller`
so a host can play the plugin on the audio thread while asking its controller
questions on the main thread, with no lock between them:

```rust
let (mut processor, controller) = plugin.split().expect("initialized");
let audio = std::thread::spawn(move || {
    // processor.process(...), queue_parameter_change, send_midi, set_transport
    processor
});
// controller.parameter_info(...), get_parameter, set_parameter, presets
let plugin = Vst3Plugin::join(audio.join().unwrap(), controller).expect("same instance");
```

- `Vst3Processor` is `Send` and not `Sync`. It touches only the processing
  state and the facts fixed at initialization, and dropping it makes no plugin
  call, so it may be dropped on the audio thread.
- `Vst3Controller` is neither `Send` nor `Sync`: it stays on the thread that
  split the instance. Its `set_parameter` writes the controller only; the
  processor hears a value through `queue_parameter_change` on its own thread.
  Its `read_state_while_processing` reads the state without stopping the
  processor; whether a given plugin keeps its own state consistent against
  `process` meanwhile is the plugin's, so a host checks a plugin before
  trusting it. Its `take_restart_flags` reports what the plugin asked for
  through `restartComponent` (titles, latency, buses, a reload), and
  `refresh_parameter_titles` re-reads the parameter descriptions after a
  titles change without touching the parameter ids the processor reads.
- Initialization, `reset`, state reads and writes, preset loads, the latency
  query, and the parameter write that tells both halves exist only on the
  whole `Vst3Plugin`, which `join` gives back.
- A component and a separate controller are connected through a proxy, as
  the SDK's own host helper does: a message either sends on the thread that
  created the instance is delivered, and one sent from any other thread (from
  inside `process`, say) is discarded and counted (`discarded_messages`), so
  no controller code runs on the audio thread.
- Drop the processor half first, or join the two: the controller half frees
  the instance only once the processor half has let go. Dropped the other way
  round, the instance is leaked and counted (`leaked_instance_count`) rather
  than freed under a processor that may still be running.

## Testing

Most VST3 tests run against a fixture plugin built from the VST3 SDK this
repository vendors (`rack-sys/test/fixture_plugin`). It records what the host
did to it in its own state: how many times it processed, how often it left
the Processing state, how many parameter points reached its processor, and
which messages passed between its component and its controller. A test reads
that back with an ordinary state read and asserts on what reached the plugin,
not only on what rack returned. Build it once, then run the tests with the
fixture required, so a missing fixture fails instead of skipping:

```bash
scripts/build-fixture-plugin.sh
RACK_REQUIRE_FIXTURE=1 cargo test
```

`RACK_TEST_FIXTURE_VST3` names a fixture bundle somewhere else. The remaining
tests load whatever real plugins are installed and skip when there are none;
every test that loads an instance takes one lock, so they run one at a time
even under a parallel test run.

## Roadmap

### AudioUnit (macOS) - ✅ COMPLETE
- [x] Plugin scanning and enumeration
- [x] Plugin loading and instantiation
- [x] Audio processing with SIMD optimization (ARM NEON + x86_64 SSE2)
- [x] Zero-copy planar audio API (eliminated 2 of 3 memcpy operations)
- [x] Dynamic channel count support (mono/stereo/surround)
- [x] Plugin state reset (clear buffers/delay lines)
- [x] Parameter control with caching
- [x] MIDI support (zero-allocation, all MIDI 1.0 messages)
- [x] Preset management (factory presets + state serialization)
- [x] GUI hosting (AUv3/AUv2/generic fallback)

### VST3 (macOS tested, Windows/Linux untested) - 🧪 EXPERIMENTAL
- [x] Plugin scanning and enumeration (automatic system path detection)
- [x] Plugin loading and instantiation
- [x] Audio processing with zero-copy planar audio
- [x] Dynamic channel count support (mono/stereo/surround)
- [x] Plugin state reset (clear buffers/delay lines)
- [x] Parameter control with validation
- [x] MIDI support (zero-allocation, MIDI 1.0 channel messages)
- [x] Preset management (factory presets + state serialization)
- [ ] Windows/Linux testing (no CI yet)
- [ ] GUI hosting (planned)

### Future Formats
- [ ] CLAP support (cross-platform)
- [ ] LV2 support (Linux)

### Advanced Features
- [ ] Multi-threading support
- [ ] Plugin latency compensation
- [ ] Offline processing
- [ ] Crash isolation
- [ ] Plugin sandboxing

## Contributing

Contributions are welcome!

**Production-Ready**:
- AudioUnit hosting on macOS (Phases 1-8 complete, thoroughly tested)

**Experimental** (compiles, needs testing):
- VST3 on macOS (tested & working)
- VST3 on Windows/Linux (untested, no CI)

**High Priority - Help Needed**:
- 🔴 **Windows/Linux VST3 testing** - verify it actually works!
- 🔴 **CI infrastructure** - Windows and Linux builds/tests
- 🟡 VST3 GUI hosting
- 🟡 CLAP backend (scanner, loader, processor, GUI)

**Lower Priority**:
- Linux LV2 support
- Advanced features (multi-threading, latency compensation, crash isolation)
- Documentation improvements
- Additional examples

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Acknowledgments

- Inspired by [VCV Rack](https://vcvrack.com/) and the modular synthesis community
- AudioUnit implementation uses Apple's AudioToolbox framework directly via C++ FFI
- VST3 implementation uses the [VST3 SDK](https://github.com/steinbergmedia/vst3sdk) via C++ FFI
- Thanks to the Rust audio community at [rust.audio](https://rust.audio)

## Why "Rack"?

The name is inspired by modular synthesizer racks and VCV Rack - the idea of a framework where you can plug in different modules (plugins) and wire them together. Plus, it was available on crates.io! 🎉
