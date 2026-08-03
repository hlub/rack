use crate::{Error, MidiEvent, MidiEventKind, ParameterInfo, PluginInfo, PluginInstance, PresetInfo, Result};
use smallvec::SmallVec;
use std::ffi::CString;
use std::marker::PhantomData;
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
    pub fn initialize_with_state(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
        state: &[u8],
    ) -> Result<()> {
        self.initialize_impl(sample_rate, max_block_size, state)
    }

    fn initialize_impl(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
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

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        unsafe {
            ffi::rack_vst3_plugin_free(self.inner.as_ptr());
        }
    }
}

impl PluginInstance for Vst3Plugin {
    fn initialize(&mut self, sample_rate: f64, max_block_size: usize) -> Result<()> {
        self.initialize_impl(sample_rate, max_block_size, &[])
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
            // Get state size
            let size = ffi::rack_vst3_plugin_get_state_size(self.inner.as_ptr());
            if size <= 0 {
                return Err(Error::Other("Failed to get plugin state size".to_string()));
            }

            // Allocate buffer
            let mut data = vec![0u8; size as usize];
            let mut actual_size = data.len();

            // Get state data
            let result = ffi::rack_vst3_plugin_get_state(
                self.inner.as_ptr(),
                data.as_mut_ptr(),
                &mut actual_size,
            );

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
        let result = plugin.initialize(48000.0, 512);
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
            .initialize(48000.0, 512)
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
            .initialize(48000.0, 512)
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
            .initialize(48000.0, 512)
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
}
