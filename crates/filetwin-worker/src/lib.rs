//! Implementation of the internal companion worker. This crate is not a host API;
//! applications embed filetwin-core and configure the companion executable.
pub mod documents;
pub mod media;
pub mod raster;

fn verify_runtime(path: &std::path::Path, component: &str) -> filetwin_core::Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let artifacts: serde_json::Value =
        serde_json::from_str(include_str!("../runtime-artifacts.json"))?;
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let expected = artifacts[component][&target]["library_sha256"]
        .as_str()
        .ok_or_else(|| {
            filetwin_core::Error::new(
                filetwin_core::ErrorCode::RuntimeUnavailable,
                "runtime",
                "No qualified native artifact for this OS/CPU",
            )
        })?;
    let mut file = std::fs::File::open(path)?;
    if file.metadata()?.len() > 128 * 1024 * 1024 {
        return Err(filetwin_core::Error::new(
            filetwin_core::ErrorCode::ModelIntegrity,
            "runtime",
            "Unexpected native library size",
        ));
    }
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(filetwin_core::Error::new(
            filetwin_core::ErrorCode::ModelIntegrity,
            "runtime",
            format!("{component} library checksum differs from the pinned native artifact"),
        ));
    }
    Ok(())
}
