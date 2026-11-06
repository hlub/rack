//! The processor half of a VST3 instance: everything the audio thread calls.

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;

use smallvec::SmallVec;

use crate::{Error, MidiEvent, MidiEventKind, Result};

use super::ffi;
use super::util::map_error;

/// One additional (non-main) output bus to capture via
/// [`Vst3Processor::process_multi_bus`], and the caller-supplied buffers
/// (planar: one mutable slice per channel) to route its audio into.
pub struct ExtraOutputBus<'a> {
    /// The bus index to capture. Must be >= 1 -- bus 0 is the main bus,
    /// already handled by `process_multi_bus`'s own `outputs` parameter.
    pub bus_index: u32,
    /// One mutable slice per channel. Length must exactly match the bus's
    /// declared channel count (see
    /// [`Vst3Processor::output_bus_channel_count`]); each slice must have
    /// space for at least `num_frames` samples.
    pub channels: &'a mut [&'a mut [f32]],
}

/// One additional (non-main) input bus to feed via
/// [`Vst3Processor::process_multi_bus`], and the caller-supplied buffers
/// (planar: one shared slice per channel) to read its audio from.
pub struct ExtraInputBus<'a> {
    /// The bus index to feed. Must be >= 1 -- bus 0 is the main bus,
    /// already handled by `process_multi_bus`'s own `inputs` parameter.
    pub bus_index: u32,
    /// One shared slice per channel. Length must exactly match the bus's
    /// declared channel count (see
    /// [`Vst3Processor::input_bus_channel_count`]); each slice must have at
    /// least `num_frames` samples. Shared (not mutable) references: these
    /// are buffers Rust hands to the plugin to read, not buffers the plugin
    /// writes into.
    pub channels: &'a [&'a [f32]],
}

/// The audio bus layout recorded at initialization: bus 0's channel count in
/// each direction, and every declared bus's. Copied into both halves so
/// neither asks the plugin for it again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct BusLayout {
    pub(super) input_channels: usize,
    pub(super) output_channels: usize,
    /// Index 0 always equals `input_channels`; most instrument plugins (e.g.
    /// AD2) declare zero input buses.
    pub(super) input_bus_channel_counts: Vec<usize>,
    /// Index 0 always equals `output_channels`.
    pub(super) output_bus_channel_counts: Vec<usize>,
}

/// The processor half of a VST3 plugin instance: `process`, MIDI input,
/// parameter automation for the next block, and the transport. Everything the
/// audio thread needs and nothing it must not call.
///
/// Obtained from [`Vst3Plugin::split`](super::Vst3Plugin::split) and given
/// back with [`Vst3Plugin::join`](super::Vst3Plugin::join).
///
/// # Thread Safety
///
/// `Send` but not `Sync`. It may be moved to another thread (the audio thread)
/// and used there while the [`Vst3Controller`](super::Vst3Controller) of the
/// same instance is used on the thread that split it: every method here
/// touches only the instance's processing state and the facts fixed at
/// initialization, which the controller half never writes, and the VST3
/// standard lets `process` run on a thread other than the UI thread. It is not
/// `Sync` because `process` needs exclusive access.
///
/// Dropping it makes no plugin call and does not free the instance: it only
/// records that this half let go, one atomic store, so it may be dropped on
/// the audio thread at no cost there. The instance is freed when the
/// controller half is dropped after it.
pub struct Vst3Processor {
    inner: NonNull<ffi::RackVST3Plugin>,
    layout: BusLayout,
    emits_midi: bool,
    // Pre-allocated pointer arrays for zero-allocation process() calls
    input_ptrs: Vec<*const f32>,
    output_ptrs: Vec<*mut f32>,
    // Persistent scratch storage for process_multi_bus's zero-allocation hot
    // path: flat, planar channel-pointer storage for every requested extra
    // bus's channels, reserved (during configure) to the sum of every
    // non-main bus's channel count -- a safe upper bound for any subset of
    // buses a single call might request, so pushing into it during
    // process_multi_bus never grows the allocation.
    extra_bus_channel_ptrs: Vec<*mut f32>,
    // One descriptor per requested extra bus, reserved (during configure)
    // to `output_bus_channel_counts.len() - 1` (every bus but bus 0).
    extra_bus_descs: Vec<ffi::RackVST3ExtraOutputBus>,
    // Persistent scratch storage for process_multi_bus's zero-allocation hot
    // path, mirroring `extra_bus_channel_ptrs` above but for extra *input*
    // buses.
    extra_input_bus_channel_ptrs: Vec<*const f32>,
    // One descriptor per requested extra input bus, mirroring
    // `extra_bus_descs` above.
    extra_input_bus_descs: Vec<ffi::RackVST3ExtraInputBus>,
    // Persistent, pre-sized scratch buffer for capturing the plugin's own
    // emitted MIDI output events in `process_multi_bus`'s zero-allocation hot
    // path. Sized once (in `configure`) to `OUTPUT_MIDI_SCRATCH_CAPACITY`
    // and never resized, so the FFI out-pointer always addresses valid slots.
    output_midi_scratch: Vec<ffi::RackVST3MidiEvent>,
    // Cell<()> is Send and not Sync, which is exactly this type's contract.
    _not_sync: PhantomData<Cell<()>>,
}

// SAFETY: the processor half touches only the instance's processing state and
// the facts fixed at initialization (see `RackVST3Plugin`'s field partition in
// rack-sys/src/vst3_instance.cpp). The controller half never writes either
// once the instance is split, and the whole instance writes them only while it
// holds both halves, so moving this half to another thread shares nothing
// either thread writes. The raw pointers in the scratch vectors point only at
// buffers of the call that set them and are never read after it.
unsafe impl Send for Vst3Processor {}

impl fmt::Debug for Vst3Processor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Vst3Processor")
            .field("instance", &self.inner)
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl Vst3Processor {
    /// Fixed upper bound on the number of emitted MIDI events captured from a
    /// single `process_multi_bus` call. A block is at most a few thousand
    /// samples, so a MIDI-FX plugin emitting hundreds of events in one block is
    /// already far beyond realistic; 256 is generous and never reached in
    /// practice, matching this crate's "generous fixed bound" precedent for its
    /// other scratch buffers. A plugin that somehow emits more has its capture
    /// truncated to this many, with the true count still reported.
    pub const OUTPUT_MIDI_SCRATCH_CAPACITY: usize = 256;

    /// A processor half over `inner`, with no layout until
    /// [`configure`](Self::configure) runs.
    pub(super) fn new(inner: NonNull<ffi::RackVST3Plugin>) -> Self {
        Self {
            inner,
            layout: BusLayout::default(),
            emits_midi: false,
            input_ptrs: Vec::new(),
            output_ptrs: Vec::new(),
            extra_bus_channel_ptrs: Vec::new(),
            extra_bus_descs: Vec::new(),
            extra_input_bus_channel_ptrs: Vec::new(),
            extra_input_bus_descs: Vec::new(),
            output_midi_scratch: Vec::new(),
            _not_sync: PhantomData,
        }
    }

    /// The instance both halves share.
    pub(super) fn instance(&self) -> NonNull<ffi::RackVST3Plugin> {
        self.inner
    }

    /// Takes the layout `initialize` recorded and reserves every scratch
    /// buffer the process calls use, so none of them allocates afterwards.
    pub(super) fn configure(&mut self, layout: BusLayout, emits_midi: bool) {
        self.input_ptrs = Vec::with_capacity(layout.input_channels.max(8));
        self.output_ptrs = Vec::with_capacity(layout.output_channels.max(8));
        self.input_ptrs.resize(layout.input_channels, std::ptr::null());
        self.output_ptrs.resize(layout.output_channels, std::ptr::null_mut());

        let extra_bus_capacity: usize = layout.output_bus_channel_counts.iter().skip(1).sum();
        let extra_bus_count = layout.output_bus_channel_counts.len().saturating_sub(1);
        self.extra_bus_channel_ptrs = Vec::with_capacity(extra_bus_capacity);
        self.extra_bus_descs = Vec::with_capacity(extra_bus_count);

        let extra_input_bus_capacity: usize = layout.input_bus_channel_counts.iter().skip(1).sum();
        let extra_input_bus_count = layout.input_bus_channel_counts.len().saturating_sub(1);
        self.extra_input_bus_channel_ptrs = Vec::with_capacity(extra_input_bus_capacity);
        self.extra_input_bus_descs = Vec::with_capacity(extra_input_bus_count);

        self.output_midi_scratch = vec![
            ffi::RackVST3MidiEvent { sample_offset: 0, status: 0, data1: 0, data2: 0, channel: 0 };
            Self::OUTPUT_MIDI_SCRATCH_CAPACITY
        ];

        self.layout = layout;
        self.emits_midi = emits_midi;
    }

    /// Whether the instance has been initialized. Reads a fact fixed at
    /// initialization; no plugin call.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        // SAFETY: `inner` is a live instance for as long as either half exists
        // (the controller half frees it only after this half was released),
        // and the function reads one field initialize writes.
        unsafe { ffi::rack_vst3_plugin_is_initialized(self.inner.as_ptr()) > 0 }
    }

    /// Number of audio input channels the plugin's main bus expects, as
    /// negotiated during `initialize`. 0 for pure instrument plugins with no
    /// audio input bus.
    #[must_use]
    pub fn input_channels(&self) -> usize {
        self.layout.input_channels
    }

    /// Number of audio output channels the plugin's main bus produces, as
    /// negotiated during `initialize`.
    #[must_use]
    pub fn output_channels(&self) -> usize {
        self.layout.output_channels
    }

    /// Number of audio output buses the plugin declares, recorded at
    /// `initialize`. Distinct from [`output_channels`](Self::output_channels),
    /// which reports only bus 0's channel count. A plugin like Addictive
    /// Drums 2 may declare several named per-instrument output buses beyond
    /// bus 0; see [`process_multi_bus`](Self::process_multi_bus) to capture
    /// them.
    #[must_use]
    pub fn output_bus_count(&self) -> usize {
        self.layout.output_bus_channel_counts.len()
    }

    /// Channel count of output bus `bus_index`, or `None` if `bus_index` is
    /// out of range. Needed to size the buffers passed to
    /// [`process_multi_bus`](Self::process_multi_bus) for a given bus.
    #[must_use]
    pub fn output_bus_channel_count(&self, bus_index: usize) -> Option<usize> {
        self.layout.output_bus_channel_counts.get(bus_index).copied()
    }

    /// Number of audio input buses the plugin declares, recorded at
    /// `initialize`. Most instrument plugins (e.g. Addictive Drums 2) declare
    /// zero input buses.
    #[must_use]
    pub fn input_bus_count(&self) -> usize {
        self.layout.input_bus_channel_counts.len()
    }

    /// Channel count of input bus `bus_index`, or `None` if `bus_index` is out
    /// of range.
    #[must_use]
    pub fn input_bus_channel_count(&self, bus_index: usize) -> Option<usize> {
        self.layout.input_bus_channel_counts.get(bus_index).copied()
    }

    /// Whether the plugin declares any event OUTPUT bus, i.e. whether it can
    /// emit its own MIDI (an arpeggiator or MIDI-FX plugin), as recorded at
    /// `initialize`.
    #[must_use]
    pub fn emits_midi(&self) -> bool {
        self.emits_midi
    }

    /// Validates bus 0's channel counts and buffer lengths against the layout.
    fn validate_main_bus(
        &self,
        inputs: &[&[f32]],
        outputs: &[&mut [f32]],
        num_frames: usize,
    ) -> Result<()> {
        if inputs.len() != self.layout.input_channels {
            return Err(Error::Other(format!(
                "Input channel count mismatch: plugin expects {}, got {}",
                self.layout.input_channels,
                inputs.len()
            )));
        }
        if outputs.len() != self.layout.output_channels {
            return Err(Error::Other(format!(
                "Output channel count mismatch: plugin expects {}, got {}",
                self.layout.output_channels,
                outputs.len()
            )));
        }
        for (index, input) in inputs.iter().enumerate() {
            if input.len() < num_frames {
                return Err(Error::Other(format!(
                    "Input channel {} has {} samples, need at least {}",
                    index,
                    input.len(),
                    num_frames
                )));
            }
        }
        for (index, output) in outputs.iter().enumerate() {
            if output.len() < num_frames {
                return Err(Error::Other(format!(
                    "Output channel {} has {} samples, need at least {}",
                    index,
                    output.len(),
                    num_frames
                )));
            }
        }
        Ok(())
    }

    /// Process one block of planar audio through bus 0 (the same contract as
    /// [`PluginInstance::process`](crate::PluginInstance::process)). Any other
    /// output bus is written into an internal silent buffer.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized, if the channel
    /// counts don't match the main bus, if a buffer is shorter than
    /// `num_frames`, or if the plugin's own `process` fails.
    pub fn process(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_frames: usize,
    ) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        self.validate_main_bus(inputs, outputs, num_frames)?;

        // Defense-in-depth: outputs must always be present; inputs may be empty
        // for instrument plugins that have no audio input buses.
        if outputs.is_empty() {
            return Err(Error::Other("Empty output channels".to_string()));
        }

        // Reuse pre-allocated pointer arrays (zero-allocation hot path)
        for (index, input_ch) in inputs.iter().enumerate() {
            self.input_ptrs[index] = input_ch.as_ptr();
        }
        for (index, output_ch) in outputs.iter_mut().enumerate() {
            self.output_ptrs[index] = output_ch.as_mut_ptr();
        }

        // SAFETY: the instance is live (see `is_initialized`), the pointer
        // arrays hold exactly the validated channel counts, each pointing at a
        // buffer of at least `num_frames` samples that outlives the call.
        let result = unsafe {
            ffi::rack_vst3_plugin_process(
                self.inner.as_ptr(),
                self.input_ptrs.as_ptr(),
                inputs.len() as u32,
                self.output_ptrs.as_ptr(),
                outputs.len() as u32,
                num_frames as u32,
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }

    /// Process audio, capturing the main output bus (bus 0, identical
    /// semantics to [`process`](Self::process)) plus zero or more additional
    /// output buses, and feeding zero or more additional input buses, in the
    /// same underlying VST3 `process()` call.
    ///
    /// A VST3 plugin computes every activated output bus in a single
    /// `process()` call regardless of how many the host actually captures;
    /// `initialize` activates every bus the plugin declares. This lets a
    /// caller bind real buffers to output buses beyond bus 0 (e.g. a drum
    /// plugin's per-instrument buses) instead of leaving them pointed at
    /// the internal silent dummy buffer, as [`process`](Self::process)
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
    /// audio-only caller wants. Capture is zero-allocation in the steady
    /// state: once `output_midi`'s `Vec` and the plugin's internal scratch
    /// buffer have each grown once, neither reallocates. At most
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
        self.validate_main_bus(inputs, outputs, num_frames)?;
        for (position, extra) in extra_outputs.iter().enumerate() {
            let duplicate = extra_outputs[..position].iter().any(|other| other.bus_index == extra.bus_index);
            validate_extra_bus(
                ExtraBusRequest { direction: "Output", parameter: "outputs", bus_index: extra.bus_index, duplicate },
                extra.channels.iter().map(|channel| channel.len()),
                &self.layout.output_bus_channel_counts,
                num_frames,
            )?;
        }
        for (position, extra) in extra_inputs.iter().enumerate() {
            let duplicate = extra_inputs[..position].iter().any(|other| other.bus_index == extra.bus_index);
            validate_extra_bus(
                ExtraBusRequest { direction: "Input", parameter: "inputs", bus_index: extra.bus_index, duplicate },
                extra.channels.iter().map(|channel| channel.len()),
                &self.layout.input_bus_channel_counts,
                num_frames,
            )?;
        }

        // Reuse pre-allocated pointer arrays (zero-allocation hot path).
        for (index, input_ch) in inputs.iter().enumerate() {
            self.input_ptrs[index] = input_ch.as_ptr();
        }
        for (index, output_ch) in outputs.iter_mut().enumerate() {
            self.output_ptrs[index] = output_ch.as_mut_ptr();
        }

        // Flatten each extra output bus's channel pointers into scratch
        // storage reserved (in `configure`) to the sum of every non-main bus's
        // channel count -- a safe upper bound for any subset of distinct buses
        // a single call might request (duplicates were refused above), so these
        // pushes never grow the allocation.
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

        // Flatten each extra input bus's channel pointers into scratch storage
        // the same way.
        self.extra_input_bus_channel_ptrs.clear();
        self.extra_input_bus_descs.clear();
        for extra in extra_inputs {
            let start = self.extra_input_bus_channel_ptrs.len();
            for channel in extra.channels {
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
        // capture, point the FFI at the pre-sized scratch buffer; otherwise
        // pass NULL/0 for the audio-only behavior.
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

        // SAFETY: the instance is live; every pointer array and descriptor was
        // validated above against the recorded layout and points at caller
        // buffers of at least `num_frames` samples that outlive the call; the
        // MIDI scratch holds `out_events_cap` slots.
        let result = unsafe {
            ffi::rack_vst3_plugin_process_multi_bus(
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
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }

        // Decode the captured raw events into rack's own `MidiEvent` type. The
        // C side already truncated to the scratch capacity; `num_output_events`
        // may still report a larger true total, which is ignored beyond the
        // captured prefix.
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

    /// Queue a normalized parameter value for the plugin's audio processor,
    /// delivered at sample offset 0 of the next process call through VST3's
    /// `IParameterChanges` -- the same path real VST3 hosts use for
    /// automation. Never calls the edit controller, and never allocates once
    /// `initialize` has run (it pre-sizes the queue), so it is the call to make
    /// from the audio thread immediately before a process call. The queue is
    /// cleared after every process call, so nothing queued here is left stale
    /// for a later one.
    ///
    /// # Errors
    ///
    /// Returns an error for an index outside the parameter list.
    pub fn queue_parameter_change(&mut self, index: usize, value: f32) -> Result<()> {
        // SAFETY: the instance is live; the function validates `index` against
        // the ids fixed at initialization and touches only the processor's
        // own parameter queue.
        let result = unsafe {
            ffi::rack_vst3_plugin_queue_parameter_change(self.inner.as_ptr(), index as u32, value)
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }

    /// [`queue_parameter_change`](Self::queue_parameter_change), also recording `generation` (from
    /// [`Vst3Controller::begin_host_write`](super::Vst3Controller::begin_host_write)) as the
    /// generation of the value the plugin is now given for the parameter. The reports it makes after
    /// are stamped with it, and the controller thread's take drops a report stamped older than the
    /// host's newest write. Real-time-safe, like the unstamped call.
    ///
    /// # Errors
    ///
    /// Returns an error for an index outside the parameter list.
    pub fn queue_parameter_change_stamped(&mut self, index: usize, value: f32, generation: u32) -> Result<()> {
        // SAFETY: as `queue_parameter_change`; the stamp is one store into a processor-owned array.
        let result = unsafe {
            ffi::rack_vst3_plugin_queue_parameter_change_stamped(self.inner.as_ptr(), index as u32, value, generation)
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }

    /// Queue MIDI events for the next process call.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugin is not initialized, or if the event
    /// queue was full and some events were dropped (the ones that fit are
    /// still queued).
    pub fn send_midi(&mut self, events: &[MidiEvent]) -> Result<()> {
        if !self.is_initialized() {
            return Err(Error::NotInitialized);
        }
        if events.is_empty() {
            return Ok(());
        }

        // Use SmallVec for zero-allocation in typical cases (16 events or fewer)
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
                // System messages have no channel, and VST3 has no standard
                // event for system real-time messages.
                MidiEventKind::TimingClock | MidiEventKind::Start | MidiEventKind::Continue |
                MidiEventKind::Stop | MidiEventKind::ActiveSensing | MidiEventKind::SystemReset => {
                    continue;
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

        // SAFETY: the instance is live; `c_events` holds exactly the count
        // passed and outlives the call.
        let result = unsafe {
            ffi::rack_vst3_plugin_send_midi(self.inner.as_ptr(), c_events.as_ptr(), c_events.len() as u32)
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
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
    /// It writes directly into the plugin's persistent process context with
    /// no queuing or locking, which `&mut self` makes sound: only the holder
    /// of this half can call it, and that is the thread that also calls
    /// `process()`.
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
        // SAFETY: the instance is live; the function writes only the
        // processor's own process context.
        let result = unsafe {
            ffi::rack_vst3_plugin_set_transport(
                self.inner.as_ptr(),
                tempo,
                i32::from(playing),
                project_time_samples,
                project_time_music,
                bar_position_music,
            )
        };
        if result != ffi::RACK_VST3_OK {
            return Err(map_error(result));
        }
        Ok(())
    }
}

impl Drop for Vst3Processor {
    fn drop(&mut self) {
        // SAFETY: the instance is live until both halves are released, and
        // this half is released exactly once, here. The call is one atomic
        // store: no plugin call, no free, safe on the audio thread.
        unsafe { ffi::rack_vst3_plugin_release_processor(self.inner.as_ptr()) }
    }
}

/// One requested extra bus, as [`validate_extra_bus`] needs it described.
struct ExtraBusRequest {
    direction: &'static str,
    parameter: &'static str,
    bus_index: u32,
    /// Whether an earlier entry of the same slice requested the same bus.
    duplicate: bool,
}

/// Validates one requested extra bus against the recorded channel counts:
/// never bus 0, never twice, in range, with exactly the bus's channel count,
/// each channel at least `num_frames` long. Bus indices must be unique because
/// the scratch storage is reserved for the sum of every *distinct* non-main
/// bus's channel count, a bound a duplicate would exceed, forcing a
/// reallocation mid-loop and leaving an already bound pointer dangling.
fn validate_extra_bus(
    request: ExtraBusRequest,
    channel_lengths: impl ExactSizeIterator<Item = usize>,
    channel_counts: &[usize],
    num_frames: usize,
) -> Result<()> {
    let ExtraBusRequest { direction, parameter, bus_index, duplicate } = request;
    if bus_index == 0 {
        return Err(Error::Other(format!(
            "Bus 0 is the main bus; pass it via `{parameter}`, not `extra_{parameter}`"
        )));
    }
    if duplicate {
        return Err(Error::Other(format!(
            "{direction} bus index {bus_index} requested more than once in extra_{parameter}"
        )));
    }
    let expected_channels = channel_counts
        .get(bus_index as usize)
        .copied()
        .ok_or_else(|| Error::Other(format!("{direction} bus index {bus_index} out of range")))?;
    if channel_lengths.len() != expected_channels {
        return Err(Error::Other(format!(
            "{direction} bus {bus_index} channel count mismatch: plugin declares {expected_channels}, got {}",
            channel_lengths.len()
        )));
    }
    for (channel_index, length) in channel_lengths.enumerate() {
        if length < num_frames {
            return Err(Error::Other(format!(
                "{direction} bus {bus_index} channel {channel_index} has {length} samples, need at least {num_frames}"
            )));
        }
    }
    Ok(())
}

/// Decode a captured raw `RackVST3MidiEvent` (the C layer's decode of a VST3
/// output `Event`) into rack's own [`MidiEvent`], the inverse of the encode in
/// [`Vst3Processor::send_midi`]. Returns `None` for a status byte with no
/// `MidiEventKind` mapping. Pure and plugin-independent, so it is unit-tested
/// directly against synthetic `RackVST3MidiEvent`s.
pub(super) fn rack_midi_to_midi_event(raw: &ffi::RackVST3MidiEvent) -> Option<MidiEvent> {
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
