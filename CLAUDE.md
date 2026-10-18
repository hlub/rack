# Rack - Audio Plugin Hosting Library

## Project Overview

Rack is a Rust library for hosting audio plugins: AudioUnit on Apple
platforms and VST3 on desktop platforms where the vendored VST3 SDK is
present. This repository (github.com/hlub/rack) is a fork of
github.com/sinkingsugar/rack that carries the VST3 patches the drumr project
needs; drumr pins one exact commit of it as a git submodule.

## Architecture

1. **rack-sys/** - C++ wrapping the native plugin APIs behind a C API
   (`rack-sys/include/rack_au.h`, `rack-sys/include/rack_vst3.h`). See
   `rack-sys/CLAUDE.md`.
2. **src/** - the Rust library: FFI declarations (`src/*/ffi.rs`) and safe
   wrappers over them, with format-agnostic traits in `src/traits.rs`.

`build.rs` builds rack-sys with CMake and finds the VST3 SDK at
`rack-sys/external/vst3sdk` (a submodule).

## The VST3 instance and its two halves

A VST3 plugin is an audio processor, called on the audio thread, and an edit
controller, a UI-thread object. `Vst3Plugin` (`src/vst3/instance.rs`) is the
whole instance; `split` gives an initialized one's `Vst3Processor`
(`src/vst3/processor.rs`, `Send`, not `Sync`) and `Vst3Controller`
(`src/vst3/controller.rs`, neither), and `join` puts them back. The two halves
run at the same time with no lock between them, so:

- Every C function belongs to the processor group, the controller group or the
  whole instance; `rack_vst3.h`'s "The two halves of an instance" lists them.
  A Rust method calls only functions of its own half's group.
- `RackVST3Plugin`'s fields are partitioned into `fixed` (written at
  construction and initialize, read by either half), `proc` (the processor
  half's) and `ctrl` (the controller half's); the comment on the struct in
  `rack-sys/src/vst3_instance.cpp` is the rule. A new field goes in one of
  them, and a function touching a field outside its half's part is a data
  race once the instance is split.
- A call the standard allows only while nothing processes (initialize,
  reset, state writes, preset loads, the latency query, the suspending state
  read, a parameter write that also queues for the processor) exists only on
  the whole `Vst3Plugin`.
- The halves are released, not freed: the processor's release is one atomic
  store, and the controller's release frees the instance only after it. A
  controller dropped first leaks and counts the instance.
- Messages between a component and a separate controller go through
  `RackConnectionProxy` (`rack-sys/src/vst3_connection_proxy.h`), which
  delivers only on the constructing thread and counts what it discards.

## Testing

```bash
scripts/build-fixture-plugin.sh
RACK_REQUIRE_FIXTURE=1 cargo test
cargo build --examples --features vst3
```

The fixture plugin (`rack-sys/test/fixture_plugin`) records in its own state
what reached it: process calls, setProcessing(false) calls, parameter points
per parameter, and messages each side received. Tests that need to know what
reached the plugin read that back; `src/vst3/test_support.rs` parses it. A
missing fixture skips its tests unless `RACK_REQUIRE_FIXTURE=1`, so always set
it. The other tests load whatever real plugins are installed and skip when
there are none. Every test that loads an instance takes
`test_support::serial_plugin_test()`, because real plugins loaded on several
test threads at once have had the test process killed.

`cargo` resolves a workspace by walking up from the manifest, so a checkout of
this repository nested inside another Cargo workspace (a submodule inside a
worktree of the drumr repository, for one) is taken for a member of it and
does not build there; build and test such a checkout from a copy outside it.

## Conventions

- Doc comments on every public item, saying what it is for and on which
  thread it may be called.
- Every `unsafe` block carries a `// SAFETY:` comment saying why it is sound.
- Real-time paths (the processor half) neither allocate nor lock once
  `initialize` has returned; scratch buffers are reserved at initialize.
- Comments state the present: what the code does and why, never what an
  earlier version did.
