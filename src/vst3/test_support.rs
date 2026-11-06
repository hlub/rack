//! Test support shared by this module's tests: the serialization lock every
//! test that loads a plugin instance takes, and the fixture plugin built by
//! `scripts/build-fixture-plugin.sh` (sources in `rack-sys/test/fixture_plugin`).

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use crate::{PluginInfo, PluginScanner, ProcessMode};

use super::{Vst3Plugin, Vst3Scanner};

/// Held by every test that loads a plugin instance, for the whole test.
///
/// Real plugins loaded concurrently on several test threads have been seen to
/// get the test process killed outright, and the instance counters the split
/// tests read are process-wide, so one test's instances must not be counted
/// in another's. Taken with recovery: a test that panicked while holding it
/// leaves nothing behind that the next test could trip on.
pub(crate) fn serial_plugin_test() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The fixture's parameter ids, which are also its parameter indices. Mirrors
/// `rack-sys/test/fixture_plugin/src/fixture_ids.h`.
pub(crate) mod param {
    pub(crate) const GAIN: usize = 0;
    pub(crate) const REQUEST_RESTART: usize = 1;
    pub(crate) const RENAME_TITLES: usize = 2;
    pub(crate) const CHANGE_IDS: usize = 3;
    pub(crate) const SEND_FROM_PROCESS: usize = 4;
    pub(crate) const MODE: usize = 5;
    pub(crate) const SELF_EDIT: usize = 6;
    pub(crate) const REPORT_FROM_PROCESS: usize = 7;
    pub(crate) const METER: usize = 8;
    pub(crate) const ECHO: usize = 9;
    pub(crate) const COUNT: usize = 10;
    /// What `SELF_EDIT` makes the controller's `GAIN` (`kSelfEditGain`).
    pub(crate) const SELF_EDIT_GAIN: f32 = 0.25;
    /// What `REPORT_FROM_PROCESS` makes the component's `MODE` (`kReportedMode`).
    pub(crate) const REPORTED_MODE: f32 = 0.9;
    /// The value the fixture's component starts `MODE` with, which is not the
    /// controller's declared default of 0 (`kModeStart`).
    pub(crate) const MODE_START: f32 = 0.5;
    /// `REQUEST_RESTART`'s step count: its value times this, rounded, is the
    /// flag word the fixture passes to `restartComponent`.
    pub(crate) const RESTART_FLAG_STEPS: f32 = 4095.0;
}

/// The VST3 `Event::type` of a legacy MIDI controller event
/// (`Event::kLegacyMIDICCOutEvent`).
pub(crate) const LEGACY_MIDI_CC_EVENT: u16 = 65535;

/// The VST3 SDK's `ControllerNumbers` (`ivstmidicontrollers.h`) for the
/// messages that have no event of their own, written out here as the SDK
/// fixes them so a test pins rack's encoding to the SDK and not to rack.
pub(crate) mod control_number {
    pub(crate) const AFTER_TOUCH: u8 = 128;
    pub(crate) const PITCH_BEND: u8 = 129;
    pub(crate) const PROGRAM_CHANGE: u8 = 130;
}

/// How many of the most recent input events the fixture keeps; mirrors
/// `kEventLogSize` in `fixture_ids.h`.
const EVENT_LOG_SIZE: usize = 8;

const COMPONENT_STATE_MAGIC: u32 = 0x5246_5843;
const CONTROLLER_STATE_MAGIC: u32 = 0x5246_5852;

/// Where the fixture bundle is: `RACK_TEST_FIXTURE_VST3` when set, otherwise
/// where the build script puts it.
fn fixture_bundle_path() -> PathBuf {
    std::env::var_os("RACK_TEST_FIXTURE_VST3").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/fixture-plugin/build/VST3/Release/rack-fixture.vst3")
    })
}

/// The fixture's scanned info, or `None` when it has not been built. Under
/// `RACK_REQUIRE_FIXTURE=1` a missing fixture fails the test instead, so a
/// run that means to exercise the fixture cannot pass by skipping it.
pub(crate) fn fixture_info() -> Option<(Vst3Scanner, PluginInfo)> {
    let path = fixture_bundle_path();
    let required = std::env::var("RACK_REQUIRE_FIXTURE").is_ok_and(|value| value == "1");
    if !path.exists() {
        assert!(
            !required,
            "the fixture plugin is not built at {}: run scripts/build-fixture-plugin.sh",
            path.display()
        );
        println!("Skipping test - fixture plugin not built at {}", path.display());
        return None;
    }
    let scanner = Vst3Scanner::new().expect("a VST3 scanner should be created");
    let plugins = scanner.scan_path(&path).expect("the fixture bundle should scan");
    let info = plugins
        .into_iter()
        .find(|plugin| plugin.name == "rack fixture")
        .expect("the fixture bundle should list the fixture component");
    Some((scanner, info))
}

/// Block size the fixture tests initialize with.
pub(crate) const FIXTURE_BLOCK: usize = 64;

/// A fixture instance, initialized for realtime processing at 48 kHz, or
/// `None` when the fixture has not been built (see [`fixture_info`]).
pub(crate) fn load_fixture() -> Option<Vst3Plugin> {
    let (scanner, info) = fixture_info()?;
    let mut plugin = scanner.load(&info).expect("the fixture should load");
    crate::PluginInstance::initialize(&mut plugin, 48000.0, FIXTURE_BLOCK, ProcessMode::Realtime)
        .expect("the fixture should initialize");
    Some(plugin)
}

/// What the fixture recorded, read back out of a state blob in rack's
/// format: a little-endian `u32` component-state size, the component's state,
/// then the controller's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FixtureCounters {
    pub(crate) process_calls: u64,
    pub(crate) set_processing_false_calls: u64,
    pub(crate) component_messages: u64,
    pub(crate) points: [u64; param::COUNT],
    pub(crate) controller_messages: u64,
    /// Input events received since the fixture was created.
    pub(crate) events_received: u64,
    /// The last (at most eight) input events, oldest first.
    pub(crate) events: Vec<ReceivedEvent>,
}

/// One input event as the fixture saw it. The legacy fields are zero for an
/// event of any other type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReceivedEvent {
    pub(crate) event_type: u16,
    pub(crate) control_number: u8,
    pub(crate) channel: u8,
    pub(crate) value: u8,
    pub(crate) value2: u8,
}

impl ReceivedEvent {
    /// A legacy MIDI controller event with these fields.
    pub(crate) fn legacy(control_number: u8, channel: u8, value: u8, value2: u8) -> Self {
        Self { event_type: LEGACY_MIDI_CC_EVENT, control_number, channel, value, value2 }
    }

    fn unpack(word: u64) -> Self {
        Self {
            event_type: (word & 0xFFFF) as u16,
            control_number: ((word >> 16) & 0xFF) as u8,
            channel: ((word >> 24) & 0xFF) as u8,
            value: ((word >> 32) & 0xFF) as u8,
            value2: ((word >> 40) & 0xFF) as u8,
        }
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    let word: [u8; 4] = bytes[at..at + 4].try_into().expect("four bytes");
    u32::from_le_bytes(word)
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let word: [u8; 8] = bytes[at..at + 8].try_into().expect("eight bytes");
    u64::from_le_bytes(word)
}

/// Parses a state blob the fixture wrote. Panics on anything else, which in a
/// test is the failure wanted.
pub(crate) fn parse_fixture_state(state: &[u8]) -> FixtureCounters {
    let component_size = read_u32(state, 0) as usize;
    let component = &state[4..4 + component_size];
    let controller = &state[4 + component_size..];
    assert_eq!(read_u32(component, 0), COMPONENT_STATE_MAGIC, "component state magic");
    assert_eq!(component.len(), 4 + 8 * (3 + param::COUNT + 1 + EVENT_LOG_SIZE + 1), "component state size");
    assert_eq!(read_u32(controller, 0), CONTROLLER_STATE_MAGIC, "controller state magic");
    let mut points = [0u64; param::COUNT];
    for (index, slot) in points.iter_mut().enumerate() {
        *slot = read_u64(component, 4 + 8 * (3 + index));
    }
    let events_received = read_u64(component, 4 + 8 * (3 + param::COUNT));
    let kept = (events_received as usize).min(EVENT_LOG_SIZE);
    let first = events_received as usize - kept;
    let events = (first..first + kept)
        .map(|number| {
            let slot = number % EVENT_LOG_SIZE;
            ReceivedEvent::unpack(read_u64(component, 4 + 8 * (3 + param::COUNT + 1 + slot)))
        })
        .collect();
    FixtureCounters {
        events_received,
        events,
        process_calls: read_u64(component, 4),
        set_processing_false_calls: read_u64(component, 12),
        component_messages: read_u64(component, 20),
        points,
        controller_messages: read_u64(controller, 4),
    }
}

/// `state`, a blob the fixture wrote, with the component's `MODE` value
/// replaced by `mode`: the state of a fixture that was left in that mode.
pub(crate) fn fixture_state_with_mode(state: &[u8], mode: f64) -> Vec<u8> {
    let component_size = read_u32(state, 0) as usize;
    let word_at = 4 + component_size - 8;
    let mut patched = state.to_vec();
    patched[word_at..word_at + 8].copy_from_slice(&mode.to_bits().to_le_bytes());
    patched
}

/// Planar silent input and zeroed output buffers sized for one fixture block.
pub(crate) struct Buffers {
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
}

impl Buffers {
    pub(crate) fn new(input_channels: usize, output_channels: usize) -> Self {
        Self {
            inputs: vec![vec![0.0; FIXTURE_BLOCK]; input_channels],
            outputs: vec![vec![0.0; FIXTURE_BLOCK]; output_channels],
        }
    }

    /// Runs `body` with the buffers as the slices `process` takes.
    pub(crate) fn with<R>(&mut self, body: impl FnOnce(&[&[f32]], &mut [&mut [f32]]) -> R) -> R {
        let inputs: Vec<&[f32]> = self.inputs.iter().map(Vec::as_slice).collect();
        let mut outputs: Vec<&mut [f32]> = self.outputs.iter_mut().map(Vec::as_mut_slice).collect();
        body(&inputs, &mut outputs)
    }
}
