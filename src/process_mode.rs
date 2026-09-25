//! [`ProcessMode`]: whether a host is driving a plugin in real time or
//! rendering offline.
//!
//! VST3's `ProcessSetup` carries this alongside the sample rate and the
//! maximum block size, and the specification explicitly licenses a plugin to
//! spend more CPU for better quality when it is told a pass is offline (the
//! practice is real: Soothe2 ships an offline-rendering quality control that
//! overrides its realtime settings). A host that never says so can only ever
//! get realtime quality, even from a plugin that would have offered better.
//!
//! It belongs on `initialize` rather than in a setter, because the mode is
//! part of `setupProcessing`, which may only be called while an instance is
//! inactive: a live instance cannot be flipped between modes, so the honest
//! shape is that an instance is *born* in the mode it will render in.

/// Which kind of pass a plugin instance is being initialised for.
///
/// Maps one-to-one onto VST3's `Vst::ProcessModes`. There is no default: a
/// caller states which kind of pass this instance is for, because the wrong
/// answer is not a compile error and is inaudible until the render is
/// compared with one made the other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessMode {
    /// Real time: the instance is driven by an audio device's own callback
    /// and must keep up with it. Every live monitoring path uses this.
    Realtime,
    /// Prefetch: blocks are delivered faster than real time but a plugin is
    /// still expected to behave as in realtime, as when a host reads ahead
    /// of the playhead.
    Prefetch,
    /// Offline: no device is waiting, so a plugin may take as long as it
    /// likes per block and is invited to render at higher quality than it
    /// could in real time. What a bounce uses.
    Offline,
}

impl ProcessMode {
    /// The value the native bridge passes to VST3's `ProcessSetup::processMode`.
    ///
    /// These are VST3's own `Vst::ProcessModes` constants, and the C side
    /// (`rack_vst3_plugin_initialize`) rejects anything that is not one of
    /// them rather than guessing -- so this mapping, not an integer chosen at
    /// a call site, is the only way a mode reaches a plugin.
    #[must_use]
    pub const fn as_raw(self) -> i32 {
        match self {
            // Vst::kRealtime
            ProcessMode::Realtime => 0,
            // Vst::kPrefetch
            ProcessMode::Prefetch => 1,
            // Vst::kOffline
            ProcessMode::Offline => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessMode;

    #[test]
    fn each_mode_maps_to_its_own_vst3_constant() {
        assert_eq!(ProcessMode::Realtime.as_raw(), 0, "Vst::kRealtime");
        assert_eq!(ProcessMode::Prefetch.as_raw(), 1, "Vst::kPrefetch");
        assert_eq!(ProcessMode::Offline.as_raw(), 2, "Vst::kOffline");
    }
}
