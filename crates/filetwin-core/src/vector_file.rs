use crate::{Error, ErrorCode, Result, api::*, local, profile};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Component, Path},
};

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidVectorFile, "vectors", message)
}

pub(crate) fn is_sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn validate(bundle: &VectorFile) -> Result<()> {
    if bundle.format != VECTOR_FILE_FORMAT {
        return Err(invalid("Expected a FileTwin vector file"));
    }
    if bundle.schema_version != SCHEMA_VERSION {
        return Err(Error::new(
            ErrorCode::UnsupportedSchemaVersion,
            "vectors",
            "Unsupported vector-file schema version",
        ));
    }
    if !bundle.directory.to_path_buf()?.is_absolute() {
        return Err(invalid("Vector directory must be an absolute path label"));
    }
    let mut known = HashMap::new();
    let mut paths = std::collections::HashSet::new();
    for record in &bundle.files {
        let path = record.path.to_path_buf()?;
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || !paths.insert(path)
        {
            return Err(invalid("Vector records require unique relative file paths"));
        }
        if record
            .file_id
            .as_ref()
            .is_some_and(|s| !s.strip_prefix("sha256:").is_some_and(is_sha))
        {
            return Err(invalid(
                "file_id must be sha256: followed by 64 lowercase hex digits",
            ));
        }
        if record.state != FileState::Ready {
            if record.vector.is_some()
                || record.vector_sha256.is_some()
                || record.reused
                || record.error.is_none()
            {
                return Err(invalid(
                    "Unavailable files need an error and must not contain a vector or claim reuse",
                ));
            }
            continue;
        }
        let id = record
            .file_id
            .as_ref()
            .ok_or_else(|| invalid("Ready file has no file_id"))?;
        let profile_id = record
            .profile_id
            .as_ref()
            .ok_or_else(|| invalid("Ready file has no profile_id"))?;
        let p = profile::find(profile_id)
            .map_err(|_| invalid("Vector profile is unavailable in this build"))?;
        if record.family.as_deref() != Some(&p.family)
            || record.bytes.is_none()
            || record.error.is_some()
        {
            return Err(invalid("Ready file metadata is inconsistent"));
        }
        let vector = record
            .vector
            .as_ref()
            .ok_or_else(|| invalid("Ready file has no vector"))?;
        let bytes = profile::vector_bytes(vector);
        profile::decode_vector(&bytes, p.dimensions)?;
        let checksum = profile::digest_hex(&bytes);
        if record.vector_sha256.as_deref() != Some(&checksum) {
            return Err(invalid("Vector checksum mismatch"));
        }
        if known
            .insert((id, profile_id), checksum.clone())
            .is_some_and(|old| old != checksum)
        {
            return Err(invalid(
                "Conflicting vectors for the same content and profile",
            ));
        }
    }
    let counts = &bundle.summary.counts;
    let ready = bundle
        .files
        .iter()
        .filter(|f| f.state == FileState::Ready)
        .count() as u64;
    let reused = bundle.files.iter().filter(|f| f.reused).count() as u64;
    let skipped = bundle
        .files
        .iter()
        .filter(|f| f.state == FileState::Skipped)
        .count() as u64;
    let processed = bundle.files.len() as u64;
    if !bundle.summary.elapsed_seconds.is_finite()
        || bundle.summary.elapsed_seconds < 0.0
        || !(1..=64).contains(&bundle.summary.workers)
        || (bundle.summary.cancelled && bundle.complete)
        || counts.files_processed != processed
        || counts.files_ready != ready
        || counts.files_skipped != skipped
        || counts.files_failed != processed - ready - skipped
        || counts.cache_hits != reused
        || counts.vectors_encoded != ready - reused
        || counts.files_discovered < processed
        || counts
            .files_total
            .is_some_and(|n| n != counts.files_discovered)
        || (bundle.complete && counts.files_total != Some(processed))
        || counts.bytes_read < counts.bytes_hashed
    {
        return Err(invalid("Vector summary is inconsistent"));
    }
    Ok(())
}

/// Read and validate the optional result of a previous run. Stored paths are
/// never opened. FileTwin hashes current input bytes before reusing a vector.
pub fn read_vectors(path: &Path) -> Result<VectorFile> {
    let path = local::resolve_root(path)?;
    let file = local::open_secure(&path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_VECTOR_FILE_BYTES {
        return Err(invalid(
            "Vector input must be a regular JSON file of at most 512 MiB",
        ));
    }
    let before = local::Revision::of(&meta);
    let mut reader = BufReader::new((&file).take(MAX_VECTOR_FILE_BYTES + 1));
    let mut decoder = serde_json::Deserializer::from_reader(&mut reader);
    let bundle = <VectorFile as serde::Deserialize>::deserialize(&mut decoder)
        .map_err(|e| invalid(format!("Invalid vector JSON: {e}")))?;
    decoder
        .end()
        .map_err(|e| invalid(format!("Trailing vector-file data: {e}")))?;
    if !local::still_current(&file, &path, &before) {
        return Err(invalid("Vector input changed while being read"));
    }
    validate(&bundle)?;
    Ok(bundle)
}

/// Output must be new or an existing valid vector file. This preflight prevents
/// an output typo from overwriting an original image, document or unrelated JSON.
pub(crate) fn validate_output(path: &Path) -> Result<Option<local::Revision>> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => {
            read_vectors(path)?;
            Ok(Some(local::Revision::of(&m)))
        }
        Ok(_) => Err(invalid(
            "Output must not be a symlink, directory or special file",
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Save exactly the same JSON value returned by encode. Replacement is atomic;
/// existing non-FileTwin files are refused. Partial results can be reused later.
pub fn write_vectors(path: &Path, bundle: &VectorFile) -> Result<()> {
    validate(bundle)?;
    let path = local::resolve_root(path)?;
    let previous = validate_output(&path)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::invalid("Output needs a parent directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        struct Bounded<'a> {
            output: &'a mut File,
            written: u64,
        }
        impl Write for Bounded<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.written + bytes.len() as u64 > MAX_VECTOR_FILE_BYTES {
                    return Err(std::io::Error::other("Vector output exceeds 512 MiB"));
                }
                let n = self.output.write(bytes)?;
                self.written += n as u64;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.output.flush()
            }
        }
        let mut writer = BufWriter::new(Bounded {
            output: temporary.as_file_mut(),
            written: 0,
        });
        serde_json::to_writer(&mut writer, bundle)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    if let Some(before) = previous {
        let current = local::open_secure(&path)?;
        if !local::still_current(&current, &path, &before) {
            return Err(invalid(
                "Output changed while the new vector file was written",
            ));
        }
        temporary.persist(&path).map_err(|e| Error::from(e.error))?;
    } else {
        temporary
            .persist_noclobber(&path)
            .map_err(|e| Error::from(e.error))?;
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}
