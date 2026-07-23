use crate::{Error, MidiEvent, MidiEventKind, ParameterInfo, PluginInfo, PluginInstance, PresetInfo, Result};
use smallvec::SmallVec;
use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr::NonNull;

use super::ffi;
use super::util::map_error;

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

            Ok(())
        }
    }

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
}
