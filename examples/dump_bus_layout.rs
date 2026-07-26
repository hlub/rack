//! Dump the audio bus layout a VST3 plugin declares.
//!
//! Diagnostic tool: prints how many audio input/output buses a plugin
//! declares via VST3's `IComponent::getBusCount`/`getBusInfo`, and each
//! bus's channel count and name. Used to check, ahead of designing real
//! multi-bus support in rack, whether a plugin (e.g. Addictive Drums 2 in
//! "individual outs" mode) already exposes more than one audio output bus.
//!
//! `rack::vst3::ffi` is a private module and the safe `Vst3Plugin` wrapper
//! does not expose its raw pointer, so this example declares the handful of
//! C functions it needs directly rather than adding a safe wrapper method
//! for a throwaway diagnostic. These calls link fine because the `rack`
//! crate's build script already links rack-sys's compiled object code into
//! any binary that depends on the `rack` crate, examples included.
//!
//! Usage: cargo run -p rack --example dump_bus_layout -- [path/to/plugin.vst3]
//! Defaults to the stock AD2 install location on macOS if no path is given.

#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
use rack::vst3::Vst3Scanner;
#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
use rack::{PluginScanner, Result};

#[cfg(all(
    not(target_os = "ios"),
    not(target_os = "tvos"),
    not(target_os = "watchos"),
    not(target_os = "visionos")
))]
fn main() -> Result<()> {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int};
    use std::path::Path;

    #[repr(C)]
    struct RackVST3Plugin {
        _private: [u8; 0],
    }

    const RACK_VST3_OK: c_int = 0;

    extern "C" {
        fn rack_vst3_plugin_new(path: *const c_char, uid: *const c_char) -> *mut RackVST3Plugin;
        fn rack_vst3_plugin_free(plugin: *mut RackVST3Plugin);
        fn rack_vst3_plugin_initialize(
            plugin: *mut RackVST3Plugin,
            sample_rate: f64,
            max_block_size: u32,
            initial_state: *const u8,
            initial_state_size: usize,
        ) -> c_int;
        fn rack_vst3_plugin_get_output_bus_count(plugin: *mut RackVST3Plugin) -> c_int;
        fn rack_vst3_plugin_get_input_bus_count(plugin: *mut RackVST3Plugin) -> c_int;
        fn rack_vst3_plugin_get_output_bus_info(
            plugin: *mut RackVST3Plugin,
            bus_index: i32,
            channels_out: *mut u32,
            name_out: *mut c_char,
            name_out_size: usize,
        ) -> c_int;
        fn rack_vst3_plugin_get_input_bus_info(
            plugin: *mut RackVST3Plugin,
            bus_index: i32,
            channels_out: *mut u32,
            name_out: *mut c_char,
            name_out_size: usize,
        ) -> c_int;
    }

    // Prints one direction's buses ("Output" or "Input"), calling whichever
    // *_bus_info FFI function matches `is_output`.
    fn print_buses(plugin: *mut RackVST3Plugin, direction: &str, count: i32, is_output: bool) {
        println!("{direction} bus count: {count}");
        for bus_index in 0..count {
            let mut channels: u32 = 0;
            let mut name_buf = [0 as c_char; 256];
            let result = unsafe {
                if is_output {
                    rack_vst3_plugin_get_output_bus_info(
                        plugin,
                        bus_index,
                        &mut channels,
                        name_buf.as_mut_ptr(),
                        name_buf.len(),
                    )
                } else {
                    rack_vst3_plugin_get_input_bus_info(
                        plugin,
                        bus_index,
                        &mut channels,
                        name_buf.as_mut_ptr(),
                        name_buf.len(),
                    )
                }
            };
            if result != RACK_VST3_OK {
                println!("  bus {bus_index}: query failed (error {result})");
                continue;
            }
            let name = unsafe { CStr::from_ptr(name_buf.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            println!("  bus {bus_index}: {channels} channel(s), name = \"{name}\"");
        }
    }

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Library/Audio/Plug-Ins/VST3/Addictive Drums 2.vst3".to_string());

    println!("Looking for VST3 plugin at: {path}\n");

    let mut scanner = Vst3Scanner::new()?;
    // Belt and braces: make sure the requested plugin's own directory is
    // searched even if it is not one of the scanner's default system paths.
    if let Some(parent) = Path::new(&path).parent() {
        let _ = scanner.add_path(parent);
    }
    let plugins = scanner.scan()?;

    let requested = Path::new(&path);
    let info = plugins
        .iter()
        .find(|candidate| candidate.path == requested)
        .or_else(|| {
            plugins
                .iter()
                .find(|candidate| candidate.path.file_name() == requested.file_name())
        });

    let Some(info) = info else {
        println!("Plugin not found at {path}");
        println!("({} plugin(s) were found during scan)", plugins.len());
        return Ok(());
    };

    println!("Found plugin: {} by {}", info.name, info.manufacturer);
    println!("  Path: {}", info.path.display());
    println!("  UID: {}\n", info.unique_id);

    let path_cstring =
        CString::new(info.path.to_str().unwrap_or_default()).expect("plugin path has no null bytes");
    let uid_cstring =
        CString::new(info.unique_id.as_str()).expect("plugin UID has no null bytes");

    let plugin = unsafe { rack_vst3_plugin_new(path_cstring.as_ptr(), uid_cstring.as_ptr()) };
    if plugin.is_null() {
        println!("Failed to create plugin instance.");
        return Ok(());
    }

    let init_result =
        unsafe { rack_vst3_plugin_initialize(plugin, 48000.0, 512, std::ptr::null(), 0) };
    if init_result != RACK_VST3_OK {
        println!("Failed to initialize plugin (error {init_result}).");
        unsafe { rack_vst3_plugin_free(plugin) };
        return Ok(());
    }

    println!("Plugin initialized successfully.\n");

    let output_bus_count = unsafe { rack_vst3_plugin_get_output_bus_count(plugin) };
    if output_bus_count < 0 {
        println!("Failed to query output bus count (error {output_bus_count}).");
    } else {
        print_buses(plugin, "Output", output_bus_count, true);
    }

    println!();

    let input_bus_count = unsafe { rack_vst3_plugin_get_input_bus_count(plugin) };
    if input_bus_count < 0 {
        println!("Failed to query input bus count (error {input_bus_count}).");
    } else {
        print_buses(plugin, "Input", input_bus_count, false);
    }

    unsafe { rack_vst3_plugin_free(plugin) };

    Ok(())
}

#[cfg(any(
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
fn main() {
    println!("VST3 is not available on mobile platforms.");
    println!("This example only works on desktop platforms (macOS, Windows, Linux).");
}
