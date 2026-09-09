//! Directory encoding and portable vector files. The library never installs signal handlers,
//! changes the working directory, prints, or terminates its embedding process.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("The initial FileTwin implementation targets macOS and Linux.");

pub mod api;
mod engine;
mod error;
mod local;
mod native;
pub mod profile;
mod scan;
mod text;
mod vector_file;
#[doc(hidden)]
pub mod worker_protocol;

pub use engine::Encoder;
pub use error::{Error, ErrorCode, Result};
pub use vector_file::{read_vectors, write_vectors};
