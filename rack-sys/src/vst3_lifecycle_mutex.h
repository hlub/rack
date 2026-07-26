#ifndef RACK_VST3_LIFECYCLE_MUTEX_H
#define RACK_VST3_LIFECYCLE_MUTEX_H

#include <mutex>

// `Hosting::Module::create` (macOS: `CFBundleLoadExecutableAndReturnError` +
// calling the bundle's `bundleEntry`/`GetPluginFactory` exports) and module
// teardown (`bundleExit`, run from `~MacModule`) are not thread-safe: at
// least JUCE-based plugins register process-global state during these calls
// (observed directly as "Class UtilsOSX is implemented in both ..." dynamic
// linker warnings when two JUCE-based bundles are loaded into one process).
// Concurrent calls into this path from multiple threads -- e.g. two
// `Vst3Scanner::scan()` calls running on separate test threads at once --
// reliably reproduce a JUCE assertion failure
// (`juce_audio_plugin_client_VST3.cpp:4193`) followed by a `SIGTRAP` that
// takes down the whole process. The same real plugins loaded strictly
// sequentially in one thread do not crash; only concurrent loading does.
//
// `vst3_instance.cpp` already serializes its own `Hosting::Module::create`/
// module-teardown calls on a lifecycle mutex for this reason (see
// `rack_vst3_plugin_new`/`rack_vst3_plugin_free`). `vst3_scanner.cpp`'s own
// `Hosting::Module::create` calls (used for metadata enumeration, not
// instantiation) were not covered by that same protection, which is the gap
// that let concurrent scans race. Both translation units now share this one
// mutex so scanning and instantiation are serialized against each other too,
// not just against themselves.
//
// A function-local static (Meyer's singleton) is used, rather than a
// `static` file-scope global, specifically so the two translation units that
// need it (`vst3_scanner.cpp`, `vst3_instance.cpp`) refer to the same
// instance -- a `static` global has internal linkage and would give each
// `.cpp` file its own separate, non-shared mutex.
inline std::mutex& rack_vst3_module_load_mutex() {
    static std::mutex mutex;
    return mutex;
}

#endif // RACK_VST3_LIFECYCLE_MUTEX_H
