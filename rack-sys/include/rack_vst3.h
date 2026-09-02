#ifndef RACK_VST3_H
#define RACK_VST3_H

#ifdef __cplusplus
extern "C" {
#endif

#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

// Opaque types
typedef struct RackVST3Scanner RackVST3Scanner;
typedef struct RackVST3Plugin RackVST3Plugin;
typedef struct RackVST3Gui RackVST3Gui;

// Plugin type enum
typedef enum {
    RACK_VST3_TYPE_EFFECT = 0,
    RACK_VST3_TYPE_INSTRUMENT = 1,
    RACK_VST3_TYPE_ANALYZER = 2,
    RACK_VST3_TYPE_SPATIAL = 3,
    RACK_VST3_TYPE_OTHER = 4,
} RackVST3PluginType;

// Plugin info struct (passed to Rust)
typedef struct {
    char name[256];
    char manufacturer[256];
    char path[1024];
    char unique_id[64];  // VST3 uses UID (16 bytes as hex string)
    uint32_t version;
    RackVST3PluginType plugin_type;
    char category[128];  // VST3 subcategories (e.g., "Fx|Reverb")
} RackVST3PluginInfo;

// Error codes (0 = success, negative = error)
#define RACK_VST3_OK 0
#define RACK_VST3_ERROR_GENERIC -1
#define RACK_VST3_ERROR_NOT_FOUND -2
#define RACK_VST3_ERROR_INVALID_PARAM -3
#define RACK_VST3_ERROR_NOT_INITIALIZED -4
#define RACK_VST3_ERROR_LOAD_FAILED -5
#define RACK_VST3_ERROR_NOT_SUPPORTED -6  // Feature not supported by this plugin
// One or more MIDI events passed to rack_vst3_plugin_send_midi were dropped
// because the plugin's input event queue was already full (see that
// function's own doc comment for the exact capacity and which events were
// actually delivered before the drop).
#define RACK_VST3_ERROR_EVENT_QUEUE_FULL -7

// ============================================================================
// Scanner API
// ============================================================================

// Create a new scanner
// Returns NULL if allocation fails
RackVST3Scanner* rack_vst3_scanner_new(void);

// Free scanner
void rack_vst3_scanner_free(RackVST3Scanner* scanner);

// Add a search path for VST3 plugins
// Returns 0 on success, negative error code on failure
int rack_vst3_scanner_add_path(RackVST3Scanner* scanner, const char* path);

// Add system default VST3 search paths
// Returns 0 on success, negative error code on failure
int rack_vst3_scanner_add_default_paths(RackVST3Scanner* scanner);

// Scan for plugins
// Returns number of plugins found (or would be found), or negative error code
//
// Two-pass usage pattern (recommended):
//   1. count = rack_vst3_scanner_scan(scanner, NULL, 0);  // Get total count
//   2. rack_vst3_scanner_scan(scanner, array, count);     // Fill array
//
// If plugins is NULL: Only counts plugins, does not extract details
// If plugins is not NULL: Fills array up to max_plugins
//
// IMPORTANT: Return value may exceed max_plugins if more plugins exist.
//            Compare return value with max_plugins to detect truncation.
//
// plugins: output array (allocated by caller), or NULL to get count only
// max_plugins: size of output array (ignored if plugins is NULL)
int rack_vst3_scanner_scan(RackVST3Scanner* scanner, RackVST3PluginInfo* plugins, size_t max_plugins);

// ============================================================================
// Plugin Instance API
// ============================================================================

// Create a new plugin instance from path and UID
// path: path to .vst3 bundle/folder
// uid: plugin UID (from scan result)
// Returns plugin instance or NULL on error
RackVST3Plugin* rack_vst3_plugin_new(const char* path, const char* uid);

// Free plugin instance
void rack_vst3_plugin_free(RackVST3Plugin* plugin);

// Initialize plugin
//
// If initial_state is non-NULL, it is applied via IComponent::setState in
// "Setup" state (after setupProcessing, before setActive), which is the
// correct VST3 lifecycle window for state restoration.  Pass NULL / 0 for
// a normal initialization with the plugin's default state.
//
// State blob format: [uint32_t component_size | component bytes | controller bytes]
//
// Returns 0 on success, negative error code on failure
int rack_vst3_plugin_initialize(RackVST3Plugin* plugin, double sample_rate, uint32_t max_block_size,
                                const uint8_t* initial_state, size_t initial_state_size);

// Check if plugin is initialized
int rack_vst3_plugin_is_initialized(RackVST3Plugin* plugin);

// Reset plugin state
// Clears all internal buffers, delay lines, and state without changing parameters.
// Useful for clearing reverb tails, delay lines, etc. between songs or after preset changes.
//
// Returns:
//   0 (RACK_VST3_OK) on success
//   RACK_VST3_ERROR_NOT_INITIALIZED if plugin is not initialized
//   negative error code on failure
//
// Thread-safety: Should be called from a non-realtime thread.
int rack_vst3_plugin_reset(RackVST3Plugin* plugin);

// Get input channel count
// Returns number of input channels, or 0 if not initialized or query failed
// Thread-safety: Should be called after initialize()
int rack_vst3_plugin_get_input_channels(RackVST3Plugin* plugin);

// Get output channel count
// Returns number of output channels, or 0 if not initialized or query failed
// Thread-safety: Should be called after initialize()
int rack_vst3_plugin_get_output_channels(RackVST3Plugin* plugin);

// Get the plugin's own reported processing latency, in samples (the delay
// its own processing introduces between input and output -- e.g. lookahead
// limiters, linear-phase EQs, convolution). Returns 0 if not initialized,
// if the plugin has no processor interface, or if the plugin genuinely
// reports zero latency (the common case for most instruments/effects).
// Thread-safety: Should be called after initialize(), from the UI/control
// thread (mirrors the VST3 SDK's own "[UI-thread & Setup Done]" contract
// for IAudioProcessor::getLatencySamples()).
uint32_t rack_vst3_plugin_get_latency_samples(RackVST3Plugin* plugin);

// Returns the number of audio output buses the plugin declares (>= 0), or a
// negative RACK_VST3_ERROR_* code on failure. This is the bus COUNT, distinct
// from rack_vst3_plugin_get_output_channels (which reports only bus 0's
// channel count).
int rack_vst3_plugin_get_output_bus_count(RackVST3Plugin* plugin);

// Returns the number of audio input buses the plugin declares (>= 0), or a
// negative RACK_VST3_ERROR_* code on failure.
int rack_vst3_plugin_get_input_bus_count(RackVST3Plugin* plugin);

// Fills `channels_out` with the channel count of output bus `bus_index`, and
// (if `name_out` is non-null) copies up to `name_out_size - 1` bytes of the
// bus's UTF-8 name into `name_out`, null-terminated. Returns RACK_VST3_OK on
// success, or a negative RACK_VST3_ERROR_* code (RACK_VST3_ERROR_INVALID_PARAM
// if bus_index is out of range).
int rack_vst3_plugin_get_output_bus_info(
    RackVST3Plugin* plugin,
    int32_t bus_index,
    uint32_t* channels_out,
    char* name_out,
    size_t name_out_size);

// Fills `channels_out` with the channel count of input bus `bus_index`, and
// (if `name_out` is non-null) copies up to `name_out_size - 1` bytes of the
// bus's UTF-8 name into `name_out`, null-terminated. Returns RACK_VST3_OK on
// success, or a negative RACK_VST3_ERROR_* code (RACK_VST3_ERROR_INVALID_PARAM
// if bus_index is out of range).
int rack_vst3_plugin_get_input_bus_info(
    RackVST3Plugin* plugin,
    int32_t bus_index,
    uint32_t* channels_out,
    char* name_out,
    size_t name_out_size);

// Process audio (planar format - one buffer per channel)
// Uses planar (non-interleaved) audio format matching VST3 internal format.
// This enables zero-copy processing in effect chains.
//
// inputs: array of input channel pointers (e.g., [left_ptr, right_ptr] for stereo)
// num_input_channels: number of input channels
// outputs: array of output channel pointers (e.g., [left_ptr, right_ptr] for stereo)
// num_output_channels: number of output channels
// frames: number of frames to process
//
// Channel Layout Examples:
//   Mono:   inputs = [mono_ptr], num_input_channels = 1
//   Stereo: inputs = [left_ptr, right_ptr], num_input_channels = 2
//   5.1:    inputs = [L, R, C, LFE, SL, SR], num_input_channels = 6
//
// Returns 0 on success, negative error code on failure
int rack_vst3_plugin_process(
    RackVST3Plugin* plugin,
    const float* const* inputs,
    uint32_t num_input_channels,
    float* const* outputs,
    uint32_t num_output_channels,
    uint32_t frames
);

// Describes one additional (non-main) output bus to capture in the same
// process() call as `rack_vst3_plugin_process_multi_bus`. `bus_index` must
// be >= 1 (bus 0 is the main bus, already covered by that function's own
// `outputs`/`num_output_channels` parameters). `channels` is a planar
// (one-pointer-per-channel) array like `outputs` above, with exactly
// `channel_count` entries; `channel_count` must exactly match the channel
// count `rack_vst3_plugin_get_output_bus_info` reports for `bus_index`.
typedef struct {
    int32_t bus_index;
    float* const* channels;
    uint32_t channel_count;
} RackVST3ExtraOutputBus;

// Describes one additional (non-main) input bus to feed in the same
// process() call as `rack_vst3_plugin_process_multi_bus`. `bus_index` must
// be >= 1 (bus 0 is the main bus, already covered by that function's own
// `inputs`/`num_input_channels` parameters). `channels` is a planar
// (one-pointer-per-channel) array, read-only from the host's perspective,
// with exactly `channel_count` entries; `channel_count` must exactly match
// the channel count `rack_vst3_plugin_get_input_bus_info` reports for
// `bus_index`.
typedef struct {
    int32_t bus_index;
    const float* const* channels;
    uint32_t channel_count;
} RackVST3ExtraInputBus;

// Forward declaration of the MIDI event struct (defined in full in the MIDI
// API section below). Declared here so rack_vst3_plugin_process_multi_bus can
// capture the plugin's own emitted MIDI output events into a caller buffer.
typedef struct RackVST3MidiEvent RackVST3MidiEvent;

// Process audio, capturing the main output bus (bus 0, same semantics as
// `rack_vst3_plugin_process`) plus zero or more additional output buses in
// the same underlying VST3 `process()` call.
//
// A VST3 plugin computes every *activated* output bus in a single
// `process()` call regardless of how many the host actually captures;
// `rack_vst3_plugin_initialize` now activates every bus the plugin
// declares. This function lets a caller additionally bind real buffers to
// buses beyond bus 0 (e.g. a drum plugin's per-instrument buses) instead of
// letting their audio fall into the internal silent dummy buffer that
// `rack_vst3_plugin_process` leaves them pointed at. Symmetrically,
// `extra_input_buses` lets a caller feed real buffers into input buses
// beyond bus 0 (e.g. an effect plugin's per-instrument input buses) in the
// same call, instead of leaving them unbound.
//
// inputs, num_input_channels, outputs, num_output_channels, frames: exactly
// as `rack_vst3_plugin_process` (bus 0 / main bus semantics, identical
// validation).
//
// extra_output_buses: array of `num_extra_output_buses` requests for
// additional output buses (bus index >= 1) to capture in this same call.
// Each entry's `bus_index` must be in range and its `channel_count` must
// exactly match that bus's declared channel count, or this call fails with
// RACK_VST3_ERROR_INVALID_PARAM (and no buffer pointers are rebound at
// all -- validation happens before any process_data mutation). Pass NULL /
// 0 to behave exactly like `rack_vst3_plugin_process`.
//
// extra_input_buses: array of `num_extra_input_buses` requests for
// additional input buses (bus index >= 1) to feed in this same call. Each
// entry's `bus_index` must be in range and its `channel_count` must exactly
// match that bus's declared channel count (see
// `rack_vst3_plugin_get_input_bus_info`), or this call fails with
// RACK_VST3_ERROR_INVALID_PARAM (and no buffer pointers are rebound at
// all -- validation happens before any process_data mutation, exactly as
// for `extra_output_buses`). Duplicate `bus_index` values across the array
// are also rejected. Pass NULL / 0 to feed no extra input buses.
//
// output_events_out: caller-owned buffer that receives the MIDI events the
// plugin itself emits during this process() call (e.g. from an arpeggiator or
// MIDI-FX plugin), decoded into `RackVST3MidiEvent`s. Pass NULL (together with
// output_events_capacity == 0) to skip output-MIDI capture entirely -- this is
// byte-for-byte identical to the audio-only behavior for callers that don't
// want emitted MIDI. Only event buses activated by
// rack_vst3_plugin_initialize (which now activates event output buses) can
// populate this.
//
// output_events_capacity: number of `RackVST3MidiEvent` slots in
// output_events_out. At most this many events are written; if the plugin
// emitted more, the buffer is truncated but *num_output_events_out still
// reports the true total.
//
// num_output_events_out: written with the true count of emitted MIDI events
// (which may exceed output_events_capacity, indicating truncation), or NULL to
// skip receiving the count. Written 0 when no capture is requested.
//
// Returns 0 on success, negative error code on failure.
//
// # Thread-safety
//
// Same as `rack_vst3_plugin_process`: real-time-safe (no allocation), must
// not be called concurrently on the same plugin from multiple threads.
int rack_vst3_plugin_process_multi_bus(
    RackVST3Plugin* plugin,
    const float* const* inputs,
    uint32_t num_input_channels,
    float* const* outputs,
    uint32_t num_output_channels,
    uint32_t frames,
    const RackVST3ExtraOutputBus* extra_output_buses,
    size_t num_extra_output_buses,
    const RackVST3ExtraInputBus* extra_input_buses,
    size_t num_extra_input_buses,
    RackVST3MidiEvent* output_events_out,
    size_t output_events_capacity,
    size_t* num_output_events_out
);

// Returns whether the plugin declares any event OUTPUT bus (i.e. whether it
// can emit its own MIDI, such as an arpeggiator or MIDI-FX plugin). Mirrors
// rack_vst3_plugin_get_output_bus_count's style. Returns false if `plugin` or
// its component is NULL, or if it declares no event output bus.
bool rack_vst3_plugin_emits_midi(RackVST3Plugin* plugin);

// Get parameter count
// Thread-safety: Read-only after initialization. Safe to call from any thread.
int rack_vst3_plugin_parameter_count(RackVST3Plugin* plugin);

// Get parameter value (normalized 0.0 to 1.0)
// Returns 0 on success, negative error code on failure
// Thread-safety: Can be called from any thread, but the same plugin instance
// must not be accessed concurrently.
int rack_vst3_plugin_get_parameter(RackVST3Plugin* plugin, uint32_t index, float* value);

// Set parameter value (normalized 0.0 to 1.0). ALSO calls
// IEditController::setParamNormalized synchronously, on whatever thread
// calls this function, so the controller (and therefore any GUI) reflects
// the new value immediately -- a plugin's parameter-changed handler can do
// arbitrary non-realtime work in response (allocation, UI updates), so this
// function is NOT real-time-safe and must not be called from an audio
// thread. Use rack_vst3_plugin_queue_parameter_change instead for automation
// applied from the audio thread; this function remains for genuinely
// non-realtime callers (e.g. loading a preset, or a user turning a knob on
// the main thread) that also need the controller/GUI kept in sync.
//
// In addition to the controller call above, this also queues the value into
// the same per-block automation queue rack_vst3_plugin_queue_parameter_change
// uses, so the plugin's audio processor receives the change on its next
// rack_vst3_plugin_process[_multi_bus] call either way.
// Returns 0 on success, negative error code on failure.
// Thread-safety: Can be called from any thread, but the same plugin instance
// must not be accessed concurrently. NOT real-time-safe (see above).
// Note: Calling during audio processing may cause clicks/pops.
int rack_vst3_plugin_set_parameter(RackVST3Plugin* plugin, uint32_t index, float value);
int rack_vst3_plugin_set_parameter_by_id(RackVST3Plugin* plugin, uint32_t param_id, float value);

// Queue a normalized parameter value change ([0.0, 1.0], clamped) to be
// delivered to the plugin's audio processor on the *next*
// rack_vst3_plugin_process / rack_vst3_plugin_process_multi_bus call, via
// VST3's IParameterChanges automation mechanism -- the same delivery path
// real VST3 hosts use for per-block parameter automation.
//
// Unlike rack_vst3_plugin_set_parameter, this does NOT call
// IEditController::setParamNormalized -- it never touches the controller, so
// it is real-time-safe and may be called from the audio thread immediately
// before rack_vst3_plugin_process[_multi_bus]. Because it skips the
// controller call, the plugin's UI/controller-side parameter cache (and
// therefore any GUI) will NOT reflect the change; use
// rack_vst3_plugin_set_parameter from a non-realtime thread instead when
// that matters (e.g. loading a preset).
//
// index: parameter index (see rack_vst3_plugin_parameter_info), not a raw
// VST3 param ID.
//
// The queued point is delivered at sample offset 0 of the next process call.
// The queue is cleared after every process call (see
// rack_vst3_plugin_process), so nothing queued here is ever left stale for a
// later, unrelated process call.
//
// Returns 0 on success, negative error code (e.g. RACK_VST3_ERROR_INVALID_PARAM
// for an out-of-range index) on failure.
// Thread-safety: real-time-safe -- rack_vst3_plugin_initialize pre-creates
// one ParameterValueQueue per known parameter (via ParameterChanges::
// setMaxParameters) before returning, so every call this function makes into
// input_param_changes takes its allocation-free reuse branch; no allocation
// occurs, full stop, once initialize() has returned. Must not be called
// concurrently with rack_vst3_plugin_process[_multi_bus] or other plugin
// operations on the same plugin.
int rack_vst3_plugin_queue_parameter_change(RackVST3Plugin* plugin, uint32_t index, float value);

uint32_t rack_vst3_plugin_get_midi_pc_param_id(RackVST3Plugin* plugin, int channel);

// Update the plugin's live process context: tempo (BPM) and playback state
// (nonzero = playing, zero = stopped). Writes directly into the plugin's
// persistent ProcessContext (built once during initialize and never
// rebuilt), which the plugin reads on every subsequent process() call.
// Thread-safety: unlike rack_vst3_plugin_queue_parameter_change (queued for
// the audio thread to apply on its own next process() call), this writes
// the struct fields immediately with no queuing or locking -- the caller
// MUST call this only from the same thread that calls
// rack_vst3_plugin_process, immediately before it, never concurrently with
// a process() call in flight.
// Returns 0 on success, negative error code on failure.
int rack_vst3_plugin_set_transport(RackVST3Plugin* plugin, double tempo, int32_t playing);

// Get parameter info
// name: output buffer for parameter name (allocated by caller)
// name_size: size of name buffer
// unit: output buffer for parameter unit string (allocated by caller, can be NULL)
// unit_size: size of unit buffer (ignored if unit is NULL)
// step_count: number of discrete steps (0 = continuous), or NULL to skip
// flags: raw VST3 ParameterInfo::ParameterFlags bitmask, or NULL to skip
// Returns 0 on success, negative error code on failure
int rack_vst3_plugin_parameter_info(
    RackVST3Plugin* plugin,
    uint32_t index,
    char* name,
    size_t name_size,
    float* min,
    float* max,
    float* default_value,
    char* unit,
    size_t unit_size,
    int32_t* step_count,
    int32_t* flags
);

// Get the display string for a normalized parameter value, as the plugin
// itself would render it (e.g. an amp-type step's model name). This is the
// VST3 IEditController::getParamStringByValue call.
//
// out_size: size of out buffer; out will be null-terminated
// Returns 0 on success, negative error code on failure
int rack_vst3_plugin_parameter_string_by_value(
    RackVST3Plugin* plugin,
    uint32_t index,
    float normalized_value,
    char* out,
    size_t out_size
);

// Convert a normalized parameter value (0.0-1.0) to the plugin's "plain"
// (native/real-world unit) value, e.g. actual milliseconds or Hz. This is
// the VST3 IEditController::normalizedParamToPlain call. Combine with
// ParameterInfo::unit for the unit label. Some plugins return the
// normalized value unchanged when no real plain-value mapping exists.
//
// Returns 0 on success, negative error code on failure
int rack_vst3_plugin_parameter_plain_value(
    RackVST3Plugin* plugin,
    uint32_t index,
    float normalized_value,
    double* out_plain
);

// Parse a display-style string (as a user might type it, or as
// rack_vst3_plugin_parameter_string_by_value would render it) into a
// normalized parameter value. This is the VST3
// IEditController::getParamValueByString call -- the inverse of
// rack_vst3_plugin_parameter_string_by_value. Whether the plugin accepts a
// bare number, a number with a unit suffix, or only its own exact display
// format is plugin-defined; not every plugin implements this meaningfully.
//
// Returns 0 on success, negative error code on failure (including when the
// plugin cannot parse the given string).
int rack_vst3_plugin_parameter_value_by_string(
    RackVST3Plugin* plugin,
    uint32_t index,
    const char* string,
    float* out_normalized
);

// Convert a plain (native/real-world unit) parameter value, e.g. actual
// milliseconds or Hz, to its normalized (0.0-1.0) form. This is the VST3
// IEditController::plainParamToNormalized call -- the inverse of
// rack_vst3_plugin_parameter_plain_value.
//
// Returns 0 on success, negative error code on failure
int rack_vst3_plugin_parameter_normalized_from_plain(
    RackVST3Plugin* plugin,
    uint32_t index,
    double plain_value,
    float* out_normalized
);

// ============================================================================
// Preset Management API
// ============================================================================

// Preset info struct
typedef struct {
    char name[256];
    int32_t preset_number;
} RackVST3PresetInfo;

// Get factory preset count
// Returns number of factory presets, or 0 if plugin has no presets
// Thread-safety: Read-only after initialization. Safe to call from any thread.
int rack_vst3_plugin_get_preset_count(RackVST3Plugin* plugin);

// Get preset info by index
// index: preset index (0 to preset_count - 1)
// name: output buffer for preset name (allocated by caller)
// name_size: size of name buffer
// category: output buffer for the preset's program-list name (allocated by
//           caller). Written as an empty string if the plugin's programs
//           aren't organized into named lists, or the list has no name.
// category_size: size of category buffer
// preset_number: output parameter for preset number (used with load_preset)
// Returns 0 on success, negative error code on failure
int rack_vst3_plugin_get_preset_info(
    RackVST3Plugin* plugin,
    uint32_t index,
    char* name,
    size_t name_size,
    char* category,
    size_t category_size,
    int32_t* preset_number
);

// Load a factory preset by preset number
// preset_number: the preset number from get_preset_info()
// Returns 0 on success, negative error code on failure
// Thread-safety: Should be called from the same thread that owns the plugin instance.
int rack_vst3_plugin_load_preset(RackVST3Plugin* plugin, int32_t preset_number);

// Get plugin state size (for allocation)
// Returns actual size in bytes needed to store state, or 0 if state cannot be retrieved
//
// NOTE: This function determines the actual state size by serializing the state.
//       It is not a constant or estimate - it queries the plugin's current state.
//       For large plugins (samplers, complex synths), this may take some time.
//       The returned size is accurate for immediate use with get_state().
//
// Thread-safety: Should be called from the same thread that owns the plugin instance.
int rack_vst3_plugin_get_state_size(RackVST3Plugin* plugin);

// Get plugin state (full state including parameters, preset, etc.)
// data: output buffer for state data (allocated by caller)
// size: input/output - buffer size on input, actual size on output
// Returns 0 on success, negative error code on failure
// Thread-safety: Should be called from the same thread that owns the plugin instance.
int rack_vst3_plugin_get_state(RackVST3Plugin* plugin, uint8_t* data, size_t* size);

// Set plugin state (restore full state including parameters, preset, etc.)
// data: state data (from previous get_state call)
// size: size of state data in bytes
// Returns 0 on success, negative error code on failure
// Thread-safety: Should be called from the same thread that owns the plugin instance.
int rack_vst3_plugin_set_state(RackVST3Plugin* plugin, const uint8_t* data, size_t size);

// ============================================================================
// MIDI API
// ============================================================================

// MIDI event types (matches VST3 event types)
typedef enum {
    RACK_VST3_MIDI_NOTE_ON = 0x90,
    RACK_VST3_MIDI_NOTE_OFF = 0x80,
    RACK_VST3_MIDI_POLYPHONIC_AFTERTOUCH = 0xA0,
    RACK_VST3_MIDI_CONTROL_CHANGE = 0xB0,
    RACK_VST3_MIDI_PROGRAM_CHANGE = 0xC0,
    RACK_VST3_MIDI_CHANNEL_AFTERTOUCH = 0xD0,
    RACK_VST3_MIDI_PITCH_BEND = 0xE0,
} RackVST3MidiEventType;

// MIDI event struct (forward-declared above for
// rack_vst3_plugin_process_multi_bus's output-capture parameters).
typedef struct RackVST3MidiEvent {
    uint32_t sample_offset;  // Sample offset within buffer
    uint8_t status;          // MIDI status byte
    uint8_t data1;           // First data byte (note/CC number)
    uint8_t data2;           // Second data byte (velocity/value)
    uint8_t channel;         // MIDI channel (0-15)
} RackVST3MidiEvent;

// Send MIDI events to plugin
// events: array of MIDI events
// event_count: number of events in array
//
// NOTE: VST3 has native support for Note On/Off, Polyphonic Aftertouch, and Control Change.
//       Program Change, Channel Aftertouch, and Pitch Bend use custom encoding via
//       LegacyMIDICCOutEvent with controlNumber >= 0x80. Not all VST3 plugins support
//       these non-native event types. If a plugin doesn't respond to Program Change,
//       Channel Aftertouch, or Pitch Bend, it's a limitation of the plugin itself.
//
// Returns 0 on success, negative error code on failure
// Thread-safety: Should be called from the same thread that owns the plugin instance.
// Not safe to call concurrently with process() or other plugin operations.
int rack_vst3_plugin_send_midi(
    RackVST3Plugin* plugin,
    const RackVST3MidiEvent* events,
    uint32_t event_count
);

#ifdef __cplusplus
}
#endif

#endif // RACK_VST3_H
