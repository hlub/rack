# rack-sys - C++ Wrapper for Native Plugin APIs

## Purpose

The C++ layer that wraps native plugin APIs (AudioUnit, VST3) and exposes a C
API for the Rust crate's FFI.

## Layout

- `include/rack_au.h`, `include/rack_vst3.h` - the C APIs. Each function's
  comment is its contract, including which thread may call it.
- `src/au_*.cpp`, `src/au_gui.mm`, `src/rack_au.cpp` - AudioUnit hosting.
- `src/vst3_scanner.cpp` - VST3 bundle scanning.
- `src/vst3_instance.cpp` - a VST3 instance: lifecycle, processing,
  parameters, presets, state, MIDI.
- `src/vst3_connection_proxy.h` - the proxy a component and a separate
  controller are connected through.
- `src/vst3_lifecycle_mutex.h` - the process-wide mutex VST3 module loading,
  initialization, reset and instance teardown take.
- `test/fixture_plugin/` - the VST3 fixture plugin rack's Rust tests load,
  built by `scripts/build-fixture-plugin.sh`, never by `build.rs`.
- `test/test_*.cpp` - standalone C++ test programs, built only with
  `-DBUILD_TESTS=ON`.

## C API conventions

- Opaque pointers; C++ allocates with `new`, Rust releases through the
  matching `_free` (or, for a VST3 instance, the two `_release_` functions).
- Functions return `RACK_*_OK` or a negative `RACK_*_ERROR_*` code; a
  positive code is a named non-error outcome (`RACK_VST3_PARAMETER_IDS_CHANGED`).
- Strings are copied into caller-owned buffers, always NUL-terminated.
- Variable-sized results the caller cannot size in advance (a VST3 state
  read) come back as an owned blob with its own `_free`, never as a guessed
  buffer size and a retry.

## VST3 thread contract

A VST3 instance is used as two halves at once once the Rust layer splits it:
a processor half on the audio thread and a controller half on the thread that
constructed the instance. `rack_vst3.h`'s "The two halves of an instance"
lists which functions belong to which half, and the comment on
`RackVST3Plugin` in `vst3_instance.cpp` says which fields each half may touch.
Keep both true when adding a function or a field: a processor function touches
only `proc`, `fixed` and `handoff`, a controller function only `ctrl`, `fixed`,
`handoff` and the edit controller, and anything else is a function of the whole instance.
`handoff` is the one part both halves touch while running: atomics only, each field saying who
writes it and in what order the other side reads it (the plugin's reported values, and the
host's own writes that make an older report stale). `rack_vst3_plugin_value_changes_pending`
reads atomics alone and may be asked from any thread.

The processor group is real-time: no allocation and no lock once
`rack_vst3_plugin_initialize` has returned. Queues and buffers are sized there.

## Building

CMake builds a static library; `build.rs` drives it. By hand:

```bash
cd rack-sys && mkdir -p build && cd build
cmake -DBUILD_TESTS=ON .. && make
```
