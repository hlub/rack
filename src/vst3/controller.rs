//! The controller half of a VST3 instance: everything the main thread asks
//! the plugin's edit controller while the processor half plays.

use std::ffi::{CStr, CString};
use std::fmt;
use std::marker::PhantomData;
use std::os::raw::{c_char, c_int};
use std::ptr::NonNull;

use crate::{Error, ParameterInfo, PluginInfo, PresetInfo, Result};

use super::ffi;
use super::processor::BusLayout;
use super::restart::RestartFlags;
use super::util::{map_error, take_state_blob};

/// The controller half of a VST3 plugin instance: parameter descriptions,
/// values and their display text, a controller-only parameter write, presets
/// and the bus facts.
///
/// Obtained from [`Vst3Plugin::split`](super::Vst3Plugin::split) and given
/// back with [`Vst3Plugin::join`](super::Vst3Plugin::join).
///
/// # Thread Safety
///
/// Neither `Send` nor `Sync`: it stays on the thread that split the instance,
/// which must be the thread that constructed it. Every `IEditController`
/// method is a UI-thread call in the VST3 standard, and plugin frameworks such
/// as JUCE tie their controller to the main run loop, so moving it to, or
/// sharing it with, another thread is refused by the compiler rather than
/// left to a comment. Its methods may run while the
/// [`Vst3Processor`](super::Vst3Processor) of the same instance processes on
/// the audio thread: none of them writes the processor's state.
///
/// Dropping it frees the instance when the processor half was dropped first.
/// Dropped while the processor half is still alive, it cannot free what the
/// processor may still be using: the instance is leaked and counted (see
/// [`leaked_instance_count`](super::leaked_instance_count)), which a debug
/// build also asserts against. So give the processor half back and
/// [`join`](super::Vst3Plugin::join) them, or drop the processor half first.
pub struct Vst3Controller {
    inner: NonNull<ffi::RackVST3Plugin>,
    info: PluginInfo,
    layout: BusLayout,
    // A raw pointer is neither Send nor Sync, which is this type's contract.
    _not_send: PhantomData<*const ()>,
}

impl fmt::Debug for Vst3Controller {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Vst3Controller")
            .field("instance", &self.inner)
            .field("plugin", &self.info.name)
            .finish_non_exhaustive()
    }
}

// VST3 ParameterInfo::ParameterFlags bit values, from the Steinberg SDK
// (pluginterfaces/vst/ivsteditcontroller.h). rack does not depend on the
// SDK headers directly, so the bits are mirrored here.
const PARAM_FLAG_CAN_AUTOMATE: i32 = 1 << 0;
const PARAM_FLAG_IS_READ_ONLY: i32 = 1 << 1;
const PARAM_FLAG_IS_LIST: i32 = 1 << 3;
const PARAM_FLAG_IS_HIDDEN: i32 = 1 << 4;

/// Reads a NUL-terminated UTF-8 string a C call wrote into `buffer`.
fn c_buffer_to_string(buffer: &[c_char], what: &str) -> Result<String> {
    // SAFETY: every C function that fills one of these buffers terminates it
    // within the buffer's length.
    let text = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    text.to_str()
        .map(str::to_string)
        .map_err(|error| Error::Other(format!("Invalid UTF-8 in {what}: {error}")))
}

impl Vst3Controller {
    /// A controller half over `inner`, with no layout until
    /// [`configure`](Self::configure) runs.
    pub(super) fn new(inner: NonNull<ffi::RackVST3Plugin>, info: PluginInfo) -> Self {
        Self { inner, info, layout: BusLayout::default(), _not_send: PhantomData }
    }

    /// The instance both halves share.
    pub(super) fn instance(&self) -> NonNull<ffi::RackVST3Plugin> {
        self.inner
    }

    /// Takes the layout `initialize` recorded.
    pub(super) fn configure(&mut self, layout: BusLayout) {
        self.layout = layout;
    }

    /// The plugin's scanned info.
    #[must_use]
    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Whether the instance has been initialized.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        // SAFETY: the instance is live while this half exists, and the
        // function reads one field initialize writes.
        unsafe { ffi::rack_vst3_plugin_is_initialized(self.inner.as_ptr()) > 0 }
    }

    /// Number of parameters, as listed at initialization.
    #[must_use]
    pub fn parameter_count(&self) -> usize {
        // SAFETY: the instance is live; the count is a fixed fact.
        let count = unsafe { ffi::rack_vst3_plugin_parameter_count(self.inner.as_ptr()) };
        usize::try_from(count).unwrap_or(0)
    }

    /// A parameter's description: name, range, default, unit, step count and
    /// flags.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized or `index` is out of
    /// range.
    pub fn parameter_info(&self, index: usize) -> Result<ParameterInfo> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }

        let mut name = vec![0 as c_char; 256];
        let mut unit = vec![0 as c_char; 32];
        let mut min = 0.0f32;
        let mut max = 0.0f32;
        let mut default_value = 0.0f32;
        let mut step_count = 0i32;
        let mut flags = 0i32;

        // SAFETY: the instance is live; every out-pointer addresses a local of
        // the stated size.
        let result = unsafe {
            ffi::rack_vst3_plugin_parameter_info(
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
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }

        Ok(ParameterInfo {
            index,
            name: c_buffer_to_string(&name, "parameter name")?,
            min,
            max,
            default: default_value,
            unit: c_buffer_to_string(&unit, "parameter unit")?,
            step_count,
            can_automate: flags & PARAM_FLAG_CAN_AUTOMATE != 0,
            is_read_only: flags & PARAM_FLAG_IS_READ_ONLY != 0,
            is_list: flags & PARAM_FLAG_IS_LIST != 0,
            is_hidden: flags & PARAM_FLAG_IS_HIDDEN != 0,
        })
    }

    /// A parameter's current normalized value, as the edit controller holds
    /// it.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized or `index` is out of
    /// range.
    pub fn get_parameter(&self, index: usize) -> Result<f32> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        let mut value = 0.0f32;
        // SAFETY: the instance is live; `value` is a local out-pointer.
        let result = unsafe {
            ffi::rack_vst3_plugin_get_parameter(self.inner.as_ptr(), index as u32, &mut value)
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(value)
    }

    /// Write a normalized parameter value (clamped to 0.0-1.0) into the edit
    /// controller only (`IEditController::setParamNormalized`), so the
    /// controller and anything showing it reflect the value.
    ///
    /// The audio processor is not told: the value reaches it only through
    /// [`Vst3Processor::queue_parameter_change`](super::Vst3Processor::queue_parameter_change)
    /// on the audio thread. That is what lets this run beside a processing
    /// processor half: it touches nothing `process` reads. Writing both is the
    /// caller's to arrange; the whole instance's
    /// [`set_parameter`](crate::PluginInstance::set_parameter) does both while
    /// nothing processes concurrently.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized or `index` is out of
    /// range.
    pub fn set_parameter(&mut self, index: usize, value: f32) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        // SAFETY: the instance is live; the function calls the controller
        // alone and touches no processing state.
        let result = unsafe {
            ffi::rack_vst3_plugin_controller_set_parameter(self.inner.as_ptr(), index as u32, value)
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }

    /// Get the display string a plugin would show for a normalized
    /// parameter value (VST3 `IEditController::getParamStringByValue`).
    ///
    /// For a discrete/list parameter (see `ParameterInfo::is_list` and
    /// `step_count`), this returns the human-readable name of the step the
    /// value falls on (e.g. an amp model name), not the raw normalized
    /// number.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized, `index` is out of
    /// range, or the plugin declines.
    pub fn parameter_value_string(&self, index: usize, normalized_value: f32) -> Result<String> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        let mut out = vec![0 as c_char; 256];
        // SAFETY: the instance is live; `out` holds the stated length.
        let result = unsafe {
            ffi::rack_vst3_plugin_parameter_string_by_value(
                self.inner.as_ptr(),
                index as u32,
                normalized_value,
                out.as_mut_ptr(),
                out.len(),
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        c_buffer_to_string(&out, "parameter value string")
    }

    /// Convert a normalized parameter value to the plugin's "plain"
    /// (native/real-world unit) value, e.g. actual milliseconds or Hz.
    /// Combine with [`ParameterInfo::unit`] for the unit label. Some
    /// plugins return the normalized value unchanged when no real
    /// plain-value mapping is implemented.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized or `index` is out of
    /// range.
    pub fn parameter_plain_value(&self, index: usize, normalized_value: f32) -> Result<f64> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        let mut out_plain = 0.0f64;
        // SAFETY: the instance is live; `out_plain` is a local out-pointer.
        let result = unsafe {
            ffi::rack_vst3_plugin_parameter_plain_value(
                self.inner.as_ptr(),
                index as u32,
                normalized_value,
                &mut out_plain,
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(out_plain)
    }

    /// Parse a display-style string (as a user might type it, or as
    /// [`parameter_value_string`](Self::parameter_value_string) would render
    /// it) into a normalized parameter value (VST3
    /// `IEditController::getParamValueByString`).
    ///
    /// Whether the plugin accepts a bare number, a number with a unit
    /// suffix, or only its own exact display format is plugin-defined; not
    /// every plugin implements this meaningfully, in which case this
    /// returns an error rather than a guessed value.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized, `index` is out of
    /// range, the string holds a NUL byte, or the plugin declines.
    pub fn parameter_value_by_string(&self, index: usize, string: &str) -> Result<f32> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        let c_string = CString::new(string)
            .map_err(|error| Error::Other(format!("Invalid parameter string: {error}")))?;
        let mut out_normalized = 0.0f32;
        // SAFETY: the instance is live; `c_string` is NUL-terminated and
        // outlives the call; `out_normalized` is a local out-pointer.
        let result = unsafe {
            ffi::rack_vst3_plugin_parameter_value_by_string(
                self.inner.as_ptr(),
                index as u32,
                c_string.as_ptr(),
                &mut out_normalized,
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(out_normalized)
    }

    /// Convert a plain (native/real-world unit) parameter value, e.g. actual
    /// milliseconds or Hz, to its normalized (0.0-1.0) form (VST3
    /// `IEditController::plainParamToNormalized`, the inverse of
    /// [`parameter_plain_value`](Self::parameter_plain_value)).
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized or `index` is out of
    /// range.
    pub fn parameter_normalized_from_plain(&self, index: usize, plain_value: f64) -> Result<f32> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        let mut out_normalized = 0.0f32;
        // SAFETY: the instance is live; `out_normalized` is a local
        // out-pointer.
        let result = unsafe {
            ffi::rack_vst3_plugin_parameter_normalized_from_plain(
                self.inner.as_ptr(),
                index as u32,
                plain_value,
                &mut out_normalized,
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(out_normalized)
    }

    /// Read the plugin's state without stopping it from processing: the
    /// processor half may go on processing on the audio thread meanwhile. Same
    /// format as [`get_state`](crate::PluginInstance::get_state) on the whole,
    /// which instead stops processing around the read.
    ///
    /// The VST3 standard allows `IComponent::getState` while processing.
    /// Whether a given plugin synchronises its own state against `process` is
    /// the plugin's, and rack does not check it: a host that has not checked
    /// a plugin reads its state from the whole instance instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized or its component
    /// refuses to write its state.
    pub fn read_state_while_processing(&self) -> Result<Vec<u8>> {
        let mut error: c_int = ffi::RACK_VST3_OK;
        // SAFETY: the instance is live; `error` is a local out-pointer; the
        // returned blob is freed by `take_state_blob` and nowhere else.
        unsafe {
            let blob = ffi::rack_vst3_plugin_read_state_while_processing(self.inner.as_ptr(), &mut error);
            take_state_blob(blob, error)
        }
    }

    /// Takes the restart flags the plugin has raised through
    /// `IComponentHandler::restartComponent` since the last take, clearing
    /// them. The plugin may raise them from any thread; acting on them is the
    /// caller's.
    #[must_use]
    pub fn take_restart_flags(&self) -> RestartFlags {
        // SAFETY: the instance is live; the function exchanges one atomic.
        RestartFlags::from_bits(unsafe { ffi::rack_vst3_plugin_take_restart_flags(self.inner.as_ptr()) })
    }

    /// Whether [`take_value_changes`](Self::take_value_changes) would report
    /// anything now. Answered from atomics alone: no plugin code runs and no
    /// lock is taken, so a host may ask every tick for any plugin, and call
    /// the take only when this is true.
    #[must_use]
    pub fn value_changes_pending(&self) -> bool {
        // SAFETY: the instance is live; the function loads atomics and calls no plugin code.
        unsafe { ffi::rack_vst3_plugin_value_changes_pending(self.inner.as_ptr()) }
    }

    /// Takes what the plugin did to its own parameter values since the last
    /// take: values its processor reported while processing (handed to the
    /// controller here, as the standard asks of a host), edits it made itself
    /// through `performEdit`, and a request to read its values again. After
    /// this a read of the controller gives the value the plugin is using.
    /// Takes the controller thread's calls, so it is the main thread's.
    #[must_use]
    pub fn take_value_changes(&self) -> super::ValueChanges {
        // SAFETY: the instance is live; the function reads atomics and calls the controller,
        // which the caller's thread owns.
        super::ValueChanges::from_bits(unsafe { ffi::rack_vst3_plugin_take_value_changes(self.inner.as_ptr()) })
    }

    /// How many messages between the plugin's component and its separate
    /// controller rack discarded because the plugin sent them from a thread
    /// other than the one that constructed the instance: the audio thread
    /// (from inside `process`) or a background thread of the plugin's own.
    /// The standard keeps these messages on the UI thread; delivering one
    /// elsewhere would run controller code beside the host's own controller
    /// calls. Both counts are 0 for a plugin whose component is its own
    /// controller. A plugin with a non-zero count is worth recording as a
    /// vendor fact.
    #[must_use]
    pub fn discarded_messages(&self) -> DiscardedMessages {
        let mut component_to_controller = 0u64;
        let mut controller_to_component = 0u64;
        // SAFETY: the instance is live; the out-pointers are locals.
        unsafe {
            ffi::rack_vst3_plugin_discarded_messages(
                self.inner.as_ptr(),
                &mut component_to_controller,
                &mut controller_to_component,
            );
        }
        DiscardedMessages { component_to_controller, controller_to_component }
    }

    /// Re-reads every parameter's description from the controller, as a host
    /// does after [`RestartFlags::param_titles_changed`], and says which
    /// indices changed. Parameter ids are never rewritten, because the
    /// processor half resolves indices to ids on the audio thread: when the
    /// count or any id changed, nothing is refreshed and the answer is
    /// [`TitleRefresh::IdsChanged`], which means the plugin needs a new
    /// instance.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized, or a description
    /// cannot be read back.
    pub fn refresh_parameter_titles(&mut self) -> Result<TitleRefresh> {
        let before = self.parameter_infos()?;
        // SAFETY: the instance is live; the function calls the controller and
        // writes only the controller half's descriptions.
        let result = unsafe { ffi::rack_vst3_plugin_refresh_parameter_titles(self.inner.as_ptr()) };
        match result {
            ffi::RACK_VST3_OK => {}
            ffi::RACK_VST3_PARAMETER_IDS_CHANGED => return Ok(TitleRefresh::IdsChanged),
            error => return Err(map_error(error)),
        }
        let changed = changed_parameters(&before, &self.parameter_infos()?);
        Ok(if changed.is_empty() { TitleRefresh::Unchanged } else { TitleRefresh::Changed(changed) })
    }

    /// Every parameter's description, in index order.
    fn parameter_infos(&self) -> Result<Vec<ParameterInfo>> {
        (0..self.parameter_count()).map(|index| self.parameter_info(index)).collect()
    }

    /// Number of factory presets the plugin listed at initialization.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized.
    pub fn preset_count(&self) -> Result<usize> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        // SAFETY: the instance is live; the list is a fixed fact.
        let count = unsafe { ffi::rack_vst3_plugin_get_preset_count(self.inner.as_ptr()) };
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// A factory preset's name, number and category.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized or `index` is out of
    /// range.
    pub fn preset_info(&self, index: usize) -> Result<PresetInfo> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        let mut name = vec![0 as c_char; 256];
        let mut category = vec![0 as c_char; 256];
        let mut preset_number: i32 = 0;
        // SAFETY: the instance is live; every out-pointer addresses a local of
        // the stated size.
        let result = unsafe {
            ffi::rack_vst3_plugin_get_preset_info(
                self.inner.as_ptr(),
                index as u32,
                name.as_mut_ptr(),
                name.len(),
                category.as_mut_ptr(),
                category.len(),
                &mut preset_number,
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }

        // An empty category means "no category" (the plugin's programs aren't
        // organized into named lists, or the list itself has no name).
        let category = c_buffer_to_string(&category, "preset category")?;
        Ok(PresetInfo {
            index,
            name: c_buffer_to_string(&name, "preset name")?,
            preset_number,
            category: if category.is_empty() { None } else { Some(category) },
        })
    }

    /// Number of audio output buses the plugin declares, recorded at
    /// `initialize`.
    #[must_use]
    pub fn output_bus_count(&self) -> usize {
        self.layout.output_bus_channel_counts.len()
    }

    /// Channel count of output bus `bus_index`, or `None` if out of range.
    #[must_use]
    pub fn output_bus_channel_count(&self, bus_index: usize) -> Option<usize> {
        self.layout.output_bus_channel_counts.get(bus_index).copied()
    }

    /// Number of audio input buses the plugin declares, recorded at
    /// `initialize`.
    #[must_use]
    pub fn input_bus_count(&self) -> usize {
        self.layout.input_bus_channel_counts.len()
    }

    /// Channel count of input bus `bus_index`, or `None` if out of range.
    #[must_use]
    pub fn input_bus_channel_count(&self, bus_index: usize) -> Option<usize> {
        self.layout.input_bus_channel_counts.get(bus_index).copied()
    }

    /// Channel count and declared display name of output bus `bus_index`.
    /// Once initialized this answers from what `initialize` recorded and makes
    /// no plugin call; before, it asks VST3 `IComponent::getBusInfo`.
    ///
    /// # Errors
    ///
    /// Returns an error if `bus_index` is out of range.
    pub fn output_bus_info(&self, bus_index: i32) -> Result<(usize, String)> {
        self.bus_info(bus_index, ffi::rack_vst3_plugin_get_output_bus_info, "output bus name")
    }

    /// Channel count and declared display name of input bus `bus_index`, the
    /// same way as [`output_bus_info`](Self::output_bus_info).
    ///
    /// # Errors
    ///
    /// Returns an error if `bus_index` is out of range.
    pub fn input_bus_info(&self, bus_index: i32) -> Result<(usize, String)> {
        self.bus_info(bus_index, ffi::rack_vst3_plugin_get_input_bus_info, "input bus name")
    }

    fn bus_info(
        &self,
        bus_index: i32,
        query: unsafe extern "C" fn(*mut ffi::RackVST3Plugin, i32, *mut u32, *mut c_char, usize) -> c_int,
        what: &str,
    ) -> Result<(usize, String)> {
        let mut channels: u32 = 0;
        let mut name = vec![0 as c_char; 256];
        // SAFETY: the instance is live; the out-pointers address locals of the
        // stated size.
        let result = unsafe {
            query(self.inner.as_ptr(), bus_index, &mut channels, name.as_mut_ptr(), name.len())
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok((channels as usize, c_buffer_to_string(&name, what)?))
    }

    /// Whether the plugin declares any event OUTPUT bus, i.e. whether it can
    /// emit its own MIDI. Once initialized this answers from what `initialize`
    /// recorded and makes no plugin call.
    #[must_use]
    pub fn emits_midi(&self) -> bool {
        // SAFETY: the instance is live.
        unsafe { ffi::rack_vst3_plugin_emits_midi(self.inner.as_ptr()) }
    }

    /// The VST3 ParamID that handles MIDI Program Change on the given channel
    /// (`IMidiMapping`), or 0xFFFFFFFF (kNoParamId) if not mapped.
    #[must_use]
    pub fn midi_pc_param_id(&self, channel: i32) -> u32 {
        // SAFETY: the instance is live; the function asks the controller.
        unsafe { ffi::rack_vst3_plugin_get_midi_pc_param_id(self.inner.as_ptr(), channel) }
    }
}

/// Messages between a plugin's component and its controller that were
/// discarded for being sent off the constructing thread, per direction. See
/// [`Vst3Controller::discarded_messages`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscardedMessages {
    /// Sent by the component (its audio processor) to the controller.
    pub component_to_controller: u64,
    /// Sent by the controller to the component.
    pub controller_to_component: u64,
}

/// What [`Vst3Controller::refresh_parameter_titles`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleRefresh {
    /// Every description is as it was.
    Unchanged,
    /// The descriptions at these indices changed (title, unit, default,
    /// flags or step count), and now read as the plugin reports them.
    Changed(Vec<usize>),
    /// The parameter count or ids changed, so nothing was refreshed: the
    /// instance keeps the list it had, and the plugin needs a new instance.
    IdsChanged,
}

/// The indices whose description differs between two lists of one length.
/// Pure, so the comparison is tested without a plugin.
pub(super) fn changed_parameters(before: &[ParameterInfo], after: &[ParameterInfo]) -> Vec<usize> {
    before
        .iter()
        .zip(after)
        .enumerate()
        .filter(|(_, (old, new))| old != new)
        .map(|(index, _)| index)
        .collect()
}

impl Drop for Vst3Controller {
    fn drop(&mut self) {
        // SAFETY: the instance is live until both halves are released, and
        // this half is released exactly once, here. The C side frees the
        // instance only when the processor half was released first.
        let freed = unsafe { ffi::rack_vst3_plugin_release_controller(self.inner.as_ptr()) };
        debug_assert!(
            freed != 0 || std::thread::panicking(),
            "a Vst3Controller was dropped while its Vst3Processor was still alive; the instance \
             of {} is leaked: drop the processor half first, or join the two",
            self.info.name
        );
    }
}
