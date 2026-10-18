mod ffi;
mod util;
mod scanner;
mod instance;
mod processor;
mod controller;
mod restart;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod fixture_tests;
#[cfg(test)]
mod split_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod controller_tests;

pub use scanner::Vst3Scanner;
pub use instance::{leaked_instance_count, live_instance_count, JoinMismatch, Vst3Plugin};
pub use processor::{ExtraInputBus, ExtraOutputBus, Vst3Processor};
pub use controller::{DiscardedMessages, TitleRefresh, Vst3Controller};
pub use restart::RestartFlags;
