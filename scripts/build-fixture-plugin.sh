#!/usr/bin/env bash
#
# Build rack's test fixture VST3 plugin from rack-sys/test/fixture_plugin.
#
# The fixture is built from the Steinberg VST3 SDK this repository already
# vendors at rack-sys/external/vst3sdk; nothing new is fetched. It lands in
# target/fixture-plugin, which git ignores, and the tests find it there (or
# wherever RACK_TEST_FIXTURE_VST3 names).
#
# Run once before `cargo test`, after checking out the SDK submodule:
#
#   scripts/build-fixture-plugin.sh
#
# Re-running is cheap: cmake skips everything already up to date. Then run the
# tests with the fixture required, so a missing fixture fails rather than
# skips:
#
#   RACK_REQUIRE_FIXTURE=1 cargo test -- --test-threads=1

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sdk="$root/rack-sys/external/vst3sdk"
build="$root/target/fixture-plugin/build"
bundle="$build/VST3/Release/rack-fixture.vst3"

if [ ! -f "$sdk/CMakeLists.txt" ]; then
    echo "error: the vendored VST3 SDK is missing at $sdk" >&2
    echo "check out the submodules first: git submodule update --init --recursive" >&2
    exit 1
fi

cmake_args=(
    -S "$root/rack-sys/test/fixture_plugin"
    -B "$build"
    -G "Unix Makefiles"
    -DRACK_VST3_SDK="$sdk"
    -DCMAKE_BUILD_TYPE=Release
    -DSMTG_CREATE_PLUGIN_LINK=OFF
    -DSMTG_RUN_VST_VALIDATOR=OFF
)

if [ "$(uname)" = "Darwin" ]; then
    # The SDK's cmake refuses to configure without full Xcode, because it
    # shells out to `xcodebuild -version`. Command Line Tools alone compile
    # it, so the version is supplied directly: the environment variable makes
    # the SDK skip running `xcodebuild`, and the cache variable is what its
    # own version comparison then reads.
    export XCODE_VERSION="${XCODE_VERSION:-16.0}"
    cmake_args+=(
        -DXCODE_VERSION="$XCODE_VERSION"
        -DCMAKE_C_COMPILER=/usr/bin/clang
        -DCMAKE_CXX_COMPILER=/usr/bin/clang++
    )
    jobs="$(sysctl -n hw.ncpu)"
else
    jobs="$(nproc)"
fi

cmake "${cmake_args[@]}"
cmake --build "$build" --target rack-fixture -j "$jobs"

if [ ! -d "$bundle" ]; then
    echo "error: the build reported success but $bundle is missing" >&2
    exit 1
fi

echo "rack fixture plugin built: $bundle"
