// The host's connection proxy between a plugin's component and its separate
// edit controller.
//
// A VST3 plugin whose component and controller are separate objects talks
// between them by IConnectionPoint::notify. Connected directly, a message the
// component sends from inside process() runs the controller's notify on the
// audio thread, beside whatever the host is asking the controller on the main
// thread at that moment. The standard says messages are a UI-thread affair,
// and the SDK's own host helper (public.sdk's ConnectionProxy, used by
// PlugProvider) answers it the same way this does: each side is connected to
// a proxy that delivers a message only on the thread that made the
// connection, and discards one sent from any other thread.
//
// This proxy differs from the SDK's in two ways. It records the owning thread
// with std::this_thread, so it needs no platform thread checker, and it counts
// what it discards, so a host can report a plugin that sends from the audio
// thread. Discarding rather than queueing for later delivery is deliberate: a
// queue would need allocation or a bound on the audio thread, for a message
// the standard says must not be sent there at all.

#pragma once

#include "pluginterfaces/base/funknown.h"
#include "pluginterfaces/vst/ivstmessage.h"

#include <atomic>
#include <cstdint>
#include <thread>

class RackConnectionProxy : public Steinberg::Vst::IConnectionPoint {
public:
    // A proxy for `source`, owned by the calling thread: the thread that
    // constructs the plugin, which for rack is the thread its controller
    // half belongs to.
    explicit RackConnectionProxy(Steinberg::Vst::IConnectionPoint* source)
        : source_(source), owner_(std::this_thread::get_id()) {
        FUNKNOWN_CTOR
    }

    virtual ~RackConnectionProxy() { FUNKNOWN_DTOR }

    DECLARE_FUNKNOWN_METHODS

    // Connects `source` to this proxy and the proxy to `other`, so what
    // `source` sends reaches `other` through notify below.
    Steinberg::tresult PLUGIN_API connect(Steinberg::Vst::IConnectionPoint* other) override {
        if (!other) {
            return Steinberg::kInvalidArgument;
        }
        if (destination_) {
            return Steinberg::kResultFalse;
        }
        destination_ = other;
        Steinberg::tresult result = source_->connect(this);
        if (result != Steinberg::kResultTrue) {
            destination_ = nullptr;
        }
        return result;
    }

    Steinberg::tresult PLUGIN_API disconnect(Steinberg::Vst::IConnectionPoint* other) override {
        if (!other || other != destination_) {
            return Steinberg::kInvalidArgument;
        }
        if (source_) {
            source_->disconnect(this);
        }
        destination_ = nullptr;
        return Steinberg::kResultTrue;
    }

    // Delivers on the owning thread; anywhere else, counts the message and
    // drops it, touching nothing but the counter, so a plugin's audio thread
    // or its own background thread never reaches the other side.
    Steinberg::tresult PLUGIN_API notify(Steinberg::Vst::IMessage* message) override {
        if (std::this_thread::get_id() != owner_) {
            discarded_.fetch_add(1, std::memory_order_relaxed);
            return Steinberg::kResultFalse;
        }
        if (!destination_) {
            return Steinberg::kResultFalse;
        }
        return destination_->notify(message);
    }

    // How many messages arrived on a thread other than the owner's.
    uint64_t discarded() const { return discarded_.load(std::memory_order_relaxed); }

    // Disconnects from the destination this proxy was connected to, if any.
    void disconnect_destination() {
        if (destination_) {
            disconnect(destination_);
        }
    }

private:
    Steinberg::IPtr<Steinberg::Vst::IConnectionPoint> source_;
    Steinberg::IPtr<Steinberg::Vst::IConnectionPoint> destination_;
    std::thread::id owner_;
    std::atomic<uint64_t> discarded_{0};
};

// IMPLEMENT_REFCOUNT and queryInterface are defined in vst3_instance.cpp, the
// one translation unit that uses the proxy.
