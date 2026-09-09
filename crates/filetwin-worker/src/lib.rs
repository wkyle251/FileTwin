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
    let actual = format!("{:x}", hash.finalize());
    let gpu_expected = artifacts[component][format!("{target}-cuda12")]["library_sha256"].as_str();
    if actual != expected && !(component == "onnxruntime" && gpu_expected == Some(actual.as_str()))
    {
        return Err(filetwin_core::Error::new(
            filetwin_core::ErrorCode::ModelIntegrity,
            "runtime",
            format!("{component} library checksum differs from the pinned native artifact"),
        ));
    }
    Ok(())
}

fn verify_cuda_providers(runtime: &std::path::Path) -> filetwin_core::Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let artifacts: serde_json::Value =
        serde_json::from_str(include_str!("../runtime-artifacts.json"))?;
    let directory = runtime
        .parent()
        .ok_or_else(|| filetwin_core::Error::invalid("Invalid runtime path"))?;
    let providers = artifacts["onnxruntime"]["linux-x86_64-cuda12"]["providers"]
        .as_array()
        .expect("Pinned providers");
    for provider in providers {
        let name = provider["file"].as_str().expect("Pinned filename");
        let mut file = std::fs::File::open(directory.join(name)).map_err(|_| filetwin_core::Error::new(
            filetwin_core::ErrorCode::RuntimeUnavailable, "runtime",
            format!("Missing {name}; provision with scripts/setup-native.py --onnxruntime-variant cuda12")))?;
        if file.metadata()?.len() > 1024 * 1024 * 1024 {
            return Err(filetwin_core::Error::new(
                filetwin_core::ErrorCode::ModelIntegrity,
                "runtime",
                "CUDA provider exceeds its size bound",
            ));
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        if format!("{:x}", hash.finalize()) != provider["sha256"].as_str().expect("Pinned checksum")
        {
            return Err(filetwin_core::Error::new(
                filetwin_core::ErrorCode::ModelIntegrity,
                "runtime",
                format!("{name} checksum differs from the pinned CUDA artifact"),
            ));
        }
    }
    Ok(())
}
