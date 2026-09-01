#include "rack_vst3.h"
#include "public.sdk/source/vst/hosting/module.h"
#include "public.sdk/source/vst/hosting/plugprovider.h"
#include "public.sdk/source/vst/moduleinfo/moduleinfoparser.h"
#include "public.sdk/source/common/readfile.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"

#include <vector>
#include <string>
#include <algorithm>
#include <cstring>
#include <mutex>

#include "vst3_lifecycle_mutex.h"

#if defined(__APPLE__)
    #include <CoreFoundation/CoreFoundation.h>
    #include <dirent.h>
    #include <sys/stat.h>
#elif defined(_WIN32)
    #include <windows.h>
    #include <shlobj.h>
#else
    #include <unistd.h>
    #include <pwd.h>
    #include <dirent.h>
    #include <sys/stat.h>
#endif

using namespace VST3;
using namespace Steinberg;
using namespace Steinberg::Vst;

// Internal scanner state
struct RackVST3Scanner {
    std::vector<std::string> search_paths;

    // Whether a scan should additionally merge in every system-registered
    // module via Hosting::Module::getModulePaths(). Set only by
    // rack_vst3_scanner_add_default_paths -- never by rack_vst3_scanner_add_path
    // -- so a scanner built purely from explicit caller paths (e.g.
    // Vst3Scanner::scan_path, which never calls add_default_paths) stays
    // genuinely scoped to just those paths, while a scanner seeded with the
    // system default directories keeps its existing, intentionally broader
    // completeness. `search_paths.empty()` cannot be used for this instead:
    // add_default_paths also populates search_paths, so a scan_path scanner
    // (search_paths holding exactly the one requested path) and a scan()
    // scanner (search_paths holding the default directories) are otherwise
    // indistinguishable at this point.
    bool include_system_modules = false;
};

// Helper: Scan a directory for .vst3 bundles/folders
// Returns list of full paths to .vst3 bundles found
static std::vector<std::string> scan_directory_for_vst3(const std::string& dir_path) {
    std::vector<std::string> vst3_paths;

    // If dir_path is itself a .vst3 bundle (not a container directory to
    // search inside), return it directly. A bundle's own contents don't
    // contain nested *.vst3 entries, so without this the directory-search
    // loop below would silently find nothing for a caller who passed a
    // specific bundle path (e.g. Vst3Instrument::load(path)).
#if !defined(_WIN32)
    if (dir_path.length() > 5 && dir_path.substr(dir_path.length() - 5) == ".vst3") {
        struct stat bundle_st;
        if (stat(dir_path.c_str(), &bundle_st) == 0 && S_ISDIR(bundle_st.st_mode)) {
            vst3_paths.push_back(dir_path);
            return vst3_paths;
        }
    }
#endif

#if defined(_WIN32)
    // Windows implementation
    std::string search_path = dir_path + "\\*.vst3";
    WIN32_FIND_DATAA find_data;
    HANDLE find_handle = FindFirstFileA(search_path.c_str(), &find_data);

    if (find_handle != INVALID_HANDLE_VALUE) {
        do {
            if (find_data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) {
                std::string full_path = dir_path + "\\" + find_data.cFileName;
                vst3_paths.push_back(full_path);
            }
        } while (FindNextFileA(find_handle, &find_data));
        FindClose(find_handle);
    }
#else
    // macOS/Linux implementation using dirent
    DIR* dir = opendir(dir_path.c_str());
    if (!dir) {
        return vst3_paths;
    }

    struct dirent* entry;
    while ((entry = readdir(dir)) != nullptr) {
        std::string name = entry->d_name;

        // Skip . and ..
        if (name == "." || name == "..") {
            continue;
        }

        // Check for .vst3 extension
        if (name.length() > 5 && name.substr(name.length() - 5) == ".vst3") {
            std::string full_path = dir_path + "/" + name;

            // Verify it's a directory (VST3 bundles are folders on macOS/Linux)
            struct stat st;
            if (stat(full_path.c_str(), &st) == 0 && S_ISDIR(st.st_mode)) {
                vst3_paths.push_back(full_path);
            }
        }
    }

    closedir(dir);
#endif

    return vst3_paths;
}

// Helper: Get default VST3 plugin paths for the current platform
static std::vector<std::string> get_default_vst3_paths() {
    std::vector<std::string> paths;

#if defined(__APPLE__)
    // macOS paths
    paths.push_back("/Library/Audio/Plug-Ins/VST3");

    // User library
    const char* home = getenv("HOME");
    if (home) {
        std::string user_path = std::string(home) + "/Library/Audio/Plug-Ins/VST3";
        paths.push_back(user_path);
    }

#elif defined(_WIN32)
    // Windows paths
    char common_files[MAX_PATH];
    if (SUCCEEDED(SHGetFolderPathA(NULL, CSIDL_PROGRAM_FILES_COMMON, NULL, 0, common_files))) {
        std::string path = std::string(common_files) + "\\VST3";
        paths.push_back(path);
    }

#else
    // Linux paths
    paths.push_back("/usr/lib/vst3");
    paths.push_back("/usr/local/lib/vst3");

    // User home
    const char* home = getenv("HOME");
    if (home) {
        std::string user_path = std::string(home) + "/.vst3";
        paths.push_back(user_path);
    }
#endif

    return paths;
}

// Helper: Convert VST3 UID to hex string
static std::string uid_to_string(const VST3::UID& uid) {
    return uid.toString();
}

// Helper: Determine plugin type from subcategories
static RackVST3PluginType determine_plugin_type(const std::string& subcategories) {
    if (subcategories.find("Instrument") != std::string::npos) {
        return RACK_VST3_TYPE_INSTRUMENT;
    } else if (subcategories.find("Analyzer") != std::string::npos) {
        return RACK_VST3_TYPE_ANALYZER;
    } else if (subcategories.find("Spatial") != std::string::npos) {
        return RACK_VST3_TYPE_SPATIAL;
    } else if (subcategories.find("Fx") != std::string::npos) {
        return RACK_VST3_TYPE_EFFECT;
    }
    return RACK_VST3_TYPE_OTHER;
}

// Helper: Parse a VST3 class version string (e.g. "1.0.0" or "1.2.3.4") into
// a packed uint32_t: major(8) | minor(8) | patch(8) | build(8). Shared by
// both metadata sources (moduleinfo.json and a full module load) so a class
// reports the same version number regardless of which one produced it.
static uint32_t parse_class_version(const std::string& version_str) {
    uint32_t version = 0;
    if (version_str.empty()) {
        return version;
    }

    int major = 0, minor = 0, patch = 0, build = 0;
    int parsed = sscanf(version_str.c_str(), "%d.%d.%d.%d", &major, &minor, &patch, &build);
    if (parsed >= 1) {
        // Clamp each component to valid byte range [0, 255]
        // This prevents integer overflow and handles negative/oversized values
        auto clamp_byte = [](int val) -> uint8_t {
            return static_cast<uint8_t>(std::max(0, std::min(255, val)));
        };

        uint8_t major_byte = clamp_byte(major);
        uint8_t minor_byte = clamp_byte(minor);
        uint8_t patch_byte = clamp_byte(patch);
        uint8_t build_byte = clamp_byte(build);

        version = (static_cast<uint32_t>(major_byte) << 24) |
                 (static_cast<uint32_t>(minor_byte) << 16) |
                 (static_cast<uint32_t>(patch_byte) << 8) |
                 static_cast<uint32_t>(build_byte);
    }
    return version;
}

// Helper: Fill a RackVST3PluginInfo's fixed-size fields from already-resolved
// class metadata. Shared by the moduleinfo.json fast path and the full
// Module::create() fallback below so their output is identical no matter
// which one actually produced the metadata.
static void fill_plugin_info(RackVST3PluginInfo& info,
                              const std::string& module_path,
                              const std::string& name,
                              const std::string& vendor,
                              const std::string& unique_id,
                              const std::string& version_str,
                              const std::string& subcategories) {
    strncpy(info.name, name.c_str(), sizeof(info.name) - 1);
    info.name[sizeof(info.name) - 1] = '\0';

    strncpy(info.manufacturer, vendor.c_str(), sizeof(info.manufacturer) - 1);
    info.manufacturer[sizeof(info.manufacturer) - 1] = '\0';

    strncpy(info.path, module_path.c_str(), sizeof(info.path) - 1);
    info.path[sizeof(info.path) - 1] = '\0';

    strncpy(info.unique_id, unique_id.c_str(), sizeof(info.unique_id) - 1);
    info.unique_id[sizeof(info.unique_id) - 1] = '\0';

    info.version = parse_class_version(version_str);
    info.plugin_type = determine_plugin_type(subcategories);

    strncpy(info.category, subcategories.c_str(), sizeof(info.category) - 1);
    info.category[sizeof(info.category) - 1] = '\0';
}

// Helper: join a ModuleInfo::ClassInfo's subCategories the same way
// VST3::Hosting::ClassInfo::subCategoriesString() joins its own (with "|"),
// so a class discovered via moduleinfo.json produces an identical category
// string to the same class discovered via a full module load.
static std::string join_subcategories(const std::vector<std::string>& subcategories) {
    std::string result;
    if (subcategories.empty()) {
        return result;
    }
    result = subcategories[0];
    for (size_t index = 1; index < subcategories.size(); ++index) {
        result += "|" + subcategories[index];
    }
    return result;
}

// Helper: enumerate a VST3 bundle's audio-effect classes purely from its
// moduleinfo.json, without loading the bundle's actual binary
// (CFBundleLoadExecutableAndReturnError on macOS). Loading every discovered
// bundle just to list it is what triggers macOS's "Class X is implemented in
// both A and B" objc runtime warning when several bundles share vendor-SDK
// class names -- common among JUCE-family plugins. moduleinfo.json (present
// on most modern VST3 bundles) carries the same name/vendor/class-list data
// without ever touching the binary.
//
// Returns true and fills `out_infos` with every audio-effect class found
// only if the whole bundle's moduleinfo.json exists, parses, and decodes
// cleanly. Returns false (leaving `out_infos` untouched) for a bundle
// missing a moduleinfo.json, one that fails to parse, or one containing a
// class this function can't fully decode (e.g. a malformed CID) -- in every
// one of those cases the caller must fall back to the full Module::create()
// load, so a plugin is never silently missed or reported with wrong data.
static bool try_enumerate_from_module_info(const std::string& module_path,
                                            std::vector<RackVST3PluginInfo>& out_infos) {
    auto module_info_path = Hosting::Module::getModuleInfoPath(module_path);
    if (!module_info_path) {
        return false;
    }

    std::string json_data = readFile(*module_info_path);
    if (json_data.empty()) {
        return false;
    }

    // No error stream: a malformed moduleinfo.json is an expected, silent
    // fallback case (stale or hand-edited files), not something to report --
    // printing to stderr here would defeat the whole purpose of this path.
    auto module_info = ModuleInfoLib::parseJson(json_data, nullptr);
    if (!module_info) {
        return false;
    }

    std::vector<RackVST3PluginInfo> decoded;
    for (const auto& class_info : module_info->classes) {
        if (class_info.category != kVstAudioEffectClass) {
            continue;
        }

        auto uid = VST3::UID::fromString(class_info.cid);
        if (!uid) {
            // Can't identify this class correctly -- fall back to the full
            // load for the whole bundle rather than drop just this class.
            return false;
        }

        std::string vendor = class_info.vendor;
        if (vendor.empty()) {
            vendor = module_info->factoryInfo.vendor;
        }

        RackVST3PluginInfo info{};
        fill_plugin_info(info, module_path, class_info.name, vendor, uid->toString(),
                          class_info.version, join_subcategories(class_info.subCategories));
        decoded.push_back(info);
    }

    out_infos = std::move(decoded);
    return true;
}

// ============================================================================
// Scanner Implementation
// ============================================================================

RackVST3Scanner* rack_vst3_scanner_new(void) {
    return new(std::nothrow) RackVST3Scanner();
}

void rack_vst3_scanner_free(RackVST3Scanner* scanner) {
    delete scanner;
}

int rack_vst3_scanner_add_path(RackVST3Scanner* scanner, const char* path) {
    if (!scanner || !path) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    scanner->search_paths.push_back(path);
    return RACK_VST3_OK;
}

int rack_vst3_scanner_add_default_paths(RackVST3Scanner* scanner) {
    if (!scanner) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    auto paths = get_default_vst3_paths();
    scanner->search_paths.insert(scanner->search_paths.end(), paths.begin(), paths.end());
    scanner->include_system_modules = true;
    return RACK_VST3_OK;
}

int rack_vst3_scanner_scan(RackVST3Scanner* scanner, RackVST3PluginInfo* plugins, size_t max_plugins) {
    if (!scanner) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    bool count_only = (plugins == nullptr);
    size_t count = 0;

    // Determine which paths to scan. A scanner with no paths added at all
    // (neither rack_vst3_scanner_add_path nor rack_vst3_scanner_add_default_paths
    // was ever called) falls back to the system default directories here --
    // no Rust caller in this codebase constructs a scanner this way today
    // (`Vst3Scanner::new()` always calls add_default_paths, `scan_path`'s
    // internal `new_empty()` always calls add_path), but a bare
    // rack_vst3_scanner_new() + scan() from a future or external C caller
    // should get the same "no scoping requested" completeness as
    // add_default_paths, including the system-module merge below.
    bool used_default_paths_only = scanner->search_paths.empty();
    std::vector<std::string> paths_to_scan = scanner->search_paths;
    if (used_default_paths_only) {
        paths_to_scan = get_default_vst3_paths();
    }

    // Collect all module paths by scanning directories for .vst3 bundles
    std::vector<std::string> module_paths;

    for (const auto& search_path : paths_to_scan) {
        auto found_bundles = scan_directory_for_vst3(search_path);
        module_paths.insert(module_paths.end(), found_bundles.begin(), found_bundles.end());
    }

    // Also include system-discovered modules (from getModulePaths), but only
    // for a scan seeded with the system default paths (rack_vst3_scanner_
    // add_default_paths) -- this is what gives that scan its intended, wider
    // completeness (e.g. VST3 bundles embedded in an installed .app, which a
    // plain directory walk of the default paths would miss). A scan seeded
    // only with explicit caller paths (rack_vst3_scanner_add_path, e.g. the
    // Rust `Vst3Scanner::scan_path`) must stay scoped to exactly what was
    // requested: unconditionally merging every system-registered module here
    // used to defeat that scoping entirely, making a scan of one specific
    // bundle path silently dlopen every other installed VST3 bundle on the
    // machine too.
    if (scanner->include_system_modules || used_default_paths_only) {
        auto system_modules = Hosting::Module::getModulePaths();
        module_paths.insert(module_paths.end(), system_modules.begin(), system_modules.end());
    }

    // Remove duplicates (e.g. the same plugin found via both a caller-added
    // path and the system-discovered module list above, or via two
    // overlapping rack_vst3_scanner_add_path calls). Kept unconditional
    // rather than nested inside the block above: rack_vst3_scanner_add_path
    // is a public C API a future caller could call more than once with
    // overlapping paths, with no getModulePaths() merge involved at all --
    // that path can still produce duplicate module_paths entries on its own
    // and deserves the same dedup the merged case already got.
    std::sort(module_paths.begin(), module_paths.end());
    module_paths.erase(std::unique(module_paths.begin(), module_paths.end()), module_paths.end());

    // Scan all found modules.
    //
    // Module::create()/module teardown are not thread-safe (see
    // vst3_lifecycle_mutex.h): hold the same lifecycle mutex
    // rack_vst3_plugin_new/rack_vst3_plugin_free already use, so a scan
    // running on one thread can't race a concurrent scan or plugin load
    // running on another. This is a cold, infrequent path (enumeration, not
    // audio processing), so serializing the whole loop has no real-time cost.
    std::lock_guard<std::mutex> lock(rack_vst3_module_load_mutex());

    for (const auto& module_path : module_paths) {
        // Prefer moduleinfo.json: it lists a bundle's audio-effect classes
        // without loading the bundle's binary at all. Only fall back to a
        // full Module::create() load (below) when a bundle has no usable
        // moduleinfo.json -- this is a plain listing scan, not a specific
        // plugin being loaded for real use, so the lazy path is always
        // preferred here when it's available.
        std::vector<RackVST3PluginInfo> module_info_classes;
        if (try_enumerate_from_module_info(module_path, module_info_classes)) {
            for (const auto& info : module_info_classes) {
                // If we're just counting, increment and continue
                if (count_only) {
                    count++;
                    continue;
                }

                // If array is full, continue counting but don't fill
                if (count >= max_plugins) {
                    count++;
                    continue;
                }

                plugins[count] = info;
                count++;
            }
            continue;
        }

        std::string error_description;
        auto module = Hosting::Module::create(module_path, error_description);

        if (!module) {
            // Failed to load module, skip it
            continue;
        }

        const auto& factory = module->getFactory();
        auto class_infos = factory.classInfos();

        // Enumerate all classes in this module
        for (const auto& class_info : class_infos) {
            // Only process audio effect classes
            if (class_info.category() != kVstAudioEffectClass) {
                continue;
            }

            // If we're just counting, increment and continue
            if (count_only) {
                count++;
                continue;
            }

            // If array is full, continue counting but don't fill
            if (count >= max_plugins) {
                count++;
                continue;
            }

            std::string vendor = class_info.vendor();
            if (vendor.empty()) {
                vendor = factory.info().vendor();
            }

            fill_plugin_info(plugins[count], module_path, class_info.name(), vendor,
                              uid_to_string(class_info.ID()), class_info.version(),
                              class_info.subCategoriesString());

            count++;
        }
    }

    return static_cast<int>(count);
}
