use std::path::PathBuf;

/// Information about a discovered audio plugin
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// Plugin name (e.g., "AUGraphicEQ")
    pub name: String,

    /// Manufacturer/vendor name (e.g., "Apple")
    pub manufacturer: String,

    /// Plugin version
    pub version: u32,

    /// Plugin type (effect, instrument, etc.)
    pub plugin_type: PluginType,

    /// Path to the plugin bundle (for AudioUnits, this is the .component path)
    pub path: PathBuf,

    /// Unique identifier for the plugin
    pub unique_id: String,
}

/// Type of audio plugin
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginType {
    /// Audio effect (processes audio input to output)
    Effect,

    /// Instrument/generator (generates audio from MIDI/events)
    Instrument,

    /// Mixer
    Mixer,

    /// Format converter
    FormatConverter,

    /// Analyzer (spectral/metering plugins)
    Analyzer,

    /// Spatial audio processor (surround, binaural, etc.)
    Spatial,

    /// Other/unknown type
    Other,
}

impl PluginInfo {
    /// Create a new PluginInfo
    pub fn new(
        name: String,
        manufacturer: String,
        version: u32,
        plugin_type: PluginType,
        path: PathBuf,
        unique_id: String,
    ) -> Self {
        Self {
            name,
            manufacturer,
            version,
            plugin_type,
            path,
            unique_id,
        }
    }
}

impl std::fmt::Display for PluginInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} by {} (v{}) [{:?}]",
            self.name, self.manufacturer, self.version, self.plugin_type
        )
    }
}

/// Information about a plugin parameter
#[derive(Debug, Clone)]
pub struct ParameterInfo {
    /// Parameter index
    pub index: usize,

    /// Parameter name
    pub name: String,

    /// Minimum value
    pub min: f32,

    /// Maximum value
    pub max: f32,

    /// Default value
    pub default: f32,

    /// Unit label (e.g., "dB", "Hz", "%")
    pub unit: String,

    /// Number of discrete steps, or 0 if the parameter is continuous.
    ///
    /// A VST3 parameter with `step_count == N` has `N + 1` valid states
    /// (e.g. `step_count == 7` means an 8-way selector such as an amp-type
    /// list). Always 0 for backends that don't report this (currently
    /// AudioUnit).
    pub step_count: i32,

    /// Whether the host can automate this parameter.
    pub can_automate: bool,

    /// Whether the parameter is read-only (host cannot set it).
    pub is_read_only: bool,

    /// Whether the parameter represents a list of discrete named values
    /// rather than a continuous range.
    pub is_list: bool,

    /// Whether the parameter is hidden from the plugin's own UI.
    pub is_hidden: bool,
}

impl ParameterInfo {
    /// Create a new ParameterInfo
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        index: usize,
        name: String,
        min: f32,
        max: f32,
        default: f32,
        unit: String,
        step_count: i32,
        can_automate: bool,
        is_read_only: bool,
        is_list: bool,
        is_hidden: bool,
    ) -> Self {
        Self {
            index,
            name,
            min,
            max,
            default,
            unit,
            step_count,
            can_automate,
            is_read_only,
            is_list,
            is_hidden,
        }
    }
}

/// Information about a plugin preset
#[derive(Debug, Clone)]
pub struct PresetInfo {
    /// Preset index (for enumeration)
    pub index: usize,

    /// Preset name
    pub name: String,

    /// Preset number (AudioUnit-specific identifier used for loading)
    pub preset_number: i32,
}

impl PresetInfo {
    /// Create a new PresetInfo
    pub fn new(index: usize, name: String, preset_number: i32) -> Self {
        Self {
            index,
            name,
            preset_number,
        }
    }
}
