//! FileTwin's local similarity engine. The library never installs signal handlers,
//! changes the working directory, prints, or terminates its embedding process.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("The initial FileTwin implementation targets macOS and Linux.");

pub mod api;
mod catalog;
mod engine;
mod error;
mod local;
mod matching;
mod native;
pub mod profile;
mod scan;
mod store;
mod text;
#[doc(hidden)]
pub mod worker_protocol;

pub use catalog::Catalog;
pub use catalog::capabilities;
pub use engine::{Engine, HostServices, JobHandle};
pub use error::{Error, ErrorCode, Result};
