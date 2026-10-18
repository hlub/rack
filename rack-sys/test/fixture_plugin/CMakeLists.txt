# The rack test fixture plugin, built from the VST3 SDK rack already vendors.
# Built by scripts/build-fixture-plugin.sh, never by build.rs: it is a test
# vehicle, not part of the library.

cmake_minimum_required(VERSION 3.25.0)

option(SMTG_ENABLE_VST3_PLUGIN_EXAMPLES "Enable VST 3 Plug-in Examples" OFF)
option(SMTG_ENABLE_VST3_HOSTING_EXAMPLES "Enable VST 3 Hosting Examples" OFF)
option(SMTG_ENABLE_VSTGUI_SUPPORT "Enable VSTGUI Support" OFF)

if(NOT DEFINED RACK_VST3_SDK)
    set(RACK_VST3_SDK "${CMAKE_CURRENT_SOURCE_DIR}/../../external/vst3sdk")
endif()
set(vst3sdk_SOURCE_DIR "${RACK_VST3_SDK}")

project(rack_fixture_plugin VERSION 1.0.0 LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_subdirectory(${vst3sdk_SOURCE_DIR} ${PROJECT_BINARY_DIR}/vst3sdk)
smtg_enable_vst3_sdk()

smtg_add_vst3plugin(rack-fixture
    src/fixture_ids.h
    src/fixture_component.cpp
    src/fixture_controller.cpp
    src/fixture_factory.cpp
)
target_link_libraries(rack-fixture PRIVATE sdk)

if(SMTG_MAC)
    smtg_target_set_bundle(rack-fixture
        BUNDLE_IDENTIFIER "com.github.hlub.rack.fixture"
        COMPANY_NAME "rack"
    )
endif()
