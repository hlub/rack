//! Empirical probe: does a real plugin's `set_state` call the host's
//! `Vst::IProgress` (`start`/`update`/`finish`) to signal completion of
//! async post-restore work (e.g. background sample loading), instead of
//! the host having to guess with a fixed wait?
//!
//! Context: `rack_vst3_plugin_set_state` (rack-sys/src/vst3_instance.cpp)
//! always waits a fixed `pump_runloop(5.0)` -- 5 seconds, every call, with
//! no early-exit condition -- after `IComponent::setState` returns, before
//! resuming processing. `ComponentHandler` (the same file) now also
//! implements `Vst::IProgress`, logging every `start`/`update`/`finish`
//! call it receives to stderr with a `[rack] IProgress::...` prefix and a
//! millisecond timestamp, purely for observation -- it never gates or
//! shortens the fixed wait. This example calls `set_state` on a real,
//! installed plugin and reports the wall-clock timing alongside whatever
//! `IProgress` log lines appear (or don't) on stderr during that call, so
//! the two can be read side by side.
//!
//! Usage:
//!   cargo run -p rack --example progress_probe --features vst3 -- [path/to/plugin.vst3]
//!
//! Defaults to the stock AD2 install location on macOS if no path is given.

#[cfg(all(feature = "vst3", target_os = "macos"))]
mod run {
    use rack::vst3::{Vst3Plugin, Vst3Scanner};
    use rack::{PluginInstance, PluginScanner, ProcessMode, Result};
    use std::path::Path;
    use std::time::Instant;

    // JUCE-based plugins (AD2 included) load state/samples asynchronously via
    // their internal MessageManager, which only runs when something pumps the
    // Cocoa event loop on the main thread -- rack_vst3_plugin_set_state's own
    // pump_runloop(5.0) does this pumping internally on every call, but the
    // plugin's *earlier* module/factory creation and NSApplication existing
    // at all still needs setting up here first, the same way every other
    // bare-rack VST3 example in this crate does (see
    // multi_bus_capture_smoke.rs's own init_nsapplication, declared the same
    // way for the same reason: this is a self-contained diagnostic, not a
    // rack dependency).
    #[link(name = "AppKit", kind = "framework")]
    extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn objc_msgSend(
            receiver: *mut std::ffi::c_void,
            sel: *const std::ffi::c_void,
            ...
        ) -> *mut std::ffi::c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *const std::ffi::c_void;
    }

    // SAFETY: well-known Objective-C selectors/argument shapes, called once
    // on the main thread before any run loop pumping happens -- same
    // sequence as multi_bus_capture_smoke.rs's own init_nsapplication.
    fn init_nsapplication() {
        unsafe {
            let class = objc_getClass(c"NSApplication".as_ptr());
            let shared_application_selector = sel_registerName(c"sharedApplication".as_ptr());
            let app = objc_msgSend(class, shared_application_selector);
            let set_activation_policy_selector = sel_registerName(c"setActivationPolicy:".as_ptr());
            objc_msgSend(app, set_activation_policy_selector, 2i64); // Prohibited: no Dock icon
        }
    }

    fn probe_one(path: &str) -> Result<()> {
        let plugin_path = Path::new(path);
        if !plugin_path.exists() {
            println!("SKIP: no plugin at {path}");
            return Ok(());
        }

        println!("=== {path} ===");

        let mut scanner = Vst3Scanner::new()?;
        if let Some(parent) = plugin_path.parent() {
            let _ = scanner.add_path(parent);
        }
        let plugins = scanner.scan()?;
        let info = plugins
            .iter()
            .find(|candidate| candidate.path == plugin_path)
            .or_else(|| {
                plugins
                    .iter()
                    .find(|candidate| candidate.path.file_name() == plugin_path.file_name())
            });
        let Some(info) = info else {
            println!("Plugin not found at {path} ({} scanned)", plugins.len());
            return Ok(());
        };
        println!("Found: {} by {}", info.name, info.manufacturer);

        let mut plugin: Vst3Plugin = scanner.load(info)?;

        let init_start = Instant::now();
        plugin.initialize(48_000.0, 512, ProcessMode::Realtime)?;
        println!("initialize: {:.3}s", init_start.elapsed().as_secs_f64());

        let get_start = Instant::now();
        let state = plugin.get_state()?;
        println!(
            "get_state:  {:.3}s ({} bytes)",
            get_start.elapsed().as_secs_f64(),
            state.len()
        );

        println!("--- calling set_state (1st, identical state) ---");
        let set_start = Instant::now();
        plugin.set_state(&state)?;
        println!("set_state:  {:.3}s", set_start.elapsed().as_secs_f64());

        println!("--- calling set_state (2nd, identical state again) ---");
        let set_start_2 = Instant::now();
        plugin.set_state(&state)?;
        println!("set_state2: {:.3}s", set_start_2.elapsed().as_secs_f64());

        println!();
        Ok(())
    }

    pub fn main() -> Result<()> {
        init_nsapplication();

        let requested: Vec<String> = std::env::args().skip(1).collect();
        let targets: Vec<String> = if requested.is_empty() {
            vec!["/Library/Audio/Plug-Ins/VST3/Addictive Drums 2.vst3".to_string()]
        } else {
            requested
        };

        for target in &targets {
            probe_one(target)?;
        }

        Ok(())
    }
}

#[cfg(all(feature = "vst3", target_os = "macos"))]
fn main() -> rack::Result<()> {
    run::main()
}

#[cfg(not(all(feature = "vst3", target_os = "macos")))]
fn main() {
    println!("This example requires --features vst3 and macOS.");
}
