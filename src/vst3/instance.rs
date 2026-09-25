use crate::{Error, MidiEvent, MidiEventKind, ParameterInfo, PluginInfo, PluginInstance, PresetInfo, ProcessMode, Result};
use smallvec::SmallVec;
use std::ffi::CString;
use std::marker::PhantomData;
use std::os::raw::c_int;
use std::ptr::NonNull;

use super::ffi;
use super::util::map_error;

/// One additional (non-main) output bus to capture via
/// [`Vst3Plugin::process_multi_bus`], and the caller-supplied buffers
/// (planar: one mutable slice per channel) to route its audio into.
pub struct ExtraOutputBus<'a> {
    /// The bus index to capture. Must be >= 1 -- bus 0 is the main bus,
    /// already handled by `process_multi_bus`'s own `outputs` parameter.
    pub bus_index: u32,
    /// One mutable slice per channel. Length must exactly match the bus's
    /// declared channel count (see
    /// [`Vst3Plugin::output_bus_channel_count`]); each slice must have
    /// space for at least `num_frames` samples.
    pub channels: &'a mut [&'a mut [f32]],
}

/// One additional (non-main) input bus to feed via
/// [`Vst3Plugin::process_multi_bus`], and the caller-supplied buffers
/// (planar: one shared slice per channel) to read its audio from.
pub struct ExtraInputBus<'a> {
    /// The bus index to feed. Must be >= 1 -- bus 0 is the main bus,
    /// already handled by `process_multi_bus`'s own `inputs` parameter.
    pub bus_index: u32,
    /// One shared slice per channel. Length must exactly match the bus's
    /// declared channel count (see
    /// [`Vst3Plugin::input_bus_channel_count`]); each slice must have at
    /// least `num_frames` samples. Shared (not mutable) references: these
    /// are buffers Rust hands to the plugin to read, not buffers the plugin
    /// writes into.
    pub channels: &'a [&'a [f32]],
}

/// An instantiated VST3 plugin
///
/// # Thread Safety
///
/// This type is `Send` but not `Sync`:
/// - `Send`: The plugin can be moved between threads safely
/// - NOT `Sync`: Multiple threads should not access the plugin simultaneously
///   without synchronization. Wrap in `Arc<Mutex<>>` if shared access is needed.
pub struct Vst3Plugin {
    inner: NonNull<ffi::RackVST3Plugin>,
    info: PluginInfo,
    // Pre-allocated pointer arrays for zero-allocation process() calls
    input_ptrs: Vec<*const f32>,
    output_ptrs: Vec<*mut f32>,
    // Channel configuration (queried from VST3 during initialize)
    input_channels: usize,
    output_channels: usize,
    // Channel count of every declared output bus (index 0..bus_count),
    // queried once during initialize. Index 0 always equals
    // `output_channels` above; this generalizes it to every bus so
    // `process_multi_bus` callers can size buffers for buses beyond bus 0.
    output_bus_channel_counts: Vec<usize>,
    // Channel count of every declared input bus (index 0..bus_count),
    // queried once during initialize, mirroring
    // `output_bus_channel_counts` above. Index 0 always equals
    // `input_channels` above; most instrument plugins (e.g. AD2) declare
    // zero input buses.
    input_bus_channel_counts: Vec<usize>,
    // Persistent scratch storage for process_multi_bus's zero-allocation hot
    // path: flat, planar channel-pointer storage for every requested extra
    // bus's channels, reserved (during initialize) to the sum of every
    // non-main bus's channel count -- a safe upper bound for any subset of
    // buses a single call might request, so pushing into it during
    // process_multi_bus never grows the allocation.
    extra_bus_channel_ptrs: Vec<*mut f32>,
    // One descriptor per requested extra bus, reserved (during initialize)
    // to `output_bus_channel_counts.len() - 1` (every bus but bus 0).
    extra_bus_descs: Vec<ffi::RackVST3ExtraOutputBus>,
    // Persistent scratch storage for process_multi_bus's zero-allocation hot
    // path, mirroring `extra_bus_channel_ptrs` above but for extra *input*
    // buses: flat, planar channel-pointer storage for every requested extra
    // input bus's channels, reserved (during initialize) to the sum of every
    // non-main input bus's channel count.
    extra_input_bus_channel_ptrs: Vec<*const f32>,
    // One descriptor per requested extra input bus, reserved (during
    // initialize) to `input_bus_channel_counts.len() - 1` (every bus but
    // bus 0), mirroring `extra_bus_descs` above.
    extra_input_bus_descs: Vec<ffi::RackVST3ExtraInputBus>,
    // Persistent, pre-sized scratch buffer for capturing the plugin's own
    // emitted MIDI output events in `process_multi_bus`'s zero-allocation hot
    // path. Sized once (in `initialize_impl`) to `OUTPUT_MIDI_SCRATCH_CAPACITY`
    // and never resized, so the FFI out-pointer always addresses valid slots.
    output_midi_scratch: Vec<ffi::RackVST3MidiEvent>,
    // PhantomData<*const ()> makes this type !Sync while keeping it Send
    _not_sync: PhantomData<*const ()>,
}

// Safety: Vst3Plugin can be sent between threads because:
// 1. Each plugin instance owns its C++ state exclusively
// 2. The plugin doesn't share mutable state with other instances
// 3. VST3 plugins are designed to be movable between threads (host requirement)
unsafe impl Send for Vst3Plugin {}

// Note: Vst3Plugin is NOT Sync due to PhantomData<*const ()>
// This is intentional - VST3 instances require synchronization for shared access

impl Vst3Plugin {
    /// Create a new VST3 plugin instance
    pub(crate) fn new(info: &PluginInfo) -> Result<Self> {
        unsafe {
            // Convert path to CString
            let path_str = info.path.to_str()
                .ok_or_else(|| Error::Other("Plugin path contains invalid UTF-8".to_string()))?;
            let path = CString::new(path_str)
                .map_err(|_| Error::Other("Plugin path contains null byte".to_string()))?;

            // Convert unique_id to CString
            let unique_id = CString::new(info.unique_id.as_str())
                .map_err(|_| Error::Other("Invalid unique_id (contains null byte)".to_string()))?;

            // Create plugin instance via FFI
            let ptr = ffi::rack_vst3_plugin_new(path.as_ptr(), unique_id.as_ptr());
            if ptr.is_null() {
                return Err(Error::PluginNotFound(format!(
                    "Failed to create VST3 instance for {}",
                    info.name
                )));
            }

            Ok(Self {
                inner: NonNull::new(ptr).expect("pointer is non-null after null check"),
                info: info.clone(),
                input_ptrs: Vec::new(),
                output_ptrs: Vec::new(),
                input_channels: 0,
                output_channels: 0,
                output_bus_channel_counts: Vec::new(),
                input_bus_channel_counts: Vec::new(),
                extra_bus_channel_ptrs: Vec::new(),
                extra_bus_descs: Vec::new(),
                extra_input_bus_channel_ptrs: Vec::new(),
                extra_input_bus_descs: Vec::new(),
                output_midi_scratch: Vec::new(),
                _not_sync: PhantomData,
            })
        }
    }

    /// Set a parameter by its raw VST3 ParamID (not the rack index).
    /// Used to reach MIDI-mapped parameters like Program Change that are
    /// not in the indexed parameter list.
    pub fn set_param_by_id(&mut self, param_id: u32, value: f32) -> Result<()> {
        unsafe {
            let result = ffi::rack_vst3_plugin_set_parameter_by_id(
                self.inner.as_ptr(),
                param_id,
                value,
            );
            if result != ffi::RACK_VST3_OK {
                return Err(crate::Error::Other(format!("set_param_by_id failed: {result}")));
            }
            Ok(())
        }
    }

    /// Get the VST3 ParamID that handles MIDI Program Change on the given channel.
    /// Returns 0xFFFFFFFF (kNoParamId) if not mapped.
    pub fn midi_pc_param_id(&self, channel: i32) -> u32 {
        unsafe {
            ffi::rack_vst3_plugin_get_midi_pc_param_id(self.inner.as_ptr(), channel)
        }
    }

    /// Update the plugin's live process context tempo, playback state and
    /// position, so its next `process()` call observes the host's real,
    /// current transport instead of whatever it was initialized with.
    ///
    /// The position is the host's alone: `project_time_samples` is written
    /// straight into the VST3 `projectTimeSamples` field and is never
    /// advanced internally, so a plugin loaded or re-initialized partway
    /// through a session is told where the host really is rather than
    /// starting again from zero. `project_time_music` is the same position
    /// in quarter notes (1.0 is one quarter note) and `bar_position_music`
    /// the start of the bar containing it, also in quarter notes; both are
    /// advertised to the plugin as valid, so a host with no musical position
    /// to report passes 0.0 and thereby claims bar 1, beat 1.
    ///
    /// Unlike [`queue_parameter_change`](PluginInstance::queue_parameter_change)
    /// (queued for the audio thread to apply on its own next `process()`
    /// call), this writes directly into the plugin's persistent process
    /// context struct with no queuing or locking on the C++ side -- callers
    /// MUST only call this from the same thread that also calls `process()`,
    /// immediately before it, never concurrently with a `process()` call in
    /// flight. The one real caller (`backline::signal_graph_audio`'s
    /// per-block transport publish step) already satisfies this by
    /// construction: it runs inside the same audio-callback invocation that
    /// then calls `process()`.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin has not been initialized.
    pub fn set_transport(
        &mut self,
        tempo: f64,
        playing: bool,
        project_time_samples: i64,
        project_time_music: f64,
        bar_position_music: f64,
    ) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let result = ffi::rack_vst3_plugin_set_transport(
                self.inner.as_ptr(),
                tempo,
                i32::from(playing),
                project_time_samples,
                project_time_music,
                bar_position_music,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(())
        }
    }

    /// Number of audio input channels the plugin's main bus expects, as
    /// negotiated during `initialize`. 0 for pure instrument plugins with no
    /// audio input bus. Only meaningful after a successful `initialize` call.
    #[must_use]
    pub fn input_channels(&self) -> usize {
        self.input_channels
    }

    /// Number of audio output channels the plugin's main bus produces, as
    /// negotiated during `initialize`. Only meaningful after a successful
    /// `initialize` call.
    #[must_use]
    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    /// The plugin's own reported processing latency, in samples: the delay
    /// its own processing introduces between input and output (e.g.
    /// lookahead limiters, linear-phase EQs, convolution). 0 if the plugin
    /// has not been initialized or genuinely reports zero latency (the
    /// common case for most instruments/effects). Only meaningful after a
    /// successful `initialize` call.
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        // The C function itself gates on the plugin's initialized state and
        // returns 0 if it isn't, so no separate is_initialized() check is
        // needed here (unlike set_transport, which must distinguish "not
        // initialized" from a real result via an Err return).
        unsafe { ffi::rack_vst3_plugin_get_latency_samples(self.inner.as_ptr()) }
    }

    /// Number of audio output buses the plugin declares (cached from
    /// `initialize`). Distinct from [`output_channels`](Self::output_channels),
    /// which reports only bus 0's channel count. A plugin like Addictive
    /// Drums 2 may declare several named per-instrument output buses beyond
    /// bus 0; see [`process_multi_bus`](Self::process_multi_bus) to capture
    /// them. Only meaningful after a successful `initialize` call.
    #[must_use]
    pub fn output_bus_count(&self) -> usize {
        self.output_bus_channel_counts.len()
    }

    /// Channel count of output bus `bus_index` (cached from `initialize`),
    /// or `None` if `bus_index` is out of range. Needed to size the buffers
    /// passed to [`process_multi_bus`](Self::process_multi_bus) for a given
    /// bus.
    #[must_use]
    pub fn output_bus_channel_count(&self, bus_index: usize) -> Option<usize> {
        self.output_bus_channel_counts.get(bus_index).copied()
    }

    /// Channel count and declared display name of output bus `bus_index`,
    /// queried live via VST3 `IComponent::getBusInfo`. Read-only; does not
    /// activate the bus.
    ///
    /// Prefer [`output_bus_channel_count`](Self::output_bus_channel_count)
    /// for just the channel count (already cached from `initialize`, no FFI
    /// round-trip); this method exists for callers that also want the bus's
    /// declared name (e.g. "Kick", "Snare").
    pub fn output_bus_info(&self, bus_index: i32) -> Result<(usize, String)> {
        unsafe {
            let mut channels: u32 = 0;
            let mut name_buf = vec![0i8; 256];
            let result = ffi::rack_vst3_plugin_get_output_bus_info(
                self.inner.as_ptr(),
                bus_index,
                &mut channels,
                name_buf.as_mut_ptr(),
                name_buf.len(),
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            let name_cstr = std::ffi::CStr::from_ptr(name_buf.as_ptr());
            let name = name_cstr
                .to_str()
                .map_err(|e| Error::Other(format!("Invalid UTF-8 in output bus name: {}", e)))?
                .to_string();

            Ok((channels as usize, name))
        }
    }

    /// Number of audio input buses the plugin declares (cached from
    /// `initialize`). Distinct from [`input_channels`](Self::input_channels),
    /// which reports only bus 0's channel count. Most instrument plugins
    /// (e.g. Addictive Drums 2) declare zero input buses. Only meaningful
    /// after a successful `initialize` call.
    #[must_use]
    pub fn input_bus_count(&self) -> usize {
        self.input_bus_channel_counts.len()
    }

    /// Channel count of input bus `bus_index` (cached from `initialize`),
    /// or `None` if `bus_index` is out of range.
    #[must_use]
    pub fn input_bus_channel_count(&self, bus_index: usize) -> Option<usize> {
        self.input_bus_channel_counts.get(bus_index).copied()
    }

    /// Channel count and declared display name of input bus `bus_index`,
    /// queried live via VST3 `IComponent::getBusInfo`. Read-only; does not
    /// activate the bus.
    ///
    /// Prefer [`input_bus_channel_count`](Self::input_bus_channel_count)
    /// for just the channel count (already cached from `initialize`, no FFI
    /// round-trip); this method exists for callers that also want the bus's
    /// declared name.
    pub fn input_bus_info(&self, bus_index: i32) -> Result<(usize, String)> {
        unsafe {
            let mut channels: u32 = 0;
            let mut name_buf = vec![0i8; 256];
            let result = ffi::rack_vst3_plugin_get_input_bus_info(
                self.inner.as_ptr(),
                bus_index,
                &mut channels,
                name_buf.as_mut_ptr(),
                name_buf.len(),
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            let name_cstr = std::ffi::CStr::from_ptr(name_buf.as_ptr());
            let name = name_cstr
                .to_str()
                .map_err(|e| Error::Other(format!("Invalid UTF-8 in input bus name: {}", e)))?
                .to_string();

            Ok((channels as usize, name))
        }
    }

    /// Initialise the plugin and restore `state` via `IComponent::setState`
    /// while in "Setup" state (after `setupProcessing`, before `setActive`).
    ///
    /// This is the correct VST3 lifecycle window for state restoration; it
    /// guarantees that async sample loading (e.g. AD2) completes before the
    /// audio callback starts.
    ///
    /// `process_mode` is fixed here for this instance's whole life, exactly as
    /// in [`PluginInstance::initialize`] -- see [`ProcessMode`].
    pub fn initialize_with_state(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
        process_mode: ProcessMode,
        state: &[u8],
    ) -> Result<()> {
        self.initialize_impl(sample_rate, max_block_size, process_mode, state)
    }

    fn initialize_impl(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
        process_mode: ProcessMode,
        state: &[u8],
    ) -> Result<()> {
        unsafe {
            let (state_ptr, state_len) = if state.is_empty() {
                (std::ptr::null(), 0)
            } else {
                (state.as_ptr(), state.len())
            };

            let result = ffi::rack_vst3_plugin_initialize(
                self.inner.as_ptr(),
                sample_rate,
                max_block_size as u32,
                process_mode.as_raw(),
                state_ptr,
                state_len,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            let input_channels = ffi::rack_vst3_plugin_get_input_channels(self.inner.as_ptr());
            let output_channels = ffi::rack_vst3_plugin_get_output_channels(self.inner.as_ptr());

            if input_channels < 0 || output_channels < 0 {
                return Err(Error::Other("Failed to query channel configuration".to_string()));
            }

            self.input_channels = input_channels as usize;
            self.output_channels = output_channels as usize;

            self.input_ptrs = Vec::with_capacity(self.input_channels.max(8));
            self.output_ptrs = Vec::with_capacity(self.output_channels.max(8));
            self.input_ptrs.resize(self.input_channels, std::ptr::null());
            self.output_ptrs.resize(self.output_channels, std::ptr::null_mut());

            // Discover every declared output bus's channel count (not just
            // bus 0) so process_multi_bus's persistent scratch storage can
            // be reserved once, here, instead of reallocating per call.
            let output_bus_count = ffi::rack_vst3_plugin_get_output_bus_count(self.inner.as_ptr());
            if output_bus_count < 0 {
                return Err(Error::Other("Failed to query output bus count".to_string()));
            }
            let mut output_bus_channel_counts = Vec::with_capacity(output_bus_count as usize);
            for bus_index in 0..output_bus_count {
                let mut channels: u32 = 0;
                let result = ffi::rack_vst3_plugin_get_output_bus_info(
                    self.inner.as_ptr(),
                    bus_index,
                    &mut channels,
                    std::ptr::null_mut(),
                    0,
                );
                if result != ffi::RACK_VST3_OK {
                    return Err(map_error(result));
                }
                output_bus_channel_counts.push(channels as usize);
            }
            self.output_bus_channel_counts = output_bus_channel_counts;

            let input_bus_count = ffi::rack_vst3_plugin_get_input_bus_count(self.inner.as_ptr());
            if input_bus_count < 0 {
                return Err(Error::Other("Failed to query input bus count".to_string()));
            }
            let mut input_bus_channel_counts = Vec::with_capacity(input_bus_count as usize);
            for bus_index in 0..input_bus_count {
                let mut channels: u32 = 0;
                let result = ffi::rack_vst3_plugin_get_input_bus_info(
                    self.inner.as_ptr(),
                    bus_index,
                    &mut channels,
                    std::ptr::null_mut(),
                    0,
                );
                if result != ffi::RACK_VST3_OK {
                    return Err(map_error(result));
                }
                input_bus_channel_counts.push(channels as usize);
            }
            self.input_bus_channel_counts = input_bus_channel_counts;

            let extra_bus_capacity: usize =
                self.output_bus_channel_counts.iter().skip(1).sum();
            let extra_bus_count = self.output_bus_channel_counts.len().saturating_sub(1);
            self.extra_bus_channel_ptrs = Vec::with_capacity(extra_bus_capacity);
            self.extra_bus_descs = Vec::with_capacity(extra_bus_count);

            let extra_input_bus_capacity: usize =
                self.input_bus_channel_counts.iter().skip(1).sum();
            let extra_input_bus_count = self.input_bus_channel_counts.len().saturating_sub(1);
            self.extra_input_bus_channel_ptrs = Vec::with_capacity(extra_input_bus_capacity);
            self.extra_input_bus_descs = Vec::with_capacity(extra_input_bus_count);

            // Pre-size the emitted-MIDI capture scratch to a generous fixed
            // bound (grown once here, never resized), so `process_multi_bus`'s
            // FFI out-pointer always addresses `OUTPUT_MIDI_SCRATCH_CAPACITY`
            // valid slots without allocating in the hot path.
            self.output_midi_scratch = vec![
                ffi::RackVST3MidiEvent {
                    sample_offset: 0,
                    status: 0,
                    data1: 0,
                    data2: 0,
                    channel: 0,
                };
                Self::OUTPUT_MIDI_SCRATCH_CAPACITY
            ];

            Ok(())
        }
    }

    /// Fixed upper bound on the number of emitted MIDI events captured from a
    /// single `process_multi_bus` call. A block is at most a few thousand
    /// samples, so a MIDI-FX plugin emitting hundreds of events in one block is
    /// already far beyond realistic; 256 is generous and never reached in
    /// practice, matching this crate's "generous fixed bound" precedent for its
    /// other scratch buffers. A plugin that somehow emits more has its capture
    /// truncated to this many, with the true count still reported.
    const OUTPUT_MIDI_SCRATCH_CAPACITY: usize = 256;

    // VST3 ParameterInfo::ParameterFlags bit values, from the Steinberg SDK
    // (pluginterfaces/vst/ivsteditcontroller.h). rack does not depend on the
    // SDK headers directly, so the bits are mirrored here.
    const PARAM_FLAG_CAN_AUTOMATE: i32 = 1 << 0;
    const PARAM_FLAG_IS_READ_ONLY: i32 = 1 << 1;
    const PARAM_FLAG_IS_LIST: i32 = 1 << 3;
    const PARAM_FLAG_IS_HIDDEN: i32 = 1 << 4;

    /// Get the display string a plugin would show for a normalized
    /// parameter value (VST3 `IEditController::getParamStringByValue`).
    ///
    /// For a discrete/list parameter (see `ParameterInfo::is_list` and
    /// `step_count`), this returns the human-readable name of the step the
    /// value falls on (e.g. an amp model name), not the raw normalized
    /// number.
    pub fn parameter_value_string(&self, index: usize, normalized_value: f32) -> Result<String> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let mut out = vec![0i8; 256];
            let result = ffi::rack_vst3_plugin_parameter_string_by_value(
                self.inner.as_ptr(),
                index as u32,
                normalized_value,
                out.as_mut_ptr(),
                out.len(),
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            let out_cstr = std::ffi::CStr::from_ptr(out.as_ptr());
            out_cstr
                .to_str()
                .map_err(|e| Error::Other(format!("Invalid UTF-8 in parameter value string: {}", e)))
                .map(str::to_string)
        }
    }

    /// Convert a normalized parameter value to the plugin's "plain"
    /// (native/real-world unit) value, e.g. actual milliseconds or Hz.
    /// Combine with [`ParameterInfo::unit`] for the unit label. Some
    /// plugins return the normalized value unchanged when no real
    /// plain-value mapping is implemented.
    pub fn parameter_plain_value(&self, index: usize, normalized_value: f32) -> Result<f64> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let mut out_plain = 0.0f64;
            let result = ffi::rack_vst3_plugin_parameter_plain_value(
                self.inner.as_ptr(),
                index as u32,
                normalized_value,
                &mut out_plain,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(out_plain)
        }
    }

    /// Parse a display-style string (as a user might type it, or as
    /// [`Vst3Plugin::parameter_value_string`] would render it) into a
    /// normalized parameter value (VST3 `IEditController::getParamValueByString`,
    /// the inverse of `parameter_value_string`).
    ///
    /// Whether the plugin accepts a bare number, a number with a unit
    /// suffix, or only its own exact display format is plugin-defined; not
    /// every plugin implements this meaningfully, in which case this
    /// returns an error rather than a guessed value.
    pub fn parameter_value_by_string(&self, index: usize, string: &str) -> Result<f32> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        let c_string = CString::new(string)
            .map_err(|error| Error::Other(format!("Invalid parameter string: {error}")))?;

        unsafe {
            let mut out_normalized = 0.0f32;
            let result = ffi::rack_vst3_plugin_parameter_value_by_string(
                self.inner.as_ptr(),
                index as u32,
                c_string.as_ptr(),
                &mut out_normalized,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(out_normalized)
        }
    }

    /// Convert a plain (native/real-world unit) parameter value, e.g. actual
    /// milliseconds or Hz, to its normalized (0.0-1.0) form (VST3
    /// `IEditController::plainParamToNormalized`, the inverse of
    /// [`Vst3Plugin::parameter_plain_value`]).
    pub fn parameter_normalized_from_plain(&self, index: usize, plain_value: f64) -> Result<f32> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let mut out_normalized = 0.0f32;
            let result = ffi::rack_vst3_plugin_parameter_normalized_from_plain(
                self.inner.as_ptr(),
                index as u32,
                plain_value,
                &mut out_normalized,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(out_normalized)
        }
    }

    /// Process audio, capturing the main output bus (bus 0, identical
    /// semantics to [`PluginInstance::process`]) plus zero or more
    /// additional output buses, and feeding zero or more additional input
    /// buses, in the same underlying VST3 `process()` call.
    ///
    /// A VST3 plugin computes every activated output bus in a single
    /// `process()` call regardless of how many the host actually captures;
    /// `initialize` activates every bus the plugin declares. This lets a
    /// caller bind real buffers to output buses beyond bus 0 (e.g. a drum
    /// plugin's per-instrument buses) instead of leaving them pointed at
    /// the internal silent dummy buffer, as [`PluginInstance::process`]
    /// does for anything but bus 0, and symmetrically feed real buffers
    /// into input buses beyond bus 0 (e.g. an effect plugin's
    /// per-instrument input buses, such as feeding one plugin's captured
    /// output bus into a second plugin's matching-named input bus) instead
    /// of leaving them unbound.
    ///
    /// `extra_outputs`/`extra_inputs` may each request buses in any order
    /// and any subset of the plugin's declared buses (beyond bus 0); each
    /// entry's `channels` length must exactly match that bus's declared
    /// channel count (see
    /// [`output_bus_channel_count`](Self::output_bus_channel_count) /
    /// [`input_bus_channel_count`](Self::input_bus_channel_count)).
    ///
    /// `output_midi`, when `Some`, is cleared and then filled with the MIDI
    /// events the plugin itself emitted during this call (e.g. an
    /// arpeggiator's or MIDI-FX plugin's output). Pass `None` to skip
    /// emitted-MIDI capture entirely, which is exactly the behavior every
    /// audio-only caller wants and is byte-for-byte identical to before this
    /// capability existed. Capture is zero-allocation in the steady state:
    /// once `output_midi`'s `Vec` and the plugin's internal scratch buffer
    /// have each grown once, neither reallocates. At most
    /// [`OUTPUT_MIDI_SCRATCH_CAPACITY`](Self::OUTPUT_MIDI_SCRATCH_CAPACITY)
    /// events are captured; a plugin emitting more has its output truncated to
    /// that many.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized, if `inputs`/
    /// `outputs` channel counts or buffer lengths don't match the plugin's
    /// main-bus configuration, or if any `extra_outputs`/`extra_inputs`
    /// entry's `bus_index` is out of range, its channel count doesn't match
    /// the bus's declared channel count, its `bus_index` duplicates another
    /// entry's in the same slice, or any of its channel buffers is shorter
    /// than `num_frames`.
    pub fn process_multi_bus(
        &mut self,
        inputs: &[&[f32]],
        extra_inputs: &[ExtraInputBus<'_>],
        outputs: &mut [&mut [f32]],
        extra_outputs: &mut [ExtraOutputBus<'_>],
        num_frames: usize,
        output_midi: Option<&mut Vec<MidiEvent>>,
    ) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        // Validate bus-0 (main) channel counts and buffer lengths exactly
        // as PluginInstance::process does.
        if inputs.len() != self.input_channels {
            return Err(Error::Other(format!(
                "Input channel count mismatch: plugin expects {}, got {}",
                self.input_channels, inputs.len()
            )));
        }
        if outputs.len() != self.output_channels {
            return Err(Error::Other(format!(
                "Output channel count mismatch: plugin expects {}, got {}",
                self.output_channels, outputs.len()
            )));
        }
        for (index, input) in inputs.iter().enumerate() {
            if input.len() < num_frames {
                return Err(Error::Other(format!(
                    "Input channel {} has {} samples, need at least {}",
                    index, input.len(), num_frames
                )));
            }
        }
        for (index, output) in outputs.iter().enumerate() {
            if output.len() < num_frames {
                return Err(Error::Other(format!(
                    "Output channel {} has {} samples, need at least {}",
                    index, output.len(), num_frames
                )));
            }
        }

        // Validate every requested extra output bus against the plugin's
        // declared bus layout before touching any scratch storage. Bus
        // indices must be unique: `extra_bus_channel_ptrs`'s capacity
        // (reserved in `initialize_impl`) is the sum of every *distinct*
        // non-main bus's channel count, a bound a duplicate request would
        // silently exceed, forcing a reallocation mid-loop below and
        // leaving an already FFI-bound pointer dangling.
        for (position, extra) in extra_outputs.iter().enumerate() {
            let bus_index = extra.bus_index as usize;
            if bus_index == 0 {
                return Err(Error::Other(
                    "Bus 0 is the main bus; pass it via `outputs`, not `extra_outputs`".to_string(),
                ));
            }
            if extra_outputs[..position]
                .iter()
                .any(|other| other.bus_index == extra.bus_index)
            {
                return Err(Error::Other(format!(
                    "Output bus index {bus_index} requested more than once in extra_outputs"
                )));
            }
            let expected_channels = self
                .output_bus_channel_counts
                .get(bus_index)
                .copied()
                .ok_or_else(|| Error::Other(format!("Output bus index {bus_index} out of range")))?;
            if extra.channels.len() != expected_channels {
                return Err(Error::Other(format!(
                    "Output bus {bus_index} channel count mismatch: plugin declares {expected_channels}, got {}",
                    extra.channels.len()
                )));
            }
            for (channel_index, channel) in extra.channels.iter().enumerate() {
                if channel.len() < num_frames {
                    return Err(Error::Other(format!(
                        "Output bus {bus_index} channel {channel_index} has {} samples, need at least {num_frames}",
                        channel.len()
                    )));
                }
            }
        }

        // Validate every requested extra input bus, mirroring the extra
        // output bus validation above (including the same duplicate-index
        // rejection, for the same scratch-buffer-capacity safety reason:
        // `extra_input_bus_channel_ptrs`'s capacity, reserved in
        // `initialize_impl`, is the sum of every *distinct* non-main input
        // bus's channel count).
        for (position, extra) in extra_inputs.iter().enumerate() {
            let bus_index = extra.bus_index as usize;
            if bus_index == 0 {
                return Err(Error::Other(
                    "Bus 0 is the main bus; pass it via `inputs`, not `extra_inputs`".to_string(),
                ));
            }
            if extra_inputs[..position]
                .iter()
                .any(|other| other.bus_index == extra.bus_index)
            {
                return Err(Error::Other(format!(
                    "Input bus index {bus_index} requested more than once in extra_inputs"
                )));
            }
            let expected_channels = self
                .input_bus_channel_counts
                .get(bus_index)
                .copied()
                .ok_or_else(|| Error::Other(format!("Input bus index {bus_index} out of range")))?;
            if extra.channels.len() != expected_channels {
                return Err(Error::Other(format!(
                    "Input bus {bus_index} channel count mismatch: plugin declares {expected_channels}, got {}",
                    extra.channels.len()
                )));
            }
            for (channel_index, channel) in extra.channels.iter().enumerate() {
                if channel.len() < num_frames {
                    return Err(Error::Other(format!(
                        "Input bus {bus_index} channel {channel_index} has {} samples, need at least {num_frames}",
                        channel.len()
                    )));
                }
            }
        }

        // Reuse pre-allocated pointer arrays (zero-allocation hot path).
        for (index, input_ch) in inputs.iter().enumerate() {
            self.input_ptrs[index] = input_ch.as_ptr();
        }
        for (index, output_ch) in outputs.iter_mut().enumerate() {
            self.output_ptrs[index] = output_ch.as_mut_ptr();
        }

        // Flatten each extra output bus's channel pointers into scratch
        // storage reserved (in `initialize_impl`) to the sum of every
        // non-main bus's channel count -- a safe upper bound for any subset
        // of buses a single call might request, so these pushes never grow
        // the allocation.
        self.extra_bus_channel_ptrs.clear();
        self.extra_bus_descs.clear();
        for extra in extra_outputs.iter_mut() {
            let start = self.extra_bus_channel_ptrs.len();
            for channel in extra.channels.iter_mut() {
                self.extra_bus_channel_ptrs.push(channel.as_mut_ptr());
            }
            let channels_ptr = self.extra_bus_channel_ptrs[start..].as_ptr();
            self.extra_bus_descs.push(ffi::RackVST3ExtraOutputBus {
                bus_index: extra.bus_index as i32,
                channels: channels_ptr,
                channel_count: extra.channels.len() as u32,
            });
        }

        // Flatten each extra input bus's channel pointers into scratch
        // storage the same way, reserved (in `initialize_impl`) to the sum
        // of every non-main input bus's channel count.
        self.extra_input_bus_channel_ptrs.clear();
        self.extra_input_bus_descs.clear();
        for extra in extra_inputs.iter() {
            let start = self.extra_input_bus_channel_ptrs.len();
            for channel in extra.channels.iter() {
                self.extra_input_bus_channel_ptrs.push(channel.as_ptr());
            }
            let channels_ptr = self.extra_input_bus_channel_ptrs[start..].as_ptr();
            self.extra_input_bus_descs.push(ffi::RackVST3ExtraInputBus {
                bus_index: extra.bus_index as i32,
                channels: channels_ptr,
                channel_count: extra.channels.len() as u32,
            });
        }

        // Bind the emitted-MIDI capture out-parameters. When the caller wants
        // capture, point the FFI at the pre-sized scratch buffer (a raw
        // pointer, so this borrow ends immediately and doesn't conflict with
        // the other field borrows in the call below); otherwise pass NULL/0 to
        // preserve the exact audio-only behavior every existing caller relies
        // on.
        let capture = output_midi.is_some();
        let (out_events_ptr, out_events_cap) = if capture {
            (self.output_midi_scratch.as_mut_ptr(), self.output_midi_scratch.len())
        } else {
            (std::ptr::null_mut(), 0usize)
        };
        let mut num_output_events: usize = 0;
        let num_events_ptr = if capture {
            &mut num_output_events as *mut usize
        } else {
            std::ptr::null_mut()
        };

        unsafe {
            let result = ffi::rack_vst3_plugin_process_multi_bus(
                self.inner.as_ptr(),
                self.input_ptrs.as_ptr(),
                inputs.len() as u32,
                self.output_ptrs.as_ptr(),
                outputs.len() as u32,
                num_frames as u32,
                self.extra_bus_descs.as_ptr(),
                self.extra_bus_descs.len(),
                self.extra_input_bus_descs.as_ptr(),
                self.extra_input_bus_descs.len(),
                out_events_ptr,
                out_events_cap,
                num_events_ptr,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }
        }

        // Decode the captured raw events into rack's own `MidiEvent` type. The
        // C side already truncated to the scratch capacity, so `count` can't
        // exceed it; `num_output_events` may still report a larger true total,
        // which we deliberately ignore beyond the captured prefix.
        if let Some(buffer) = output_midi {
            buffer.clear();
            let count = num_output_events.min(self.output_midi_scratch.len());
            for raw in &self.output_midi_scratch[..count] {
                if let Some(event) = rack_midi_to_midi_event(raw) {
                    buffer.push(event);
                }
            }
        }

        Ok(())
    }

    /// Whether the plugin declares any event OUTPUT bus, i.e. whether it can
    /// emit its own MIDI (an arpeggiator or MIDI-FX plugin). When true,
    /// [`process_multi_bus`](Self::process_multi_bus) with `Some(buffer)`
    /// will capture those emitted events.
    #[must_use]
    pub fn emits_midi(&self) -> bool {
        unsafe { ffi::rack_vst3_plugin_emits_midi(self.inner.as_ptr()) }
    }

    /// Same as [`PluginInstance::set_state`], but the wait for async
    /// completion (e.g. sample loading) after `setState` is `pump_seconds`
    /// instead of a fixed 5.0 -- for a caller that will poll for real
    /// readiness itself afterward (delivering a real note/signal and
    /// checking for real output) rather than trusting a fixed wait. Passing
    /// the same 5.0 [`PluginInstance::set_state`] always used is equivalent
    /// to calling that method.
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as
    /// [`PluginInstance::set_state`].
    pub fn set_state_with_pump_seconds(&mut self, data: &[u8], pump_seconds: f64) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        if data.is_empty() {
            return Err(Error::Other("State data is empty".to_string()));
        }

        unsafe {
            let result = ffi::rack_vst3_plugin_set_state_pumped(
                self.inner.as_ptr(),
                data.as_ptr(),
                data.len(),
                pump_seconds,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(())
        }
    }
}

/// Decode a captured raw `RackVST3MidiEvent` (the C layer's decode of a VST3
/// output `Event`) into rack's own [`MidiEvent`], the inverse of the encode in
/// [`Vst3Plugin::send_midi`]. Returns `None` for a status byte with no
/// `MidiEventKind` mapping. Pure and plugin-independent, so it is unit-tested
/// directly against synthetic `RackVST3MidiEvent`s.
fn rack_midi_to_midi_event(raw: &ffi::RackVST3MidiEvent) -> Option<MidiEvent> {
    let channel = raw.channel;
    let kind = match raw.status & 0xF0 {
        0x90 => MidiEventKind::NoteOn { note: raw.data1, velocity: raw.data2, channel },
        0x80 => MidiEventKind::NoteOff { note: raw.data1, velocity: raw.data2, channel },
        0xA0 => MidiEventKind::PolyphonicAftertouch { note: raw.data1, pressure: raw.data2, channel },
        0xB0 => MidiEventKind::ControlChange { controller: raw.data1, value: raw.data2, channel },
        0xC0 => MidiEventKind::ProgramChange { program: raw.data1, channel },
        0xD0 => MidiEventKind::ChannelAftertouch { pressure: raw.data1, channel },
        0xE0 => {
            // Reassemble the 14-bit pitch-bend value from LSB (data1) and MSB
            // (data2), mirroring `send_midi`'s split encode exactly.
            let value = ((raw.data1 as u16) & 0x7F) | (((raw.data2 as u16) & 0x7F) << 7);
            MidiEventKind::PitchBend { value, channel }
        }
        _ => return None,
    };
    Some(MidiEvent { sample_offset: raw.sample_offset, kind })
}

/// Given the result code from a `rack_vst3_plugin_get_state` call, the
/// size of the buffer that was handed to it, and the size the call
/// reported back, decides whether the failure was specifically the
/// documented overflow case (the real state is larger than the buffer
/// given, so the call fails but still reports the true required size back
/// through its size out-parameter -- see `get_state`'s own comment on this
/// contract) and, if so, what buffer size to retry the call with.
///
/// Pure and independent of the real FFI call, so the retry decision
/// itself is unit-tested directly, without needing a real plugin whose
/// state exceeds the ~4 MB initial guess -- no such fixture is known to be
/// available on any real test machine (see `get_state`'s own comment on
/// that gap).
fn state_retry_size(result: c_int, buffer_len: usize, reported_size: usize) -> Option<usize> {
    if result != ffi::RACK_VST3_OK && reported_size > buffer_len {
        Some(reported_size)
    } else {
        None
    }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        unsafe {
            ffi::rack_vst3_plugin_free(self.inner.as_ptr());
        }
    }
}

impl PluginInstance for Vst3Plugin {
    fn initialize(&mut self, sample_rate: f64, max_block_size: usize, process_mode: ProcessMode) -> Result<()> {
        self.initialize_impl(sample_rate, max_block_size, process_mode, &[])
    }

    fn reset(&mut self) -> Result<()> {
        unsafe {
            let result = ffi::rack_vst3_plugin_reset(self.inner.as_ptr());

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(())
        }
    }

    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_frames: usize,
    ) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        // Validate channel counts match plugin configuration
        if inputs.len() != self.input_channels {
            return Err(Error::Other(format!(
                "Input channel count mismatch: plugin expects {}, got {}",
                self.input_channels, inputs.len()
            )));
        }
        if outputs.len() != self.output_channels {
            return Err(Error::Other(format!(
                "Output channel count mismatch: plugin expects {}, got {}",
                self.output_channels, outputs.len()
            )));
        }

        // Defense-in-depth: outputs must always be present; inputs may be empty
        // for instrument plugins that have no audio input buses.
        if outputs.is_empty() {
            return Err(Error::Other("Empty output channels".to_string()));
        }

        // Validate all channels have the same length
        for (i, input) in inputs.iter().enumerate() {
            if input.len() < num_frames {
                return Err(Error::Other(format!(
                    "Input channel {} has {} samples, need at least {}",
                    i,
                    input.len(),
                    num_frames
                )));
            }
        }

        for (i, output) in outputs.iter().enumerate() {
            if output.len() < num_frames {
                return Err(Error::Other(format!(
                    "Output channel {} has {} samples, need at least {}",
                    i,
                    output.len(),
                    num_frames
                )));
            }
        }

        // Reuse pre-allocated pointer arrays (zero-allocation hot path)
        // Fill with current buffer pointers
        for (i, input_ch) in inputs.iter().enumerate() {
            self.input_ptrs[i] = input_ch.as_ptr();
        }
        for (i, output_ch) in outputs.iter_mut().enumerate() {
            self.output_ptrs[i] = output_ch.as_mut_ptr();
        }

        unsafe {
            let result = ffi::rack_vst3_plugin_process(
                self.inner.as_ptr(),
                self.input_ptrs.as_ptr(),
                inputs.len() as u32,
                self.output_ptrs.as_ptr(),
                outputs.len() as u32,
                num_frames as u32,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(())
        }
    }

    fn parameter_count(&self) -> usize {
        unsafe {
            let count = ffi::rack_vst3_plugin_parameter_count(self.inner.as_ptr());
            if count < 0 {
                0
            } else {
                count as usize
            }
        }
    }

    fn parameter_info(&self, index: usize) -> Result<ParameterInfo> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let mut name = vec![0i8; 256];
            let mut unit = vec![0i8; 32];
            let mut min = 0.0f32;
            let mut max = 0.0f32;
            let mut default_value = 0.0f32;
            let mut step_count = 0i32;
            let mut flags = 0i32;

            let result = ffi::rack_vst3_plugin_parameter_info(
                self.inner.as_ptr(),
                index as u32,
                name.as_mut_ptr(),
                name.len(),
                &mut min,
                &mut max,
                &mut default_value,
                unit.as_mut_ptr(),
                unit.len(),
                &mut step_count,
                &mut flags,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            // Convert name to String
            let name_cstr = std::ffi::CStr::from_ptr(name.as_ptr());
            let name_str = name_cstr
                .to_str()
                .map_err(|e| Error::Other(format!("Invalid UTF-8 in parameter name: {}", e)))?
                .to_string();

            // Convert unit to String
            let unit_cstr = std::ffi::CStr::from_ptr(unit.as_ptr());
            let unit_str = unit_cstr
                .to_str()
                .map_err(|e| Error::Other(format!("Invalid UTF-8 in parameter unit: {}", e)))?
                .to_string();

            Ok(ParameterInfo {
                index,
                name: name_str,
                min,
                max,
                default: default_value,
                unit: unit_str,
                step_count,
                can_automate: flags & Self::PARAM_FLAG_CAN_AUTOMATE != 0,
                is_read_only: flags & Self::PARAM_FLAG_IS_READ_ONLY != 0,
                is_list: flags & Self::PARAM_FLAG_IS_LIST != 0,
                is_hidden: flags & Self::PARAM_FLAG_IS_HIDDEN != 0,
            })
        }
    }

    fn get_parameter(&self, index: usize) -> Result<f32> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let mut value = 0.0f32;
            let result =
                ffi::rack_vst3_plugin_get_parameter(self.inner.as_ptr(), index as u32, &mut value);

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(value)
        }
    }

    fn set_parameter(&mut self, index: usize, value: f32) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let result =
                ffi::rack_vst3_plugin_set_parameter(self.inner.as_ptr(), index as u32, value);

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(())
        }
    }

    /// Real-time-safe VST3 implementation: queues the point into VST3's
    /// `IParameterChanges` per-block automation mechanism -- the same
    /// delivery path real VST3 hosts use for automation -- without ever
    /// calling `IEditController::setParamNormalized`. See this method's
    /// trait-level doc ([`PluginInstance::queue_parameter_change`]) for the
    /// cross-backend contract; here specifically, once `initialize()` has
    /// run (which pre-sizes the underlying parameter-change queue via
    /// `setMaxParameters`, so this never allocates), this method is
    /// real-time-safe and is the correct method to call from the audio
    /// thread immediately before a process call. The tradeoff: the
    /// plugin's controller/GUI does not observe the change this way -- use
    /// `set_parameter`/[`set_param_by_id`](Self::set_param_by_id) from a
    /// non-realtime thread instead when that matters (e.g. loading a preset
    /// from the main thread).
    ///
    /// The queued point is delivered at sample offset 0 of the very next
    /// process call; the underlying queue is cleared after every process
    /// call, so nothing queued here is ever left stale for a later,
    /// unrelated process call.
    fn queue_parameter_change(&mut self, index: usize, value: f32) -> Result<()> {
        unsafe {
            let result = ffi::rack_vst3_plugin_queue_parameter_change(
                self.inner.as_ptr(),
                index as u32,
                value,
            );
            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }
            Ok(())
        }
    }

    fn send_midi(&mut self, events: &[MidiEvent]) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        if events.is_empty() {
            return Ok(());
        }

        // Convert Rust MIDI events to C MIDI events
        // Use SmallVec for zero-allocation in typical cases (≤16 events)
        let mut c_events: SmallVec<[ffi::RackVST3MidiEvent; 16]> = SmallVec::with_capacity(events.len());

        for event in events {
            let (status, data1, data2, channel) = match &event.kind {
                MidiEventKind::NoteOn { note, velocity, channel } => (0x90, *note, *velocity, *channel),
                MidiEventKind::NoteOff { note, velocity, channel } => (0x80, *note, *velocity, *channel),
                MidiEventKind::PolyphonicAftertouch { note, pressure, channel } => (0xA0, *note, *pressure, *channel),
                MidiEventKind::ControlChange { controller, value, channel } => (0xB0, *controller, *value, *channel),
                MidiEventKind::ProgramChange { program, channel } => (0xC0, *program, 0, *channel),
                MidiEventKind::ChannelAftertouch { pressure, channel } => (0xD0, *pressure, 0, *channel),
                MidiEventKind::PitchBend { value, channel } => {
                    // Pitch bend is 14-bit (0-16383), centered at 8192
                    let lsb = (value & 0x7F) as u8;
                    let msb = ((value >> 7) & 0x7F) as u8;
                    (0xE0, lsb, msb, *channel)
                }
                // System messages don't have a channel - skip them for now
                // VST3 doesn't have a standard way to send system real-time messages
                MidiEventKind::TimingClock | MidiEventKind::Start | MidiEventKind::Continue |
                MidiEventKind::Stop | MidiEventKind::ActiveSensing | MidiEventKind::SystemReset => {
                    continue; // Skip system messages
                }
            };

            c_events.push(ffi::RackVST3MidiEvent {
                sample_offset: event.sample_offset,
                status,
                data1,
                data2,
                channel,
            });
        }

        unsafe {
            let result = ffi::rack_vst3_plugin_send_midi(
                self.inner.as_ptr(),
                c_events.as_ptr(),
                c_events.len() as u32,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(())
        }
    }

    fn preset_count(&self) -> Result<usize> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let count = ffi::rack_vst3_plugin_get_preset_count(self.inner.as_ptr());
            if count < 0 {
                Ok(0) // Plugin has no presets
            } else {
                Ok(count as usize)
            }
        }
    }

    fn preset_info(&self, index: usize) -> Result<PresetInfo> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let mut name = vec![0i8; 256];
            let mut category = vec![0i8; 256];
            let mut preset_number: i32 = 0;

            let result = ffi::rack_vst3_plugin_get_preset_info(
                self.inner.as_ptr(),
                index as u32,
                name.as_mut_ptr(),
                name.len(),
                category.as_mut_ptr(),
                category.len(),
                &mut preset_number,
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            // Convert name to String
            let name_cstr = std::ffi::CStr::from_ptr(name.as_ptr());
            let name_str = name_cstr
                .to_str()
                .map_err(|e| Error::Other(format!("Invalid UTF-8 in preset name: {}", e)))?
                .to_string();

            // Convert category to String; an empty string means "no category"
            // (the plugin's programs aren't organized into named lists, or
            // the list itself has no name).
            let category_cstr = std::ffi::CStr::from_ptr(category.as_ptr());
            let category_str = category_cstr
                .to_str()
                .map_err(|e| Error::Other(format!("Invalid UTF-8 in preset category: {}", e)))?
                .to_string();
            let category = if category_str.is_empty() { None } else { Some(category_str) };

            Ok(PresetInfo {
                index,
                name: name_str,
                preset_number,
                category,
            })
        }
    }

    fn load_preset(&mut self, preset_number: i32) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            let result = ffi::rack_vst3_plugin_load_preset(self.inner.as_ptr(), preset_number);

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(())
        }
    }

    fn get_state(&self) -> Result<Vec<u8>> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        unsafe {
            // Get state size: a generous initial guess (see
            // rack_vst3_plugin_get_state_size's own doc comment for why it
            // is a guess rather than a real measurement).
            let size = ffi::rack_vst3_plugin_get_state_size(self.inner.as_ptr());
            if size <= 0 {
                return Err(Error::Other("Failed to get plugin state size".to_string()));
            }

            // Allocate buffer
            let mut data = vec![0u8; size as usize];
            let mut actual_size = data.len();

            // Get state data
            let mut result = ffi::rack_vst3_plugin_get_state(
                self.inner.as_ptr(),
                data.as_mut_ptr(),
                &mut actual_size,
            );

            // rack_vst3_plugin_get_state's contract: when the real state is
            // larger than the buffer we gave it, it fails but still writes
            // the true required size back through `actual_size` (and does
            // not touch it on any other kind of failure, so this condition
            // can only be true after that specific overflow). state_retry_size
            // (a pure function, unit-tested on its own) decides whether this
            // is that overflow case; retry once with a buffer sized to the
            // real requirement instead of failing outright -- a state above
            // the initial guess is rare but not invalid.
            //
            // Known accepted risk: each rack_vst3_plugin_get_state call
            // brackets its own getState with a full suspend/resume cycle
            // (setProcessing(false)/setActive(false) then setActive(true)/
            // setProcessing(true)), so retrying here means two such cycles
            // run back to back for this one logical get_state() -- exactly
            // the pattern rack_vst3_plugin_get_state_size's own comment
            // warns crashes some JUCE plugins (AD2 included) when done for
            // an ordinary size-then-fetch. This is deliberately accepted
            // rather than avoided: it only triggers for the rare case of a
            // real state exceeding the initial guess, and failing outright
            // instead would be strictly worse for that caller.
            if let Some(retry_len) = state_retry_size(result, data.len(), actual_size) {
                data = vec![0u8; retry_len];
                let mut retry_size = data.len();
                result = ffi::rack_vst3_plugin_get_state(
                    self.inner.as_ptr(),
                    data.as_mut_ptr(),
                    &mut retry_size,
                );
                actual_size = retry_size;
            }

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            // Resize to actual size
            data.resize(actual_size, 0);

            Ok(data)
        }
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        if data.is_empty() {
            return Err(Error::Other("State data is empty".to_string()));
        }

        unsafe {
            let result = ffi::rack_vst3_plugin_set_state(
                self.inner.as_ptr(),
                data.as_ptr(),
                data.len(),
            );

            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }

            Ok(())
        }
    }

    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn is_initialized(&self) -> bool {
        unsafe {
            let result = ffi::rack_vst3_plugin_is_initialized(self.inner.as_ptr());
            result > 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginScanner;

    fn get_test_plugin() -> Result<(crate::vst3::Vst3Scanner, PluginInfo)> {
        let scanner = crate::vst3::Vst3Scanner::new()?;
        let plugins = scanner.scan()?;

        if plugins.is_empty() {
            return Err(Error::Other("No VST3 plugins found for testing".to_string()));
        }

        Ok((scanner, plugins[0].clone()))
    }

    /// Finds an installed VST3 plugin whose name contains `needle`
    /// (case-insensitive), for tests that need a specific plugin's bus
    /// layout (e.g. a real multi-output-bus or sidechain-input-bus plugin)
    /// rather than whatever the first scanned plugin happens to be. Returns
    /// `None` (rather than an `Err`) so call sites can uniformly skip with a
    /// message when the machine running the test doesn't have that plugin
    /// installed, matching this module's existing tolerance for a
    /// plugin-less test environment.
    fn find_plugin_by_name(needle: &str) -> Option<(crate::vst3::Vst3Scanner, PluginInfo)> {
        let scanner = crate::vst3::Vst3Scanner::new().ok()?;
        let plugins = scanner.scan().ok()?;
        let needle_lower = needle.to_lowercase();
        let info = plugins
            .into_iter()
            .find(|plugin| plugin.name.to_lowercase().contains(&needle_lower))?;
        Some((scanner, info))
    }

    /// A `cfg(test)`-only global allocator wrapper that counts Rust-side
    /// allocations, scoped per-thread so it survives `cargo test`'s default
    /// parallel test execution without one test's allocations polluting
    /// another's count -- only this crate's single test binary (there are
    /// no integration test targets, see `Cargo.toml`) exists to declare a
    /// `#[global_allocator]`, so there is no conflict with anything else in
    /// the crate's test setup.
    ///
    /// Important, honest scope: this can only observe allocations Rust code
    /// performs through its own allocation API (`Box`, `Vec`, `String`, ...
    /// -- compiled to calls into the `__rust_alloc` intrinsic that
    /// `#[global_allocator]` redirects). It does NOT intercept
    /// `malloc`/`operator new` calls made directly by `rack-sys`, the
    /// linked C++ library -- confirmed empirically with a standalone probe
    /// outside this crate: a direct FFI call to libc's `malloc` registers
    /// zero allocations against a counting `#[global_allocator]`, while a
    /// Rust `Vec` allocation registers one. `#[global_allocator]` is a
    /// Rust-compiler-level redirect, not a process-wide malloc/`operator
    /// new` interposition, so it cannot see native-side allocations -- see
    /// `queue_parameter_change_performs_no_rust_side_allocation`'s own doc
    /// comment for what this means for that test's real scope.
    mod alloc_probe {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static TRACKING: Cell<bool> = Cell::new(false);
            static COUNT: Cell<usize> = Cell::new(0);
        }

        struct CountingAllocator;

        unsafe impl GlobalAlloc for CountingAllocator {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                TRACKING.with(|tracking| {
                    if tracking.get() {
                        COUNT.with(|count| count.set(count.get() + 1));
                    }
                });
                System.alloc(layout)
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                System.dealloc(ptr, layout)
            }
        }

        #[global_allocator]
        static GLOBAL: CountingAllocator = CountingAllocator;

        /// Runs `body` on the calling thread, counting Rust-side
        /// allocations made while it runs. Allocations on other threads,
        /// or made by this same thread outside `body` (setup/teardown),
        /// are not counted.
        pub fn count_rust_allocations<F: FnOnce()>(body: F) -> usize {
            COUNT.with(|count| count.set(0));
            TRACKING.with(|tracking| tracking.set(true));
            body();
            TRACKING.with(|tracking| tracking.set(false));
            COUNT.with(|count| count.get())
        }
    }

    #[test]
    fn test_plugin_creation() {
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let result = scanner.load(&info);
        assert!(result.is_ok(), "Plugin creation should succeed");
    }

    #[test]
    fn test_plugin_initialize() {
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        let result = plugin.initialize(48000.0, 512, ProcessMode::Realtime);
        assert!(result.is_ok(), "Plugin initialization should succeed");
        assert!(plugin.is_initialized(), "Plugin should be initialized");
    }

    #[test]
    fn test_drop_behavior() {
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        {
            let _plugin = scanner.load(&info).expect("Plugin creation should succeed");
        } // Plugin dropped here
          // If Drop is implemented correctly, this shouldn't leak or crash
    }

    #[test]
    fn test_parameter_range_clamping() {
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        plugin
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("Plugin initialization should succeed");

        let count = plugin.parameter_count();
        if count == 0 {
            println!("Plugin has no parameters, skipping clamping test");
            return;
        }

        // Test setting values outside 0.0-1.0 range (should be clamped by C++ layer)
        plugin
            .set_parameter(0, 2.0)
            .expect("Should handle value > 1.0");
        let value = plugin.get_parameter(0).expect("Failed to get parameter");
        assert!(
            value <= 1.0,
            "Value > 1.0 should be clamped to 1.0, got {}",
            value
        );

        plugin
            .set_parameter(0, -1.0)
            .expect("Should handle value < 0.0");
        let value = plugin.get_parameter(0).expect("Failed to get parameter");
        assert!(
            value >= 0.0,
            "Value < 0.0 should be clamped to 0.0, got {}",
            value
        );
    }

    #[test]
    fn test_parameter_extreme_values() {
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        plugin
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("Plugin initialization should succeed");

        let count = plugin.parameter_count();
        if count == 0 {
            println!("Plugin has no parameters, skipping extreme values test");
            return;
        }

        // Test extreme values (clamped by C++ layer before passing to VST3)
        plugin
            .set_parameter(0, -1000.0)
            .expect("Should handle extreme negative values");
        let value = plugin.get_parameter(0).expect("Failed to get parameter");
        assert!(
            value >= 0.0 && value <= 1.0,
            "Extreme negative value should be clamped to 0.0-1.0, got {}",
            value
        );

        plugin
            .set_parameter(0, 1000.0)
            .expect("Should handle extreme positive values");
        let value = plugin.get_parameter(0).expect("Failed to get parameter");
        assert!(
            value >= 0.0 && value <= 1.0,
            "Extreme positive value should be clamped to 0.0-1.0, got {}",
            value
        );

        // Test f32::INFINITY and f32::NEG_INFINITY
        plugin
            .set_parameter(0, f32::INFINITY)
            .expect("Should handle f32::INFINITY");
        let value = plugin.get_parameter(0).expect("Failed to get parameter");
        assert!(
            value >= 0.0 && value <= 1.0,
            "f32::INFINITY should be clamped to 0.0-1.0, got {}",
            value
        );

        plugin
            .set_parameter(0, f32::NEG_INFINITY)
            .expect("Should handle f32::NEG_INFINITY");
        let value = plugin.get_parameter(0).expect("Failed to get parameter");
        assert!(
            value >= 0.0 && value <= 1.0,
            "f32::NEG_INFINITY should be clamped to 0.0-1.0, got {}",
            value
        );
    }

    #[test]
    fn test_preset_info_reports_category() {
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        plugin
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("Plugin initialization should succeed");

        let count = plugin.preset_count().expect("preset_count should succeed");
        if count == 0 {
            println!("Plugin has no presets, skipping preset category test");
            return;
        }

        // Whatever the plugin actually reports (grouped or not), the
        // category field must round-trip through the C API without
        // panicking or truncating the name -- an empty program-list name
        // must come back as `None`, never `Some(String::new())`.
        let preset = plugin.preset_info(0).expect("preset_info should succeed");
        assert!(!preset.name.is_empty(), "preset name should not be empty");
        if let Some(category) = &preset.category {
            assert!(!category.is_empty(), "category, if present, must not be an empty string");
        }
    }

    fn raw_event(sample_offset: u32, status: u8, data1: u8, data2: u8, channel: u8)
        -> ffi::RackVST3MidiEvent
    {
        ffi::RackVST3MidiEvent { sample_offset, status, data1, data2, channel }
    }

    #[test]
    fn decode_note_on() {
        let raw = raw_event(64, 0x90, 60, 100, 3);
        let event = rack_midi_to_midi_event(&raw).expect("note-on should decode");
        assert_eq!(event.sample_offset, 64);
        assert_eq!(
            event.kind,
            MidiEventKind::NoteOn { note: 60, velocity: 100, channel: 3 }
        );
    }

    #[test]
    fn decode_note_off() {
        let raw = raw_event(0, 0x80, 62, 40, 5);
        let event = rack_midi_to_midi_event(&raw).expect("note-off should decode");
        assert_eq!(
            event.kind,
            MidiEventKind::NoteOff { note: 62, velocity: 40, channel: 5 }
        );
    }

    #[test]
    fn decode_control_change() {
        let raw = raw_event(0, 0xB0, 1, 64, 0);
        let event = rack_midi_to_midi_event(&raw).expect("cc should decode");
        assert_eq!(
            event.kind,
            MidiEventKind::ControlChange { controller: 1, value: 64, channel: 0 }
        );
    }

    #[test]
    fn decode_pitch_bend_reassembles_14_bit() {
        // 8192 (center) splits into LSB 0x00, MSB 0x40 in send_midi's encode.
        let raw = raw_event(0, 0xE0, 0x00, 0x40, 2);
        let event = rack_midi_to_midi_event(&raw).expect("pitch bend should decode");
        assert_eq!(event.kind, MidiEventKind::PitchBend { value: 8192, channel: 2 });
    }

    #[test]
    fn decode_program_and_channel_aftertouch() {
        assert_eq!(
            rack_midi_to_midi_event(&raw_event(0, 0xC0, 7, 0, 1)).unwrap().kind,
            MidiEventKind::ProgramChange { program: 7, channel: 1 }
        );
        assert_eq!(
            rack_midi_to_midi_event(&raw_event(0, 0xD0, 90, 0, 4)).unwrap().kind,
            MidiEventKind::ChannelAftertouch { pressure: 90, channel: 4 }
        );
    }

    #[test]
    fn decode_unknown_status_returns_none() {
        // 0xF0 (system) has no channel-voice MidiEventKind mapping here.
        assert!(rack_midi_to_midi_event(&raw_event(0, 0xF0, 0, 0, 0)).is_none());
    }

    // -- state_retry_size: pure retry-decision logic extracted out of
    // -- get_state (b63261f's fix), unit-tested directly rather than through
    // -- a real oversized-state fixture that isn't available on any known
    // -- test machine.

    #[test]
    fn state_retry_size_none_on_success() {
        // A successful call never needs a retry, regardless of what the
        // out-parameter happens to hold.
        assert_eq!(state_retry_size(ffi::RACK_VST3_OK, 4096, 4096), None);
        assert_eq!(state_retry_size(ffi::RACK_VST3_OK, 4096, 0), None);
    }

    #[test]
    fn state_retry_size_none_on_failure_without_overflow() {
        // A failure that is NOT the documented overflow case (the reported
        // size is not larger than the buffer given) must not trigger a
        // retry -- e.g. a generic failure that leaves the out-parameter at
        // its initial value, or even reports a smaller/equal size.
        assert_eq!(state_retry_size(ffi::RACK_VST3_ERROR_GENERIC, 4096, 4096), None);
        assert_eq!(state_retry_size(ffi::RACK_VST3_ERROR_GENERIC, 4096, 0), None);
        assert_eq!(state_retry_size(ffi::RACK_VST3_ERROR_GENERIC, 4096, 2048), None);
    }

    #[test]
    fn state_retry_size_some_on_overflow_uses_reported_size() {
        // The documented overflow case: failure, with the out-parameter
        // reporting a real size larger than the buffer that was given.
        // Retry with a buffer sized to that real requirement.
        assert_eq!(
            state_retry_size(ffi::RACK_VST3_ERROR_GENERIC, 4 * 1024 * 1024, 5 * 1024 * 1024),
            Some(5 * 1024 * 1024)
        );
        // Off-by-one at the boundary: reported size exactly one byte over
        // the buffer still counts as overflow.
        assert_eq!(state_retry_size(ffi::RACK_VST3_ERROR_GENERIC, 100, 101), Some(101));
    }

    #[test]
    fn state_retry_size_none_when_reported_size_not_larger() {
        // Reported size equal to (not larger than) the buffer given is not
        // overflow, even on failure -- the buffer was already big enough.
        assert_eq!(state_retry_size(ffi::RACK_VST3_ERROR_GENERIC, 100, 100), None);
    }

    // -- Regression tests for the stale-extra-output-bus-pointer /
    // -- unfed-second-input-bus bug fixes below. These need a real
    // -- installed VST3 plugin with a specific bus layout (a multi-output-
    // -- bus instrument, or an effect with a genuine second input bus), so
    // -- each looks up a specific plugin by name and skips (rather than
    // -- failing) when the machine running the test doesn't have it
    // -- installed -- the same tolerance every other test in this module
    // -- already has for a plugin-less environment.

    #[test]
    fn process_multi_bus_resets_bus_no_longer_requested() {
        // Addictive Drums 2 declares 14 output buses (bus 0 "Master" plus 13
        // per-instrument buses) and 0 input buses -- a real fixture for the
        // stale-extra-output-bus-pointer bug (rack_vst3_plugin_process_
        // multi_bus used to leave a bus's process_data pointer bound to
        // whatever buffer a previous call passed for it, forever, once that
        // bus stopped being requested).
        let Some((scanner, info)) = find_plugin_by_name("Addictive Drums 2") else {
            println!("Skipping test - Addictive Drums 2 not installed");
            return;
        };

        let mut plugin = scanner.load(&info).expect("AD2 should load");
        plugin.initialize(48000.0, 512, ProcessMode::Realtime).expect("AD2 should initialize");

        assert!(
            plugin.output_bus_count() > 1,
            "AD2 should declare more than one output bus"
        );
        let bus_index = 1u32; // "Kick" per AD2's documented bus layout
        let extra_channels = plugin
            .output_bus_channel_count(bus_index as usize)
            .expect("AD2 bus 1 should exist");
        assert_eq!(extra_channels, 1, "AD2's Kick bus is documented as mono");

        let num_frames = 64usize;
        let mut main_left = vec![0.0f32; num_frames];
        let mut main_right = vec![0.0f32; num_frames];
        let mut extra_buf = vec![777.0f32; num_frames];

        // Call 1: bind bus 1 to extra_buf and process. AD2's process_data
        // now points bus 1's channel(s) directly at extra_buf.
        {
            let mut extra_channel_slices: Vec<&mut [f32]> = vec![extra_buf.as_mut_slice()];
            let mut extra_outputs = [ExtraOutputBus {
                bus_index,
                channels: &mut extra_channel_slices[..],
            }];
            let mut main_outputs: Vec<&mut [f32]> =
                vec![main_left.as_mut_slice(), main_right.as_mut_slice()];
            plugin
                .process_multi_bus(
                    &[],
                    &[],
                    &mut main_outputs[..],
                    &mut extra_outputs[..],
                    num_frames,
                    None,
                )
                .expect("first process_multi_bus call (bus 1 requested) should succeed");
        }

        // Refill extra_buf with a sentinel AD2 would not otherwise produce,
        // so any write to it during call 2 is unambiguous.
        const SENTINEL: f32 = 555.0;
        extra_buf.fill(SENTINEL);

        // Call 2: omit bus 1 entirely. Before the fix, AD2's
        // process_data.outputs[1] would still point at extra_buf from call
        // 1 above, and AD2 -- like every VST3 plugin -- computes every
        // activated output bus on every process() call regardless of
        // whether the host captures it, so it would overwrite extra_buf
        // here even though this call never mentions bus 1.
        {
            let mut main_outputs: Vec<&mut [f32]> =
                vec![main_left.as_mut_slice(), main_right.as_mut_slice()];
            plugin
                .process_multi_bus(&[], &[], &mut main_outputs[..], &mut [], num_frames, None)
                .expect("second process_multi_bus call (bus 1 omitted) should succeed");
        }

        assert!(
            extra_buf.iter().all(|&sample| sample == SENTINEL),
            "bus 1's buffer must not be written to once a later call stops \
             requesting it -- got {extra_buf:?}, expected untouched sentinel {SENTINEL}"
        );
    }

    #[test]
    fn process_leaves_unfed_second_input_bus_silent_not_null() {
        // TDR Nova and soothe2 both declare a real second (sidechain) input
        // bus on macOS -- a fixture for the unfed-second-input-bus bug
        // (every declared input bus is activated during initialize, but
        // only bus 0 was ever pointed at real memory; a plugin that reads
        // an activated bus unconditionally would null-deref on any other
        // bus).
        let Some((scanner, info)) =
            find_plugin_by_name("TDR Nova").or_else(|| find_plugin_by_name("soothe2"))
        else {
            println!("Skipping test - no installed plugin with a second input bus found");
            return;
        };

        let mut plugin = scanner.load(&info).expect("plugin should load");
        plugin.initialize(48000.0, 512, ProcessMode::Realtime).expect("plugin should initialize");

        assert!(
            plugin.input_bus_channel_count(1).is_some(),
            "test plugin must declare a second (sidechain) input bus"
        );

        let num_frames = 64usize;
        let input_channels = plugin.input_channels();
        let output_channels = plugin.output_channels();
        let in_bufs: Vec<Vec<f32>> = (0..input_channels).map(|_| vec![0.0f32; num_frames]).collect();
        let in_slices: Vec<&[f32]> = in_bufs.iter().map(|buffer| buffer.as_slice()).collect();
        let mut out_bufs: Vec<Vec<f32>> =
            (0..output_channels).map(|_| vec![0.0f32; num_frames]).collect();
        let mut out_slices: Vec<&mut [f32]> =
            out_bufs.iter_mut().map(|buffer| buffer.as_mut_slice()).collect();

        // Bus 1 (the sidechain) is activated but this call, like every
        // `process()` call, only ever feeds bus 0 -- exactly the condition
        // that used to null-deref inside the plugin before this fix.
        let result = plugin.process(&in_slices, &mut out_slices, num_frames);
        assert!(
            result.is_ok(),
            "process() with an unfed second input bus should succeed, got {result:?}"
        );

        // Also exercise process_multi_bus's own reset (a separate code path
        // that shares the same underlying reset_bus_bindings fix).
        let in_slices: Vec<&[f32]> = in_bufs.iter().map(|buffer| buffer.as_slice()).collect();
        let mut out_slices: Vec<&mut [f32]> =
            out_bufs.iter_mut().map(|buffer| buffer.as_mut_slice()).collect();
        let result =
            plugin.process_multi_bus(&in_slices, &[], &mut out_slices, &mut [], num_frames, None);
        assert!(
            result.is_ok(),
            "process_multi_bus with an unfed second input bus should succeed, got {result:?}"
        );
    }

    #[test]
    fn send_midi_boundary_at_documented_capacity() {
        // Boundary-precise regression test tied to the documented 256-event
        // capacity value itself, rather than an arbitrary count that merely
        // happens to sit below it (an earlier version of this test sent 200
        // events and only asserted success -- code review pointed out that
        // assertion would have passed even against the pre-fix code, which
        // never checked addEvent's return value and always reported OK
        // regardless of how many events were actually dropped, so it never
        // proved the capacity had genuinely changed). Sending exactly 256
        // events -- filling the queue exactly to its documented capacity --
        // must succeed with none dropped; one further event queued on top of
        // an already-full queue must then be reported as dropped. This
        // combination fails if the real capacity is either smaller than 256
        // (the first send would already report a drop) or larger (the
        // second send would not).
        //
        // What this does NOT prove: whether AD2's own audio-thread event
        // processing actually received and acted on each of the 256 host-
        // side-accepted events -- this crate has no fixture or mechanism to
        // introspect an instrument plugin's internal event handling that
        // deeply. What it proves precisely is that the host-side queue
        // (RackVST3Plugin::input_events) accepts exactly 256 events with no
        // truncation and starts truncating at the 257th, matching
        // kEventListCapacity's documented value exactly.
        let Some((scanner, info)) = find_plugin_by_name("Addictive Drums 2") else {
            println!("Skipping test - Addictive Drums 2 not installed");
            return;
        };
        let mut plugin = scanner.load(&info).expect("AD2 should load");
        plugin.initialize(48000.0, 512, ProcessMode::Realtime).expect("AD2 should initialize");

        let make_note_ons = |count: u32| -> Vec<MidiEvent> {
            (0..count)
                .map(|index| MidiEvent {
                    sample_offset: 0,
                    kind: MidiEventKind::NoteOn {
                        note: 36,
                        velocity: 100,
                        channel: (index % 16) as u8,
                    },
                })
                .collect()
        };

        let at_capacity = make_note_ons(256);
        let result = plugin.send_midi(&at_capacity);
        assert!(
            result.is_ok(),
            "exactly 256 events (the documented capacity) should all be accepted, got {result:?}"
        );

        // send_midi never clears the queue itself -- only process()/
        // process_multi_bus() does -- so the queue is still exactly full
        // here: one more event must now be reported as dropped.
        let one_more = make_note_ons(1);
        let result = plugin.send_midi(&one_more);
        assert!(
            result.is_err(),
            "a 257th event queued onto an already-full 256-capacity queue must be reported as dropped, got {result:?}"
        );
    }

    #[test]
    fn send_midi_reports_truncation_past_documented_capacity() {
        // 300 events exceeds the documented/configured 256-event capacity,
        // so the queue-full condition must now be surfaced as an error
        // rather than silently discarding the overflow (the pre-fix
        // behavior, which additionally overflowed at the undocumented,
        // much lower default of 50).
        let Some((scanner, info)) = find_plugin_by_name("Addictive Drums 2") else {
            println!("Skipping test - Addictive Drums 2 not installed");
            return;
        };
        let mut plugin = scanner.load(&info).expect("AD2 should load");
        plugin.initialize(48000.0, 512, ProcessMode::Realtime).expect("AD2 should initialize");

        let events: Vec<MidiEvent> = (0..300u32)
            .map(|index| MidiEvent {
                sample_offset: 0,
                kind: MidiEventKind::NoteOn {
                    note: 36,
                    velocity: 100,
                    channel: (index % 16) as u8,
                },
            })
            .collect();

        let result = plugin.send_midi(&events);
        assert!(
            result.is_err(),
            "300 events should overflow the documented 256-event capacity and be reported"
        );
    }

    #[test]
    fn reset_leaves_plugin_ready_to_process_again() {
        // rack_vst3_plugin_reset used to leave the plugin deactivated then
        // reactivated but never re-issued setProcessing(true), violating
        // the VST3 lifecycle order this fix restores. A process() call
        // immediately after reset() is exactly the sequence a real caller
        // (e.g. drumr's own pattern-clear/panic-reset path) performs.
        //
        // Note: whether a given plugin's own process() implementation
        // actually enforces "must be in Processing state" strongly enough
        // to fail observably when it isn't is plugin-dependent; this test
        // exercises the correct lifecycle call sequence and documents the
        // expected contract, but a plugin that tolerates the old, spec-
        // violating sequence would not turn this test red on its own.
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        plugin
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("Plugin initialization should succeed");

        let num_frames = 64usize;
        let input_channels = plugin.input_channels();
        let output_channels = plugin.output_channels();
        let in_bufs: Vec<Vec<f32>> = (0..input_channels).map(|_| vec![0.0f32; num_frames]).collect();
        let mut out_bufs: Vec<Vec<f32>> =
            (0..output_channels).map(|_| vec![0.0f32; num_frames]).collect();

        {
            let in_slices: Vec<&[f32]> = in_bufs.iter().map(|buffer| buffer.as_slice()).collect();
            let mut out_slices: Vec<&mut [f32]> =
                out_bufs.iter_mut().map(|buffer| buffer.as_mut_slice()).collect();
            plugin
                .process(&in_slices, &mut out_slices, num_frames)
                .expect("process() before reset should succeed");
        }

        plugin.reset().expect("reset() should succeed");

        {
            let in_slices: Vec<&[f32]> = in_bufs.iter().map(|buffer| buffer.as_slice()).collect();
            let mut out_slices: Vec<&mut [f32]> =
                out_bufs.iter_mut().map(|buffer| buffer.as_mut_slice()).collect();
            let result = plugin.process(&in_slices, &mut out_slices, num_frames);
            assert!(
                result.is_ok(),
                "process() after reset() should succeed, got {result:?}"
            );
        }
    }

    #[test]
    fn drop_after_process_tears_down_cleanly_from_processing_state() {
        // rack_vst3_plugin_free used to call component->setActive(false)
        // directly, without first calling setProcessing(false) -- even
        // though initialize() (and, by the time a caller has actually
        // processed audio, at least one process() call) already leaves the
        // plugin with Processing == true. That is the exact spec-violating
        // teardown order 2ae4dee fixed for both free() and reset();
        // reset()'s own regression test is above (reset_leaves_plugin_
        // ready_to_process_again). This is free()'s -- the more dangerous
        // half of that fix, since this workspace has already been bitten by
        // a real production teardown crash on exactly this kind of path
        // (see the parent drumr repo's crates/backline/docs/
        // ad2-teardown-crash-design.md).
        //
        // This crate has no mock IComponent layer and no way to query a
        // real plugin's internal Processing/Active state from the outside,
        // so there is no direct assertion available that
        // setProcessing(false) was actually called before
        // setActive(false)/terminate(). The real, observable signal this
        // test has access to is indirect: drop a plugin while it is in
        // exactly the state the bug required (initialized, then processed
        // at least once, so Processing == true), and confirm teardown
        // completes without crashing or deadlocking -- free() holds the
        // process-wide module-load mutex for its whole duration, so a
        // plugin that got stuck tearing down from Processing state would
        // hang this entire test binary, not just this one test -- and that
        // the shared VST3 module loader is left in a working state
        // afterward, by successfully loading, initializing and processing a
        // second, independent instance of the same plugin right after. Like
        // the reset() regression test above, a plugin whose own
        // setActive()/terminate() tolerates the old, spec-violating call
        // order would not turn this test red on its own -- its value is in
        // exercising the exact call sequence a regression could reorder,
        // not in a positive assertion that the sequence itself was
        // followed.
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        {
            let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
            plugin
                .initialize(48000.0, 512, ProcessMode::Realtime)
                .expect("Plugin initialization should succeed");

            let num_frames = 64usize;
            let input_channels = plugin.input_channels();
            let output_channels = plugin.output_channels();
            let in_bufs: Vec<Vec<f32>> =
                (0..input_channels).map(|_| vec![0.0f32; num_frames]).collect();
            let mut out_bufs: Vec<Vec<f32>> =
                (0..output_channels).map(|_| vec![0.0f32; num_frames]).collect();
            let in_slices: Vec<&[f32]> = in_bufs.iter().map(|buffer| buffer.as_slice()).collect();
            let mut out_slices: Vec<&mut [f32]> =
                out_bufs.iter_mut().map(|buffer| buffer.as_mut_slice()).collect();
            plugin
                .process(&in_slices, &mut out_slices, num_frames)
                .expect("process() should succeed, leaving the plugin in Processing state");

            // Explicit drop, immediately after process(), is exactly the
            // scenario the bug required: setProcessing(true) already in
            // effect, never undone, when free() used to deactivate and
            // terminate directly.
            drop(plugin);
        }

        // Teardown must have left the shared module loader usable: load,
        // initialize and process a second, independent instance of the
        // same plugin.
        let mut second = scanner.load(&info).expect("second load after teardown should succeed");
        second
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("second initialize after teardown should succeed");

        let num_frames = 64usize;
        let input_channels = second.input_channels();
        let output_channels = second.output_channels();
        let in_bufs: Vec<Vec<f32>> =
            (0..input_channels).map(|_| vec![0.0f32; num_frames]).collect();
        let mut out_bufs: Vec<Vec<f32>> =
            (0..output_channels).map(|_| vec![0.0f32; num_frames]).collect();
        let in_slices: Vec<&[f32]> = in_bufs.iter().map(|buffer| buffer.as_slice()).collect();
        let mut out_slices: Vec<&mut [f32]> =
            out_bufs.iter_mut().map(|buffer| buffer.as_mut_slice()).collect();
        second
            .process(&in_slices, &mut out_slices, num_frames)
            .expect("second instance should process successfully after first instance's teardown");
    }

    #[test]
    fn get_state_set_state_round_trip() {
        // Exercises get_state's normal (no-retry) path end to end. The
        // retry-on-overflow branch (the fix for real states larger than the
        // 4 MB initial guess) only triggers for a state larger than that
        // guess; no plugin installed on a real test machine is known to
        // produce one, so that branch is covered by careful reading of
        // rack_vst3_plugin_get_state's contract (see get_state's own
        // comments) rather than by an automated test here.
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        plugin
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("Plugin initialization should succeed");

        let state = plugin.get_state().expect("get_state should succeed");
        assert!(!state.is_empty(), "state should not be empty");

        plugin
            .set_state(&state)
            .expect("set_state with a just-captured state should succeed");
    }

    #[test]
    fn queue_parameter_change_does_not_require_controller_call() {
        // queue_parameter_change is the real-time-safe path added for the
        // audio-thread parameter delivery gap: it must succeed on a real
        // parameter index without touching the controller, and reject an
        // out-of-range index the same way set_parameter already does.
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        plugin
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("Plugin initialization should succeed");

        if plugin.parameter_count() == 0 {
            println!("Plugin has no parameters, skipping queue_parameter_change test");
            return;
        }

        plugin
            .queue_parameter_change(0, 0.5)
            .expect("queuing a real parameter index should succeed");

        let out_of_range = plugin.parameter_count();
        let result = plugin.queue_parameter_change(out_of_range, 0.5);
        assert!(result.is_err(), "an out-of-range index should be rejected");
    }

    #[test]
    fn queue_parameter_change_never_touches_the_controller() {
        // The one thing that actually distinguishes queue_parameter_change
        // from an accidental alias for set_parameter is that it never calls
        // IEditController::setParamNormalized. get_parameter reads directly
        // from that same controller call
        // (rack_vst3_plugin_get_parameter -> controller->getParamNormalized,
        // see rack-sys/src/vst3_instance.cpp), so it is a real, live signal
        // for "was the controller touched" -- proven live within this same
        // test by also calling the known-controller-touching set_parameter
        // and observing get_parameter move in response. If that positive
        // control did not move get_parameter, the negative checks below
        // would be meaningless (a broken/stale accessor would look
        // "unchanged" for any input).
        //
        // What this test deliberately does NOT claim: that get_parameter
        // reflects a queued value once it has actually been delivered to
        // and applied by the plugin's own audio processor via process().
        // Verified empirically against a real installed plugin (Addictive
        // Drums 2) and by reading the source: process() feeds queued points
        // into ProcessData::inputParameterChanges for the processor alone
        // (rack_vst3_plugin_process wires plugin->input_param_changes in
        // and clears it after every call) and never writes anything back
        // into the controller -- nothing in this crate reads the
        // processor's own outputParameterChanges and re-applies it to the
        // controller, and the VST3 spec does not require a plugin to keep
        // the two in sync automatically. So get_parameter staying unchanged
        // after process() is expected and reconfirms controller isolation
        // holds for the real-time call too, but it cannot serve as
        // affirmative proof that the processor actually received and
        // applied the point -- that would need either inspecting the
        // processor's internal state (not exposed by this crate's C API)
        // or a specific plugin+parameter+signal-path assumption fragile
        // enough not to generalize across installed plugins. The queue's
        // wiring into ProcessData::inputParameterChanges is instead verified
        // by reading rack_vst3_plugin_queue_parameter_change and process()'s
        // own implementation directly.
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        plugin
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("Plugin initialization should succeed");

        let index = match (0..plugin.parameter_count()).find(|&index| {
            plugin
                .parameter_info(index)
                .map(|info| info.can_automate && !info.is_read_only)
                .unwrap_or(false)
        }) {
            Some(index) => index,
            None => {
                println!("Plugin has no automatable parameter, skipping");
                return;
            }
        };

        let before = plugin.get_parameter(index).expect("get_parameter should succeed");
        let target = if before < 0.5 { 0.9 } else { 0.1 };

        plugin
            .queue_parameter_change(index, target)
            .expect("queuing a real parameter index should succeed");
        let after_queue = plugin.get_parameter(index).expect("get_parameter should succeed");
        assert_eq!(
            after_queue, before,
            "queue_parameter_change must not touch the controller: get_parameter should still \
             read the pre-queue value"
        );

        let num_frames = 64usize;
        let input_channels = plugin.input_channels();
        let output_channels = plugin.output_channels();
        let in_bufs: Vec<Vec<f32>> = (0..input_channels).map(|_| vec![0.0f32; num_frames]).collect();
        let mut out_bufs: Vec<Vec<f32>> =
            (0..output_channels).map(|_| vec![0.0f32; num_frames]).collect();
        let in_slices: Vec<&[f32]> = in_bufs.iter().map(|buffer| buffer.as_slice()).collect();
        let mut out_slices: Vec<&mut [f32]> =
            out_bufs.iter_mut().map(|buffer| buffer.as_mut_slice()).collect();
        plugin
            .process(&in_slices, &mut out_slices, num_frames)
            .expect("process() should succeed with the queued change pending");

        let after_process = plugin.get_parameter(index).expect("get_parameter should succeed");
        assert_eq!(
            after_process, before,
            "process() draining the queue must not touch the controller either: get_parameter \
             should still read the pre-queue value"
        );

        // Positive control: a real controller call (set_parameter) does move
        // get_parameter, proving the two checks above are a real signal and
        // not a stale/broken accessor that would "pass" for any input.
        plugin
            .set_parameter(index, target)
            .expect("set_parameter should succeed");
        let after_set = plugin.get_parameter(index).expect("get_parameter should succeed");
        assert_ne!(
            after_set, before,
            "set_parameter is the known controller-touching path and must move get_parameter away \
             from its pre-queue value -- otherwise the unchanged assertions above would not be \
             meaningful (comparing to `before` rather than `target` tolerates a stepped/quantized \
             parameter rounding the requested value)"
        );
    }

    #[test]
    fn queue_parameter_change_performs_no_rust_side_allocation() {
        // d284632 fixed queue_parameter_change's real allocation-free claim:
        // input_param_changes/output_param_changes were default-constructed
        // ParameterChanges with no setMaxParameters call, so the *first*
        // process block in which the number of distinct parameters queued
        // exceeded every previous block's high-water mark still allocated
        // via ParameterChanges::addParameterData's queues.emplace_back --
        // in C++, inside rack-sys. That fix had no automated test at all,
        // since it changes a non-functional property (allocation behavior)
        // that no functional test can observe as a pass/fail difference.
        //
        // This test queues changes for every distinct parameter the loaded
        // plugin has -- enough to exceed any smaller high-water mark a
        // partial fix might still leave in place -- while alloc_probe (see
        // its own doc comment above) counts Rust-side allocations, and
        // asserts there are none.
        //
        // IMPORTANT, honest limitation, matching this crate's own
        // convention (see e.g. reset_leaves_plugin_ready_to_process_again's
        // and get_state_set_state_round_trip's doc comments) of stating
        // plainly what a test can't prove rather than overclaiming: as
        // alloc_probe's own doc comment explains, a Rust
        // `#[global_allocator]` cannot observe allocations rack-sys's
        // linked C++ code performs directly via malloc/`operator new` --
        // which is exactly where d284632's real fix lives. A true
        // process-wide interception (overriding the process's
        // malloc/free/calloc/realloc plus the Itanium-ABI-mangled `operator
        // new`/`operator delete` symbols, so the C++ library's own
        // allocations route through the same counter) is possible in
        // principle, but proved fragile enough while prototyping it for
        // this task not to build casually: a naive override that itself
        // calls back into Rust's `System` allocator (which also ultimately
        // resolves to `malloc`) recurses infinitely and segfaults; a safe
        // version needs `dlsym(RTLD_NEXT, ...)`-style real-symbol lookup and
        // has to cover every allocation entry point deliberately. That is
        // disproportionate restructuring for this one test, so it was not
        // attempted, and the underlying C++ fix's correctness still rests
        // on reading `ParameterChanges::setMaxParameters`/`addParameterData`
        // directly, exactly as d284632's own commit message already says.
        //
        // What this test DOES prove, for real: the Rust wrapper function
        // itself never allocates on the Rust side across a realistic number
        // of distinct parameter indices -- a genuine, if narrower,
        // regression guard against a future change (a stray
        // `Vec`/`format!`/`String`) creeping into this real-time-safe hot
        // path from the Rust side.
        let (scanner, info) = match get_test_plugin() {
            Ok(result) => result,
            Err(_) => {
                println!("Skipping test - no VST3 plugins found");
                return;
            }
        };

        let mut plugin = scanner.load(&info).expect("Plugin creation should succeed");
        plugin
            .initialize(48000.0, 512, ProcessMode::Realtime)
            .expect("Plugin initialization should succeed");

        let count = plugin.parameter_count();
        if count == 0 {
            println!("Plugin has no parameters, skipping");
            return;
        }

        let allocations = alloc_probe::count_rust_allocations(|| {
            for index in 0..count {
                plugin
                    .queue_parameter_change(index, 0.5)
                    .expect("queuing a real parameter index should succeed");
            }
        });

        assert_eq!(
            allocations, 0,
            "queue_parameter_change must not allocate on the Rust side once initialize() has \
             returned (see this test's own doc comment for what this can and cannot prove about \
             the underlying C++ fix)"
        );
    }
}
