use crate::{Error, MidiEvent, ParameterInfo, PluginInfo, PluginInstance, PresetInfo, ProcessMode, Result};
use std::ffi::CString;
use std::fmt;
use std::os::raw::c_int;
use std::ptr::NonNull;

use super::controller::{DiscardedMessages, Vst3Controller};
use super::ffi;
use super::processor::{BusLayout, ExtraInputBus, ExtraOutputBus, Vst3Processor};
use super::restart::RestartFlags;
use super::util::{map_error, take_state_blob};

#[cfg(test)]
use super::processor::rack_midi_to_midi_event;
#[cfg(test)]
use crate::MidiEventKind;

/// An instantiated VST3 plugin: the whole instance, both of its halves held
/// together.
///
/// Every operation on a plugin is available here. Those that are safe while
/// the plugin processes on another thread are also available on one of the
/// two halves [`split`](Self::split) gives: the [`Vst3Processor`] (`process`,
/// MIDI, parameter automation, transport) for the audio thread, and the
/// [`Vst3Controller`] (parameter descriptions and values, presets, bus facts)
/// for the thread that split it. What is left only here needs the instance
/// not to be processing anywhere: initialization, `reset`, state reads and
/// writes, preset loads, the latency query, and the parameter write that tells
/// both the controller and the processor.
///
/// # Thread Safety
///
/// This type is `Send` but not `Sync`:
/// - `Send`: the whole instance can be moved between threads, because moving
///   it moves both halves together and neither is reachable apart from a
///   borrow of it.
/// - NOT `Sync`: multiple threads must not access it at once.
pub struct Vst3Plugin {
    // Declared first, so dropping the whole drops (releases) the processor
    // half before the controller half, which then frees the instance.
    processor: Vst3Processor,
    controller: Vst3Controller,
}

// SAFETY: the whole holds both halves of one instance, so nothing else can
// reach it while it moves: each half's own restriction (the processor is not
// Sync, the controller is not Send) protects a split instance, and an unsplit
// one is used by one thread at a time, which `&mut self` and the missing Sync
// impl enforce. VST3 plugins are designed to be created on one thread and
// processed on another.
unsafe impl Send for Vst3Plugin {}

impl fmt::Debug for Vst3Plugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Vst3Plugin")
            .field("processor", &self.processor)
            .field("controller", &self.controller)
            .finish()
    }
}

/// Two halves that [`Vst3Plugin::join`] refused because they are not halves
/// of the same instance. Both are given back unchanged.
pub struct JoinMismatch {
    /// The processor half that was passed in.
    pub processor: Vst3Processor,
    /// The controller half that was passed in.
    pub controller: Vst3Controller,
}

impl fmt::Debug for JoinMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinMismatch")
            .field("processor", &self.processor)
            .field("controller", &self.controller)
            .finish()
    }
}

impl fmt::Display for JoinMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the processor and the controller are halves of two different instances of {}",
            self.controller.info().name
        )
    }
}

impl std::error::Error for JoinMismatch {}

/// How many VST3 instances exist in this process: created and not yet freed.
/// An instance whose halves were dropped in the wrong order (see
/// [`Vst3Controller`]) is never freed and stays counted here.
#[must_use]
pub fn live_instance_count() -> u64 {
    // SAFETY: reads one process-wide atomic.
    unsafe { ffi::rack_vst3_live_instance_count() }
}

/// How many VST3 instances this process has leaked because a controller half
/// was dropped while its processor half was still alive.
#[must_use]
pub fn leaked_instance_count() -> u64 {
    // SAFETY: reads one process-wide atomic.
    unsafe { ffi::rack_vst3_leaked_instance_count() }
}

impl Vst3Plugin {
    /// Create a new VST3 plugin instance
    pub(crate) fn new(info: &PluginInfo) -> Result<Self> {
        let path_str = info.path.to_str()
            .ok_or_else(|| Error::Other("Plugin path contains invalid UTF-8".to_string()))?;
        let path = CString::new(path_str)
            .map_err(|_| Error::Other("Plugin path contains null byte".to_string()))?;
        let unique_id = CString::new(info.unique_id.as_str())
            .map_err(|_| Error::Other("Invalid unique_id (contains null byte)".to_string()))?;

        // SAFETY: both strings are NUL-terminated and outlive the call.
        let ptr = unsafe { ffi::rack_vst3_plugin_new(path.as_ptr(), unique_id.as_ptr()) };
        let inner = NonNull::new(ptr).ok_or_else(|| {
            Error::PluginNotFound(format!("Failed to create VST3 instance for {}", info.name))
        })?;

        Ok(Self {
            processor: Vst3Processor::new(inner),
            controller: Vst3Controller::new(inner, info.clone()),
        })
    }

    fn instance(&self) -> *mut ffi::RackVST3Plugin {
        self.controller.instance().as_ptr()
    }

    /// Splits an initialized instance into its processor half, which may be
    /// moved to the audio thread, and its controller half, which stays on
    /// this thread. Call it on the thread that constructed the instance. It
    /// makes no plugin call and allocates nothing.
    ///
    /// # Errors
    ///
    /// Gives the instance back unchanged if it is not initialized: the
    /// halves' facts (bus layout, parameter ids) are fixed only by
    /// initialization.
    pub fn split(self) -> std::result::Result<(Vst3Processor, Vst3Controller), Vst3Plugin> {
        if !self.is_initialized() {
            return Err(self);
        }
        let Self { processor, controller } = self;
        Ok((processor, controller))
    }

    /// Joins a processor half and a controller half back into the whole
    /// instance, on the controller half's thread. Makes no plugin call and
    /// allocates nothing.
    ///
    /// # Errors
    ///
    /// Gives both halves back in a [`JoinMismatch`] if they are halves of two
    /// different instances.
    pub fn join(
        processor: Vst3Processor,
        controller: Vst3Controller,
    ) -> std::result::Result<Vst3Plugin, JoinMismatch> {
        if processor.instance() != controller.instance() {
            return Err(JoinMismatch { processor, controller });
        }
        Ok(Self { processor, controller })
    }

    /// Both halves at once, borrowed from the whole.
    pub fn halves_mut(&mut self) -> (&mut Vst3Processor, &mut Vst3Controller) {
        (&mut self.processor, &mut self.controller)
    }

    /// The processor half, borrowed from the whole.
    pub fn processor_mut(&mut self) -> &mut Vst3Processor {
        &mut self.processor
    }

    /// The controller half, borrowed from the whole.
    #[must_use]
    pub fn controller(&self) -> &Vst3Controller {
        &self.controller
    }

    /// Set a parameter by its raw VST3 ParamID (not the rack index), telling
    /// both the controller and the processor, as
    /// [`set_parameter`](PluginInstance::set_parameter) does. Used to reach
    /// MIDI-mapped parameters like Program Change that are not in the indexed
    /// parameter list. Whole only, because it writes the processor's
    /// parameter queue from this thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin has no controller.
    pub fn set_param_by_id(&mut self, param_id: u32, value: f32) -> Result<()> {
        // SAFETY: the instance is live and whole, so nothing processes
        // concurrently with the queue write.
        let result = unsafe { ffi::rack_vst3_plugin_set_parameter_by_id(self.instance(), param_id, value) };
        if result != ffi::RACK_VST3_OK {
            return Err(crate::Error::Other(format!("set_param_by_id failed: {result}")));
        }
        Ok(())
    }

    /// Get the VST3 ParamID that handles MIDI Program Change on the given channel.
    /// Returns 0xFFFFFFFF (kNoParamId) if not mapped.
    #[must_use]
    pub fn midi_pc_param_id(&self, channel: i32) -> u32 {
        self.controller.midi_pc_param_id(channel)
    }

    /// See [`Vst3Controller::take_restart_flags`]. A host takes them here at
    /// the end of configuring an instance, before splitting it, so requests a
    /// plugin raised while it was configured (a state restore commonly raises
    /// some) are not mistaken for changes it made while playing.
    #[must_use]
    pub fn take_restart_flags(&self) -> RestartFlags {
        self.controller.take_restart_flags()
    }

    /// See [`Vst3Controller::discarded_messages`].
    #[must_use]
    pub fn discarded_messages(&self) -> DiscardedMessages {
        self.controller.discarded_messages()
    }

    /// See [`Vst3Processor::set_transport`].
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
        self.processor
            .set_transport(tempo, playing, project_time_samples, project_time_music, bar_position_music)
    }

    /// Number of audio input channels the plugin's main bus expects, as
    /// negotiated during `initialize`. 0 for pure instrument plugins with no
    /// audio input bus. Only meaningful after a successful `initialize` call.
    #[must_use]
    pub fn input_channels(&self) -> usize {
        self.processor.input_channels()
    }

    /// Number of audio output channels the plugin's main bus produces, as
    /// negotiated during `initialize`. Only meaningful after a successful
    /// `initialize` call.
    #[must_use]
    pub fn output_channels(&self) -> usize {
        self.processor.output_channels()
    }

    /// The plugin's own reported processing latency, in samples: the delay
    /// its own processing introduces between input and output (e.g.
    /// lookahead limiters, linear-phase EQs, convolution). 0 if the plugin
    /// has not been initialized or genuinely reports zero latency (the
    /// common case for most instruments/effects). Only meaningful after a
    /// successful `initialize` call.
    ///
    /// Whole only: `IAudioProcessor::getLatencySamples` is a UI-thread call
    /// on the processor object, which belongs to the audio thread once the
    /// instance is split. Read it during configuration.
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        // SAFETY: the instance is live and whole. The C function gates on the
        // initialized state itself and returns 0 if it isn't.
        unsafe { ffi::rack_vst3_plugin_get_latency_samples(self.instance()) }
    }

    /// See [`Vst3Controller::output_bus_count`].
    #[must_use]
    pub fn output_bus_count(&self) -> usize {
        self.controller.output_bus_count()
    }

    /// See [`Vst3Controller::output_bus_channel_count`].
    #[must_use]
    pub fn output_bus_channel_count(&self, bus_index: usize) -> Option<usize> {
        self.controller.output_bus_channel_count(bus_index)
    }

    /// See [`Vst3Controller::output_bus_info`].
    ///
    /// # Errors
    ///
    /// Returns an error if `bus_index` is out of range.
    pub fn output_bus_info(&self, bus_index: i32) -> Result<(usize, String)> {
        self.controller.output_bus_info(bus_index)
    }

    /// See [`Vst3Controller::input_bus_count`].
    #[must_use]
    pub fn input_bus_count(&self) -> usize {
        self.controller.input_bus_count()
    }

    /// See [`Vst3Controller::input_bus_channel_count`].
    #[must_use]
    pub fn input_bus_channel_count(&self, bus_index: usize) -> Option<usize> {
        self.controller.input_bus_channel_count(bus_index)
    }

    /// See [`Vst3Controller::input_bus_info`].
    ///
    /// # Errors
    ///
    /// Returns an error if `bus_index` is out of range.
    pub fn input_bus_info(&self, bus_index: i32) -> Result<(usize, String)> {
        self.controller.input_bus_info(bus_index)
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
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin refuses initialization.
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
        let (state_ptr, state_len) = if state.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (state.as_ptr(), state.len())
        };

        // SAFETY: the instance is live and whole; `state` outlives the call.
        let result = unsafe {
            ffi::rack_vst3_plugin_initialize(
                self.instance(),
                sample_rate,
                max_block_size as u32,
                process_mode.as_raw(),
                state_ptr,
                state_len,
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }

        let layout = self.read_bus_layout()?;
        let emits_midi = self.controller.emits_midi();
        self.processor.configure(layout.clone(), emits_midi);
        self.controller.configure(layout);
        Ok(())
    }

    /// Reads the bus layout initialize recorded: bus 0's channel counts and
    /// every declared bus's.
    fn read_bus_layout(&self) -> Result<BusLayout> {
        // SAFETY: the instance is live and initialized; these read facts
        // initialize recorded.
        let (input_channels, output_channels) = unsafe {
            (
                ffi::rack_vst3_plugin_get_input_channels(self.instance()),
                ffi::rack_vst3_plugin_get_output_channels(self.instance()),
            )
        };
        let (Ok(input_channels), Ok(output_channels)) =
            (usize::try_from(input_channels), usize::try_from(output_channels))
        else {
            return Err(Error::Other("Failed to query channel configuration".to_string()));
        };

        // SAFETY: as above.
        let output_bus_count = unsafe { ffi::rack_vst3_plugin_get_output_bus_count(self.instance()) };
        let output_bus_channel_counts =
            self.bus_channel_counts(output_bus_count, ffi::rack_vst3_plugin_get_output_bus_info, "output")?;
        // SAFETY: as above.
        let input_bus_count = unsafe { ffi::rack_vst3_plugin_get_input_bus_count(self.instance()) };
        let input_bus_channel_counts =
            self.bus_channel_counts(input_bus_count, ffi::rack_vst3_plugin_get_input_bus_info, "input")?;

        Ok(BusLayout { input_channels, output_channels, input_bus_channel_counts, output_bus_channel_counts })
    }

    fn bus_channel_counts(
        &self,
        bus_count: c_int,
        query: unsafe extern "C" fn(*mut ffi::RackVST3Plugin, i32, *mut u32, *mut std::os::raw::c_char, usize) -> c_int,
        direction: &str,
    ) -> Result<Vec<usize>> {
        let bus_count = usize::try_from(bus_count)
            .map_err(|_| Error::Other(format!("Failed to query {direction} bus count")))?;
        let mut counts = Vec::with_capacity(bus_count);
        for bus_index in 0..bus_count {
            let mut channels: u32 = 0;
            // SAFETY: the instance is live; `channels` is a local out-pointer
            // and no name is asked for.
            let result = unsafe {
                query(self.instance(), bus_index as i32, &mut channels, std::ptr::null_mut(), 0)
            };
            if result != ffi::RACK_VST3_OK {
                return Err(map_error(result));
            }
            counts.push(channels as usize);
        }
        Ok(counts)
    }

    /// See [`Vst3Controller::parameter_value_string`].
    ///
    /// # Errors
    ///
    /// As there.
    pub fn parameter_value_string(&self, index: usize, normalized_value: f32) -> Result<String> {
        self.controller.parameter_value_string(index, normalized_value)
    }

    /// See [`Vst3Controller::parameter_plain_value`].
    ///
    /// # Errors
    ///
    /// As there.
    pub fn parameter_plain_value(&self, index: usize, normalized_value: f32) -> Result<f64> {
        self.controller.parameter_plain_value(index, normalized_value)
    }

    /// See [`Vst3Controller::parameter_value_by_string`].
    ///
    /// # Errors
    ///
    /// As there.
    pub fn parameter_value_by_string(&self, index: usize, string: &str) -> Result<f32> {
        self.controller.parameter_value_by_string(index, string)
    }

    /// See [`Vst3Controller::parameter_normalized_from_plain`].
    ///
    /// # Errors
    ///
    /// As there.
    pub fn parameter_normalized_from_plain(&self, index: usize, plain_value: f64) -> Result<f32> {
        self.controller.parameter_normalized_from_plain(index, plain_value)
    }

    /// See [`Vst3Processor::process_multi_bus`].
    ///
    /// # Errors
    ///
    /// As there.
    pub fn process_multi_bus(
        &mut self,
        inputs: &[&[f32]],
        extra_inputs: &[ExtraInputBus<'_>],
        outputs: &mut [&mut [f32]],
        extra_outputs: &mut [ExtraOutputBus<'_>],
        num_frames: usize,
        output_midi: Option<&mut Vec<MidiEvent>>,
    ) -> Result<()> {
        self.processor
            .process_multi_bus(inputs, extra_inputs, outputs, extra_outputs, num_frames, output_midi)
    }

    /// Whether the plugin declares any event OUTPUT bus, i.e. whether it can
    /// emit its own MIDI (an arpeggiator or MIDI-FX plugin). When true,
    /// [`process_multi_bus`](Self::process_multi_bus) with `Some(buffer)`
    /// will capture those emitted events. Once initialized this answers from
    /// what `initialize` recorded and makes no plugin call.
    #[must_use]
    pub fn emits_midi(&self) -> bool {
        self.controller.emits_midi()
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
        // SAFETY: the instance is live and whole; `data` outlives the call.
        let result = unsafe {
            ffi::rack_vst3_plugin_set_state_pumped(self.instance(), data.as_ptr(), data.len(), pump_seconds)
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }
}

impl PluginInstance for Vst3Plugin {
    fn initialize(&mut self, sample_rate: f64, max_block_size: usize, process_mode: ProcessMode) -> Result<()> {
        self.initialize_impl(sample_rate, max_block_size, process_mode, &[])
    }

    fn reset(&mut self) -> Result<()> {
        // SAFETY: the instance is live and whole.
        let result = unsafe { ffi::rack_vst3_plugin_reset(self.instance()) };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }

    fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_frames: usize,
    ) -> Result<()> {
        self.processor.process(inputs, outputs, num_frames)
    }

    fn parameter_count(&self) -> usize {
        self.controller.parameter_count()
    }

    fn parameter_info(&self, index: usize) -> Result<ParameterInfo> {
        self.controller.parameter_info(index)
    }

    fn get_parameter(&self, index: usize) -> Result<f32> {
        self.controller.get_parameter(index)
    }

    /// Writes the value into the edit controller and also queues it for the
    /// processor's next block, so both halves learn of it. Whole only: the
    /// queue write is the processor's state, which a split instance's
    /// controller half must not touch (it has its own controller-only
    /// [`Vst3Controller::set_parameter`]).
    fn set_parameter(&mut self, index: usize, value: f32) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        // SAFETY: the instance is live and whole, so nothing processes
        // concurrently with the queue write.
        let result = unsafe { ffi::rack_vst3_plugin_set_parameter(self.instance(), index as u32, value) };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }

    /// Real-time-safe: see [`Vst3Processor::queue_parameter_change`]. The
    /// plugin's controller/GUI does not observe the change this way -- use
    /// `set_parameter`/[`set_param_by_id`](Self::set_param_by_id) from a
    /// non-realtime thread instead when that matters.
    fn queue_parameter_change(&mut self, index: usize, value: f32) -> Result<()> {
        self.processor.queue_parameter_change(index, value)
    }

    fn send_midi(&mut self, events: &[MidiEvent]) -> Result<()> {
        self.processor.send_midi(events)
    }

    fn preset_count(&self) -> Result<usize> {
        self.controller.preset_count()
    }

    fn preset_info(&self, index: usize) -> Result<PresetInfo> {
        self.controller.preset_info(index)
    }

    fn load_preset(&mut self, preset_number: i32) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        // SAFETY: the instance is live and whole; a preset load may write the
        // processor's parameter queue, which only the whole may do.
        let result = unsafe { ffi::rack_vst3_plugin_load_preset(self.instance(), preset_number) };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }
    /// Reads the state with the plugin out of the Processing state around
    /// the read: the conservative read for a plugin not known to keep its own
    /// state consistent against `process`. A split instance's controller half
    /// has [`Vst3Controller::read_state_while_processing`] instead.
    fn get_state(&self) -> Result<Vec<u8>> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        let mut error: c_int = ffi::RACK_VST3_OK;
        // SAFETY: the instance is live and whole, so stopping processing
        // around the read stops no other thread's work; the returned blob is
        // freed by `take_state_blob` and nowhere else.
        unsafe {
            let blob = ffi::rack_vst3_plugin_read_state(self.instance(), &mut error);
            take_state_blob(blob, error)
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
                self.instance(),
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
        self.controller.info()
    }

    fn is_initialized(&self) -> bool {
        self.controller.is_initialized()
    }
}

// The halves' thread contracts, checked at compile time: the processor half
// and the whole are Send, the controller half is not, and none of the three is
// Sync. A blanket impl for every type and a second one for the types with the
// property make the method's owner ambiguous, so naming it with the marker
// left to inference compiles only when the second impl does not apply: when
// the type lacks the property.
const _: fn() = || {
    trait AmbiguousIfSend<Marker> {
        fn lacks_send() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    struct IsSend;
    impl<T: ?Sized + Send> AmbiguousIfSend<IsSend> for T {}

    trait AmbiguousIfSync<Marker> {
        fn lacks_sync() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    struct IsSync;
    impl<T: ?Sized + Sync> AmbiguousIfSync<IsSync> for T {}

    fn is_send<T: Send>() {}

    is_send::<Vst3Processor>();
    let _ = <Vst3Processor as AmbiguousIfSync<_>>::lacks_sync;
    let _ = <Vst3Controller as AmbiguousIfSend<_>>::lacks_send;
    let _ = <Vst3Controller as AmbiguousIfSync<_>>::lacks_sync;
    is_send::<Vst3Plugin>();
    let _ = <Vst3Plugin as AmbiguousIfSync<_>>::lacks_sync;
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginScanner;
    use super::super::test_support::serial_plugin_test;

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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
        // Exercises get_state end to end on a real installed plugin.
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
        let _serial = serial_plugin_test();
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
