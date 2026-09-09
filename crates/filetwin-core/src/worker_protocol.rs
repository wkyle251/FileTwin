//! Internal protocol shared with the native companion; not the public CLI protocol.
use crate::{Result, api::RuntimeConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io::Read, path::PathBuf};

pub const VERSION: u32 = 1;
pub const MAX_RESPONSE: usize = 1024 * 1024;
pub const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_FILE_SECONDS: u64 = 300;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub path: PathBuf,
    pub format: String,
    pub profile_id: String,
    #[serde(default)]
    pub profiles: std::collections::BTreeMap<String, String>,
    pub model_dir: PathBuf,
    pub runtime: RuntimeConfig,
    pub memory_bytes: u64,
    #[serde(default)]
    pub parent_pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Encoded {
    pub family: String,
    pub format: String,
    pub vector: Vec<f32>,
    pub extraction: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u32,
    pub result: Result<Encoded>,
}

pub fn encode_text(reader: impl Read) -> Result<(Vec<f32>, u64)> {
    let e = crate::text::encode(reader, false, &|| Ok(()))?;
    Ok((e.vector, e.characters))
}
