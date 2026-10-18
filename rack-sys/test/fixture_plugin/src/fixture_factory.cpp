// The fixture's plug-in factory: one effect component and its separate edit
// controller, so rack's tests exercise the two-object case (and the
// component/controller connection) that most real plugins use.

#include "fixture_ids.h"

#include "public.sdk/source/main/pluginfactory.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace RackFixture {
FUnknown* createFixtureComponent (void* context);
FUnknown* createFixtureController (void* context);
}

BEGIN_FACTORY_DEF ("rack", "https://github.com/hlub/rack", "")

	DEF_CLASS2 (INLINE_UID_FROM_FUID (RackFixture::kComponentUID),
	            PClassInfo::kManyInstances,
	            kVstAudioEffectClass,
	            "rack fixture",
	            Vst::kDistributable,
	            "Fx",
	            "1.0.0",
	            kVstVersionString,
	            RackFixture::createFixtureComponent)

	DEF_CLASS2 (INLINE_UID_FROM_FUID (RackFixture::kControllerUID),
	            PClassInfo::kManyInstances,
	            kVstComponentControllerClass,
	            "rack fixture Controller",
	            0,
	            "",
	            "1.0.0",
	            kVstVersionString,
	            RackFixture::createFixtureController)

END_FACTORY
