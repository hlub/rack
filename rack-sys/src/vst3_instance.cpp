#include "rack_vst3.h"
#include "public.sdk/source/vst/hosting/module.h"
#include "public.sdk/source/vst/hosting/plugprovider.h"
#include "public.sdk/source/vst/hosting/hostclasses.h"
#include "public.sdk/source/vst/hosting/processdata.h"
#include "public.sdk/source/vst/hosting/parameterchanges.h"
#include "public.sdk/source/vst/hosting/eventlist.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstprocesscontext.h"
#include "pluginterfaces/vst/ivstunits.h"
#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/vst/ivsthostapplication.h"
#include "pluginterfaces/vst/ivstmidicontrollers.h"

#include <vector>
#include <string>
#include <cstring>
#include <cmath>
#include <mutex>
#include <algorithm>

#include "vst3_lifecycle_mutex.h"

#if defined(__APPLE__)
#include <CoreFoundation/CoreFoundation.h>
static void pump_runloop(double seconds) {
    CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, false);
}
#else
static void pump_runloop(double) {}
#endif

using namespace VST3;
using namespace Steinberg;
using namespace Steinberg::Vst;

// Helper: Convert UTF-16 to UTF-8
// VST3 uses char16 (UTF-16) for strings
// Handles surrogate pairs and malformed input safely
static std::string utf16_to_utf8(const char16* utf16_str) {
    if (!utf16_str) {
        return "";
    }

    std::string result;
    result.reserve(128);

    // Safety limit to prevent infinite loops on corrupted data
    // VST3 spec limits plugin names to 256 chars
    const size_t MAX_STRING_LENGTH = 4096;
    size_t processed = 0;

    while (*utf16_str && processed < MAX_STRING_LENGTH) {
        char16 c = *utf16_str++;
        processed++;

        // UTF-16 to UTF-8 conversion with complete error handling
        if (c < 0x80) {
            // U+0000 to U+007F: ASCII range (1 byte in UTF-8)
            result.push_back(static_cast<char>(c));
        }
        else if (c < 0x800) {
            // U+0080 to U+07FF: 2 bytes in UTF-8
            result.push_back(static_cast<char>(0xC0 | (c >> 6)));
            result.push_back(static_cast<char>(0x80 | (c & 0x3F)));
        }
        else if (c >= 0xD800 && c <= 0xDBFF) {
            // High surrogate (U+D800 to U+DBFF): Start of surrogate pair
            // Must be followed by low surrogate to form valid codepoint

            // Safety check: Ensure we haven't reached end of string
            if (*utf16_str == 0) {
                // Malformed: High surrogate at end of string
                result.append("\xEF\xBF\xBD"); // UTF-8 replacement character U+FFFD
                break;
            }

            char16 low = *utf16_str;

            // Validate low surrogate range (U+DC00 to U+DFFF)
            if (low >= 0xDC00 && low <= 0xDFFF) {
                // Valid surrogate pair - decode to codepoint U+10000 to U+10FFFF
                utf16_str++;
                processed++;

                uint32_t high_bits = (c & 0x3FF);      // 10 bits from high surrogate
                uint32_t low_bits = (low & 0x3FF);     // 10 bits from low surrogate
                uint32_t codepoint = 0x10000 + (high_bits << 10) + low_bits;

                // Validate codepoint is within valid Unicode range
                // Max valid codepoint is 0x10FFFF (which is exactly 0x10000 + (0x3FF << 10) + 0x3FF)
                if (codepoint > 0x10FFFF) {
                    // Overflow - invalid codepoint (should never happen with valid surrogates)
                    result.append("\xEF\xBF\xBD"); // UTF-8 replacement character U+FFFD
                } else {
                    // Encode as 4-byte UTF-8 sequence
                    result.push_back(static_cast<char>(0xF0 | (codepoint >> 18)));
                    result.push_back(static_cast<char>(0x80 | ((codepoint >> 12) & 0x3F)));
                    result.push_back(static_cast<char>(0x80 | ((codepoint >> 6) & 0x3F)));
                    result.push_back(static_cast<char>(0x80 | (codepoint & 0x3F)));
                }
            } else {
                // Malformed: High surrogate not followed by low surrogate
                result.append("\xEF\xBF\xBD"); // UTF-8 replacement character U+FFFD
            }
        }
        else if (c >= 0xDC00 && c <= 0xDFFF) {
            // Malformed: Low surrogate (U+DC00 to U+DFFF) without preceding high surrogate
            result.append("\xEF\xBF\xBD"); // UTF-8 replacement character U+FFFD
        }
        else {
            // U+0800 to U+FFFF (excluding surrogates): 3 bytes in UTF-8
            result.push_back(static_cast<char>(0xE0 | (c >> 12)));
            result.push_back(static_cast<char>(0x80 | ((c >> 6) & 0x3F)));
            result.push_back(static_cast<char>(0x80 | (c & 0x3F)));
        }
    }

    return result;
}

// Helper: Convert UTF-8 to UTF-16, into a fixed-size char16 buffer
// (String128-sized: 128 code units including the null terminator).
// VST3's getParamValueByString takes a String128 input buffer.
// Handles multi-byte sequences and surrogate pairs for codepoints above
// U+FFFF; malformed input is replaced with U+FFFD rather than propagated,
// matching utf16_to_utf8's error-handling convention above.
static void utf8_to_utf16(const char* utf8_str, char16* out, size_t out_capacity) {
    if (!utf8_str || out_capacity == 0) {
        if (out_capacity > 0) {
            out[0] = 0;
        }
        return;
    }

    size_t written = 0;
    const unsigned char* input = reinterpret_cast<const unsigned char*>(utf8_str);

    while (*input && written + 1 < out_capacity) {
        uint32_t codepoint = 0;
        int extra_bytes = 0;

        if (*input < 0x80) {
            codepoint = *input;
            extra_bytes = 0;
        } else if ((*input & 0xE0) == 0xC0) {
            codepoint = *input & 0x1F;
            extra_bytes = 1;
        } else if ((*input & 0xF0) == 0xE0) {
            codepoint = *input & 0x0F;
            extra_bytes = 2;
        } else if ((*input & 0xF8) == 0xF0) {
            codepoint = *input & 0x07;
            extra_bytes = 3;
        } else {
            // Malformed leading byte
            out[written++] = 0xFFFD;
            input++;
            continue;
        }
        input++;

        bool valid = true;
        for (int index = 0; index < extra_bytes; index++) {
            if ((*input & 0xC0) != 0x80) {
                valid = false;
                break;
            }
            codepoint = (codepoint << 6) | (*input & 0x3F);
            input++;
        }

        if (!valid) {
            out[written++] = 0xFFFD;
            continue;
        }

        if (codepoint <= 0xFFFF) {
            out[written++] = static_cast<char16>(codepoint);
        } else if (written + 2 < out_capacity) {
            // Encode as a UTF-16 surrogate pair
            uint32_t adjusted = codepoint - 0x10000;
            out[written++] = static_cast<char16>(0xD800 + (adjusted >> 10));
            out[written++] = static_cast<char16>(0xDC00 + (adjusted & 0x3FF));
        } else {
            break;
        }
    }

    out[written] = 0;
}

// Helper: Convert hex string to UID
static bool string_to_uid(const char* str, VST3::UID& uid) {
    if (!str) {
        return false;
    }
    auto uid_opt = VST3::UID::fromString(std::string(str));
    if (!uid_opt) {
        return false;
    }
    uid = *uid_opt;
    return true;
}

// Memory stream implementation for state serialization.
//
// DECLARE_FUNKNOWN_METHODS injects a field __funknownRefCount which
// IMPLEMENT_REFCOUNT uses for addRef/release.  FUNKNOWN_CTOR initialises it
// to 1 and must be called in every constructor.  The ref_count_ field that
// was here before was a dead field (never touched by the refcount machinery);
// it has been removed.
class MemoryStream : public IBStream {
public:
    MemoryStream() : position_(0) { FUNKNOWN_CTOR }
    MemoryStream(const uint8_t* data, size_t size) : position_(0) {
        FUNKNOWN_CTOR
        buffer_.assign(data, data + size);
    }

    virtual ~MemoryStream() = default;

    // IUnknown
    DECLARE_FUNKNOWN_METHODS

    // IBStream
    tresult PLUGIN_API read(void* buffer, int32 numBytes, int32* numBytesRead) override {
        if (!buffer || numBytes < 0) {
            return kInvalidArgument;
        }

        int32 available = static_cast<int32>(buffer_.size()) - position_;
        int32 to_read = std::min(numBytes, available);

        if (to_read > 0) {
            memcpy(buffer, buffer_.data() + position_, to_read);
            position_ += to_read;
        }

        if (numBytesRead) {
            *numBytesRead = to_read;
        }

        // A short read (to_read < numBytes, e.g. hitting end-of-stream) is
        // not a failure per the IBStream contract -- the official SDK's own
        // MemoryStream::read() always returns success and lets the caller
        // check numBytesRead. Returning kResultFalse here for a short read
        // was wrong: a caller doing a conventional buffered-read loop (read
        // a fixed chunk size repeatedly until none remain) would see this
        // as an error on the final read and could abort parsing early,
        // silently leaving later fields at their pre-restore values.
        // Confirmed as the root cause of AD2 state restore silently not
        // applying individually-automated parameter values while kit
        // selection (parsed earlier in the stream) restored correctly.
        return kResultOk;
    }

    tresult PLUGIN_API write(void* buffer, int32 numBytes, int32* numBytesWritten) override {
        if (!buffer || numBytes < 0) {
            return kInvalidArgument;
        }

        // Resize buffer if needed
        if (position_ + numBytes > static_cast<int32>(buffer_.size())) {
            buffer_.resize(position_ + numBytes);
        }

        memcpy(buffer_.data() + position_, buffer, numBytes);
        position_ += numBytes;

        if (numBytesWritten) {
            *numBytesWritten = numBytes;
        }

        return kResultOk;
    }

    tresult PLUGIN_API seek(int64 pos, int32 mode, int64* result) override {
        // Calculate new position as int64 to prevent overflow/truncation
        int64 new_position = position_;

        switch (mode) {
            case kIBSeekSet:
                new_position = pos;
                break;
            case kIBSeekCur:
                new_position = static_cast<int64>(position_) + pos;
                break;
            case kIBSeekEnd:
                new_position = static_cast<int64>(buffer_.size()) + pos;
                break;
            default:
                return kInvalidArgument;
        }

        // Clamp to valid range [0, buffer_size]
        if (new_position < 0) {
            new_position = 0;
        }
        int64 buffer_size = static_cast<int64>(buffer_.size());
        if (new_position > buffer_size) {
            new_position = buffer_size;
        }

        // Safe to cast after clamping
        position_ = static_cast<int32>(new_position);

        if (result) {
            *result = position_;
        }

        return kResultOk;
    }

    tresult PLUGIN_API tell(int64* pos) override {
        if (!pos) {
            return kInvalidArgument;
        }
        *pos = position_;
        return kResultOk;
    }

    // Accessors
    const std::vector<uint8_t>& getData() const { return buffer_; }
    size_t getSize() const { return buffer_.size(); }
    void clear() { buffer_.clear(); position_ = 0; }

private:
    std::vector<uint8_t> buffer_;
    int32 position_;
};

IMPLEMENT_REFCOUNT(MemoryStream)

tresult PLUGIN_API MemoryStream::queryInterface(const TUID _iid, void** obj) {
    QUERY_INTERFACE(_iid, obj, FUnknown::iid, IBStream)
    QUERY_INTERFACE(_iid, obj, IBStream::iid, IBStream)
    *obj = nullptr;
    return kNoInterface;
}

// Minimal IComponentHandler. The VST3 spec requires the host to provide one
// via IEditController::setComponentHandler before relying on the controller
// -- some plugins' internal parameter-cache synchronization (e.g. inside
// setComponentState) may depend on a handler being present, even though we
// don't need to act on beginEdit/performEdit/endEdit/restartComponent
// ourselves in a headless host.
class ComponentHandler : public IComponentHandler {
public:
    ComponentHandler() { FUNKNOWN_CTOR }
    virtual ~ComponentHandler() = default;

    DECLARE_FUNKNOWN_METHODS

    tresult PLUGIN_API beginEdit(ParamID) override { return kResultOk; }
    tresult PLUGIN_API performEdit(ParamID, ParamValue) override { return kResultOk; }
    tresult PLUGIN_API endEdit(ParamID) override { return kResultOk; }
    tresult PLUGIN_API restartComponent(int32) override { return kResultOk; }
};

IMPLEMENT_REFCOUNT(ComponentHandler)

tresult PLUGIN_API ComponentHandler::queryInterface(const TUID _iid, void** obj) {
    QUERY_INTERFACE(_iid, obj, FUnknown::iid, IComponentHandler)
    QUERY_INTERFACE(_iid, obj, IComponentHandler::iid, IComponentHandler)
    *obj = nullptr;
    return kNoInterface;
}

// Internal plugin state
struct RackVST3Plugin {
    // Module and factory
    Hosting::Module::Ptr module;

    // Component and controller
    IPtr<IComponent> component;
    IPtr<IAudioProcessor> processor;
    IPtr<IEditController> controller;

    // Host-provided IComponentHandler, required by the VST3 spec. Kept
    // alive for the plugin's lifetime; the controller only holds a raw
    // pointer to it (set via setComponentHandler), not a counted reference.
    IPtr<IComponentHandler> component_handler;

    // Connection proxy (if component != controller)
    IPtr<IConnectionPoint> component_cp;
    IPtr<IConnectionPoint> controller_cp;

    // Plugin info
    std::string path;
    VST3::UID uid;

    // Audio configuration
    double sample_rate = 0.0;
    uint32_t max_block_size = 0;
    bool initialized = false;

    // I/O configuration
    int32 num_input_channels = 0;
    int32 num_output_channels = 0;

    // Channel count per declared audio bus, indexed by bus index, populated
    // during initialize() once every declared bus (not just bus 0) is
    // activated. output_bus_channel_counts[0] always equals
    // num_output_channels (kept in sync deliberately -- num_output_channels
    // stays the stable single-bus field existing callers already rely on via
    // rack_vst3_plugin_get_output_channels; this vector generalizes it to
    // every bus for rack_vst3_plugin_process_multi_bus's validation).
    std::vector<int32_t> input_bus_channel_counts;
    std::vector<int32_t> output_bus_channel_counts;

    // Processing structures
    HostProcessData process_data;
    ParameterChanges input_param_changes;
    ParameterChanges output_param_changes;
    EventList input_events;
    EventList output_events;

    // Process context (tells plugin it is playing at 120 BPM)
    ProcessContext process_context;
    int64 sample_position = 0;

    // Audio buffers for pointer arrays.
    // dummy_buf: a silent buffer shared across all aux output buses that we
    // don't capture. The plugin may write to these; we ignore the contents.
    std::vector<float> dummy_buf;
    std::vector<float*> input_ptrs;
    std::vector<float*> output_ptrs;

    // Parameter cache
    struct ParameterInfo {
        ParamID id;
        std::string title;
        std::string units;
        ParamValue min_value;
        ParamValue max_value;
        ParamValue default_value;
        int32 flags = 0;
        int32 step_count = 0;
    };
    std::vector<ParameterInfo> parameters;

    // Preset cache (factory presets from IUnitInfo)
    struct PresetInfo {
        int32 program_list_id;
        int32 program_index;
        std::string name;
        // Name of the IUnitInfo program list this preset belongs to (e.g. a
        // plugin could expose separate "Factory Presets" and "Drum Kits"
        // lists). Empty if the list itself has no name.
        std::string category;
    };
    std::vector<PresetInfo> presets;
};

// ============================================================================
// Plugin Instance Implementation
// ============================================================================

RackVST3Plugin* rack_vst3_plugin_new(const char* path, const char* uid) {
    if (!path || !uid) {
        return nullptr;
    }

    std::lock_guard<std::mutex> lock(rack_vst3_module_load_mutex());

    auto plugin = new(std::nothrow) RackVST3Plugin();
    if (!plugin) {
        return nullptr;
    }

    plugin->path = path;

    // Parse UID
    if (!string_to_uid(uid, plugin->uid)) {
        delete plugin;
        return nullptr;
    }

    // Load module
    std::string error_description;
    plugin->module = Hosting::Module::create(path, error_description);
    if (!plugin->module) {
        delete plugin;
        return nullptr;
    }

    // Create component
    const auto& factory = plugin->module->getFactory();
    plugin->component = factory.createInstance<IComponent>(plugin->uid);
    if (!plugin->component) {
        delete plugin;
        return nullptr;
    }

    // Get processor interface
    plugin->processor = U::cast<IAudioProcessor>(plugin->component);
    if (!plugin->processor) {
        delete plugin;
        return nullptr;
    }

    // Initialize component
    if (plugin->component->initialize(FUnknownPtr<IHostApplication>(new HostApplication())) != kResultOk) {
        // Component creation succeeded but initialization failed - no need to terminate
        // IPtr will automatically release when plugin is deleted
        plugin->component = nullptr;
        plugin->processor = nullptr;
        plugin->module = nullptr;
        delete plugin;
        return nullptr;
    }

    // Try to get edit controller
    TUID controllerCID;
    if (plugin->component->getControllerClassId(controllerCID) == kResultTrue) {
        // Controller is separate from component
        VST3::UID controllerUID = VST3::UID::fromTUID(controllerCID);

        plugin->controller = factory.createInstance<IEditController>(controllerUID);
        if (plugin->controller) {
            // Initialize controller - if this fails, clean up properly
            if (plugin->controller->initialize(FUnknownPtr<IHostApplication>(new HostApplication())) != kResultOk) {
                // Controller init failed - terminate component and clean up
                plugin->component->terminate();
                plugin->controller = nullptr;
                plugin->component = nullptr;
                plugin->processor = nullptr;
                plugin->module = nullptr;
                delete plugin;
                return nullptr;
            }
        }
    } else {
        // Component is also the controller (single component architecture)
        plugin->controller = U::cast<IEditController>(plugin->component);
    }

    // The VST3 spec requires the host to provide an IComponentHandler to
    // the controller. Some plugins' internal parameter-cache
    // synchronization (e.g. within setComponentState) may depend on one
    // being present even for a headless host that never acts on its
    // beginEdit/performEdit/endEdit/restartComponent callbacks.
    if (plugin->controller) {
        plugin->component_handler = IPtr<IComponentHandler>(new ComponentHandler(), false);
        plugin->controller->setComponentHandler(plugin->component_handler);
    }

    // Set up connection points if controller is separate
    if (plugin->controller && reinterpret_cast<void*>(plugin->controller.get()) != reinterpret_cast<void*>(plugin->component.get())) {
        plugin->component_cp = U::cast<IConnectionPoint>(plugin->component);
        plugin->controller_cp = U::cast<IConnectionPoint>(plugin->controller);

        if (plugin->component_cp && plugin->controller_cp) {
            plugin->component_cp->connect(plugin->controller_cp);
            plugin->controller_cp->connect(plugin->component_cp);
        }
    }

    return plugin;
}

void rack_vst3_plugin_free(RackVST3Plugin* plugin) {
    if (!plugin) {
        return;
    }

    std::lock_guard<std::mutex> lock(rack_vst3_module_load_mutex());

    // Deactivate if active
    if (plugin->initialized && plugin->component) {
        plugin->component->setActive(false);
    }

    // Disconnect connection points
    if (plugin->component_cp && plugin->controller_cp) {
        plugin->component_cp->disconnect(plugin->controller_cp);
        plugin->controller_cp->disconnect(plugin->component_cp);
    }

    // Terminate controller
    if (plugin->controller && reinterpret_cast<void*>(plugin->controller.get()) != reinterpret_cast<void*>(plugin->component.get())) {
        plugin->controller->terminate();
        plugin->controller = nullptr;
    }

    // Terminate component
    if (plugin->component) {
        plugin->component->terminate();
        plugin->component = nullptr;
    }

    plugin->processor = nullptr;
    plugin->module = nullptr;

    delete plugin;
}

int rack_vst3_plugin_initialize(RackVST3Plugin* plugin, double sample_rate, uint32_t max_block_size,
                                const uint8_t* initial_state, size_t initial_state_size) {
    if (!plugin || !plugin->component || !plugin->processor) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    std::lock_guard<std::mutex> lock(rack_vst3_module_load_mutex());

    plugin->sample_rate = sample_rate;
    plugin->max_block_size = max_block_size;

    // Setup processing with 32-bit float samples in realtime mode
    ProcessSetup setup;
    setup.processMode = kRealtime;
    setup.symbolicSampleSize = kSample32;
    setup.maxSamplesPerBlock = max_block_size;
    setup.sampleRate = sample_rate;

    if (plugin->processor->setupProcessing(setup) != kResultOk) {
        return RACK_VST3_ERROR_GENERIC;
    }

    // Get bus configuration
    int32 numInputBuses = plugin->component->getBusCount(kAudio, kInput);
    int32 numOutputBuses = plugin->component->getBusCount(kAudio, kOutput);

    // Activate every declared audio bus (not just bus 0/"main"), so a plugin
    // like AD2's per-instrument output buses actually produce audio when a
    // caller later binds real buffers to them via
    // rack_vst3_plugin_process_multi_bus. A plugin declaring zero buses in a
    // direction (numInputBuses/numOutputBuses == 0) simply runs these loops
    // zero times -- activateBus is never called and the corresponding
    // *_bus_channel_counts vector stays empty, same as today's num_input_
    // channels/num_output_channels staying at their 0 default in that case.
    // activateBus's return value is intentionally not checked here, matching
    // this function's existing convention (the event-bus activation loop
    // below, and the previous single-bus activation this replaces, both
    // already ignored it) -- getBusInfo is queried unconditionally regardless
    // of whether activation reports success, so a channel count is still
    // recorded even if a given bus somehow fails to activate.
    plugin->input_bus_channel_counts.clear();
    plugin->input_bus_channel_counts.reserve(numInputBuses);
    for (int32 b = 0; b < numInputBuses; ++b) {
        plugin->component->activateBus(kAudio, kInput, b, true);
        BusInfo busInfo;
        int32 channelCount = 0;
        if (plugin->component->getBusInfo(kAudio, kInput, b, busInfo) == kResultOk) {
            channelCount = busInfo.channelCount;
        }
        plugin->input_bus_channel_counts.push_back(channelCount);
        if (b == 0) {
            plugin->num_input_channels = channelCount;
        }
    }

    plugin->output_bus_channel_counts.clear();
    plugin->output_bus_channel_counts.reserve(numOutputBuses);
    for (int32 b = 0; b < numOutputBuses; ++b) {
        plugin->component->activateBus(kAudio, kOutput, b, true);
        BusInfo busInfo;
        int32 channelCount = 0;
        if (plugin->component->getBusInfo(kAudio, kOutput, b, busInfo) == kResultOk) {
            channelCount = busInfo.channelCount;
        }
        plugin->output_bus_channel_counts.push_back(channelCount);
        if (b == 0) {
            plugin->num_output_channels = channelCount;
        }
    }

    // Activate MIDI/event input bus so the plugin receives note events.
    int32 numEventInputBuses = plugin->component->getBusCount(kEvent, kInput);
    for (int32 i = 0; i < numEventInputBuses; ++i) {
        plugin->component->activateBus(kEvent, kInput, i, true);
    }

    // Activate every MIDI/event OUTPUT bus so a plugin that emits its own MIDI
    // (e.g. an arpeggiator or MIDI-FX plugin) actually populates
    // process_data.outputEvents during process(). Without this, a declared
    // event-out bus stays inactive and never produces events, so
    // rack_vst3_plugin_process_multi_bus's output-MIDI capture would always be
    // empty. Symmetric to the event-input-bus activation just above.
    int32 numEventOutputBuses = plugin->component->getBusCount(kEvent, kOutput);
    for (int32 i = 0; i < numEventOutputBuses; ++i) {
        plugin->component->activateBus(kEvent, kOutput, i, true);
    }

    // Activate component
    if (plugin->component->setActive(true) != kResultOk) {
        return RACK_VST3_ERROR_GENERIC;
    }

    // Apply initial state after setActive but before setProcessing.
    //
    // AD2's setState spawns a background sample-loading thread.  That thread
    // needs the plugin to be in "Active" state so its audio system is ready to
    // receive the loaded samples.  Calling setState before setActive causes the
    // thread to crash.  Calling it in "Active" state (here) is safe.
    // We pump the Cocoa run loop for 5 s to let the async load finish before
    // setProcessing(true) starts the audio callback.
    if (initial_state && initial_state_size > sizeof(uint32_t)) {
        uint32_t component_state_size = 0;
        memcpy(&component_state_size, initial_state, sizeof(component_state_size));

        if (sizeof(component_state_size) + component_state_size <= initial_state_size) {
            const uint8_t* comp_data = initial_state + sizeof(component_state_size);
            IPtr<MemoryStream> comp_stream(new MemoryStream(comp_data, component_state_size), false);
            plugin->component->setState(comp_stream);

            if (plugin->controller) {
                void* comp_ptr = reinterpret_cast<void*>(plugin->component.get());
                void* ctrl_ptr = reinterpret_cast<void*>(plugin->controller.get());
                if (comp_ptr != ctrl_ptr) {
                    size_t ctrl_offset = sizeof(component_state_size) + component_state_size;
                    if (ctrl_offset < initial_state_size) {
                        const uint8_t* ctrl_data = initial_state + ctrl_offset;
                        size_t ctrl_size = initial_state_size - ctrl_offset;
                        IPtr<MemoryStream> ctrl_stream(new MemoryStream(ctrl_data, ctrl_size), false);
                        plugin->controller->setState(ctrl_stream);
                    }
                }
            }
        }

        pump_runloop(5.0);
    }

    // Start processing
    if (plugin->processor->setProcessing(true) != kResultOk) {
        plugin->component->setActive(false);
        return RACK_VST3_ERROR_GENERIC;
    }

    // Prepare process_data with 0 samples so rack does NOT own the buffer
    // pointers. We manage our own buffers to avoid heap corruption on cleanup
    // (if prepare allocates buffers and we replace channelBuffers32 with Rust
    // pointers, the destructor would try to free our Rust memory).
    plugin->process_data.prepare(*plugin->component, 0, kSample32);

    // Allocate a silent dummy buffer for aux output buses we don't capture.
    plugin->dummy_buf.assign(max_block_size, 0.0f);
    float* dummy_ptr = plugin->dummy_buf.data();

    // Point all output buses at the dummy buffer by default.
    for (int32 b = 0; b < plugin->process_data.numOutputs; ++b) {
        AudioBusBuffers& bus = plugin->process_data.outputs[b];
        for (int32 ch = 0; ch < bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = dummy_ptr;
        }
    }

    // Set up process context so the plugin knows it is in playback at 120 BPM.
    memset(&plugin->process_context, 0, sizeof(ProcessContext));
    plugin->process_context.state =
        ProcessContext::kPlaying |
        ProcessContext::kTempoValid |
        ProcessContext::kTimeSigValid |
        ProcessContext::kSmpteValid |
        ProcessContext::kClockValid;
    plugin->process_context.sampleRate = sample_rate;
    plugin->process_context.tempo = 120.0;
    plugin->process_context.timeSigNumerator = 4;
    plugin->process_context.timeSigDenominator = 4;
    plugin->process_data.processContext = &plugin->process_context;

    // Build parameter cache; log any program-change-flagged parameters.
    if (plugin->controller) {
        int32 param_count = plugin->controller->getParameterCount();
        plugin->parameters.clear();
        plugin->parameters.reserve(param_count);

        for (int32 i = 0; i < param_count; ++i) {
            ParameterInfo vst3_param_info;
            if (plugin->controller->getParameterInfo(i, vst3_param_info) == kResultOk) {
                RackVST3Plugin::ParameterInfo info;
                info.id = vst3_param_info.id;

                // Convert UTF-16 to UTF-8 (proper conversion for international characters)
                info.title = utf16_to_utf8(vst3_param_info.title);
                info.units = utf16_to_utf8(vst3_param_info.units);

                // VST3 parameters are already normalized 0.0-1.0
                info.min_value = 0.0;
                info.max_value = 1.0;
                info.default_value = vst3_param_info.defaultNormalizedValue;
                info.flags = vst3_param_info.flags;
                info.step_count = vst3_param_info.stepCount;

                plugin->parameters.push_back(info);
            }
        }

        // Query IMidiMapping to find which parameter handles MIDI Program Change.
    }

    // Enumerate factory presets if available
    IPtr<IUnitInfo> unit_info = U::cast<IUnitInfo>(plugin->controller);
    if (unit_info) {
        int32 program_list_count = unit_info->getProgramListCount();
        for (int32 i = 0; i < program_list_count; ++i) {
            ProgramListInfo list_info;
            if (unit_info->getProgramListInfo(i, list_info) == kResultOk) {
                std::string list_name = utf16_to_utf8(list_info.name);
                // Enumerate programs in this list
                for (int32 j = 0; j < list_info.programCount; ++j) {
                    String128 program_name;
                    if (unit_info->getProgramName(list_info.id, j, program_name) == kResultOk) {
                        RackVST3Plugin::PresetInfo preset;
                        preset.program_list_id = list_info.id;
                        preset.program_index = j;
                        preset.name = utf16_to_utf8(program_name);
                        preset.category = list_name;
                        plugin->presets.push_back(preset);
                    }
                }
            }
        }
    }

    plugin->initialized = true;
    return RACK_VST3_OK;
}

int rack_vst3_plugin_is_initialized(RackVST3Plugin* plugin) {
    return (plugin && plugin->initialized) ? 1 : 0;
}

int rack_vst3_plugin_reset(RackVST3Plugin* plugin) {
    // Check null pointer BEFORE acquiring mutex (no lock needed if null)
    if (!plugin) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // Acquire mutex BEFORE checking state to prevent TOCTOU race condition
    // Without this, another thread could change initialized between check and lock
    std::lock_guard<std::mutex> lock(rack_vst3_module_load_mutex());

    if (!plugin->initialized || !plugin->component) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // VST3 doesn't have a direct "reset" like AudioUnit
    // We can deactivate and reactivate the component
    plugin->component->setActive(false);
    if (plugin->component->setActive(true) != kResultOk) {
        return RACK_VST3_ERROR_GENERIC;
    }

    return RACK_VST3_OK;
}

int rack_vst3_plugin_get_input_channels(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->initialized) {
        return 0;
    }
    return plugin->num_input_channels;
}

int rack_vst3_plugin_get_output_channels(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->initialized) {
        return 0;
    }
    return plugin->num_output_channels;
}

int rack_vst3_plugin_get_output_bus_count(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->component) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    return plugin->component->getBusCount(kAudio, kOutput);
}

int rack_vst3_plugin_get_input_bus_count(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->component) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    return plugin->component->getBusCount(kAudio, kInput);
}

bool rack_vst3_plugin_emits_midi(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->component) {
        return false;
    }
    return plugin->component->getBusCount(kEvent, kOutput) > 0;
}

// Shared implementation for rack_vst3_plugin_get_{output,input}_bus_info:
// looks up BusInfo for `bus_index` on the given direction and copies its
// channel count / UTF-8 name out to the caller-supplied buffers. Read-only:
// does not activate the bus.
static int get_bus_info(
    RackVST3Plugin* plugin,
    BusDirection direction,
    int32_t bus_index,
    uint32_t* channels_out,
    char* name_out,
    size_t name_out_size)
{
    if (!plugin || !plugin->component) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    int32 bus_count = plugin->component->getBusCount(kAudio, direction);
    if (bus_index < 0 || bus_index >= bus_count) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    BusInfo bus_info;
    if (plugin->component->getBusInfo(kAudio, direction, bus_index, bus_info) != kResultOk) {
        return RACK_VST3_ERROR_GENERIC;
    }

    if (channels_out) {
        *channels_out = static_cast<uint32_t>(bus_info.channelCount);
    }

    if (name_out && name_out_size > 0) {
        std::string name_utf8 = utf16_to_utf8(bus_info.name);
        size_t copy_len = std::min(name_utf8.size(), name_out_size - 1);
        memcpy(name_out, name_utf8.data(), copy_len);
        name_out[copy_len] = '\0';
    }

    return RACK_VST3_OK;
}

int rack_vst3_plugin_get_output_bus_info(
    RackVST3Plugin* plugin,
    int32_t bus_index,
    uint32_t* channels_out,
    char* name_out,
    size_t name_out_size)
{
    return get_bus_info(plugin, kOutput, bus_index, channels_out, name_out, name_out_size);
}

int rack_vst3_plugin_get_input_bus_info(
    RackVST3Plugin* plugin,
    int32_t bus_index,
    uint32_t* channels_out,
    char* name_out,
    size_t name_out_size)
{
    return get_bus_info(plugin, kInput, bus_index, channels_out, name_out, name_out_size);
}

int rack_vst3_plugin_process(
    RackVST3Plugin* plugin,
    const float* const* inputs,
    uint32_t num_input_channels,
    float* const* outputs,
    uint32_t num_output_channels,
    uint32_t frames)
{
    if (!plugin || !plugin->initialized || !plugin->processor) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // Validate input parameters to prevent buffer overruns
    // Channel counts must match what was configured during initialization
    if (num_input_channels != static_cast<uint32_t>(plugin->num_input_channels)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (num_output_channels != static_cast<uint32_t>(plugin->num_output_channels)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    // Frame count must not exceed the max block size configured during initialization
    if (frames > plugin->max_block_size) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Validate buffer pointers when channel counts > 0
    if (num_input_channels > 0 && !inputs) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (num_output_channels > 0 && !outputs) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Update dynamic fields only (prepare() was called during initialization)
    plugin->process_data.numSamples = frames;

    // Advance the timeline position so the plugin sees a moving clock.
    plugin->process_context.projectTimeSamples = plugin->sample_position;
    plugin->sample_position += frames;

    // Set input buffers — write pointers INTO the prepare()-allocated float**
    // array rather than replacing the array pointer. This avoids freeing
    // Rust-owned memory in HostProcessData::destroyBuffers().
    if (num_input_channels > 0 && inputs) {
        AudioBusBuffers& bus = plugin->process_data.inputs[0];
        for (uint32_t ch = 0; ch < num_input_channels && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = const_cast<float*>(inputs[ch]);
        }
    }

    // Set output buffers the same way.
    if (num_output_channels > 0 && outputs) {
        AudioBusBuffers& bus = plugin->process_data.outputs[0];
        for (uint32_t ch = 0; ch < num_output_channels && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = outputs[ch];
        }
    }

    // Set parameter and event interfaces
    plugin->process_data.inputParameterChanges = &plugin->input_param_changes;
    plugin->process_data.outputParameterChanges = &plugin->output_param_changes;
    plugin->process_data.inputEvents = &plugin->input_events;
    plugin->process_data.outputEvents = &plugin->output_events;

    // Process
    tresult result = plugin->processor->process(plugin->process_data);

    // Clear input/output events and parameter changes for next call
    plugin->input_events.clear();
    plugin->input_param_changes.clearQueue();
    plugin->output_events.clear();
    plugin->output_param_changes.clearQueue();

    return (result == kResultOk) ? RACK_VST3_OK : RACK_VST3_ERROR_GENERIC;
}

// Scale a VST3 normalized [0.0, 1.0] value to a MIDI 1.0 data byte [0, 127],
// the exact inverse of send_midi's `data2 / 127.0f` encode. Rounds to nearest
// and clamps, so a normalized 1.0 velocity maps back to 127.
static uint8_t normalized_to_midi_data(float normalized) {
    float scaled = std::round(normalized * 127.0f);
    if (scaled < 0.0f) {
        scaled = 0.0f;
    } else if (scaled > 127.0f) {
        scaled = 127.0f;
    }
    return static_cast<uint8_t>(scaled);
}

// Decode a single VST3 output Event into a RackVST3MidiEvent, the exact
// inverse of the encode in rack_vst3_plugin_send_midi. Pure: it reads only the
// given event and touches no plugin state, so it can be unit-tested against a
// hand-built synthetic Event without a real loaded plugin. Returns true and
// fills `out` for a MIDI-representable event; returns false (leaving `out`
// otherwise untouched) for VST3 event types with no MIDI 1.0 equivalent (e.g.
// note-expression, chord, or scale events), which the caller should skip.
//
// Deliberately NOT static: rack-sys's C++ unit test links against this symbol
// directly to exercise the decode without a loaded plugin.
bool vst3_event_to_rack_midi(const Event& event, RackVST3MidiEvent* out) {
    if (!out) {
        return false;
    }

    // sampleOffset is a non-negative frame offset; clamp any stray negative.
    out->sample_offset =
        event.sampleOffset > 0 ? static_cast<uint32_t>(event.sampleOffset) : 0u;

    switch (event.type) {
        case Event::kNoteOnEvent:
            out->status = 0x90;
            out->channel = static_cast<uint8_t>(event.noteOn.channel);
            out->data1 = static_cast<uint8_t>(event.noteOn.pitch);
            out->data2 = normalized_to_midi_data(event.noteOn.velocity);
            return true;

        case Event::kNoteOffEvent:
            out->status = 0x80;
            out->channel = static_cast<uint8_t>(event.noteOff.channel);
            out->data1 = static_cast<uint8_t>(event.noteOff.pitch);
            out->data2 = normalized_to_midi_data(event.noteOff.velocity);
            return true;

        case Event::kPolyPressureEvent:
            out->status = 0xA0;
            out->channel = static_cast<uint8_t>(event.polyPressure.channel);
            out->data1 = static_cast<uint8_t>(event.polyPressure.pitch);
            out->data2 = normalized_to_midi_data(event.polyPressure.pressure);
            return true;

        case Event::kLegacyMIDICCOutEvent: {
            // Inverse of send_midi's controlNumber convention: control numbers
            // below 0x80 are real CC; 0x80/0x81/0x82 encode Program Change /
            // Channel Pressure / Pitch Bend respectively.
            const LegacyMIDICCOutEvent& cc = event.midiCCOut;
            out->channel = static_cast<uint8_t>(cc.channel);
            const uint8_t value = static_cast<uint8_t>(cc.value);
            const uint8_t value2 = static_cast<uint8_t>(cc.value2);
            if (cc.controlNumber < 0x80) {
                out->status = 0xB0;               // Control Change
                out->data1 = cc.controlNumber;
                out->data2 = value;
                return true;
            }
            switch (cc.controlNumber) {
                case 0x80:                        // Program Change
                    out->status = 0xC0;
                    out->data1 = value;
                    out->data2 = 0;
                    return true;
                case 0x81:                        // Channel Pressure
                    out->status = 0xD0;
                    out->data1 = value;
                    out->data2 = 0;
                    return true;
                case 0x82:                        // Pitch Bend (LSB, MSB)
                    out->status = 0xE0;
                    out->data1 = value;           // LSB
                    out->data2 = value2;          // MSB
                    return true;
                default:
                    return false;                 // unknown non-CC encoding
            }
        }

        default:
            return false;                         // no MIDI 1.0 equivalent
    }
}

int rack_vst3_plugin_process_multi_bus(
    RackVST3Plugin* plugin,
    const float* const* inputs,
    uint32_t num_input_channels,
    float* const* outputs,
    uint32_t num_output_channels,
    uint32_t frames,
    const RackVST3ExtraOutputBus* extra_output_buses,
    size_t num_extra_output_buses,
    const RackVST3ExtraInputBus* extra_input_buses,
    size_t num_extra_input_buses,
    RackVST3MidiEvent* output_events_out,
    size_t output_events_capacity,
    size_t* num_output_events_out)
{
    if (!plugin || !plugin->initialized || !plugin->processor) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // Validate bus-0 (main) parameters exactly as rack_vst3_plugin_process does.
    if (num_input_channels != static_cast<uint32_t>(plugin->num_input_channels)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (num_output_channels != static_cast<uint32_t>(plugin->num_output_channels)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (frames > plugin->max_block_size) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (num_input_channels > 0 && !inputs) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (num_output_channels > 0 && !outputs) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Validate every requested extra output bus up front, before touching any
    // process_data state, so a bad request never leaves buffer pointers
    // partially rebound. bus_index == 0 is rejected here -- that bus is
    // already handled by the outputs/num_output_channels parameters above.
    if (num_extra_output_buses > 0 && !extra_output_buses) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    for (size_t i = 0; i < num_extra_output_buses; ++i) {
        const RackVST3ExtraOutputBus& extra = extra_output_buses[i];
        if (extra.bus_index <= 0 ||
            static_cast<size_t>(extra.bus_index) >= plugin->output_bus_channel_counts.size()) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
        if (extra.channel_count !=
            static_cast<uint32_t>(plugin->output_bus_channel_counts[static_cast<size_t>(extra.bus_index)])) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
        if (extra.channel_count > 0 && !extra.channels) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
    }

    // Validate every requested extra input bus up front, same discipline as
    // extra output buses above: bus_index == 0 is rejected (already handled
    // by inputs/num_input_channels), bus_index must be in range against
    // input_bus_channel_counts, channel_count must match exactly, and no
    // bus_index may repeat -- a duplicate would rebind the same
    // process_data.inputs[bus_index] slot twice, which is harmless here but
    // is rejected anyway for symmetry with the Rust-layer scratch-buffer
    // safety requirement (see Vst3Plugin::process_multi_bus).
    if (num_extra_input_buses > 0 && !extra_input_buses) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    for (size_t i = 0; i < num_extra_input_buses; ++i) {
        const RackVST3ExtraInputBus& extra = extra_input_buses[i];
        if (extra.bus_index <= 0 ||
            static_cast<size_t>(extra.bus_index) >= plugin->input_bus_channel_counts.size()) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
        if (extra.channel_count !=
            static_cast<uint32_t>(plugin->input_bus_channel_counts[static_cast<size_t>(extra.bus_index)])) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
        if (extra.channel_count > 0 && !extra.channels) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
        for (size_t j = 0; j < i; ++j) {
            if (extra_input_buses[j].bus_index == extra.bus_index) {
                return RACK_VST3_ERROR_INVALID_PARAM;
            }
        }
    }

    // Update dynamic fields only (prepare() was called during initialization)
    plugin->process_data.numSamples = frames;

    // Advance the timeline position so the plugin sees a moving clock.
    plugin->process_context.projectTimeSamples = plugin->sample_position;
    plugin->sample_position += frames;

    // Set input buffers — write pointers INTO the prepare()-allocated float**
    // array rather than replacing the array pointer. This avoids freeing
    // Rust-owned memory in HostProcessData::destroyBuffers().
    if (num_input_channels > 0 && inputs) {
        AudioBusBuffers& bus = plugin->process_data.inputs[0];
        for (uint32_t ch = 0; ch < num_input_channels && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = const_cast<float*>(inputs[ch]);
        }
    }

    // Set bus-0 (main) output buffers the same way.
    if (num_output_channels > 0 && outputs) {
        AudioBusBuffers& bus = plugin->process_data.outputs[0];
        for (uint32_t ch = 0; ch < num_output_channels && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = outputs[ch];
        }
    }

    // Rebind each requested extra output bus's channel pointers -- same
    // pointer-rebind pattern as bus 0 above, generalized to an arbitrary bus
    // index. Fixed-size loops over caller-supplied arrays only, no
    // allocation: real-time-safe.
    for (size_t i = 0; i < num_extra_output_buses; ++i) {
        const RackVST3ExtraOutputBus& extra = extra_output_buses[i];
        AudioBusBuffers& bus = plugin->process_data.outputs[extra.bus_index];
        for (uint32_t ch = 0; ch < extra.channel_count && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = extra.channels[ch];
        }
    }

    // Rebind each requested extra input bus's channel pointers -- mirrors
    // bus 0's own input rebind above, generalized to an arbitrary bus index.
    // `const_cast` here matches how bus 0's input is rebound above: VST3's
    // `AudioBusBuffers::channelBuffers32` doesn't distinguish const for
    // inputs at the struct level, even though these buffers are logically
    // read-only from the host's perspective (`extra.channels` is
    // `const float* const*`).
    for (size_t i = 0; i < num_extra_input_buses; ++i) {
        const RackVST3ExtraInputBus& extra = extra_input_buses[i];
        AudioBusBuffers& bus = plugin->process_data.inputs[extra.bus_index];
        for (uint32_t ch = 0; ch < extra.channel_count && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = const_cast<float*>(extra.channels[ch]);
        }
    }

    // Set parameter and event interfaces
    plugin->process_data.inputParameterChanges = &plugin->input_param_changes;
    plugin->process_data.outputParameterChanges = &plugin->output_param_changes;
    plugin->process_data.inputEvents = &plugin->input_events;
    plugin->process_data.outputEvents = &plugin->output_events;

    // Process. A single VST3 process() call computes every activated bus at
    // once, so this one call produces bus 0's audio plus every extra bus
    // rebound above.
    tresult result = plugin->processor->process(plugin->process_data);

    // Capture the plugin's own emitted MIDI output events (e.g. from an
    // arpeggiator) BEFORE they are cleared below. Skipped entirely when the
    // caller passes NULL for output_events_out, so audio-only callers see
    // byte-for-byte identical behavior. `produced` counts every decodable
    // event (the true total), even past output_events_capacity, so the caller
    // can detect truncation; only the first output_events_capacity are written.
    size_t produced = 0;
    if (output_events_out) {
        int32 event_count = plugin->output_events.getEventCount();
        for (int32 i = 0; i < event_count; ++i) {
            Event vst3_event;
            if (plugin->output_events.getEvent(i, vst3_event) != kResultOk) {
                continue;
            }
            RackVST3MidiEvent decoded;
            if (!vst3_event_to_rack_midi(vst3_event, &decoded)) {
                continue;
            }
            if (produced < output_events_capacity) {
                output_events_out[produced] = decoded;
            }
            ++produced;
        }
    }
    if (num_output_events_out) {
        *num_output_events_out = produced;
    }

    // Clear input/output events and parameter changes for next call
    plugin->input_events.clear();
    plugin->input_param_changes.clearQueue();
    plugin->output_events.clear();
    plugin->output_param_changes.clearQueue();

    return (result == kResultOk) ? RACK_VST3_OK : RACK_VST3_ERROR_GENERIC;
}

// ============================================================================
// Parameter API
// ============================================================================

int rack_vst3_plugin_parameter_count(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->controller) {
        return 0;
    }
    return static_cast<int>(plugin->parameters.size());
}

int rack_vst3_plugin_get_parameter(RackVST3Plugin* plugin, uint32_t index, float* value) {
    if (!plugin || !plugin->controller || !value) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->parameters.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    ParamID param_id = plugin->parameters[index].id;
    ParamValue normalized = plugin->controller->getParamNormalized(param_id);
    *value = static_cast<float>(normalized);

    return RACK_VST3_OK;
}

int rack_vst3_plugin_set_parameter(RackVST3Plugin* plugin, uint32_t index, float value) {
    if (!plugin || !plugin->controller) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->parameters.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Clamp normalized value to 0.0-1.0
    // VST3 uses normalized parameter values (0.0-1.0 range)
    // This matches AudioUnit behavior for consistency across plugin formats
    if (value < 0.0f) value = 0.0f;
    if (value > 1.0f) value = 1.0f;

    ParamID param_id = plugin->parameters[index].id;

    // Set parameter value on controller (for UI reflection)
    plugin->controller->setParamNormalized(param_id, value);

    // Queue parameter change for component notification in next process() call
    // This ensures the component receives the parameter change properly
    int32 queue_index = 0;
    IParamValueQueue* queue = plugin->input_param_changes.addParameterData(param_id, queue_index);
    if (queue) {
        // Add parameter change at sample offset 0 (beginning of next buffer)
        int32 point_index = 0;
        queue->addPoint(0, value, point_index);
    }

    return RACK_VST3_OK;
}

int rack_vst3_plugin_set_parameter_by_id(RackVST3Plugin* plugin, uint32_t param_id, float value) {
    if (!plugin || !plugin->controller) return RACK_VST3_ERROR_INVALID_PARAM;
    if (value < 0.0f) value = 0.0f;
    if (value > 1.0f) value = 1.0f;
    plugin->controller->setParamNormalized(param_id, value);
    int32 queue_index = 0;
    IParamValueQueue* queue = plugin->input_param_changes.addParameterData(param_id, queue_index);
    if (queue) {
        int32 point_index = 0;
        queue->addPoint(0, value, point_index);
    }
    return RACK_VST3_OK;
}

uint32_t rack_vst3_plugin_get_midi_pc_param_id(RackVST3Plugin* plugin, int channel) {
    if (!plugin || !plugin->controller) return kNoParamId;
    IPtr<IMidiMapping> midiMapping = U::cast<IMidiMapping>(plugin->controller);
    if (!midiMapping) return kNoParamId;
    ParamID pid = kNoParamId;
    midiMapping->getMidiControllerAssignment(0, channel, kCtrlProgramChange, pid);
    return pid;
}

int rack_vst3_plugin_parameter_info(
    RackVST3Plugin* plugin,
    uint32_t index,
    char* name,
    size_t name_size,
    float* min,
    float* max,
    float* default_value,
    char* unit,
    size_t unit_size,
    int32_t* step_count,
    int32_t* flags)
{
    if (!plugin || !plugin->controller || !name || name_size == 0) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->parameters.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    const auto& param_info = plugin->parameters[index];

    // Name
    strncpy(name, param_info.title.c_str(), name_size - 1);
    name[name_size - 1] = '\0';

    // Min/max/default
    if (min) *min = static_cast<float>(param_info.min_value);
    if (max) *max = static_cast<float>(param_info.max_value);
    if (default_value) *default_value = static_cast<float>(param_info.default_value);

    // Unit
    if (unit && unit_size > 0) {
        strncpy(unit, param_info.units.c_str(), unit_size - 1);
        unit[unit_size - 1] = '\0';
    }

    if (step_count) *step_count = param_info.step_count;
    if (flags) *flags = param_info.flags;

    return RACK_VST3_OK;
}

int rack_vst3_plugin_parameter_string_by_value(
    RackVST3Plugin* plugin,
    uint32_t index,
    float normalized_value,
    char* out,
    size_t out_size)
{
    if (!plugin || !plugin->controller || !out || out_size == 0) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->parameters.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    const auto& param_info = plugin->parameters[index];

    String128 display;
    tresult result = plugin->controller->getParamStringByValue(
        param_info.id, static_cast<ParamValue>(normalized_value), display);
    if (result != kResultOk) {
        return RACK_VST3_ERROR_GENERIC;
    }

    std::string display_utf8 = utf16_to_utf8(display);
    strncpy(out, display_utf8.c_str(), out_size - 1);
    out[out_size - 1] = '\0';

    return RACK_VST3_OK;
}

int rack_vst3_plugin_parameter_plain_value(
    RackVST3Plugin* plugin,
    uint32_t index,
    float normalized_value,
    double* out_plain)
{
    if (!plugin || !plugin->controller || !out_plain) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->parameters.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    const auto& param_info = plugin->parameters[index];

    // normalizedParamToPlain has no documented failure return in the VST3
    // SDK (unlike getParamStringByValue); it always returns a ParamValue.
    // Some plugins return the normalized value unchanged when no real
    // plain-value mapping is implemented, which is itself informative.
    *out_plain = plugin->controller->normalizedParamToPlain(
        param_info.id, static_cast<ParamValue>(normalized_value));

    return RACK_VST3_OK;
}

int rack_vst3_plugin_parameter_value_by_string(
    RackVST3Plugin* plugin,
    uint32_t index,
    const char* string,
    float* out_normalized)
{
    if (!plugin || !plugin->controller || !string || !out_normalized) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->parameters.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    const auto& param_info = plugin->parameters[index];

    String128 utf16_string;
    utf8_to_utf16(string, utf16_string, 128);

    ParamValue normalized = 0.0;
    tresult result = plugin->controller->getParamValueByString(
        param_info.id, utf16_string, normalized);
    if (result != kResultOk) {
        return RACK_VST3_ERROR_GENERIC;
    }

    *out_normalized = static_cast<float>(normalized);
    return RACK_VST3_OK;
}

int rack_vst3_plugin_parameter_normalized_from_plain(
    RackVST3Plugin* plugin,
    uint32_t index,
    double plain_value,
    float* out_normalized)
{
    if (!plugin || !plugin->controller || !out_normalized) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->parameters.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    const auto& param_info = plugin->parameters[index];

    // plainParamToNormalized has no documented failure return in the VST3
    // SDK (same as its inverse, normalizedParamToPlain above) -- it always
    // returns a ParamValue.
    *out_normalized = static_cast<float>(plugin->controller->plainParamToNormalized(
        param_info.id, static_cast<ParamValue>(plain_value)));

    return RACK_VST3_OK;
}

// ============================================================================
// Preset Management (Stub - TODO: Implement)
// ============================================================================

int rack_vst3_plugin_get_preset_count(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->initialized) {
        return 0;
    }
    return static_cast<int>(plugin->presets.size());
}

int rack_vst3_plugin_get_preset_info(
    RackVST3Plugin* plugin,
    uint32_t index,
    char* name,
    size_t name_size,
    char* category,
    size_t category_size,
    int32_t* preset_number)
{
    if (!plugin || !plugin->initialized || !name || name_size == 0 ||
        !category || category_size == 0) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->presets.size()) {
        return RACK_VST3_ERROR_NOT_FOUND;
    }

    const auto& preset = plugin->presets[index];

    // Copy name
    strncpy(name, preset.name.c_str(), name_size - 1);
    name[name_size - 1] = '\0';

    // Copy category (empty string means "no category")
    strncpy(category, preset.category.c_str(), category_size - 1);
    category[category_size - 1] = '\0';

    // Preset number is just the index
    if (preset_number) {
        *preset_number = static_cast<int32_t>(index);
    }

    return RACK_VST3_OK;
}

int rack_vst3_plugin_load_preset(RackVST3Plugin* plugin, int32_t preset_number) {
    if (!plugin || !plugin->initialized || !plugin->controller) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    if (preset_number < 0 || preset_number >= static_cast<int32_t>(plugin->presets.size())) {
        return RACK_VST3_ERROR_NOT_FOUND;
    }

    const auto& preset = plugin->presets[preset_number];

    // VST3 preset loading is complex - requires IProgramListData interface
    // Get IProgramListData from IUnitInfo
    IPtr<IUnitInfo> unit_info = U::cast<IUnitInfo>(plugin->controller);
    if (!unit_info) {
        return RACK_VST3_ERROR_GENERIC;
    }

    // Try to get preset data using IProgramListData
    IPtr<IProgramListData> program_data = U::cast<IProgramListData>(unit_info);
    if (program_data) {
        // Create stream to receive preset data
        // IPtr<MemoryStream>(new MemoryStream(), false) explanation:
        //   - new MemoryStream() creates object with refCount=1 (COM convention)
        //   - IPtr(..., false) takes ownership WITHOUT calling addRef() (false = don't addRef)
        //   - IPtr destructor calls release() when going out of scope
        //   - This ensures cleanup on ALL code paths (success, error, early return)
        IPtr<MemoryStream> stream(new MemoryStream(), false);

        // Get program data
        tresult result = program_data->getProgramData(preset.program_list_id, preset.program_index, stream);
        if (result == kResultOk) {
            // Reset stream and apply the preset data
            stream->seek(0, IBStream::kIBSeekSet, nullptr);
            result = program_data->setProgramData(preset.program_list_id, preset.program_index, stream);

            if (result == kResultOk) {
                return RACK_VST3_OK;
            }
        }
        // IPtr automatically releases stream on scope exit
    }

    // Fallback 1: Try to find and set a "program" parameter
    // Some plugins expose preset selection as a regular parameter
    // Only use discrete/list parameters to avoid false positives
    int32 param_count = plugin->controller->getParameterCount();
    for (int32 i = 0; i < param_count; i++) {
        Vst::ParameterInfo param_info;
        if (plugin->controller->getParameterInfo(i, param_info) != kResultOk) {
            continue;
        }

        // Only consider discrete parameters (stepCount > 0) or parameters with kIsProgramChange flag
        bool is_discrete = param_info.stepCount > 0;
        bool is_program_change = (param_info.flags & ParameterInfo::kIsProgramChange) != 0;

        if (!is_discrete && !is_program_change) {
            continue; // Skip continuous parameters without program change flag
        }

        // Convert parameter title to UTF-8 and lowercase for comparison
        std::string param_title = utf16_to_utf8(param_info.title);
        std::transform(param_title.begin(), param_title.end(), param_title.begin(),
                      [](unsigned char c) { return std::tolower(c); });

        // More specific matching to reduce false positives:
        // - Must be discrete or marked as program change
        // - Title must contain complete words "program", "preset", or "patch"
        // - Check for word boundaries to avoid matching "programming", "unpresettable", etc.
        bool is_program_param = false;
        if (is_program_change) {
            // Explicitly marked as program change - always use
            is_program_param = true;
        } else {
            // Check for specific keywords as complete words with word boundaries
            // We need to search for each keyword separately, not use find_first_of
            const char* keywords[] = {"program", "preset", "patch"};
            const size_t keyword_lengths[] = {7, 6, 5};

            for (size_t k = 0; k < 3; ++k) {
                const char* keyword = keywords[k];
                size_t keyword_len = keyword_lengths[k];
                size_t pos = 0;

                while ((pos = param_title.find(keyword, pos)) != std::string::npos) {
                    // Check word boundaries
                    bool start_ok = (pos == 0 || !std::isalnum(param_title[pos - 1]));
                    bool end_ok = (pos + keyword_len >= param_title.length() ||
                                  !std::isalnum(param_title[pos + keyword_len]));

                    if (start_ok && end_ok) {
                        is_program_param = true;
                        break;
                    }
                    pos++;
                }

                if (is_program_param) {
                    break; // Found a match, stop searching
                }
            }
        }

        if (is_program_param) {
            // Verify program_index is within valid range
            if (preset.program_index > param_info.stepCount) {
                continue; // Index out of range for this parameter
            }

            // Map program_index to normalized value
            float normalized_value = static_cast<float>(preset.program_index) /
                                    static_cast<float>(param_info.stepCount);

            // Clamp to 0.0-1.0 range
            normalized_value = std::max(0.0f, std::min(1.0f, normalized_value));

            // Set the parameter
            ParamID param_id = param_info.id;
            if (plugin->controller->setParamNormalized(param_id, normalized_value) == kResultOk) {
                // Queue the parameter change for the processor
                int32 queue_index = 0;
                IParamValueQueue* queue = plugin->input_param_changes.addParameterData(param_id, queue_index);
                if (queue) {
                    int32 point_index = 0;
                    queue->addPoint(0, normalized_value, point_index);
                }

                // Successfully loaded via parameter-based fallback
                return RACK_VST3_OK;
            }
        }
    }

    // Fallback 2: Try using IUnitInfo to select the unit containing this program
    // Some plugins may respond to unit selection by loading the first program in that unit
    int32 unit_count = unit_info->getUnitCount();
    for (int32 i = 0; i < unit_count; i++) {
        UnitInfo unit_info_struct;
        if (unit_info->getUnitInfo(i, unit_info_struct) == kResultOk) {
            // Check if this unit's program list matches our preset
            if (unit_info_struct.programListId == preset.program_list_id) {
                // Try to select this unit (might trigger program load)
                tresult result = unit_info->selectUnit(unit_info_struct.id);
                if (result == kResultOk) {
                    // Unit selected - this might have loaded a program
                    // We can't verify, so return success as best effort
                    return RACK_VST3_OK;
                }
            }
        }
    }

    // No fallback succeeded - plugin doesn't support programmatic preset loading
    // This is a plugin limitation, not a runtime error
    return RACK_VST3_ERROR_NOT_SUPPORTED;
}

// ============================================================================
// State helpers
// ============================================================================

// The VST3 spec forbids calling getState/setState while the plugin is in
// "Processing" state (i.e. after setProcessing(true)).  suspend_for_state /
// resume_after_state bracket every state operation so we always enter from
// the "Active" level.  The Rust layer holds the plugin mutex during state
// operations, so the audio callback is already blocked (it uses try_lock and
// emits silence when the mutex is taken).  No process() calls occur between
// suspend and resume; only the plugin's internal VST3 state machine changes.
static void suspend_for_state(RackVST3Plugin* plugin) {
    if (!plugin->initialized) return;
    plugin->processor->setProcessing(false);
    plugin->component->setActive(false);
}

static void resume_after_state(RackVST3Plugin* plugin, double post_activate_pump_seconds = 0.0) {
    if (!plugin->initialized) return;
    plugin->component->setActive(true);
    if (post_activate_pump_seconds > 0.0) {
        // Let any async work triggered by setActive (e.g. JUCE prepareToPlay) complete
        // before starting processing.  Only used in set_state, not get_state.
        pump_runloop(post_activate_pump_seconds);
    }
    plugin->processor->setProcessing(true);
}

int rack_vst3_plugin_get_state_size(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->component) {
        return 0;
    }
    // Return a generous upper bound so the caller can allocate a buffer and
    // call rack_vst3_plugin_get_state once.  Doing a real getState call here
    // just to measure the size would mean two full suspend/resume cycles back
    // to back, which causes crashes in some JUCE plugins (AD2 included).
    // rack_vst3_plugin_get_state updates *size to the actual number of bytes.
    return 4 * 1024 * 1024;  // 4 MB — well above any observed AD2 state size
}

int rack_vst3_plugin_get_state(RackVST3Plugin* plugin, uint8_t* data, size_t* size) {
    if (!plugin || !data || !size || !plugin->component) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (*size == 0) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Suspend processing: getState is only valid outside Processing state.
    suspend_for_state(plugin);

    IPtr<MemoryStream> stream(new MemoryStream(), false);

    // 4-byte header: component state size (filled after serialization).
    uint32_t size_marker_placeholder = 0;
    stream->write(&size_marker_placeholder, sizeof(size_marker_placeholder), nullptr);

    int64 component_start_pos = 0;
    stream->tell(&component_start_pos);

    tresult result = plugin->component->getState(stream);
    if (result != kResultOk) {
        fprintf(stderr, "[rack] IComponent::getState failed: tresult=0x%08x\n",
                static_cast<unsigned>(result));
        resume_after_state(plugin);
        return RACK_VST3_ERROR_GENERIC;
    }

    int64 component_end_pos = 0;
    stream->tell(&component_end_pos);
    uint32_t component_state_size = static_cast<uint32_t>(component_end_pos - component_start_pos);

    // Write the component state size into the 4-byte header.
    stream->seek(0, IBStream::kIBSeekSet, nullptr);
    stream->write(&component_state_size, sizeof(component_state_size), nullptr);

    // Seek to end of component state to append controller state.
    stream->seek(component_end_pos, IBStream::kIBSeekSet, nullptr);

    if (plugin->controller &&
        reinterpret_cast<void*>(plugin->controller.get()) !=
        reinterpret_cast<void*>(plugin->component.get())) {
        // Non-OK from getState means the controller has no separate state
        // (common for JUCE plugins); the component state alone is used.
        plugin->controller->getState(stream);
    }

    resume_after_state(plugin);

    size_t state_size = stream->getSize();
    if (state_size > *size) {
        *size = state_size;
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    memcpy(data, stream->getData().data(), state_size);
    *size = state_size;
    return RACK_VST3_OK;
}

int rack_vst3_plugin_set_state(RackVST3Plugin* plugin, const uint8_t* data, size_t size) {
    if (!plugin || !data || size == 0 || !plugin->component) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (size < sizeof(uint32_t)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    uint32_t component_state_size = 0;
    memcpy(&component_state_size, data, sizeof(component_state_size));

    if (static_cast<size_t>(sizeof(component_state_size)) + component_state_size > size) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // VST3 host state-restore pattern (adapted from SDK basewrapper.cpp),
    // matching the pattern already proven safe in initialize_with_state:
    // setState must be called while the component is ACTIVE (only
    // processing needs to be stopped), not while deactivated. Deactivating
    // first causes component->setState to return kResultOk without
    // actually updating parameter values later visible via
    // getParamNormalized -- confirmed by direct testing on AD2, where a
    // round-tripped mixer parameter silently failed to restore under the
    // old deactivate-first sequence.
    //
    //   setProcessing(false)                 — already-initialized plugins
    //   setActive(true)                      — not-yet-initialized plugins
    //   component->setState(comp_stream)
    //   pump CFRunLoop                       — let async sample loading finish
    //   controller->setComponentState(sync)  — separate controller objects only
    //   setProcessing(true)                  — already-initialized plugins
    //
    // The Rust caller holds the plugin mutex throughout, so the audio callback
    // is blocked (try_lock returns Err, emits silence).

    if (plugin->initialized) {
        plugin->processor->setProcessing(false);
    } else {
        plugin->component->setActive(true);
    }

    const uint8_t* comp_data = data + sizeof(component_state_size);
    // IPtr(ptr, false): FUNKNOWN_CTOR initialised __funknownRefCount to 1,
    // and IPtr takes ownership of that reference without an extra addRef.
    // The plugin calls addRef/release symmetrically during setState, so the
    // count returns to 1 when setState returns, and the IPtr destructor at
    // end of scope drops it to 0 and frees the stream.
    IPtr<MemoryStream> comp_stream(
        new MemoryStream(comp_data, component_state_size), false);
    tresult result = plugin->component->setState(comp_stream);
    if (result != kResultOk) {
        if (plugin->initialized) {
            plugin->processor->setProcessing(true);
        }
        return RACK_VST3_ERROR_GENERIC;
    }

    // Pump until AD2's async sample-loading thread finishes.
    pump_runloop(5.0);

    // Sync controller to component state.  Only for plugins where they are
    // separate objects; for same-object plugins (JUCE), setComponentState
    // would call setState again and start a second conflicting load.
    if (plugin->controller) {
        void* comp_ptr = reinterpret_cast<void*>(plugin->component.get());
        void* ctrl_ptr = reinterpret_cast<void*>(plugin->controller.get());
        if (comp_ptr != ctrl_ptr) {
            IPtr<MemoryStream> sync_stream(
                new MemoryStream(comp_data, component_state_size), false);
            plugin->controller->setComponentState(sync_stream);

            size_t ctrl_offset = sizeof(component_state_size) + component_state_size;
            if (ctrl_offset < size) {
                const uint8_t* ctrl_data = data + ctrl_offset;
                size_t ctrl_size = size - ctrl_offset;
                IPtr<MemoryStream> ctrl_stream(new MemoryStream(ctrl_data, ctrl_size), false);
                plugin->controller->setState(ctrl_stream);
            }
        }
    }

    if (plugin->initialized) {
        plugin->processor->setProcessing(true);
    }

    return RACK_VST3_OK;
}

// ============================================================================
// MIDI API
// ============================================================================

int rack_vst3_plugin_send_midi(
    RackVST3Plugin* plugin,
    const RackVST3MidiEvent* events,
    uint32_t event_count)
{
    if (!plugin || !plugin->initialized) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // Null events array is only valid if event_count is 0
    if (!events && event_count > 0) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Early return for zero events (valid - nothing to do)
    if (event_count == 0) {
        return RACK_VST3_OK;
    }

    // Convert MIDI events to VST3 events
    for (uint32_t i = 0; i < event_count; ++i) {
        const auto& midi_event = events[i];

        Event vst3_event;
        memset(&vst3_event, 0, sizeof(Event));
        vst3_event.sampleOffset = midi_event.sample_offset;
        vst3_event.busIndex = 0;

        uint8_t status = midi_event.status & 0xF0;

        switch (status) {
            case 0x90:  // Note On
                vst3_event.type = Event::kNoteOnEvent;
                vst3_event.noteOn.channel = midi_event.channel;
                vst3_event.noteOn.pitch = midi_event.data1;
                vst3_event.noteOn.velocity = static_cast<float>(midi_event.data2) / 127.0f;
                vst3_event.noteOn.noteId = -1;  // Not specified
                plugin->input_events.addEvent(vst3_event);
                break;

            case 0x80:  // Note Off
                vst3_event.type = Event::kNoteOffEvent;
                vst3_event.noteOff.channel = midi_event.channel;
                vst3_event.noteOff.pitch = midi_event.data1;
                vst3_event.noteOff.velocity = static_cast<float>(midi_event.data2) / 127.0f;
                vst3_event.noteOff.noteId = -1;  // Not specified
                plugin->input_events.addEvent(vst3_event);
                break;

            case 0xA0:  // Polyphonic Key Pressure (Aftertouch)
                vst3_event.type = Event::kPolyPressureEvent;
                vst3_event.polyPressure.channel = midi_event.channel;
                vst3_event.polyPressure.pitch = midi_event.data1;
                vst3_event.polyPressure.pressure = static_cast<float>(midi_event.data2) / 127.0f;
                plugin->input_events.addEvent(vst3_event);
                break;

            case 0xB0:  // Control Change
                // Use LegacyMIDICCOutEvent for CC messages
                // This is the standard VST3 approach for MIDI CC data
                vst3_event.type = Event::kLegacyMIDICCOutEvent;
                vst3_event.midiCCOut.channel = midi_event.channel;
                vst3_event.midiCCOut.controlNumber = midi_event.data1;
                vst3_event.midiCCOut.value = midi_event.data2;
                vst3_event.midiCCOut.value2 = 0;
                plugin->input_events.addEvent(vst3_event);
                break;

            case 0xC0:  // Program Change
                // VST3 doesn't have a dedicated program change event type
                // Use LegacyMIDICCOutEvent with controlNumber >= 0x80 for non-CC MIDI
                // Note: Not all plugins may support this encoding
                vst3_event.type = Event::kLegacyMIDICCOutEvent;
                vst3_event.midiCCOut.channel = midi_event.channel;
                vst3_event.midiCCOut.controlNumber = 0x80;  // >= 0x80 indicates non-CC MIDI
                vst3_event.midiCCOut.value = midi_event.data1;
                vst3_event.midiCCOut.value2 = 0;
                plugin->input_events.addEvent(vst3_event);
                break;

            case 0xD0:  // Channel Pressure (Aftertouch)
                // VST3 doesn't have a dedicated channel pressure event type
                // Use LegacyMIDICCOutEvent with controlNumber >= 0x80 for non-CC MIDI
                // Note: Not all plugins may support this encoding
                vst3_event.type = Event::kLegacyMIDICCOutEvent;
                vst3_event.midiCCOut.channel = midi_event.channel;
                vst3_event.midiCCOut.controlNumber = 0x81;  // >= 0x80 indicates non-CC MIDI
                vst3_event.midiCCOut.value = midi_event.data1;
                vst3_event.midiCCOut.value2 = 0;
                plugin->input_events.addEvent(vst3_event);
                break;

            case 0xE0: {  // Pitch Bend
                // VST3 doesn't have a dedicated pitch bend event type
                // Use LegacyMIDICCOutEvent with controlNumber >= 0x80 for non-CC MIDI
                // Note: Not all plugins may support this encoding
                // The plugin should interpret value (LSB) and value2 (MSB) as 14-bit pitch bend
                vst3_event.type = Event::kLegacyMIDICCOutEvent;
                vst3_event.midiCCOut.channel = midi_event.channel;
                vst3_event.midiCCOut.controlNumber = 0x82;  // >= 0x80 indicates non-CC MIDI
                vst3_event.midiCCOut.value = midi_event.data1;   // LSB
                vst3_event.midiCCOut.value2 = midi_event.data2;  // MSB
                plugin->input_events.addEvent(vst3_event);
                break;
            }

            default:
                // Unknown MIDI event - skip it
                continue;
        }
    }

    return RACK_VST3_OK;
}
