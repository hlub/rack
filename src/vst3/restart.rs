//! What a plugin asked its host to do through
//! `IComponentHandler::restartComponent`.

/// The flags a plugin has passed to `IComponentHandler::restartComponent`
/// since they were last taken: the VST3 `RestartFlags` of
/// `ivsteditcontroller.h`, each one a request for the host to re-read or
/// rebuild something.
///
/// rack records them and does nothing else: acting on them (re-reading
/// parameter titles, rebuilding a bus layout, a new instance for a reload) is
/// the host's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestartFlags(u32);

const RELOAD_COMPONENT: u32 = 1 << 0;
const IO_CHANGED: u32 = 1 << 1;
const PARAM_VALUES_CHANGED: u32 = 1 << 2;
const LATENCY_CHANGED: u32 = 1 << 3;
const PARAM_TITLES_CHANGED: u32 = 1 << 4;
const MIDI_CC_ASSIGNMENT_CHANGED: u32 = 1 << 5;
const NOTE_EXPRESSION_CHANGED: u32 = 1 << 6;
const IO_TITLES_CHANGED: u32 = 1 << 7;
const PREFETCHABLE_SUPPORT_CHANGED: u32 = 1 << 8;
const ROUTING_INFO_CHANGED: u32 = 1 << 9;
const KEYSWITCH_CHANGED: u32 = 1 << 10;
const PARAM_ID_MAPPING_CHANGED: u32 = 1 << 11;

impl RestartFlags {
    /// The flags as the plugin passed them, unknown bits included.
    #[must_use]
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw flag word.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Whether nothing was requested.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// `kReloadComponent`: the component must be reloaded, which for a host
    /// means a new instance.
    #[must_use]
    pub fn reload_component(self) -> bool {
        self.0 & RELOAD_COMPONENT != 0
    }

    /// `kIoChanged`: the bus layout changed.
    #[must_use]
    pub fn io_changed(self) -> bool {
        self.0 & IO_CHANGED != 0
    }

    /// `kParamValuesChanged`: several parameter values changed at once (as
    /// after a program change); re-read them.
    #[must_use]
    pub fn param_values_changed(self) -> bool {
        self.0 & PARAM_VALUES_CHANGED != 0
    }

    /// `kLatencyChanged`: the reported latency changed.
    #[must_use]
    pub fn latency_changed(self) -> bool {
        self.0 & LATENCY_CHANGED != 0
    }

    /// `kParamTitlesChanged`: parameter titles, default values or flags
    /// changed.
    #[must_use]
    pub fn param_titles_changed(self) -> bool {
        self.0 & PARAM_TITLES_CHANGED != 0
    }

    /// `kMidiCCAssignmentChanged`: the MIDI controller mapping changed.
    #[must_use]
    pub fn midi_cc_assignment_changed(self) -> bool {
        self.0 & MIDI_CC_ASSIGNMENT_CHANGED != 0
    }

    /// `kNoteExpressionChanged`: the note expression descriptions changed.
    #[must_use]
    pub fn note_expression_changed(self) -> bool {
        self.0 & NOTE_EXPRESSION_CHANGED != 0
    }

    /// `kIoTitlesChanged`: bus titles changed.
    #[must_use]
    pub fn io_titles_changed(self) -> bool {
        self.0 & IO_TITLES_CHANGED != 0
    }

    /// `kPrefetchableSupportChanged`: the prefetch support changed.
    #[must_use]
    pub fn prefetchable_support_changed(self) -> bool {
        self.0 & PREFETCHABLE_SUPPORT_CHANGED != 0
    }

    /// `kRoutingInfoChanged`: the routing information changed.
    #[must_use]
    pub fn routing_info_changed(self) -> bool {
        self.0 & ROUTING_INFO_CHANGED != 0
    }

    /// `kKeyswitchChanged`: the key switches changed.
    #[must_use]
    pub fn keyswitch_changed(self) -> bool {
        self.0 & KEYSWITCH_CHANGED != 0
    }

    /// `kParamIDMappingChanged`: the mapping of parameter ids changed.
    #[must_use]
    pub fn param_id_mapping_changed(self) -> bool {
        self.0 & PARAM_ID_MAPPING_CHANGED != 0
    }
}

#[cfg(test)]
mod tests {
    use super::RestartFlags;

    /// Every accessor, in bit order, so each can be checked against its bit.
    fn accessors() -> [(u32, fn(RestartFlags) -> bool); 12] {
        [
            (1 << 0, RestartFlags::reload_component),
            (1 << 1, RestartFlags::io_changed),
            (1 << 2, RestartFlags::param_values_changed),
            (1 << 3, RestartFlags::latency_changed),
            (1 << 4, RestartFlags::param_titles_changed),
            (1 << 5, RestartFlags::midi_cc_assignment_changed),
            (1 << 6, RestartFlags::note_expression_changed),
            (1 << 7, RestartFlags::io_titles_changed),
            (1 << 8, RestartFlags::prefetchable_support_changed),
            (1 << 9, RestartFlags::routing_info_changed),
            (1 << 10, RestartFlags::keyswitch_changed),
            (1 << 11, RestartFlags::param_id_mapping_changed),
        ]
    }

    #[test]
    fn each_accessor_reads_its_own_bit_and_no_other() {
        for (bit, accessor) in accessors() {
            let flags = RestartFlags::from_bits(bit);
            assert!(accessor(flags), "bit {bit:#x} is read by its own accessor");
            for (other_bit, other) in accessors() {
                assert_eq!(other(flags), other_bit == bit, "bit {bit:#x} read by accessor of {other_bit:#x}");
            }
            assert!(!flags.is_empty());
            assert_eq!(flags.bits(), bit);
        }
    }

    #[test]
    fn no_bits_is_empty_and_unknown_bits_are_kept() {
        assert!(RestartFlags::default().is_empty());
        let unknown = RestartFlags::from_bits(1 << 20);
        assert!(!unknown.is_empty(), "a bit this version does not name is still a request");
        assert_eq!(unknown.bits(), 1 << 20);
    }
}
