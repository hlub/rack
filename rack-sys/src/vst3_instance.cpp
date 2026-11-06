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
#include <memory>
#include <string>
#include <cstring>
#include <cmath>
#include <mutex>
#include <algorithm>
#include <atomic>
#include <chrono>

#include "vst3_lifecycle_mutex.h"
#include "vst3_connection_proxy.h"

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
    // Moves the written bytes out, leaving the stream empty.
    std::vector<uint8_t> takeData() { position_ = 0; return std::move(buffer_); }
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

// Milliseconds elapsed since this process started. Used only to correlate
// the Vst::IProgress observational log lines below with when a caller
// invoked rack_vst3_plugin_set_state and how long its own fixed
// pump_runloop wait took -- see that function's own doc comment.
static long long log_elapsed_ms() {
    using Clock = std::chrono::steady_clock;
    static const Clock::time_point process_start = Clock::now();
    return std::chrono::duration_cast<std::chrono::milliseconds>(
        Clock::now() - process_start).count();
}

static const char* progress_type_name(Vst::IProgress::ProgressType type) {
    switch (type) {
        case Vst::IProgress::ProgressType::AsyncStateRestoration:
            return "AsyncStateRestoration";
        case Vst::IProgress::ProgressType::UIBackgroundTask:
            return "UIBackgroundTask";
        default:
            return "Unknown";
    }
}

// The host's IComponentHandler. The VST3 spec requires the host to provide
// one via IEditController::setComponentHandler before relying on the
// controller -- some plugins' internal parameter-cache synchronization (e.g.
// inside setComponentState) may depend on a handler being present.
// beginEdit/performEdit/endEdit are accepted and ignored: a headless host has
// no automation to record. restartComponent's flags are recorded, ORed into
// one atomic word the host takes with rack_vst3_plugin_take_restart_flags:
// a plugin may call it from any thread (its own background loader, or even
// process()), so recording is one lock-free fetch_or and acting on the flags
// is the host's, on its own thread.
//
// Also implements Vst::IProgress, purely for observation. A plugin whose
// setState triggers async background loading (e.g. sample streaming) may,
// per the [optional] Vst::IProgress extension (ivsteditcontroller.h), call
// start/update/finish on the host's IComponentHandler to push a real
// completion signal instead of the host guessing with a fixed wait.
// rack_vst3_plugin_set_state currently always waits a fixed
// pump_runloop(5.0) after IComponent::setState with no early-exit
// condition (see that function's own doc comment); these three methods
// only log what they are called with here -- they never gate, shorten, or
// otherwise change that wait -- so real plugins can be observed calling
// (or not calling) this mechanism before it is relied on for anything.
class ComponentHandler : public IComponentHandler, public Vst::IProgress {
public:
    ComponentHandler() { FUNKNOWN_CTOR }
    virtual ~ComponentHandler() = default;

    DECLARE_FUNKNOWN_METHODS

    tresult PLUGIN_API beginEdit(ParamID) override { return kResultOk; }
    // A plugin edits a value of its own (its GUI, an internal change) and says so here. The
    // controller already holds the new value. That something moved is kept, and, for a plugin
    // whose controller is a separate object from its component, so is the value: the standard
    // asks the host to hand a performEdit value to the processor, and nothing else will (the
    // sound would otherwise stay as it was while the controller shows the edit). One atomic
    // store per parameter, preallocated by bind, so a plugin may call this from any thread.
    //
    // The edit is stamped with the host's write generation at this moment, so the take can tell
    // an edit from before a host write of the same parameter (which the host's value then
    // supersedes) from one after it. `edited_`, the flag the pending question looks at, is set
    // last, with release: a take that sees it also sees the stored edit, and one that runs
    // between the stores finds `edits_any_` already set.
    tresult PLUGIN_API performEdit(ParamID id, ParamValue value) override {
        if (forward_.load(std::memory_order_acquire) && std::isfinite(value)) {
            auto found = std::lower_bound(ids_->begin(), ids_->end(), std::make_pair(id, uint32_t{0}));
            if (found != ids_->end() && found->first == id) {
                float narrowed = static_cast<float>(value);
                uint32_t bits = 0;
                std::memcpy(&bits, &narrowed, sizeof(bits));
                uint64_t stamp = write_generation_->load(std::memory_order_acquire);
                edits_[found->second].store((stamp << 32) | bits, std::memory_order_release);
                edits_any_.store(true, std::memory_order_release);
            }
        }
        edited_.store(true, std::memory_order_release);
        return kResultOk;
    }

    // Start keeping the values of performEdit, for a plugin with a separate controller. `ids` is
    // the plugin's ids with their indices, sorted by id, `write_generation` and `host_write_gen`
    // the hand-off part's counter and per-parameter newest host writes; all must outlive this
    // handler and never move. Called once (the first initialize; a second leaves the binding as
    // it is), before the plugin can call performEdit from another thread (the release store of
    // forward_ publishes the rest).
    void bind(const std::vector<std::pair<ParamID, uint32_t>>* ids, size_t parameter_count,
              const std::atomic<uint32_t>* write_generation, const std::atomic<uint32_t>* host_write_gen) {
        if (forward_.load(std::memory_order_acquire)) {
            return;
        }
        ids_ = ids;
        write_generation_ = write_generation;
        host_write_gen_ = host_write_gen;
        edits_.reset(new std::atomic<uint64_t>[parameter_count]);
        for (size_t index = 0; index < parameter_count; ++index) {
            edits_[index].store(kNoEdit, std::memory_order_relaxed);
        }
        parameter_count_ = parameter_count;
        forward_.store(true, std::memory_order_release);
    }

    // The values performEdit left since the last take, as (index, value), up to `capacity`;
    // taking clears them. An edit is dropped when the host has written the same parameter since
    // (its generation is newer than the edit's stamp): the host's value is the newer one and the
    // controller holds it. Controller thread.
    size_t take_edits(uint32_t* indices, float* values, size_t capacity) {
        if (!edits_any_.exchange(false, std::memory_order_acq_rel)) {
            return 0;
        }
        size_t count = 0;
        for (size_t index = 0; index < parameter_count_ && count < capacity; ++index) {
            uint64_t edit = edits_[index].exchange(kNoEdit, std::memory_order_acq_rel);
            if (edit == kNoEdit) {
                continue;
            }
            uint32_t stamp = static_cast<uint32_t>(edit >> 32);
            uint32_t newest = host_write_gen_[index].load(std::memory_order_acquire);
            if (static_cast<int32_t>(newest - stamp) > 0) {
                continue;
            }
            uint32_t bits = static_cast<uint32_t>(edit);
            float value = 0.0f;
            std::memcpy(&value, &bits, sizeof(value));
            indices[count] = static_cast<uint32_t>(index);
            values[count] = value;
            ++count;
        }
        return count;
    }

    // Forget every edit nobody took, and that the plugin edited: for the functions of the whole
    // instance that replace what the plugin is (a state write, a preset load, a reset), after
    // which an edit made before describes a plugin that no longer exists. Nothing processes while
    // the instance is whole; a plugin's own thread may still call performEdit, and an edit it makes
    // after this call is a new one.
    void discard_edits() {
        if (!forward_.load(std::memory_order_acquire)) {
            edited_.store(false, std::memory_order_release);
            return;
        }
        edits_any_.store(false, std::memory_order_release);
        for (size_t index = 0; index < parameter_count_; ++index) {
            edits_[index].store(kNoEdit, std::memory_order_relaxed);
        }
        edited_.store(false, std::memory_order_release);
    }
    tresult PLUGIN_API endEdit(ParamID) override { return kResultOk; }
    tresult PLUGIN_API restartComponent(int32 flags) override {
        pending_restart_flags_.fetch_or(static_cast<uint32_t>(flags), std::memory_order_release);
        return kResultOk;
    }

    // The flags raised since the last take, cleared by taking them.
    uint32_t take_restart_flags() {
        return pending_restart_flags_.exchange(0, std::memory_order_acquire);
    }

    // Whether anything take_edited or take_values_changed_request would report is waiting: two
    // atomic loads, for a host that asks every tick without taking a lock to do it.
    bool changes_pending() const {
        uint32_t mask = static_cast<uint32_t>(Vst::RestartFlags::kParamValuesChanged);
        return edited_.load(std::memory_order_acquire) || edits_any_.load(std::memory_order_acquire) ||
               (pending_restart_flags_.load(std::memory_order_acquire) & mask) != 0;
    }

    // Whether the plugin passed performEdit since the last take, cleared by taking it.
    bool take_edited() { return edited_.exchange(false, std::memory_order_acquire); }

    // Whether the plugin asked, through restartComponent, for its parameter values to be read
    // again since the last take. Clears only that one request: the other flags stay for
    // take_restart_flags.
    bool take_values_changed_request() {
        uint32_t mask = static_cast<uint32_t>(Vst::RestartFlags::kParamValuesChanged);
        return (pending_restart_flags_.fetch_and(~mask, std::memory_order_acq_rel) & mask) != 0;
    }

    tresult PLUGIN_API start(Vst::IProgress::ProgressType type,
                              const tchar* optionalDescription,
                              Vst::IProgress::ID& outID) override {
        outID = next_progress_id_.fetch_add(1, std::memory_order_relaxed);
        std::string description =
            optionalDescription ? utf16_to_utf8(optionalDescription) : std::string();
        fprintf(stderr,
                "[rack] IProgress::start id=%llu type=%s description=\"%s\" t=%lldms\n",
                static_cast<unsigned long long>(outID), progress_type_name(type),
                description.c_str(), log_elapsed_ms());
        return kResultOk;
    }

    tresult PLUGIN_API update(Vst::IProgress::ID id, ParamValue normValue) override {
        fprintf(stderr, "[rack] IProgress::update id=%llu value=%.4f t=%lldms\n",
                static_cast<unsigned long long>(id), normValue, log_elapsed_ms());
        return kResultOk;
    }

    tresult PLUGIN_API finish(Vst::IProgress::ID id) override {
        fprintf(stderr, "[rack] IProgress::finish id=%llu t=%lldms\n",
                static_cast<unsigned long long>(id), log_elapsed_ms());
        return kResultOk;
    }

private:
    // Handed back to the plugin as each progress's opaque ID. This
    // observational-only implementation has no real progress list to look
    // an ID up in, but start() must still hand back a unique one per call.
    std::atomic<uint64_t> next_progress_id_{1};

    std::atomic<uint32_t> pending_restart_flags_{0};
    std::atomic<bool> edited_{false};

    // performEdit values kept for the processor (see performEdit and bind).
    static constexpr uint64_t kNoEdit = ~uint64_t{0};
    std::atomic<bool> forward_{false};
    const std::vector<std::pair<ParamID, uint32_t>>* ids_ = nullptr;
    const std::atomic<uint32_t>* write_generation_ = nullptr;
    const std::atomic<uint32_t>* host_write_gen_ = nullptr;
    std::unique_ptr<std::atomic<uint64_t>[]> edits_;
    size_t parameter_count_ = 0;
    std::atomic<bool> edits_any_{false};
};

IMPLEMENT_REFCOUNT(ComponentHandler)

tresult PLUGIN_API ComponentHandler::queryInterface(const TUID _iid, void** obj) {
    QUERY_INTERFACE(_iid, obj, FUnknown::iid, IComponentHandler)
    QUERY_INTERFACE(_iid, obj, IComponentHandler::iid, IComponentHandler)
    QUERY_INTERFACE(_iid, obj, Vst::IProgress::iid, Vst::IProgress)
    *obj = nullptr;
    return kNoInterface;
}

IMPLEMENT_REFCOUNT(RackConnectionProxy)

tresult PLUGIN_API RackConnectionProxy::queryInterface(const TUID _iid, void** obj) {
    QUERY_INTERFACE(_iid, obj, FUnknown::iid, IConnectionPoint)
    QUERY_INTERFACE(_iid, obj, IConnectionPoint::iid, IConnectionPoint)
    *obj = nullptr;
    return kNoInterface;
}

// Capacity of RackVST3Plugin::input_events/output_events, in events. Chosen
// to match src/vst3/instance.rs's OUTPUT_MIDI_SCRATCH_CAPACITY (256), which
// documents this as the real, effective cap on how many MIDI events a
// single process() call can carry in either direction -- Steinberg's
// EventList defaults its maxSize to 50 when default-constructed, which is
// smaller than that documented capacity unless overridden here.
static constexpr int32 kEventListCapacity = 256;

// One hosted plugin instance.
//
// The fields are partitioned by who may touch them, because the Rust layer
// splits an initialized instance into a processor half (moved to the audio
// thread) and a controller half (kept on the thread that split it), and the
// two then run at the same time with nothing serializing them:
//
// - The lifecycle objects (module, the three interfaces, path, uid) are
//   written by rack_vst3_plugin_new and rack_vst3_plugin_free only. The
//   `processor` interface is called by the processor half for process() alone;
//   its other methods (setupProcessing, setProcessing, getLatencySamples) and
//   every bus activation are called only by functions of the whole instance,
//   which run while the instance is not split.
// - `fixed` is written only by rack_vst3_plugin_new and
//   rack_vst3_plugin_initialize and is read by either half.
// - `proc` belongs to the processor half: process(), the MIDI and parameter
//   queues, the transport. A function of the whole instance may write it too,
//   because nothing processes concurrently with one.
// - `ctrl` belongs to the controller half.
// - `handoff` is the one part both halves touch while running: atomics only, each
//   documented with its writer and its ordering (the plugin's reported values and the
//   host's own writes, so a report is not mistaken for a newer say than the host's).
//   rack_vst3_plugin_process[_multi_bus] write it; _controller_set_parameter,
//   _take_value_changes and _value_changes_pending are the controller side's functions
//   that touch it.
// - `released_halves` is the hand-off between the two halves when they are
//   dropped (see rack_vst3_plugin_release_controller).
//
// A function that touches a field outside its own half's part is a data race
// the moment the instance is split; the C API's own grouping in rack_vst3.h
// says which half each function belongs to.
struct RackVST3Plugin {
    // Lifecycle objects.
    Hosting::Module::Ptr module;
    IPtr<IComponent> component;
    IPtr<IAudioProcessor> processor;
    IPtr<IEditController> controller;
    std::string path;
    VST3::UID uid;

    // Which halves have let go (kProcessorReleased, kControllerReleased).
    // The one field both halves write: the processor's release store
    // happens-after its last process() call, and the controller's acquiring
    // read-modify-write sees it, so the controller frees the instance only
    // once nothing can be processing in it.
    std::atomic<uint32_t> released_halves{0};

    // A parameter's descriptive metadata, as the controller last reported it.
    struct ParamMeta {
        std::string title;
        std::string units;
        ParamValue min_value = 0.0;
        ParamValue max_value = 1.0;
        ParamValue default_value = 0.0;
        int32 flags = 0;
        int32 step_count = 0;
    };

    // A factory preset from IUnitInfo.
    struct PresetInfo {
        int32 program_list_id;
        int32 program_index;
        std::string name;
        // Name of the IUnitInfo program list this preset belongs to (e.g. a
        // plugin could expose separate "Factory Presets" and "Drum Kits"
        // lists). Empty if the list itself has no name.
        std::string category;
    };

    // Facts fixed at initialization, read by either half.
    struct Fixed {
        bool initialized = false;
        double sample_rate = 0.0;
        uint32_t max_block_size = 0;

        // Bus 0's channel count in each direction.
        int32 num_input_channels = 0;
        int32 num_output_channels = 0;

        // Channel count and name of every declared audio bus, indexed by bus
        // index. output_bus_channel_counts[0] always equals
        // num_output_channels (likewise for inputs); these generalize it to
        // every bus for rack_vst3_plugin_process_multi_bus's validation.
        std::vector<int32_t> input_bus_channel_counts;
        std::vector<int32_t> output_bus_channel_counts;
        std::vector<std::string> input_bus_names;
        std::vector<std::string> output_bus_names;

        // Whether the plugin declares an event output bus. Cached because the
        // live answer is an IComponent call, which the audio thread must not
        // make.
        bool emits_midi = false;

        // Every parameter's id, indexed by rack's parameter index. Never
        // rewritten after initialize: the processor half resolves an index to
        // an id here on the audio thread, so a refresh of the controller's
        // descriptions (ctrl.param_meta) must leave this alone.
        std::vector<ParamID> param_ids;

        // The same ids, each with its index, sorted by id: how the processor half, on the audio
        // thread, finds the index of an id a plugin reports a value for, without allocating.
        std::vector<std::pair<ParamID, uint32_t>> ids_by_id;

        // Whether each parameter (by index) was read-only (ParameterInfo::kIsReadOnly) when the
        // controller listed it: a meter or another output the plugin shows and nobody sets.
        // Such a parameter is never recorded as a reported value and never written to the
        // controller on the plugin's say-so, however often the plugin reports it.
        std::vector<uint8_t> read_only;

        std::vector<PresetInfo> presets;
    } fixed;

    // The processor half's state.
    struct Processing {
        HostProcessData process_data;
        ParameterChanges input_param_changes;
        ParameterChanges output_param_changes;

        // The generation of the last value handed to the plugin for each parameter, by
        // queue_parameter_change_stamped. Audio thread only (a function of the whole instance
        // may reset it); see Handoff.
        std::vector<uint32_t> delivered_gen;
        EventList input_events{kEventListCapacity};
        EventList output_events{kEventListCapacity};

        // Process context: tempo/playback state and host position the plugin
        // reads on every process() call. Built once in initialize(), then
        // kept live by rack_vst3_plugin_set_transport (see that function's
        // own doc comment) -- including projectTimeSamples, which this
        // library never advances on its own, so there is no per-instance
        // position counter here to disagree with the host's.
        ProcessContext process_context;

        // dummy_buf: a silent buffer shared across all aux output buses that
        // we don't capture. The plugin may write to these; we ignore the
        // contents.
        std::vector<float> dummy_buf;
        // input_silence_buf: a zeroed buffer shared across all input buses no
        // caller has fed real audio into for a given process call (every
        // input bus beyond bus 0, plus bus 0 itself when the caller passes
        // zero channels). Unlike dummy_buf, this is read BY the plugin, not
        // written TO by it, so it must actually contain silence rather than
        // arbitrary leftover data -- it is zeroed once here and never written
        // to again, which holds only because a well-behaved plugin never
        // writes into an input buffer (the VST3 spec reserves that for the
        // host).
        std::vector<float> input_silence_buf;
    } proc;

    // The hand-off part: the fields both the audio thread and the controller thread touch, and
    // nothing else. Every field is an atomic and the arrays are sized once in initialize, so
    // recording a report allocates nothing. A function of the whole instance may reset them too,
    // because nothing processes concurrently with one.
    //
    // THE WRITE GENERATION PROTOCOL. A plugin's report of a parameter's value is the plugin's
    // newer say than the controller's only if the plugin had been given the host's newest value
    // for that parameter when it made the report; otherwise it describes the plugin from before
    // the host wrote, and applying it would put the old value back on the controller.
    //
    //  - Every host write of parameter i gets a generation g from `write_generation` (a counter
    //    that only goes up) before anything else of the write happens, and the controller thread
    //    notes it in host_write_gen[i] (begin_host_write). The generation then travels with the
    //    value to the audio thread, through the host's slot and rack_vst3_plugin_queue_parameter_
    //    change_stamped, which sets delivered_gen[i] = g on the audio thread at the moment the
    //    value is handed to the plugin. A write the audio thread has not handed over yet (the host
    //    delivers a bounded number of parameters per block, so this can last several blocks)
    //    leaves delivered_gen[i] older than host_write_gen[i].
    //  - After each block the audio thread publishes each reported value as ONE 64-bit atomic
    //    holding (delivered_gen[i], the value): the pair cannot tear.
    //  - The take (controller thread) accepts a report only when its generation is not older than
    //    host_write_gen[i] (compared modulo 2^32).
    //
    // Why that is enough. host_write_gen[i] is written and read by the controller thread alone, so
    // the take sees the newest write exactly. A report's stamp is the generation of the last
    // value handed to the plugin before the report was made, read by the audio thread itself. If
    // the stamp is older than the newest host write g, the plugin had not been given that write's
    // value when it reported, so the report cannot reflect it and is dropped (this covers a write
    // still waiting for a drain, and a write whose drain happened in the same block after the
    // plugin reported in an earlier block). If the stamp is at least g, the value of g (or of a
    // newer write) was handed over before the report, so the report is the plugin's say about a
    // state that includes the host's value (a clamp, a snap, a side effect) and is applied. Both
    // windows of the block-counting scheme this replaced are closed: neither depends on which
    // block a drain or a note falls in. The ordering needed is only release/acquire on the one
    // packed word per parameter: the audio thread's release store publishes (stamp, value)
    // together, the take's acquire exchange reads exactly one such pair, and the generation
    // reaches the audio thread through the host's own slot (stored before its changed bit with
    // release, taken with acquire), so delivered_gen always holds a generation the controller
    // thread already allocated. No store-then-load pair across the two threads exists, so no
    // sequentially consistent fence is needed.
    struct Handoff {
        // The newest generation allocated. Controller thread: fetch_add in begin_host_write.
        std::atomic<uint32_t> write_generation{0};

        // The generation of the newest host write of each parameter. Controller thread only.
        std::unique_ptr<std::atomic<uint32_t>[]> host_write_gen;

        // Values the plugin reported through output_param_changes, last one per parameter, as
        // (generation << 32 | float bits), or kNoReport. The audio thread stores with release; the
        // take exchanges with acquire. reported_any says some entry may be set.
        std::unique_ptr<std::atomic<uint64_t>[]> reported;
        std::atomic<bool> reported_any{false};

        // The same for read-only parameters (meters), which are forwarded to the controller by
        // the take and never counted as a change of the plugin's values.
        std::unique_ptr<std::atomic<uint64_t>[]> meters;
        std::atomic<bool> meters_any{false};
    } handoff;

    static constexpr uint64_t kNoReport = ~uint64_t{0};

    // The controller half's state.
    struct Control {
        // Each parameter's metadata, indexed like fixed.param_ids.
        std::vector<ParamMeta> param_meta;

        // Host-provided IComponentHandler, required by the VST3 spec. Kept
        // alive for the plugin's lifetime; the controller only holds a raw
        // pointer to it (set via setComponentHandler), not a counted
        // reference. It also records the plugin's restart requests, in an
        // atomic any thread may raise and the controller half takes.
        IPtr<ComponentHandler> component_handler;

        // The proxies the component and a separate controller are connected
        // through (see vst3_connection_proxy.h): component_proxy carries what
        // the component sends, controller_proxy what the controller sends.
        // Null when the component is its own controller.
        IPtr<RackConnectionProxy> component_proxy;
        IPtr<RackConnectionProxy> controller_proxy;
    } ctrl;
};

static void discard_reported_values(RackVST3Plugin* plugin);

// Rebinds every output bus's channel pointers back to the plugin's shared
// dummy (silent, write-only) buffer, and every input bus's channel pointers
// back to its shared silence (zeroed, read-only) buffer. Called at the start
// of every process()/process_multi_bus() call, before that call's own
// bus-0/extra-bus buffers are bound over it.
//
// This is the fix for two related bugs: (1) a bus a caller bound a real
// buffer into on a previous call, then omits from a later call (or calls
// rack_vst3_plugin_process, which only ever rebinds bus 0), would otherwise
// keep pointing at that caller-owned buffer forever -- a VST3 plugin writes
// every *activated* output bus on every process() call, so that stale
// pointer is a use-after-free the moment the caller's backing storage goes
// away. (2) every declared input bus is activated during initialize (so a
// plugin sees it as connected), but only bus 0 (and, since this fix, every
// bus reset here) is ever pointed at real memory -- without this, a bus
// beyond 0 stays null and a plugin that unconditionally reads an activated
// input bus null-derefs.
//
// O(total channel count across all buses) per call: a small, fixed,
// allocation-free cost, matching this file's real-time-safety requirements.
static void reset_bus_bindings(RackVST3Plugin* plugin) {
    float* dummy_ptr = plugin->proc.dummy_buf.data();
    for (int32 b = 0; b < plugin->proc.process_data.numOutputs; ++b) {
        AudioBusBuffers& bus = plugin->proc.process_data.outputs[b];
        for (int32 ch = 0; ch < bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = dummy_ptr;
        }
    }

    float* silence_ptr = plugin->proc.input_silence_buf.data();
    for (int32 b = 0; b < plugin->proc.process_data.numInputs; ++b) {
        AudioBusBuffers& bus = plugin->proc.process_data.inputs[b];
        for (int32 ch = 0; ch < bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = silence_ptr;
        }
    }
}

// Reads the controller's whole parameter list: each parameter's id into
// `ids` and its description into `meta`, index for index. A parameter whose
// getParameterInfo fails is left out of both, so the two always have the same
// length. Controller calls only; the caller says which of its own fields the
// lists are.
static void read_parameter_list(IEditController* controller, std::vector<ParamID>& ids,
                                std::vector<RackVST3Plugin::ParamMeta>& meta) {
    int32 param_count = controller->getParameterCount();
    ids.clear();
    meta.clear();
    if (param_count <= 0) {
        return;
    }
    ids.reserve(static_cast<size_t>(param_count));
    meta.reserve(static_cast<size_t>(param_count));

    for (int32 i = 0; i < param_count; ++i) {
        ParameterInfo vst3_param_info;
        if (controller->getParameterInfo(i, vst3_param_info) != kResultOk) {
            continue;
        }
        RackVST3Plugin::ParamMeta info;
        info.title = utf16_to_utf8(vst3_param_info.title);
        info.units = utf16_to_utf8(vst3_param_info.units);
        // VST3 parameters are already normalized 0.0-1.0
        info.min_value = 0.0;
        info.max_value = 1.0;
        info.default_value = vst3_param_info.defaultNormalizedValue;
        info.flags = vst3_param_info.flags;
        info.step_count = vst3_param_info.stepCount;
        ids.push_back(vst3_param_info.id);
        meta.push_back(std::move(info));
    }
}

// Forward declarations: defined below under "State helpers", but also
// needed by rack_vst3_plugin_free and rack_vst3_plugin_reset (defined
// earlier in the file) so teardown and reset follow the exact same
// setProcessing(false)-before-setActive(false) lifecycle order every other
// suspend point in this file uses.
static void suspend_for_state(RackVST3Plugin* plugin);
static void resume_after_state(RackVST3Plugin* plugin, double post_activate_pump_seconds = 0.0,
                                bool reactivate = true, bool resume_processing = true);

// ============================================================================
// Plugin Instance Implementation
// ============================================================================

static constexpr uint32_t kProcessorReleased = 1u << 0;
static constexpr uint32_t kControllerReleased = 1u << 1;

// Instances created and not yet freed, and instances leaked by a controller
// half released before its processor half. Process-wide, for tests and for a
// host that wants to report a leak.
static std::atomic<uint64_t> g_live_instances{0};
static std::atomic<uint64_t> g_leaked_instances{0};
static std::atomic<bool> g_leak_reported{false};

uint64_t rack_vst3_live_instance_count(void) {
    return g_live_instances.load(std::memory_order_relaxed);
}

uint64_t rack_vst3_leaked_instance_count(void) {
    return g_leaked_instances.load(std::memory_order_relaxed);
}

void rack_vst3_plugin_release_processor(RackVST3Plugin* plugin) {
    if (!plugin) {
        return;
    }
    plugin->released_halves.fetch_or(kProcessorReleased, std::memory_order_release);
}

int rack_vst3_plugin_release_controller(RackVST3Plugin* plugin) {
    if (!plugin) {
        return 0;
    }
    uint32_t before = plugin->released_halves.fetch_or(kControllerReleased, std::memory_order_acq_rel);
    if (before & kProcessorReleased) {
        rack_vst3_plugin_free(plugin);
        return 1;
    }
    // The processor half may still be in use on another thread, so nothing
    // here may be freed or called. The instance is leaked, which costs memory
    // but never a crash.
    g_leaked_instances.fetch_add(1, std::memory_order_relaxed);
    if (!g_leak_reported.exchange(true, std::memory_order_relaxed)) {
        fprintf(stderr,
                "[rack] a VST3 controller half was released while its processor half was still "
                "held; the instance of %s is leaked (further leaks are counted, not printed)\n",
                plugin->path.c_str());
    }
    return 0;
}

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
        plugin->ctrl.component_handler = IPtr<ComponentHandler>(new ComponentHandler(), false);
        plugin->controller->setComponentHandler(plugin->ctrl.component_handler);
    }

    // Connect a separate controller to the component through one proxy per
    // direction, as the SDK's PlugProvider::connectComponents does, so a
    // message either side sends off this thread is discarded rather than
    // delivered on the audio thread (see vst3_connection_proxy.h). Messages
    // sent while connecting arrive here, on the constructing thread, and are
    // delivered.
    if (plugin->controller && reinterpret_cast<void*>(plugin->controller.get()) != reinterpret_cast<void*>(plugin->component.get())) {
        IPtr<IConnectionPoint> component_cp = U::cast<IConnectionPoint>(plugin->component);
        IPtr<IConnectionPoint> controller_cp = U::cast<IConnectionPoint>(plugin->controller);

        if (component_cp && controller_cp) {
            plugin->ctrl.component_proxy = owned(new RackConnectionProxy(component_cp));
            plugin->ctrl.controller_proxy = owned(new RackConnectionProxy(controller_cp));
            plugin->ctrl.component_proxy->connect(controller_cp);
            plugin->ctrl.controller_proxy->connect(component_cp);
        }
    }

    g_live_instances.fetch_add(1, std::memory_order_relaxed);
    return plugin;
}

void rack_vst3_plugin_free(RackVST3Plugin* plugin) {
    if (!plugin) {
        return;
    }

    std::lock_guard<std::mutex> lock(rack_vst3_module_load_mutex());

    // VST3 lifecycle order is setProcessing(false), then setActive(false),
    // then terminate() -- initialize() leaves the plugin with
    // setProcessing(true) already in effect, so teardown must undo that
    // first. suspend_for_state does exactly this (and is a no-op if the
    // plugin was never initialized), ignoring the plugin's return values the
    // same way setActive's own return value is already ignored just below:
    // a plugin refusing to stop processing must not block cleanup, since the
    // plugin is being destroyed regardless.
    suspend_for_state(plugin);

    // Disconnect and release the proxies as PlugProvider::disconnectComponents
    // does, before either side is terminated: a proxy holds its side's
    // connection point, and a plugin may check at terminate or at its own
    // deletion that nothing still does (DPF reports one still held).
    if (plugin->ctrl.component_proxy) {
        plugin->ctrl.component_proxy->disconnect_destination();
    }
    if (plugin->ctrl.controller_proxy) {
        plugin->ctrl.controller_proxy->disconnect_destination();
    }
    plugin->ctrl.component_proxy = nullptr;
    plugin->ctrl.controller_proxy = nullptr;

    // Terminate controller
    if (plugin->controller && reinterpret_cast<void*>(plugin->controller.get()) != reinterpret_cast<void*>(plugin->component.get())) {
        plugin->controller->terminate();
        plugin->controller = nullptr;
    }

    // Terminate component. Its IAudioProcessor reference goes first, so the
    // component's own last reference is the one released here (DPF reports a
    // processor reference still held when the component is deleted).
    plugin->processor = nullptr;
    if (plugin->component) {
        plugin->component->terminate();
        plugin->component = nullptr;
    }

    // Every reference into the plugin's code is released before the module
    // that holds that code.
    plugin->module = nullptr;

    g_live_instances.fetch_sub(1, std::memory_order_relaxed);
    delete plugin;
}

// Whether the plugin has an edit controller that is a different object from
// its component. A controller that is the component itself (JUCE) reads the
// component's own values already, and calling setComponentState on it would
// call setState again and start a second, conflicting load; one that does not
// exist has nothing to tell. Neither is ever touched by the helpers below.
static bool has_separate_controller(const RackVST3Plugin* plugin) {
    return plugin->controller &&
           reinterpret_cast<const void*>(plugin->controller.get()) !=
               reinterpret_cast<const void*>(plugin->component.get());
}

// Tells a separate edit controller what the component's state is, so the values
// the controller reports (getParamNormalized) are the values the component
// uses. The VST3 contract makes this the host's job: a controller only learns
// the component's state through IEditController::setComponentState, and a
// component that starts in anything but the controller's declared defaults
// (a saved last session, a default program) is otherwise shown to the user as
// its defaults while it sounds as its own state. `data` is the component's
// state alone, without rack's size marker; an empty one tells nothing. Calls
// only IEditController::setComponentState, on a stream of this function's own.
static void sync_controller_with_component_state(RackVST3Plugin* plugin, const uint8_t* data,
                                                 size_t size) {
    if (!has_separate_controller(plugin) || size == 0) {
        return;
    }
    IPtr<MemoryStream> sync_stream(new MemoryStream(data, size), false);
    plugin->controller->setComponentState(sync_stream);
}

// The same, for the component's state as it is right now: read from the
// component and handed to the controller. initialize runs it once the plugin
// is active and any initial state has settled, so a plugin loaded with no
// state at all, or with one, is shown to the controller as it really is. The
// component is not asked for its state at all when there is no separate
// controller to tell: a state read right after activation is a state cycle,
// and back-to-back state cycles have crashed JUCE plugins.
static void sync_controller_with_component(RackVST3Plugin* plugin) {
    if (!has_separate_controller(plugin)) {
        return;
    }
    IPtr<MemoryStream> state(new MemoryStream(), false);
    if (plugin->component->getState(state) != kResultOk) {
        return;
    }
    std::vector<uint8_t> bytes = state->takeData();
    sync_controller_with_component_state(plugin, bytes.data(), bytes.size());
}

// Hands a separate controller its own part of a state in rack's format (the
// bytes after the component's), once the controller has been told the
// component's state: the VST3 order is setComponentState, then setState.
static void apply_controller_part(RackVST3Plugin* plugin, const uint8_t* data, size_t size) {
    if (!has_separate_controller(plugin) || size < sizeof(uint32_t)) {
        return;
    }
    uint32_t component_state_size = 0;
    memcpy(&component_state_size, data, sizeof(component_state_size));
    size_t ctrl_offset = sizeof(component_state_size) + component_state_size;
    if (ctrl_offset >= size) {
        return;
    }
    IPtr<MemoryStream> ctrl_stream(new MemoryStream(data + ctrl_offset, size - ctrl_offset), false);
    plugin->controller->setState(ctrl_stream);
}

int rack_vst3_plugin_initialize(RackVST3Plugin* plugin, double sample_rate, uint32_t max_block_size,
                                int32_t process_mode,
                                const uint8_t* initial_state, size_t initial_state_size) {
    if (!plugin || !plugin->component || !plugin->processor) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Mapped explicitly, never cast: the caller's own integers are a stable
    // part of this header's contract, and an unknown one is refused rather
    // than handed to setupProcessing as whatever it happens to equal. The
    // mode cannot be changed later -- setupProcessing may only be called
    // while the instance is inactive -- so getting it wrong here is not
    // recoverable by any later call.
    int32 mode = kRealtime;
    switch (process_mode) {
        case 0: mode = kRealtime; break;
        case 1: mode = kPrefetch; break;
        case 2: mode = kOffline;  break;
        default: return RACK_VST3_ERROR_INVALID_PARAM;
    }

    std::lock_guard<std::mutex> lock(rack_vst3_module_load_mutex());

    plugin->fixed.sample_rate = sample_rate;
    plugin->fixed.max_block_size = max_block_size;

    // Setup processing with 32-bit float samples, in the mode the host asked
    // for: the VST3 specification invites a plugin to render at higher
    // quality than realtime when it is told a pass is offline.
    ProcessSetup setup;
    setup.processMode = mode;
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
    plugin->fixed.input_bus_channel_counts.clear();
    plugin->fixed.input_bus_channel_counts.reserve(numInputBuses);
    plugin->fixed.input_bus_names.clear();
    plugin->fixed.input_bus_names.reserve(numInputBuses);
    for (int32 b = 0; b < numInputBuses; ++b) {
        plugin->component->activateBus(kAudio, kInput, b, true);
        BusInfo busInfo;
        int32 channelCount = 0;
        std::string name;
        if (plugin->component->getBusInfo(kAudio, kInput, b, busInfo) == kResultOk) {
            channelCount = busInfo.channelCount;
            name = utf16_to_utf8(busInfo.name);
        }
        plugin->fixed.input_bus_channel_counts.push_back(channelCount);
        plugin->fixed.input_bus_names.push_back(std::move(name));
        if (b == 0) {
            plugin->fixed.num_input_channels = channelCount;
        }
    }

    plugin->fixed.output_bus_channel_counts.clear();
    plugin->fixed.output_bus_channel_counts.reserve(numOutputBuses);
    plugin->fixed.output_bus_names.clear();
    plugin->fixed.output_bus_names.reserve(numOutputBuses);
    for (int32 b = 0; b < numOutputBuses; ++b) {
        plugin->component->activateBus(kAudio, kOutput, b, true);
        BusInfo busInfo;
        int32 channelCount = 0;
        std::string name;
        if (plugin->component->getBusInfo(kAudio, kOutput, b, busInfo) == kResultOk) {
            channelCount = busInfo.channelCount;
            name = utf16_to_utf8(busInfo.name);
        }
        plugin->fixed.output_bus_channel_counts.push_back(channelCount);
        plugin->fixed.output_bus_names.push_back(std::move(name));
        if (b == 0) {
            plugin->fixed.num_output_channels = channelCount;
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
    plugin->fixed.emits_midi = numEventOutputBuses > 0;

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
        }

        pump_runloop(5.0);
    }

    // The controller is told the component's state whether or not an initial
    // state was applied: a plugin with none starts in a state of its own that
    // the controller has not been shown either.
    sync_controller_with_component(plugin);
    if (initial_state && initial_state_size > sizeof(uint32_t)) {
        apply_controller_part(plugin, initial_state, initial_state_size);
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
    plugin->proc.process_data.prepare(*plugin->component, 0, kSample32);

    // Allocate a silent dummy buffer for aux output buses we don't capture,
    // and a zeroed silence buffer for input buses no caller feeds (see
    // reset_bus_bindings for why these must be two distinct buffers rather
    // than one shared between input and output).
    plugin->proc.dummy_buf.assign(max_block_size, 0.0f);
    plugin->proc.input_silence_buf.assign(max_block_size, 0.0f);

    // Point every output bus at the dummy buffer and every input bus at the
    // silence buffer by default; rack_vst3_plugin_process[_multi_bus] repeat
    // this same reset on every call so a bus a caller previously bound real
    // audio into, then later omits, can never keep pointing at that stale
    // memory (see reset_bus_bindings's own doc comment).
    reset_bus_bindings(plugin);

    // Set up the process context once; tempo/kPlaying and the position
    // fields start at this placeholder 120 BPM/playing/position-zero default
    // and are kept live afterward by rack_vst3_plugin_set_transport, which
    // writes directly into this same persistent struct (never rebuilt) on
    // every real host tempo/transport/position change.
    //
    // Only the flags this library can actually back with host-supplied data
    // are advertised. kTimeSigValid, kSmpteValid and kClockValid are
    // deliberately NOT set: nothing in this C API carries a time signature,
    // an SMPTE offset/frame rate or a MIDI clock distance, so claiming they
    // are valid would point a plugin that trusts them at a placeholder (the
    // 4/4 below) or at memset-zero garbage (a 0 fps frame rate, a
    // "next MIDI clock is this instant" 0). Unclaimed lets the plugin fall
    // back to its own defaults instead. The 4/4 is still written so a plugin
    // that reads the fields without checking the flag -- a common enough
    // real-world sloppiness -- finds a sane time signature rather than 0/0.
    memset(&plugin->proc.process_context, 0, sizeof(ProcessContext));
    plugin->proc.process_context.state =
        ProcessContext::kPlaying |
        ProcessContext::kTempoValid |
        ProcessContext::kProjectTimeMusicValid |
        ProcessContext::kBarPositionValid;
    plugin->proc.process_context.sampleRate = sample_rate;
    plugin->proc.process_context.tempo = 120.0;
    plugin->proc.process_context.timeSigNumerator = 4;
    plugin->proc.process_context.timeSigDenominator = 4;
    plugin->proc.process_data.processContext = &plugin->proc.process_context;

    // Build the parameter list: the ids into `fixed`, the descriptions into
    // `ctrl`, so a later refresh of the descriptions never rewrites the ids
    // the audio thread reads.
    if (plugin->controller) {
        read_parameter_list(plugin->controller, plugin->fixed.param_ids, plugin->ctrl.param_meta);

        // Pre-create one ParameterValueQueue per known parameter now, up
        // front, rather than letting ParameterChanges::addParameterData grow
        // its `queues` vector lazily on first use. addParameterData only
        // takes its allocation-free reuse branch (queues[usedQueueCount]) once
        // usedQueueCount < queues.size(); clearQueue() resets usedQueueCount
        // to 0 every process() call but never shrinks queues, so without this,
        // the *first* process block in which the number of distinct
        // parameters queued exceeds every previous block's count still
        // allocates -- including, for output_param_changes, an allocation
        // triggered by the plugin's own internal code while already running
        // on the audio thread inside process(). Both queues are sized to the
        // full known parameter count so rack_vst3_plugin_queue_parameter_change
        // (which only ever queues indices already validated against
        // fixed.param_ids) can genuinely guarantee no allocation once
        // initialize() has returned.
        plugin->proc.input_param_changes.setMaxParameters(static_cast<int32>(plugin->fixed.param_ids.size()));
        plugin->proc.output_param_changes.setMaxParameters(static_cast<int32>(plugin->fixed.param_ids.size()));

        // What the audio thread records of the values the plugin reports, sized now for the
        // same reason: recording must never allocate. The hand-off part, the id map and the
        // binding of the handler's edit forwarding that points into them are set up by the first
        // initialize only: a plugin's own thread may already be calling performEdit, which reads
        // them, and an instance's parameter list does not change between initializes.
        if (!plugin->handoff.reported) {
            size_t parameter_count = plugin->fixed.param_ids.size();
            plugin->handoff.host_write_gen.reset(new std::atomic<uint32_t>[parameter_count]());
            plugin->handoff.reported.reset(new std::atomic<uint64_t>[parameter_count]);
            plugin->handoff.meters.reset(new std::atomic<uint64_t>[parameter_count]);
            for (size_t index = 0; index < parameter_count; ++index) {
                plugin->handoff.reported[index].store(RackVST3Plugin::kNoReport, std::memory_order_relaxed);
                plugin->handoff.meters[index].store(RackVST3Plugin::kNoReport, std::memory_order_relaxed);
            }
            plugin->proc.delivered_gen.assign(parameter_count, 0);
            plugin->fixed.read_only.assign(parameter_count, 0);
            for (size_t index = 0; index < parameter_count && index < plugin->ctrl.param_meta.size(); ++index) {
                plugin->fixed.read_only[index] =
                    (plugin->ctrl.param_meta[index].flags & ParameterInfo::kIsReadOnly) ? 1 : 0;
            }
            plugin->fixed.ids_by_id.clear();
            for (size_t index = 0; index < parameter_count; ++index) {
                plugin->fixed.ids_by_id.emplace_back(plugin->fixed.param_ids[index],
                                                     static_cast<uint32_t>(index));
            }
            std::sort(plugin->fixed.ids_by_id.begin(), plugin->fixed.ids_by_id.end());
            // A separate controller is told of a plugin's own edits by performEdit alone, and the
            // processor must hear them from the host; a component that is its own controller
            // applies its own edits, and is left alone.
            if (plugin->ctrl.component_handler && plugin->ctrl.component_proxy) {
                plugin->ctrl.component_handler->bind(&plugin->fixed.ids_by_id, parameter_count,
                                                     &plugin->handoff.write_generation,
                                                     plugin->handoff.host_write_gen.get());
            }
        }
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
                        plugin->fixed.presets.push_back(preset);
                    }
                }
            }
        }
    }

    plugin->fixed.initialized = true;
    return RACK_VST3_OK;
}

int rack_vst3_plugin_is_initialized(RackVST3Plugin* plugin) {
    return (plugin && plugin->fixed.initialized) ? 1 : 0;
}

int rack_vst3_plugin_reset(RackVST3Plugin* plugin) {
    // Check null pointer BEFORE acquiring mutex (no lock needed if null)
    if (!plugin) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // Acquire mutex BEFORE checking state to prevent TOCTOU race condition
    // Without this, another thread could change initialized between check and lock
    std::lock_guard<std::mutex> lock(rack_vst3_module_load_mutex());

    if (!plugin->fixed.initialized || !plugin->component) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // Whatever the plugin reported before a reset describes what it was before it.
    discard_reported_values(plugin);

    // VST3 doesn't have a direct "reset" like AudioUnit; deactivating and
    // reactivating the component clears its internal buffers/delay lines.
    // The teardown half must leave Processing state before deactivating --
    // the same setProcessing(false)-before-setActive(false) order every
    // other suspend point in this file uses (see suspend_for_state) -- and
    // reset() must leave the plugin back in the same "ready to process"
    // state initialize() itself leaves it in (Processing already
    // restarted), since a caller may call process() immediately after a
    // successful reset() and the VST3 spec does not define that call's
    // behavior outside Processing state.
    suspend_for_state(plugin);

    if (plugin->component->setActive(true) != kResultOk) {
        return RACK_VST3_ERROR_GENERIC;
    }

    if (plugin->processor->setProcessing(true) != kResultOk) {
        plugin->component->setActive(false);
        return RACK_VST3_ERROR_GENERIC;
    }

    return RACK_VST3_OK;
}

int rack_vst3_plugin_get_input_channels(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->fixed.initialized) {
        return 0;
    }
    return plugin->fixed.num_input_channels;
}

int rack_vst3_plugin_get_output_channels(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->fixed.initialized) {
        return 0;
    }
    return plugin->fixed.num_output_channels;
}

// getLatencySamples() is documented [UI-thread & Setup Done] by the VST3 SDK
// (ivstaudioprocessor.h) -- valid any time after setupProcessing, which
// initialize() already calls before setting plugin->fixed.initialized, so gating
// on plugin->fixed.initialized alone (same as get_input_channels/get_output_channels
// above) is sufficient; no separate "setup done" flag is needed.
uint32_t rack_vst3_plugin_get_latency_samples(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->fixed.initialized || !plugin->processor) {
        return 0;
    }
    return plugin->processor->getLatencySamples();
}

// Bus counts, bus names and emits_midi answer from `fixed` once the instance
// is initialized, so the controller half reads them with no plugin call; the
// live IComponent query is made only before initialize, which only the whole
// instance can be.
int rack_vst3_plugin_get_output_bus_count(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->component) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (plugin->fixed.initialized) {
        return static_cast<int>(plugin->fixed.output_bus_channel_counts.size());
    }
    return plugin->component->getBusCount(kAudio, kOutput);
}

int rack_vst3_plugin_get_input_bus_count(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->component) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (plugin->fixed.initialized) {
        return static_cast<int>(plugin->fixed.input_bus_channel_counts.size());
    }
    return plugin->component->getBusCount(kAudio, kInput);
}

bool rack_vst3_plugin_emits_midi(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->component) {
        return false;
    }
    if (plugin->fixed.initialized) {
        return plugin->fixed.emits_midi;
    }
    return plugin->component->getBusCount(kEvent, kOutput) > 0;
}

// Copies a UTF-8 name into a caller-supplied buffer, truncating to fit and
// always terminating it.
static void copy_name(const std::string& name, char* name_out, size_t name_out_size) {
    if (name_out && name_out_size > 0) {
        size_t copy_len = std::min(name.size(), name_out_size - 1);
        memcpy(name_out, name.data(), copy_len);
        name_out[copy_len] = '\0';
    }
}

// Shared implementation for rack_vst3_plugin_get_{output,input}_bus_info:
// looks up `bus_index` in the given direction and copies its channel count /
// UTF-8 name out to the caller-supplied buffers -- from `fixed` once
// initialized, live from IComponent::getBusInfo before. Read-only: does not
// activate the bus.
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

    if (plugin->fixed.initialized) {
        const auto& counts = direction == kInput ? plugin->fixed.input_bus_channel_counts
                                                 : plugin->fixed.output_bus_channel_counts;
        const auto& names = direction == kInput ? plugin->fixed.input_bus_names
                                                : plugin->fixed.output_bus_names;
        if (bus_index < 0 || static_cast<size_t>(bus_index) >= counts.size()) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
        if (channels_out) {
            *channels_out = static_cast<uint32_t>(counts[static_cast<size_t>(bus_index)]);
        }
        copy_name(names[static_cast<size_t>(bus_index)], name_out, name_out_size);
        return RACK_VST3_OK;
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
    copy_name(utf16_to_utf8(bus_info.name), name_out, name_out_size);
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

// Keep, for the controller half, the last value the plugin reported for each parameter through
// the output parameter changes of the block just processed, stamped with the generation of the
// value the plugin was last given for it (see Handoff). Runs on the audio thread: it reads the
// plugin's queues and writes preallocated atomics, and allocates, locks and waits for nothing. A
// parameter whose id the controller does not list is ignored; a read-only one (a meter that
// reports every block) is kept apart, to be forwarded to the controller and never counted as a
// change; a value that is not finite is ignored.
static void record_reported_values(RackVST3Plugin* plugin) {
    IParameterChanges& changes = plugin->proc.output_param_changes;
    int32 queue_count = changes.getParameterCount();
    if (queue_count <= 0 || !plugin->handoff.reported) {
        return;
    }
    const auto& ids = plugin->fixed.ids_by_id;
    bool any = false;
    bool any_meter = false;
    for (int32 queue_index = 0; queue_index < queue_count; ++queue_index) {
        IParamValueQueue* queue = changes.getParameterData(queue_index);
        if (!queue) {
            continue;
        }
        int32 point_count = queue->getPointCount();
        if (point_count <= 0) {
            continue;
        }
        int32 offset = 0;
        ParamValue value = 0.0;
        if (queue->getPoint(point_count - 1, offset, value) != kResultOk || !std::isfinite(value)) {
            continue;
        }
        auto found = std::lower_bound(ids.begin(), ids.end(),
                                      std::make_pair(queue->getParameterId(), uint32_t{0}));
        if (found == ids.end() || found->first != queue->getParameterId()) {
            continue;
        }
        uint32_t index = found->second;
        float narrowed = static_cast<float>(value);
        uint32_t bits = 0;
        std::memcpy(&bits, &narrowed, sizeof(bits));
        if (plugin->fixed.read_only[index]) {
            plugin->handoff.meters[index].store(bits, std::memory_order_release);
            any_meter = true;
            continue;
        }
        uint64_t stamped = (uint64_t{plugin->proc.delivered_gen[index]} << 32) | bits;
        plugin->handoff.reported[index].store(stamped, std::memory_order_release);
        any = true;
    }
    if (any) {
        plugin->handoff.reported_any.store(true, std::memory_order_release);
    }
    if (any_meter) {
        plugin->handoff.meters_any.store(true, std::memory_order_release);
    }
}

// Forget every value the plugin reported and nobody took. For the functions of the whole
// instance that replace what the plugin is: a state write, a preset load, a reset. Reports made
// before one describe a plugin that no longer exists, and taking them afterwards would put the
// old values back on the controller. Nothing processes while the instance is whole.
static void discard_reported_values(RackVST3Plugin* plugin) {
    if (!plugin->handoff.reported) {
        return;
    }
    size_t parameter_count = plugin->fixed.param_ids.size();
    for (size_t index = 0; index < parameter_count; ++index) {
        plugin->handoff.reported[index].store(RackVST3Plugin::kNoReport, std::memory_order_relaxed);
        plugin->handoff.meters[index].store(RackVST3Plugin::kNoReport, std::memory_order_relaxed);
        // The plugin is a new one: no host write is outstanding for it and it has been given none.
        plugin->handoff.host_write_gen[index].store(0, std::memory_order_relaxed);
        plugin->proc.delivered_gen[index] = 0;
    }
    plugin->handoff.reported_any.store(false, std::memory_order_release);
    plugin->handoff.meters_any.store(false, std::memory_order_release);
    if (plugin->ctrl.component_handler) {
        plugin->ctrl.component_handler->discard_edits();
    }
}

int rack_vst3_plugin_process(
    RackVST3Plugin* plugin,
    const float* const* inputs,
    uint32_t num_input_channels,
    float* const* outputs,
    uint32_t num_output_channels,
    uint32_t frames)
{
    if (!plugin || !plugin->fixed.initialized || !plugin->processor) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // Validate input parameters to prevent buffer overruns
    // Channel counts must match what was configured during initialization
    if (num_input_channels != static_cast<uint32_t>(plugin->fixed.num_input_channels)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (num_output_channels != static_cast<uint32_t>(plugin->fixed.num_output_channels)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    // Frame count must not exceed the max block size configured during initialization
    if (frames > plugin->fixed.max_block_size) {
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
    plugin->proc.process_data.numSamples = frames;

    // Reset every bus to its default (silent/dummy) binding before applying
    // this call's own bus-0 buffers below. This function only ever rebinds
    // bus 0, so without this reset any bus a previous
    // rack_vst3_plugin_process_multi_bus call bound to a caller-owned buffer
    // would keep pointing at it here -- see reset_bus_bindings's own doc
    // comment.
    reset_bus_bindings(plugin);

    // Set input buffers — write pointers INTO the prepare()-allocated float**
    // array rather than replacing the array pointer. This avoids freeing
    // Rust-owned memory in HostProcessData::destroyBuffers().
    if (num_input_channels > 0 && inputs) {
        AudioBusBuffers& bus = plugin->proc.process_data.inputs[0];
        for (uint32_t ch = 0; ch < num_input_channels && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = const_cast<float*>(inputs[ch]);
        }
    }

    // Set output buffers the same way.
    if (num_output_channels > 0 && outputs) {
        AudioBusBuffers& bus = plugin->proc.process_data.outputs[0];
        for (uint32_t ch = 0; ch < num_output_channels && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = outputs[ch];
        }
    }

    // Set parameter and event interfaces
    plugin->proc.process_data.inputParameterChanges = &plugin->proc.input_param_changes;
    plugin->proc.process_data.outputParameterChanges = &plugin->proc.output_param_changes;
    plugin->proc.process_data.inputEvents = &plugin->proc.input_events;
    plugin->proc.process_data.outputEvents = &plugin->proc.output_events;

    // Process
    tresult result = plugin->processor->process(plugin->proc.process_data);

    // Clear input/output events and parameter changes for next call
    plugin->proc.input_events.clear();
    plugin->proc.input_param_changes.clearQueue();
    plugin->proc.output_events.clear();
    record_reported_values(plugin);
    plugin->proc.output_param_changes.clearQueue();

    return (result == kResultOk) ? RACK_VST3_OK : RACK_VST3_ERROR_GENERIC;
}

// Scale a VST3 normalized [0.0, 1.0] value to a MIDI 1.0 data byte [0, 127],
// the exact inverse of rack_midi_to_vst3_event's `data2 / 127.0f` encode. Rounds to nearest
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
// inverse of rack_midi_to_vst3_event. Pure: it reads only the
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
            // Inverse of rack_midi_to_vst3_event's controlNumber convention,
            // which is the SDK's own ControllerNumbers (ivstmidicontrollers.h):
            // numbers below kAfterTouch are real MIDI controllers, and
            // kAfterTouch, kPitchBend and kCtrlProgramChange stand for
            // Channel Pressure, Pitch Bend and Program Change.
            const LegacyMIDICCOutEvent& cc = event.midiCCOut;
            out->channel = static_cast<uint8_t>(cc.channel);
            const uint8_t value = static_cast<uint8_t>(cc.value);
            const uint8_t value2 = static_cast<uint8_t>(cc.value2);
            if (cc.controlNumber < kAfterTouch) {
                out->status = 0xB0;               // Control Change
                out->data1 = cc.controlNumber;
                out->data2 = value;
                return true;
            }
            switch (cc.controlNumber) {
                case kCtrlProgramChange:          // Program Change
                    out->status = 0xC0;
                    out->data1 = value;
                    out->data2 = 0;
                    return true;
                case kAfterTouch:                 // Channel Pressure
                    out->status = 0xD0;
                    out->data1 = value;
                    out->data2 = 0;
                    return true;
                case kPitchBend:                  // Pitch Bend (LSB, MSB)
                    out->status = 0xE0;
                    out->data1 = value;           // LSB
                    out->data2 = value2;          // MSB
                    return true;
                default:
                    return false;                 // no MIDI 1.0 message of this number
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
    if (!plugin || !plugin->fixed.initialized || !plugin->processor) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    // Validate bus-0 (main) parameters exactly as rack_vst3_plugin_process does.
    if (num_input_channels != static_cast<uint32_t>(plugin->fixed.num_input_channels)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (num_output_channels != static_cast<uint32_t>(plugin->fixed.num_output_channels)) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (frames > plugin->fixed.max_block_size) {
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
            static_cast<size_t>(extra.bus_index) >= plugin->fixed.output_bus_channel_counts.size()) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
        if (extra.channel_count !=
            static_cast<uint32_t>(plugin->fixed.output_bus_channel_counts[static_cast<size_t>(extra.bus_index)])) {
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
            static_cast<size_t>(extra.bus_index) >= plugin->fixed.input_bus_channel_counts.size()) {
            return RACK_VST3_ERROR_INVALID_PARAM;
        }
        if (extra.channel_count !=
            static_cast<uint32_t>(plugin->fixed.input_bus_channel_counts[static_cast<size_t>(extra.bus_index)])) {
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
    plugin->proc.process_data.numSamples = frames;

    // Reset every bus to its default (silent/dummy) binding before applying
    // this call's own bus-0 and extra-bus buffers below, so any bus this
    // particular call's request omits -- one a previous call bound a real
    // buffer into, or an input bus no caller has ever fed -- ends up
    // pointing at shared silent/dummy storage rather than a stale
    // caller-owned pointer. See reset_bus_bindings's own doc comment.
    reset_bus_bindings(plugin);

    // Set input buffers — write pointers INTO the prepare()-allocated float**
    // array rather than replacing the array pointer. This avoids freeing
    // Rust-owned memory in HostProcessData::destroyBuffers().
    if (num_input_channels > 0 && inputs) {
        AudioBusBuffers& bus = plugin->proc.process_data.inputs[0];
        for (uint32_t ch = 0; ch < num_input_channels && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = const_cast<float*>(inputs[ch]);
        }
    }

    // Set bus-0 (main) output buffers the same way.
    if (num_output_channels > 0 && outputs) {
        AudioBusBuffers& bus = plugin->proc.process_data.outputs[0];
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
        AudioBusBuffers& bus = plugin->proc.process_data.outputs[extra.bus_index];
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
        AudioBusBuffers& bus = plugin->proc.process_data.inputs[extra.bus_index];
        for (uint32_t ch = 0; ch < extra.channel_count && ch < (uint32_t)bus.numChannels; ++ch) {
            bus.channelBuffers32[ch] = const_cast<float*>(extra.channels[ch]);
        }
    }

    // Set parameter and event interfaces
    plugin->proc.process_data.inputParameterChanges = &plugin->proc.input_param_changes;
    plugin->proc.process_data.outputParameterChanges = &plugin->proc.output_param_changes;
    plugin->proc.process_data.inputEvents = &plugin->proc.input_events;
    plugin->proc.process_data.outputEvents = &plugin->proc.output_events;

    // Process. A single VST3 process() call computes every activated bus at
    // once, so this one call produces bus 0's audio plus every extra bus
    // rebound above.
    tresult result = plugin->processor->process(plugin->proc.process_data);

    // Capture the plugin's own emitted MIDI output events (e.g. from an
    // arpeggiator) BEFORE they are cleared below. Skipped entirely when the
    // caller passes NULL for output_events_out, so audio-only callers see
    // byte-for-byte identical behavior. `produced` counts every decodable
    // event (the true total), even past output_events_capacity, so the caller
    // can detect truncation; only the first output_events_capacity are written.
    size_t produced = 0;
    if (output_events_out) {
        int32 event_count = plugin->proc.output_events.getEventCount();
        for (int32 i = 0; i < event_count; ++i) {
            Event vst3_event;
            if (plugin->proc.output_events.getEvent(i, vst3_event) != kResultOk) {
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
    plugin->proc.input_events.clear();
    plugin->proc.input_param_changes.clearQueue();
    plugin->proc.output_events.clear();
    record_reported_values(plugin);
    plugin->proc.output_param_changes.clearQueue();

    return (result == kResultOk) ? RACK_VST3_OK : RACK_VST3_ERROR_GENERIC;
}

// ============================================================================
// Parameter API
// ============================================================================

int rack_vst3_plugin_parameter_count(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->controller) {
        return 0;
    }
    return static_cast<int>(plugin->fixed.param_ids.size());
}

int rack_vst3_plugin_get_parameter(RackVST3Plugin* plugin, uint32_t index, float* value) {
    if (!plugin || !plugin->controller || !value) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    ParamID param_id = plugin->fixed.param_ids[index];
    ParamValue normalized = plugin->controller->getParamNormalized(param_id);
    *value = static_cast<float>(normalized);

    return RACK_VST3_OK;
}

int rack_vst3_plugin_set_parameter(RackVST3Plugin* plugin, uint32_t index, float value) {
    if (!plugin || !plugin->controller) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Clamp normalized value to 0.0-1.0
    // VST3 uses normalized parameter values (0.0-1.0 range)
    // This matches AudioUnit behavior for consistency across plugin formats
    if (value < 0.0f) value = 0.0f;
    if (value > 1.0f) value = 1.0f;

    ParamID param_id = plugin->fixed.param_ids[index];

    // Set parameter value on controller (for UI reflection)
    plugin->controller->setParamNormalized(param_id, value);

    // Queue parameter change for component notification in next process() call
    // This ensures the component receives the parameter change properly
    int32 queue_index = 0;
    IParamValueQueue* queue = plugin->proc.input_param_changes.addParameterData(param_id, queue_index);
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
    IParamValueQueue* queue = plugin->proc.input_param_changes.addParameterData(param_id, queue_index);
    if (queue) {
        int32 point_index = 0;
        queue->addPoint(0, value, point_index);
    }
    return RACK_VST3_OK;
}

// The controller half's parameter write: setParamNormalized and nothing else.
// The processor's input_param_changes is the audio thread's own queue, read
// and cleared by process(); writing it from here while the instance is split
// would race that. The processor learns the value through
// rack_vst3_plugin_queue_parameter_change on its own thread.
int rack_vst3_plugin_controller_set_parameter(RackVST3Plugin* plugin, uint32_t index, float value) {
    if (!plugin || !plugin->controller) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (value < 0.0f) value = 0.0f;
    if (value > 1.0f) value = 1.0f;
    plugin->controller->setParamNormalized(plugin->fixed.param_ids[index], value);
    return RACK_VST3_OK;
}

// See this function's own doc comment in rack_vst3.h: unlike
// rack_vst3_plugin_set_parameter[_by_id] above, this deliberately never
// touches plugin->controller (no setParamNormalized call) -- only queues the
// point into input_param_changes, the exact same queue
// rack_vst3_plugin_process[_multi_bus] threads into
// ProcessData::inputParameterChanges and clears after every call. That
// makes this function real-time-safe and the two functions' controller-vs-
// audio-thread scope the only difference between them.
int rack_vst3_plugin_queue_parameter_change(RackVST3Plugin* plugin, uint32_t index, float value) {
    if (!plugin || !plugin->controller) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (value < 0.0f) value = 0.0f;
    if (value > 1.0f) value = 1.0f;

    ParamID param_id = plugin->fixed.param_ids[index];

    int32 queue_index = 0;
    IParamValueQueue* queue = plugin->proc.input_param_changes.addParameterData(param_id, queue_index);
    if (queue) {
        int32 point_index = 0;
        queue->addPoint(0, value, point_index);
    }

    return RACK_VST3_OK;
}

int rack_vst3_plugin_queue_parameter_change_stamped(RackVST3Plugin* plugin, uint32_t index, float value,
                                                    uint32_t generation) {
    int result = rack_vst3_plugin_queue_parameter_change(plugin, index, value);
    if (result == RACK_VST3_OK) {
        plugin->proc.delivered_gen[index] = generation;
    }
    return result;
}

uint32_t rack_vst3_plugin_begin_host_write(RackVST3Plugin* plugin, uint32_t index) {
    if (!plugin || !plugin->handoff.host_write_gen || index >= plugin->fixed.param_ids.size()) {
        return 0;
    }
    uint32_t generation = plugin->handoff.write_generation.fetch_add(1, std::memory_order_acq_rel) + 1;
    if (generation == 0) {
        // The counter wrapped; 0 means "none", so the wrap takes the next one.
        generation = plugin->handoff.write_generation.fetch_add(1, std::memory_order_acq_rel) + 1;
    }
    plugin->handoff.host_write_gen[index].store(generation, std::memory_order_release);
    return generation;
}

uint32_t rack_vst3_plugin_get_midi_pc_param_id(RackVST3Plugin* plugin, int channel) {
    if (!plugin || !plugin->controller) return kNoParamId;
    IPtr<IMidiMapping> midiMapping = U::cast<IMidiMapping>(plugin->controller);
    if (!midiMapping) return kNoParamId;
    ParamID pid = kNoParamId;
    midiMapping->getMidiControllerAssignment(0, channel, kCtrlProgramChange, pid);
    return pid;
}

void rack_vst3_plugin_discarded_messages(RackVST3Plugin* plugin, uint64_t* component_to_controller,
                                         uint64_t* controller_to_component) {
    uint64_t from_component = 0;
    uint64_t from_controller = 0;
    if (plugin && plugin->ctrl.component_proxy) {
        from_component = plugin->ctrl.component_proxy->discarded();
    }
    if (plugin && plugin->ctrl.controller_proxy) {
        from_controller = plugin->ctrl.controller_proxy->discarded();
    }
    if (component_to_controller) {
        *component_to_controller = from_component;
    }
    if (controller_to_component) {
        *controller_to_component = from_controller;
    }
}

uint32_t rack_vst3_plugin_take_restart_flags(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->ctrl.component_handler) {
        return 0;
    }
    return plugin->ctrl.component_handler->take_restart_flags();
}

uint32_t rack_vst3_plugin_take_value_changes(RackVST3Plugin* plugin, uint32_t* reported_indices,
                                             size_t capacity, size_t* reported_count) {
    if (reported_count) {
        *reported_count = 0;
    }
    if (!plugin || !plugin->controller) {
        return 0;
    }
    size_t listed = 0;
    uint32_t changes = 0;
    if (plugin->handoff.reported_any.exchange(false, std::memory_order_acq_rel)) {
        size_t parameter_count = plugin->fixed.param_ids.size();
        for (size_t index = 0; index < parameter_count; ++index) {
            uint64_t report = plugin->handoff.reported[index].exchange(RackVST3Plugin::kNoReport,
                                                                     std::memory_order_acq_rel);
            if (report == RackVST3Plugin::kNoReport) {
                continue;
            }
            // Older than the newest host write: made by a plugin that had not been given it.
            uint32_t stamp = static_cast<uint32_t>(report >> 32);
            uint32_t newest = plugin->handoff.host_write_gen[index].load(std::memory_order_acquire);
            if (static_cast<int32_t>(stamp - newest) < 0) {
                continue;
            }
            uint32_t bits = static_cast<uint32_t>(report);
            float narrowed = 0.0f;
            std::memcpy(&narrowed, &bits, sizeof(narrowed));
            plugin->controller->setParamNormalized(plugin->fixed.param_ids[index], narrowed);
            changes |= RACK_VST3_VALUES_REPORTED;
            if (reported_indices && listed < capacity) {
                reported_indices[listed++] = static_cast<uint32_t>(index);
            }
        }
    }
    if (plugin->handoff.meters_any.exchange(false, std::memory_order_acq_rel)) {
        size_t parameter_count = plugin->fixed.param_ids.size();
        for (size_t index = 0; index < parameter_count; ++index) {
            uint64_t meter = plugin->handoff.meters[index].exchange(RackVST3Plugin::kNoReport,
                                                                  std::memory_order_acq_rel);
            if (meter == RackVST3Plugin::kNoReport) {
                continue;
            }
            uint32_t bits = static_cast<uint32_t>(meter);
            float narrowed = 0.0f;
            std::memcpy(&narrowed, &bits, sizeof(narrowed));
            // Forwarded so a plugin's own window moves its meters; not a change of its values.
            plugin->controller->setParamNormalized(plugin->fixed.param_ids[index], narrowed);
        }
    }
    if (plugin->ctrl.component_handler) {
        if (plugin->ctrl.component_handler->take_edited()) {
            changes |= RACK_VST3_VALUES_EDITED;
        }
        if (plugin->ctrl.component_handler->take_values_changed_request()) {
            changes |= RACK_VST3_VALUES_REQUESTED;
        }
    }
    if (reported_count) {
        *reported_count = listed;
    }
    return changes;
}

size_t rack_vst3_plugin_take_edits(RackVST3Plugin* plugin, uint32_t* indices, float* values, size_t capacity) {
    if (!plugin || !plugin->ctrl.component_handler || !indices || !values) {
        return 0;
    }
    return plugin->ctrl.component_handler->take_edits(indices, values, capacity);
}

bool rack_vst3_plugin_value_changes_pending(RackVST3Plugin* plugin) {
    if (!plugin) {
        return false;
    }
    if (plugin->handoff.reported_any.load(std::memory_order_acquire)) {
        return true;
    }
    return plugin->ctrl.component_handler && plugin->ctrl.component_handler->changes_pending();
}

// Re-reads the controller's parameter list into a temporary and compares ids:
// fixed.param_ids is what the processor half resolves indices against on the
// audio thread, so it is never rewritten here. Changed ids mean the plugin
// wants a new instance, and nothing changes.
int rack_vst3_plugin_refresh_parameter_titles(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->controller) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }
    if (!plugin->fixed.initialized) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }
    std::vector<ParamID> ids;
    std::vector<RackVST3Plugin::ParamMeta> meta;
    read_parameter_list(plugin->controller, ids, meta);
    if (ids != plugin->fixed.param_ids) {
        return RACK_VST3_PARAMETER_IDS_CHANGED;
    }
    plugin->ctrl.param_meta = std::move(meta);
    return RACK_VST3_OK;
}

// Writes directly into plugin->proc.process_context (a plain member, pointed to
// once by plugin->proc.process_data.processContext during initialize -- see the
// memset block there -- and never rebuilt), so this call is sufficient on
// its own with no need to touch process_data again. Safe with no
// locking/queuing only because the caller is required (see this function's
// own declaration in rack_vst3.h) to call it from the same thread that
// drives process(), immediately before it -- the same thread-confinement
// argument that already makes plain, unsynchronized access to
// process_context safe within process() itself.
int rack_vst3_plugin_set_transport(
    RackVST3Plugin* plugin,
    double tempo,
    int32_t playing,
    int64_t project_time_samples,
    double project_time_music,
    double bar_position_music)
{
    if (!plugin || !plugin->fixed.initialized) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    plugin->proc.process_context.tempo = tempo;
    if (playing) {
        plugin->proc.process_context.state |= ProcessContext::kPlaying;
    } else {
        plugin->proc.process_context.state &= ~ProcessContext::kPlaying;
    }

    // The host's position, taken verbatim. Nothing else in this library
    // writes these three fields, and in particular process() does not
    // advance projectTimeSamples itself: a counter kept here would restart
    // at zero for every newly constructed or re-initialized instance while
    // the rest of the host is minutes in, and would count only the blocks
    // this one instance happened to process. The host knows where it is;
    // this library does not.
    plugin->proc.process_context.projectTimeSamples = project_time_samples;
    plugin->proc.process_context.projectTimeMusic = project_time_music;
    plugin->proc.process_context.barPositionMusic = bar_position_music;

    return RACK_VST3_OK;
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

    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    const auto& param_info = plugin->ctrl.param_meta[index];

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

    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    ParamID param_id = plugin->fixed.param_ids[index];

    String128 display;
    tresult result = plugin->controller->getParamStringByValue(
        param_id, static_cast<ParamValue>(normalized_value), display);
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

    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    ParamID param_id = plugin->fixed.param_ids[index];

    // normalizedParamToPlain has no documented failure return in the VST3
    // SDK (unlike getParamStringByValue); it always returns a ParamValue.
    // Some plugins return the normalized value unchanged when no real
    // plain-value mapping is implemented, which is itself informative.
    *out_plain = plugin->controller->normalizedParamToPlain(
        param_id, static_cast<ParamValue>(normalized_value));

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

    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    ParamID param_id = plugin->fixed.param_ids[index];

    String128 utf16_string;
    utf8_to_utf16(string, utf16_string, 128);

    ParamValue normalized = 0.0;
    tresult result = plugin->controller->getParamValueByString(
        param_id, utf16_string, normalized);
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

    if (index >= plugin->fixed.param_ids.size()) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    ParamID param_id = plugin->fixed.param_ids[index];

    // plainParamToNormalized has no documented failure return in the VST3
    // SDK (same as its inverse, normalizedParamToPlain above) -- it always
    // returns a ParamValue.
    *out_normalized = static_cast<float>(plugin->controller->plainParamToNormalized(
        param_id, static_cast<ParamValue>(plain_value)));

    return RACK_VST3_OK;
}

// ============================================================================
// Preset Management (Stub - TODO: Implement)
// ============================================================================

int rack_vst3_plugin_get_preset_count(RackVST3Plugin* plugin) {
    if (!plugin || !plugin->fixed.initialized) {
        return 0;
    }
    return static_cast<int>(plugin->fixed.presets.size());
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
    if (!plugin || !plugin->fixed.initialized || !name || name_size == 0 ||
        !category || category_size == 0) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    if (index >= plugin->fixed.presets.size()) {
        return RACK_VST3_ERROR_NOT_FOUND;
    }

    const auto& preset = plugin->fixed.presets[index];

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
    if (!plugin || !plugin->fixed.initialized || !plugin->controller) {
        return RACK_VST3_ERROR_NOT_INITIALIZED;
    }

    if (preset_number < 0 || preset_number >= static_cast<int32_t>(plugin->fixed.presets.size())) {
        return RACK_VST3_ERROR_NOT_FOUND;
    }

    // Reports made before the preset describe the sound it replaces.
    discard_reported_values(plugin);

    const auto& preset = plugin->fixed.presets[preset_number];

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
                IParamValueQueue* queue = plugin->proc.input_param_changes.addParameterData(param_id, queue_index);
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
    if (!plugin->fixed.initialized) return;
    plugin->processor->setProcessing(false);
    plugin->component->setActive(false);
}

// reactivate/resume_processing let a caller opt out of the setActive(true)/
// setProcessing(true) steps while still sharing the pump: read_state's own
// suspend_for_state fully deactivates first (setProcessing(false) then
// setActive(false)), so its resume needs both steps to get back to
// Active+Processing, and takes the (reactivate=true, resume_processing=true)
// defaults below unchanged. rack_vst3_plugin_set_state's own doc comment
// explains why it must never deactivate the component before calling
// IComponent::setState (doing so silently drops some plugins' parameter
// values) -- it stays Active throughout and manages setProcessing itself
// around a call that also has to run before the component is ever marked
// initialized, a case this function's own initialized-only guard below
// does not cover. set_state therefore calls this only for the pump,
// passing reactivate=false, resume_processing=false so the guard and both
// side effects are skipped and only the wait for async plugin work (e.g.
// AD2's background sample loader) after setState runs.
static void resume_after_state(RackVST3Plugin* plugin, double post_activate_pump_seconds,
                                bool reactivate, bool resume_processing) {
    if (!plugin->fixed.initialized && (reactivate || resume_processing)) return;
    if (reactivate) {
        plugin->component->setActive(true);
    }
    if (post_activate_pump_seconds > 0.0) {
        // Let any async work triggered by setState/setActive (e.g. JUCE
        // prepareToPlay, or AD2's background sample loader) complete before
        // resuming processing.
        pump_runloop(post_activate_pump_seconds);
    }
    if (resume_processing) {
        plugin->processor->setProcessing(true);
    }
}

// Writes the instance's state in rack's one state format: a 4-byte
// component-state size, the component's state, then the controller's state
// when the controller is a separate object. Both state reads call this, so
// their formats cannot diverge. Calls only IComponent::getState and
// IEditController::getState, on a stream of the caller's own, so it touches
// none of the processor half's fields. Returns the component's tresult; a
// controller that declines (common for JUCE plugins, whose controller has no
// separate state) leaves the component state alone in the stream.
static tresult write_state(RackVST3Plugin* plugin, MemoryStream* stream) {
    uint32_t size_marker_placeholder = 0;
    stream->write(&size_marker_placeholder, sizeof(size_marker_placeholder), nullptr);

    int64 component_start_pos = 0;
    stream->tell(&component_start_pos);

    tresult result = plugin->component->getState(stream);
    if (result != kResultOk) {
        fprintf(stderr, "[rack] IComponent::getState failed: tresult=0x%08x\n",
                static_cast<unsigned>(result));
        return result;
    }

    int64 component_end_pos = 0;
    stream->tell(&component_end_pos);
    uint32_t component_state_size = static_cast<uint32_t>(component_end_pos - component_start_pos);

    stream->seek(0, IBStream::kIBSeekSet, nullptr);
    stream->write(&component_state_size, sizeof(component_state_size), nullptr);
    stream->seek(component_end_pos, IBStream::kIBSeekSet, nullptr);

    if (plugin->controller &&
        reinterpret_cast<void*>(plugin->controller.get()) !=
        reinterpret_cast<void*>(plugin->component.get())) {
        plugin->controller->getState(stream);
    }
    return kResultOk;
}


// A state blob rack hands to the caller: the bytes of one state read, owned
// here until rack_vst3_state_blob_free.
struct RackVST3StateBlob {
    std::vector<uint8_t> bytes;
};

// Wraps a written stream's bytes in a blob, or reports the failure.
static RackVST3StateBlob* finish_state_read(tresult written, MemoryStream* stream, int& status) {
    if (written != kResultOk) {
        status = RACK_VST3_ERROR_GENERIC;
        return nullptr;
    }
    auto blob = new (std::nothrow) RackVST3StateBlob{stream->takeData()};
    if (!blob) {
        status = RACK_VST3_ERROR_GENERIC;
        return nullptr;
    }
    status = RACK_VST3_OK;
    return blob;
}

RackVST3StateBlob* rack_vst3_plugin_read_state(RackVST3Plugin* plugin, int* error) {
    int ignored = RACK_VST3_OK;
    int& status = error ? *error : ignored;
    if (!plugin || !plugin->component) {
        status = RACK_VST3_ERROR_INVALID_PARAM;
        return nullptr;
    }
    if (!plugin->fixed.initialized) {
        status = RACK_VST3_ERROR_NOT_INITIALIZED;
        return nullptr;
    }

    // Leave the Processing state around getState: the conservative bracket
    // for a plugin not known to keep its own state consistent against
    // process(). One bracket per read, since the stream grows to whatever the
    // plugin writes; a second back-to-back suspend/resume cycle is a pattern
    // that has crashed JUCE plugins (AD2 included).
    suspend_for_state(plugin);
    IPtr<MemoryStream> stream(new MemoryStream(), false);
    tresult written = write_state(plugin, stream);
    resume_after_state(plugin);
    return finish_state_read(written, stream, status);
}

RackVST3StateBlob* rack_vst3_plugin_read_state_while_processing(RackVST3Plugin* plugin, int* error) {
    int ignored = RACK_VST3_OK;
    int& status = error ? *error : ignored;
    if (!plugin || !plugin->component) {
        status = RACK_VST3_ERROR_INVALID_PARAM;
        return nullptr;
    }
    if (!plugin->fixed.initialized) {
        status = RACK_VST3_ERROR_NOT_INITIALIZED;
        return nullptr;
    }

    // No suspend: IComponent::getState is "[UI-thread & (Initialized |
    // Connected | Setup Done | Activated | Processing)]" in the VST3 headers,
    // so the standard allows it while the processor half processes. Whether a
    // given plugin synchronises its own state against process() is the
    // plugin's, and is not knowable here.
    IPtr<MemoryStream> stream(new MemoryStream(), false);
    tresult written = write_state(plugin, stream);
    return finish_state_read(written, stream, status);
}

const uint8_t* rack_vst3_state_blob_data(const RackVST3StateBlob* blob) {
    return blob ? blob->bytes.data() : nullptr;
}

size_t rack_vst3_state_blob_size(const RackVST3StateBlob* blob) {
    return blob ? blob->bytes.size() : 0;
}

void rack_vst3_state_blob_free(RackVST3StateBlob* blob) {
    delete blob;
}

// Identical to rack_vst3_plugin_set_state below except the post-setState
// wait is `pump_seconds` rather than a hardcoded 5.0 -- added so a caller
// that has its own way of detecting real completion (backline's
// audio-probe warmup: deliver a real note/signal and watch for real,
// non-silent output) can ask for a short or zero pump here and do its own
// polling afterward, instead of the fixed blind wait. See
// `docs/plugin-pool-design.md`'s Section 1.5 for why no plugin signals
// completion on its own and no shorter fixed constant can be derived
// instead -- this exists to let the caller supply an empirically-obtained
// duration, or none at all if it means to poll for readiness itself.
int rack_vst3_plugin_set_state_pumped(RackVST3Plugin* plugin, const uint8_t* data, size_t size, double pump_seconds) {
    if (!plugin || !data || size == 0 || !plugin->component) {
        return RACK_VST3_ERROR_INVALID_PARAM;
    }

    // Reports made before this state describe the plugin that held the one before.
    discard_reported_values(plugin);

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

    if (plugin->fixed.initialized) {
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
        if (plugin->fixed.initialized) {
            plugin->processor->setProcessing(true);
        }
        return RACK_VST3_ERROR_GENERIC;
    }

    // Wait for AD2's async sample-loading thread to finish. Routed through
    // resume_after_state (the same helper read_state's own suspend/resume
    // bracket uses for its own, unrelated post-activation pump) so this
    // wait has one implementation, not two hand-copied pump_runloop(5.0)
    // calls -- but the component here was never deactivated (see this
    // function's own comment on why setState must run while Active) and
    // may still be pre-initialize(), so both of resume_after_state's other
    // side effects are opted out of: reactivate=false (already Active,
    // and calling setActive(true) again risks resembling the
    // back-to-back double-activation sequence documented elsewhere in this
    // codebase to crash some JUCE plugins, AD2 included) and
    // resume_processing=false (this function already restores
    // setProcessing itself below, only when plugin->fixed.initialized).
    resume_after_state(plugin, pump_seconds, /*reactivate=*/false, /*resume_processing=*/false);

    // Sync controller to component state (separate controller objects only;
    // see sync_controller_with_component_state).
    sync_controller_with_component_state(plugin, comp_data, component_state_size);
    apply_controller_part(plugin, data, size);

    if (plugin->fixed.initialized) {
        plugin->processor->setProcessing(true);
    }

    return RACK_VST3_OK;
}

int rack_vst3_plugin_set_state(RackVST3Plugin* plugin, const uint8_t* data, size_t size) {
    return rack_vst3_plugin_set_state_pumped(plugin, data, size, 5.0);
}

// ============================================================================
// MIDI API
// ============================================================================

// Encodes one MIDI 1.0 event as a VST3 Event, the exact inverse of
// vst3_event_to_rack_midi. Pure: it reads only `midi_event` and touches no
// plugin state, so it is unit-tested against hand-built events with no plugin
// loaded. Returns true and fills `out` for a status rack knows how to
// deliver; returns false for any other status, which the caller skips.
//
// VST3 has dedicated events for notes and polyphonic pressure. Everything
// else rack delivers as a kLegacyMIDICCOutEvent whose controlNumber is the
// SDK's own ControllerNumbers entry (ivstmidicontrollers.h): the MIDI
// controller number itself below kAfterTouch, kAfterTouch for Channel
// Pressure, kPitchBend for Pitch Bend (value is the LSB, value2 the MSB) and
// kCtrlProgramChange for Program Change. These numbers are not an arbitrary
// private scheme: JUCE, which both hosts and implements most plugins, reads
// and writes the same numbers, so a plugin built on it gets the message the
// sender meant. The SDK documents this event as one a plugin emits, and a host
// that wants a plugin's mapped controllers (CC, pitch bend, channel pressure)
// to act as parameters goes through IMidiMapping instead; rack does not.
//
// Deliberately NOT static: rack-sys's C++ unit test links against this symbol.
bool rack_midi_to_vst3_event(const RackVST3MidiEvent& midi_event, Event* out) {
    if (!out) {
        return false;
    }
    memset(out, 0, sizeof(Event));
    out->sampleOffset = midi_event.sample_offset;
    out->busIndex = 0;

    const auto legacy = [out, &midi_event](uint8_t control_number, uint8_t value, uint8_t value2) {
        out->type = Event::kLegacyMIDICCOutEvent;
        out->midiCCOut.channel = midi_event.channel;
        out->midiCCOut.controlNumber = control_number;
        out->midiCCOut.value = value;
        out->midiCCOut.value2 = value2;
    };

    switch (midi_event.status & 0xF0) {
        case 0x90:  // Note On
            out->type = Event::kNoteOnEvent;
            out->noteOn.channel = midi_event.channel;
            out->noteOn.pitch = midi_event.data1;
            out->noteOn.velocity = static_cast<float>(midi_event.data2) / 127.0f;
            out->noteOn.noteId = -1;  // Not specified
            return true;

        case 0x80:  // Note Off
            out->type = Event::kNoteOffEvent;
            out->noteOff.channel = midi_event.channel;
            out->noteOff.pitch = midi_event.data1;
            out->noteOff.velocity = static_cast<float>(midi_event.data2) / 127.0f;
            out->noteOff.noteId = -1;  // Not specified
            return true;

        case 0xA0:  // Polyphonic Key Pressure
            out->type = Event::kPolyPressureEvent;
            out->polyPressure.channel = midi_event.channel;
            out->polyPressure.pitch = midi_event.data1;
            out->polyPressure.pressure = static_cast<float>(midi_event.data2) / 127.0f;
            return true;

        case 0xB0:  // Control Change: the controller number is the control number
            legacy(midi_event.data1, midi_event.data2, 0);
            return true;

        case 0xC0:  // Program Change
            legacy(kCtrlProgramChange, midi_event.data1, 0);
            return true;

        case 0xD0:  // Channel Pressure
            legacy(kAfterTouch, midi_event.data1, 0);
            return true;

        case 0xE0:  // Pitch Bend: data1 is the LSB, data2 the MSB
            legacy(kPitchBend, midi_event.data1, midi_event.data2);
            return true;

        default:
            return false;
    }
}

int rack_vst3_plugin_send_midi(
    RackVST3Plugin* plugin,
    const RackVST3MidiEvent* events,
    uint32_t event_count)
{
    if (!plugin || !plugin->fixed.initialized) {
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

    // Convert MIDI events to VST3 events. addEvent silently returns
    // kResultFalse (dropping the event) once input_events reaches
    // kEventListCapacity; track how many were dropped so this function can
    // report it rather than let it pass as if every event succeeded.
    uint32_t dropped_count = 0;

    for (uint32_t i = 0; i < event_count; ++i) {
        Event vst3_event;
        if (!rack_midi_to_vst3_event(events[i], &vst3_event)) {
            continue;  // Unknown MIDI event - skip it
        }
        if (plugin->proc.input_events.addEvent(vst3_event) != kResultTrue) {
            ++dropped_count;
        }
    }

    // Every event that fit was already queued above; report truncation
    // rather than silently discarding the events beyond capacity, so a
    // caller flooding the queue faster than process() drains it can find
    // out. This mirrors the equivalent decision documented for
    // rack_vst3_plugin_process_multi_bus's own output-MIDI capture (report
    // truncation via a count, don't fail the whole call retroactively for
    // work already done).
    if (dropped_count > 0) {
        return RACK_VST3_ERROR_EVENT_QUEUE_FULL;
    }

    return RACK_VST3_OK;
}
