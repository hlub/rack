//! Raw FFI bindings to the rack-sys VST3 C API
//!
//! This module contains unsafe FFI declarations. All safe wrappers
//! should be in scanner.rs and instance.rs.

#![allow(dead_code)]

use std::os::raw::{c_char, c_int};

// Opaque types (zero-sized to prevent construction)
#[repr(C)]
pub struct RackVST3Scanner {
    _private: [u8; 0],
}

#[repr(C)]
pub struct RackVST3Plugin {
    _private: [u8; 0],
}

#[repr(C)]
pub struct RackVST3Gui {
    _private: [u8; 0],
}

// Plugin type enum
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RackVST3PluginType {
    Effect = 0,
    Instrument = 1,
    Analyzer = 2,
    Spatial = 3,
    Other = 4,
}

// Plugin info struct (matches C layout exactly)
#[repr(C)]
#[derive(Clone)]
pub struct RackVST3PluginInfo {
    pub name: [c_char; 256],
    pub manufacturer: [c_char; 256],
    pub path: [c_char; 1024],
    pub unique_id: [c_char; 64],
    pub version: u32,
    pub plugin_type: RackVST3PluginType,
    pub category: [c_char; 128],
}

// Preset info struct (matches C layout exactly)
#[repr(C)]
#[derive(Clone)]
pub struct RackVST3PresetInfo {
    pub name: [c_char; 256],
    pub preset_number: i32,
}

/// Describes one additional (non-main) output bus to capture, passed to
/// `rack_vst3_plugin_process_multi_bus`. Matches
/// `RackVST3ExtraOutputBus` in `rack_vst3.h` exactly.
#[repr(C)]
pub struct RackVST3ExtraOutputBus {
    pub bus_index: i32,
    pub channels: *const *mut f32,
    pub channel_count: u32,
}

/// Describes one additional (non-main) input bus to feed, passed to
/// `rack_vst3_plugin_process_multi_bus`. Matches
/// `RackVST3ExtraInputBus` in `rack_vst3.h` exactly. `channels` is
/// `*const *const f32` (not `*const *mut f32` like the output side) because
/// these buffers are read-only from the host's perspective, matching how
/// the main bus-0 `inputs` parameter is typed elsewhere in this file.
#[repr(C)]
pub struct RackVST3ExtraInputBus {
    pub bus_index: i32,
    pub channels: *const *const f32,
    pub channel_count: u32,
}

// Error codes
pub const RACK_VST3_OK: c_int = 0;
pub const RACK_VST3_ERROR_GENERIC: c_int = -1;
pub const RACK_VST3_ERROR_NOT_FOUND: c_int = -2;
pub const RACK_VST3_ERROR_INVALID_PARAM: c_int = -3;
pub const RACK_VST3_ERROR_NOT_INITIALIZED: c_int = -4;
pub const RACK_VST3_ERROR_LOAD_FAILED: c_int = -5;
pub const RACK_VST3_ERROR_NOT_SUPPORTED: c_int = -6;

extern "C" {
    // ============================================================================
    // Scanner API
    // ============================================================================

    /// Create a new scanner
    ///
    /// # Returns
    ///
    /// Returns a pointer to a new scanner, or NULL if allocation fails
    ///
    /// # Safety
    ///
    /// - The returned pointer must be freed with `rack_vst3_scanner_free`
    /// - The pointer is valid until `rack_vst3_scanner_free` is called
    /// - Must not be called from multiple threads without synchronization
    pub fn rack_vst3_scanner_new() -> *mut RackVST3Scanner;

    /// Free scanner
    ///
    /// # Safety
    ///
    /// - `scanner` must be a valid pointer returned by `rack_vst3_scanner_new`
    /// - `scanner` must not be used after this call
    /// - Must not be called multiple times with the same pointer
    /// - If `scanner` is NULL, this function does nothing (safe no-op)
    pub fn rack_vst3_scanner_free(scanner: *mut RackVST3Scanner);

    /// Add a search path for VST3 plugins
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `scanner` must be a valid pointer returned by `rack_vst3_scanner_new`
    /// - `path` must be a valid null-terminated C string
    /// - `path` must remain valid for the duration of the call
    pub fn rack_vst3_scanner_add_path(scanner: *mut RackVST3Scanner, path: *const c_char) -> c_int;

    /// Add system default VST3 search paths
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `scanner` must be a valid pointer returned by `rack_vst3_scanner_new`
    pub fn rack_vst3_scanner_add_default_paths(scanner: *mut RackVST3Scanner) -> c_int;

    /// Scan for plugins
    ///
    /// Two-pass usage pattern:
    /// 1. `count = rack_vst3_scanner_scan(scanner, NULL, 0);` - Get plugin count
    /// 2. `rack_vst3_scanner_scan(scanner, array, count);` - Fill array with plugin info
    ///
    /// # Returns
    ///
    /// - On success: number of plugins found (may exceed max_plugins if more exist)
    /// - On error: negative error code (see RACK_VST3_ERROR_* constants)
    ///
    /// # Safety
    ///
    /// - `scanner` must be a valid pointer returned by `rack_vst3_scanner_new`
    /// - If `plugins` is NULL, `max_plugins` is ignored (count-only mode)
    /// - If `plugins` is not NULL:
    ///   - Must point to an array with at least `max_plugins` elements
    ///   - Array must be valid for writes
    ///   - The first min(return_value, max_plugins) elements will be initialized
    /// - C++ code guarantees:
    ///   - All string fields are null-terminated
    ///   - Strings fit within their buffer sizes
    ///   - No buffer overflows occur
    /// - Thread-safety: Scanner can be used from multiple threads with proper synchronization
    pub fn rack_vst3_scanner_scan(
        scanner: *mut RackVST3Scanner,
        plugins: *mut RackVST3PluginInfo,
        max_plugins: usize,
    ) -> c_int;

    // ============================================================================
    // Plugin Instance API
    // ============================================================================

    /// Create a new plugin instance from path and UID
    ///
    /// # Safety
    ///
    /// - `path` must be a valid null-terminated C string pointing to .vst3 bundle
    /// - `uid` must be a valid null-terminated C string with VST3 UID (from scan)
    /// - Both strings must remain valid for the duration of the call
    /// - Returns NULL if plugin not found or allocation fails
    /// - Returned pointer must be freed with `rack_vst3_plugin_free`
    pub fn rack_vst3_plugin_new(path: *const c_char, uid: *const c_char) -> *mut RackVST3Plugin;

    /// Free plugin instance
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `plugin` must not be used after this call
    /// - Must not be called multiple times with the same pointer
    /// - If `plugin` is NULL, this function does nothing (safe no-op)
    pub fn rack_vst3_plugin_free(plugin: *mut RackVST3Plugin);

    /// Initialize plugin with sample rate and buffer size
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `sample_rate` must be positive and reasonable (e.g., 44100-192000)
    /// - `max_block_size` must be positive and reasonable (e.g., 64-8192)
    /// - Must be called before `rack_vst3_plugin_process`
    /// - Protected by global mutex for VST3 framework thread-safety
    pub fn rack_vst3_plugin_initialize(
        plugin: *mut RackVST3Plugin,
        sample_rate: f64,
        max_block_size: u32,
        initial_state: *const u8,
        initial_state_size: usize,
    ) -> c_int;

    /// Check if plugin is initialized
    ///
    /// # Returns
    ///
    /// - 1 if initialized
    /// - 0 if not initialized
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    pub fn rack_vst3_plugin_is_initialized(plugin: *mut RackVST3Plugin) -> c_int;

    /// Reset plugin state
    ///
    /// Clears all internal buffers, delay lines, and state without changing parameters.
    /// Useful for clearing reverb tails, delay lines, etc. between songs or after preset changes.
    ///
    /// # Returns
    ///
    /// - `RACK_VST3_OK` (0) on success
    /// - `RACK_VST3_ERROR_NOT_INITIALIZED` if plugin is not initialized
    /// - Negative error code on failure
    ///
    /// # Thread Safety
    ///
    /// Should be called from a non-realtime thread.
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Plugin must be initialized (returns error if not)
    pub fn rack_vst3_plugin_reset(plugin: *mut RackVST3Plugin) -> c_int;

    /// Get input channel count
    ///
    /// # Returns
    ///
    /// - Number of input channels (>= 0)
    /// - 0 if not initialized or query failed
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Should be called after `rack_vst3_plugin_initialize`
    pub fn rack_vst3_plugin_get_input_channels(plugin: *mut RackVST3Plugin) -> c_int;

    /// Get output channel count
    ///
    /// # Returns
    ///
    /// - Number of output channels (>= 0)
    /// - 0 if not initialized or query failed
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Should be called after `rack_vst3_plugin_initialize`
    pub fn rack_vst3_plugin_get_output_channels(plugin: *mut RackVST3Plugin) -> c_int;

    /// Get the plugin's own reported processing latency, in samples.
    ///
    /// The delay the plugin's own processing introduces between input and
    /// output (e.g. lookahead limiters, linear-phase EQs, convolution).
    ///
    /// # Returns
    ///
    /// - The plugin's reported latency in samples
    /// - 0 if not initialized, if the plugin has no processor interface, or
    ///   if the plugin genuinely reports zero latency (the common case)
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Should be called after `rack_vst3_plugin_initialize`, from the
    ///   UI/control thread (mirrors the VST3 SDK's own "[UI-thread & Setup
    ///   Done]" contract for `IAudioProcessor::getLatencySamples`)
    pub fn rack_vst3_plugin_get_latency_samples(plugin: *mut RackVST3Plugin) -> u32;

    /// Get the number of audio output buses the plugin declares
    ///
    /// Distinct from `rack_vst3_plugin_get_output_channels`, which reports
    /// only bus 0's channel count. A plugin like AD2 in "individual outs"
    /// mode may declare several named output buses.
    ///
    /// # Returns
    ///
    /// - Number of output buses (>= 0)
    /// - Negative `RACK_VST3_ERROR_*` code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    pub fn rack_vst3_plugin_get_output_bus_count(plugin: *mut RackVST3Plugin) -> c_int;

    /// Get the number of audio input buses the plugin declares
    ///
    /// # Returns
    ///
    /// - Number of input buses (>= 0)
    /// - Negative `RACK_VST3_ERROR_*` code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    pub fn rack_vst3_plugin_get_input_bus_count(plugin: *mut RackVST3Plugin) -> c_int;

    /// Get channel count and name for a single audio output bus
    ///
    /// Read-only query; does not activate the bus.
    ///
    /// # Returns
    ///
    /// - 0 on success (values written to output pointers)
    /// - `RACK_VST3_ERROR_INVALID_PARAM` if `bus_index` is out of range
    /// - Negative error code on other failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `channels_out` can be NULL, or must point to a valid `u32`
    /// - `name_out` can be NULL, or must point to a buffer with at least
    ///   `name_out_size` bytes
    /// - `name_out` (if not NULL) will be null-terminated
    pub fn rack_vst3_plugin_get_output_bus_info(
        plugin: *mut RackVST3Plugin,
        bus_index: i32,
        channels_out: *mut u32,
        name_out: *mut c_char,
        name_out_size: usize,
    ) -> c_int;

    /// Get channel count and name for a single audio input bus
    ///
    /// Read-only query; does not activate the bus.
    ///
    /// # Returns
    ///
    /// - 0 on success (values written to output pointers)
    /// - `RACK_VST3_ERROR_INVALID_PARAM` if `bus_index` is out of range
    /// - Negative error code on other failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `channels_out` can be NULL, or must point to a valid `u32`
    /// - `name_out` can be NULL, or must point to a buffer with at least
    ///   `name_out_size` bytes
    /// - `name_out` (if not NULL) will be null-terminated
    pub fn rack_vst3_plugin_get_input_bus_info(
        plugin: *mut RackVST3Plugin,
        bus_index: i32,
        channels_out: *mut u32,
        name_out: *mut c_char,
        name_out_size: usize,
    ) -> c_int;

    /// Process audio through the plugin (planar format)
    ///
    /// Uses planar (non-interleaved) audio format - one buffer per channel.
    /// This matches VST3's internal format, enabling zero-copy processing.
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer and initialized
    /// - `inputs` must point to an array of `num_input_channels` const f32 pointers
    /// - Each input channel pointer must point to a buffer with at least `frames` f32 values
    /// - `outputs` must point to an array of `num_output_channels` mutable f32 pointers
    /// - Each output channel pointer must point to a buffer with space for at least `frames` f32 values
    /// - `frames` must not exceed the `max_block_size` from initialization
    /// - Input and output buffers must not overlap unless doing in-place processing
    /// - Must not be called concurrently on the same plugin from multiple threads
    ///
    /// # Channel Layout
    ///
    /// For stereo: inputs/outputs = [left_ptr, right_ptr], num_channels = 2
    /// For mono: inputs/outputs = [mono_ptr], num_channels = 1
    pub fn rack_vst3_plugin_process(
        plugin: *mut RackVST3Plugin,
        inputs: *const *const f32,
        num_input_channels: u32,
        outputs: *const *mut f32,
        num_output_channels: u32,
        frames: u32,
    ) -> c_int;

    /// Process audio through the plugin, capturing the main output bus (bus
    /// 0, identical semantics to `rack_vst3_plugin_process`) plus zero or
    /// more additional output buses, and feeding zero or more additional
    /// input buses, in the same underlying VST3 `process()` call.
    ///
    /// A VST3 plugin computes every activated output bus in a single
    /// `process()` call regardless of how many the host actually captures;
    /// `rack_vst3_plugin_initialize` activates every bus the plugin
    /// declares. This lets a caller bind real buffers to output buses beyond
    /// bus 0 (e.g. a drum plugin's per-instrument buses) instead of leaving
    /// them pointed at the internal silent dummy buffer, as
    /// `rack_vst3_plugin_process` does, and symmetrically bind real buffers
    /// to input buses beyond bus 0 (e.g. an effect plugin's per-instrument
    /// input buses) instead of leaving them unbound.
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure. `RACK_VST3_ERROR_INVALID_PARAM` if
    ///   any requested extra output or extra input bus's `bus_index` is out
    ///   of range (including 0, which is handled by
    ///   `outputs`/`num_output_channels` or `inputs`/`num_input_channels`
    ///   instead) or its `channel_count` doesn't exactly match that bus's
    ///   declared channel count. Validation happens before any buffer
    ///   pointers are rebound, so a failed call leaves the plugin's process
    ///   state untouched.
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer and initialized
    /// - `inputs`/`num_input_channels`/`outputs`/`num_output_channels`/
    ///   `frames`: identical requirements to `rack_vst3_plugin_process`
    ///   (bus 0 / main bus semantics)
    /// - `extra_output_buses` must point to an array of
    ///   `num_extra_output_buses` valid `RackVST3ExtraOutputBus` entries, or
    ///   be NULL if `num_extra_output_buses` is 0
    /// - Each `extra_output_buses` entry's `channels` must point to an array
    ///   of `channel_count` mutable f32 pointers, each pointing to a buffer
    ///   with space for at least `frames` f32 values
    /// - `extra_input_buses` must point to an array of
    ///   `num_extra_input_buses` valid `RackVST3ExtraInputBus` entries, or
    ///   be NULL if `num_extra_input_buses` is 0
    /// - Each `extra_input_buses` entry's `channels` must point to an array
    ///   of `channel_count` const f32 pointers, each pointing to a buffer
    ///   with at least `frames` valid f32 values
    /// - Must not be called concurrently on the same plugin from multiple
    ///   threads
    /// - `output_events_out` must either be NULL (with `output_events_capacity`
    ///   0), or point to an array of at least `output_events_capacity`
    ///   `RackVST3MidiEvent` slots. On return, the first
    ///   `min(*num_output_events_out, output_events_capacity)` slots hold the
    ///   plugin's emitted MIDI events.
    /// - `num_output_events_out` must be NULL or point to a valid `usize`; it
    ///   receives the true emitted-event count (which may exceed
    ///   `output_events_capacity`, indicating truncation).
    pub fn rack_vst3_plugin_process_multi_bus(
        plugin: *mut RackVST3Plugin,
        inputs: *const *const f32,
        num_input_channels: u32,
        outputs: *const *mut f32,
        num_output_channels: u32,
        frames: u32,
        extra_output_buses: *const RackVST3ExtraOutputBus,
        num_extra_output_buses: usize,
        extra_input_buses: *const RackVST3ExtraInputBus,
        num_extra_input_buses: usize,
        output_events_out: *mut RackVST3MidiEvent,
        output_events_capacity: usize,
        num_output_events_out: *mut usize,
    ) -> c_int;

    /// Whether the plugin declares any event OUTPUT bus (i.e. can emit its own
    /// MIDI, such as an arpeggiator or MIDI-FX plugin).
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    pub fn rack_vst3_plugin_emits_midi(plugin: *mut RackVST3Plugin) -> bool;

    /// Get parameter count
    ///
    /// # Returns
    ///
    /// - Parameter count (>= 0) on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    pub fn rack_vst3_plugin_parameter_count(plugin: *mut RackVST3Plugin) -> c_int;

    /// Get parameter value (normalized 0.0 to 1.0)
    ///
    /// # Returns
    ///
    /// - 0 on success (value written to `value` pointer)
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `index` must be less than parameter count
    /// - `value` must be a valid pointer to an f32
    pub fn rack_vst3_plugin_get_parameter(
        plugin: *mut RackVST3Plugin,
        index: u32,
        value: *mut f32,
    ) -> c_int;

    /// Set parameter value (normalized 0.0 to 1.0)
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `index` must be less than parameter count
    /// - `value` should be in range 0.0-1.0 (values outside may be clamped)
    pub fn rack_vst3_plugin_set_parameter(
        plugin: *mut RackVST3Plugin,
        index: u32,
        value: f32,
    ) -> c_int;

    pub fn rack_vst3_plugin_set_parameter_by_id(
        plugin: *mut RackVST3Plugin,
        param_id: u32,
        value: f32,
    ) -> c_int;

    pub fn rack_vst3_plugin_get_midi_pc_param_id(
        plugin: *mut RackVST3Plugin,
        channel: c_int,
    ) -> u32;

    /// Update the plugin's live process context tempo/playback state.
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Unlike `rack_vst3_plugin_set_parameter`, this writes the plugin's
    ///   process context directly with no queuing or locking -- the caller
    ///   must call this only from the same thread that calls
    ///   `rack_vst3_plugin_process`, immediately before it, never
    ///   concurrently with a `process` call in flight.
    /// - `playing` is nonzero for "transport playing", zero for "stopped".
    pub fn rack_vst3_plugin_set_transport(
        plugin: *mut RackVST3Plugin,
        tempo: f64,
        playing: i32,
    ) -> c_int;

    /// Get parameter info (name, min, max, default, unit)
    ///
    /// # Returns
    ///
    /// - 0 on success (values written to output pointers)
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `index` must be less than parameter count
    /// - `name` must point to a buffer with at least `name_size` bytes
    /// - `min`, `max`, `default_value` must be valid pointers to f32
    /// - `unit` can be NULL, or must point to a buffer with at least `unit_size` bytes
    /// - `name` and `unit` (if not NULL) will be null-terminated
    /// - `name_size` should be at least 256 bytes for typical parameter names
    /// - `unit_size` should be at least 32 bytes for typical unit strings
    pub fn rack_vst3_plugin_parameter_info(
        plugin: *mut RackVST3Plugin,
        index: u32,
        name: *mut c_char,
        name_size: usize,
        min: *mut f32,
        max: *mut f32,
        default_value: *mut f32,
        unit: *mut c_char,
        unit_size: usize,
        step_count: *mut i32,
        flags: *mut i32,
    ) -> c_int;

    /// Get the display string for a normalized parameter value, as the
    /// plugin itself would render it (VST3 `getParamStringByValue`).
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `index` must be less than parameter count
    /// - `out` must point to a buffer with at least `out_size` bytes
    /// - `out` will be null-terminated
    pub fn rack_vst3_plugin_parameter_string_by_value(
        plugin: *mut RackVST3Plugin,
        index: u32,
        normalized_value: f32,
        out: *mut c_char,
        out_size: usize,
    ) -> c_int;

    /// Convert a normalized parameter value to the plugin's "plain"
    /// (native/real-world unit) value (VST3 `normalizedParamToPlain`).
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `index` must be less than parameter count
    /// - `out_plain` must point to a valid `f64`
    pub fn rack_vst3_plugin_parameter_plain_value(
        plugin: *mut RackVST3Plugin,
        index: u32,
        normalized_value: f32,
        out_plain: *mut f64,
    ) -> c_int;

    /// Parse a display-style string into a normalized parameter value
    /// (VST3 `getParamValueByString`, the inverse of
    /// `rack_vst3_plugin_parameter_string_by_value`).
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `index` must be less than parameter count
    /// - `string` must be a valid, null-terminated UTF-8 C string
    /// - `out_normalized` must point to a valid `f32`
    pub fn rack_vst3_plugin_parameter_value_by_string(
        plugin: *mut RackVST3Plugin,
        index: u32,
        string: *const c_char,
        out_normalized: *mut f32,
    ) -> c_int;

    /// Convert a plain (native/real-world unit) parameter value to its
    /// normalized form (VST3 `plainParamToNormalized`, the inverse of
    /// `rack_vst3_plugin_parameter_plain_value`).
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `index` must be less than parameter count
    /// - `out_normalized` must point to a valid `f32`
    pub fn rack_vst3_plugin_parameter_normalized_from_plain(
        plugin: *mut RackVST3Plugin,
        index: u32,
        plain_value: f64,
        out_normalized: *mut f32,
    ) -> c_int;

    // ============================================================================
    // Preset Management API
    // ============================================================================

    /// Get factory preset count
    ///
    /// # Returns
    ///
    /// - Number of factory presets (>= 0)
    /// - 0 if plugin has no presets
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Plugin must be initialized
    pub fn rack_vst3_plugin_get_preset_count(plugin: *mut RackVST3Plugin) -> c_int;

    /// Get preset info by index
    ///
    /// # Returns
    ///
    /// - 0 on success (name and preset_number written to output pointers)
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Plugin must be initialized
    /// - `index` must be less than preset count
    /// - `name` must point to a buffer with at least `name_size` bytes
    /// - `category` must point to a buffer with at least `category_size` bytes
    /// - `preset_number` must be a valid pointer to an i32
    /// - `name` and `category` will be null-terminated
    /// - `name_size` should be at least 256 bytes for typical preset names
    pub fn rack_vst3_plugin_get_preset_info(
        plugin: *mut RackVST3Plugin,
        index: u32,
        name: *mut c_char,
        name_size: usize,
        category: *mut c_char,
        category_size: usize,
        preset_number: *mut i32,
    ) -> c_int;

    /// Load a factory preset by preset number
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Plugin must be initialized
    /// - `preset_number` should be a valid preset number from get_preset_info
    pub fn rack_vst3_plugin_load_preset(plugin: *mut RackVST3Plugin, preset_number: i32) -> c_int;

    /// Get plugin state size (for allocation)
    ///
    /// # Returns
    ///
    /// - Size in bytes needed to store state (> 0)
    /// - 0 if state cannot be retrieved
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Plugin must be initialized
    pub fn rack_vst3_plugin_get_state_size(plugin: *mut RackVST3Plugin) -> c_int;

    /// Get plugin state (full state including parameters, preset, etc.)
    ///
    /// # Returns
    ///
    /// - 0 on success (state written to data buffer, actual size written to size pointer)
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Plugin must be initialized
    /// - `data` must point to a buffer with at least `*size` bytes
    /// - `size` must be a valid pointer to a size_t
    /// - On input, `*size` is the buffer size
    /// - On output, `*size` is the actual size written
    /// - Typical usage: call get_state_size() first, allocate buffer, then call get_state()
    pub fn rack_vst3_plugin_get_state(
        plugin: *mut RackVST3Plugin,
        data: *mut u8,
        size: *mut usize,
    ) -> c_int;

    /// Set plugin state (restore full state including parameters, preset, etc.)
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - Plugin must be initialized
    /// - `data` must point to valid state data (from previous get_state call)
    /// - `data` must remain valid for the duration of the call
    /// - `size` must be the size of the state data in bytes
    pub fn rack_vst3_plugin_set_state(
        plugin: *mut RackVST3Plugin,
        data: *const u8,
        size: usize,
    ) -> c_int;

    // ============================================================================
    // MIDI API
    // ============================================================================

    /// Send MIDI events to plugin
    ///
    /// # Returns
    ///
    /// - 0 on success
    /// - Negative error code on failure
    ///
    /// # Safety
    ///
    /// - `plugin` must be a valid pointer returned by `rack_vst3_plugin_new`
    /// - `events` must point to an array with at least `event_count` elements, or NULL if event_count is 0
    /// - All events must have valid channel (0-15) and data values
    /// - Must not be called concurrently with process() or other plugin operations
    pub fn rack_vst3_plugin_send_midi(
        plugin: *mut RackVST3Plugin,
        events: *const RackVST3MidiEvent,
        event_count: u32,
    ) -> c_int;
}

// MIDI event struct (matches C layout exactly)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RackVST3MidiEvent {
    pub sample_offset: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
    pub channel: u8,
}
