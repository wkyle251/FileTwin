//! Development-only parity harness: the exact Rust preprocessing tensor and
//! inference result are compared with the official TorchScript model by Python.
use filetwin_core::worker_protocol::Request;
use filetwin_worker::raster;
use std::io::{Read, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    let request: Request = serde_json::from_slice(&bytes)?;
    let (rgb, _) = raster::read_rgb(&request)?;
    let tensor = raster::preprocess(&rgb);
    let vector = raster::Sscd::load(&request)?.tensor(tensor.clone())?;
    for value in tensor.into_iter().chain(vector) {
        std::io::stdout().write_all(&value.to_le_bytes())?;
    }
    Ok(())
}
