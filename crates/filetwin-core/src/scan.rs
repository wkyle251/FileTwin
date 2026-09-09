use crate::{
    Error, ErrorCode, Result,
    api::{CacheMode, ExactDuplicates, Validation},
    engine::Work,
    local::{self, FilterSet, Revision, SecureDir},
    native, profile, store, text,
    worker_protocol::Encoded,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub(crate) fn discover(work: &mut Work<'_>) -> Result<()> {
    let sources = work.job.request.sources.clone().expect("Resolved sources");
    let filters = FilterSet::new(work.job.request.filters.clone().expect("Resolved filters"))?;
    let excluded = vec![
        work.config.data_dir.clone(),
        work.config.model_dir.clone(),
        work.config.temp_dir.clone(),
    ];
    // Re-list after interruption. The current inventory is replaced; published
    // snapshots remain immutable and committed global vectors remain reusable.
    {
        let tx = work.db.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM directory_tasks WHERE job_id=?1",
            [&work.job.id],
        )?;
        tx.execute("DELETE FROM job_files WHERE job_id=?1", [&work.job.id])?;
        tx.execute("DELETE FROM job_locations WHERE job_id=?1", [&work.job.id])?;
        for source in &sources {
            let path = source.path()?;
            tx.execute("INSERT INTO directory_tasks(job_id,source_id,path,root,depth) VALUES(?1,?2,?3,?3,0)",params![work.job.id,source.source_id,local::path_bytes(&path)])?;
        }
        tx.commit()?;
    }
    work.job.snapshot = None;
    work.job.run = None;
    work.job.source_partial = false;
    work.job.counts.files_discovered = 0;
    work.job.counts.files_ready = 0;
    work.job.counts.files_failed = 0;
    work.job.counts.files_excluded = 0;
    work.job.counts.locations = 0;
    work.checkpoint()?;
    let namespace = store::namespace(&work.db)?;
    loop {
        work.check()?;
        let task=work.db.query_row("SELECT id,source_id,path,root,depth FROM directory_tasks WHERE job_id=?1 AND done=0 ORDER BY id LIMIT 1",[&work.job.id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,Vec<u8>>(3)?,r.get::<_,u32>(4)?))).optional()?;
        let Some((task_id, source, path, root, depth)) = task else {
            break;
        };
        let path = local::from_bytes(path);
        let root = local::from_bytes(root);
        if local::excluded_artifact(&path, &excluded) {
            work.job.counts.files_excluded += 1;
            work.db
                .execute("UPDATE directory_tasks SET done=1 WHERE id=?1", [task_id])?;
            continue;
        }
        let result = (|| -> Result<()> {
            let handle = local::open_secure(&path)?;
            let meta = handle.metadata()?;
            if meta.is_file() {
                return process_file(work, &path, &root, &source, &namespace, &filters, &excluded);
            }
            if !meta.is_dir() {
                return Err(Error::new(
                    ErrorCode::SourceChanged,
                    "discovery",
                    "Source changed type",
                ));
            }
            if depth >= 1024 {
                return Err(Error::new(
                    ErrorCode::UnsupportedCapability,
                    "discovery",
                    "Directory nesting exceeds the preview's 1024-level limit",
                ));
            }
            let directory = SecureDir::open(&path)?;
            for name in directory {
                work.check()?;
                let name = name?;
                let child = path.join(name);
                let relative = child.strip_prefix(&root).unwrap_or(&child);
                if local::excluded_artifact(&child, &excluded) || filters.hidden_excluded(relative)
                {
                    excluded_entry(work, &child, &source, "excluded_by_scope")?;
                    continue;
                }
                let metadata = match std::fs::symlink_metadata(&child) {
                    Ok(m) => m,
                    Err(e) => {
                        work.job.source_partial = true;
                        work.file_error(Error::from(e).for_file(&source, None))?;
                        continue;
                    }
                };
                if metadata.is_dir() {
                    if work.job.request.recursive == Some(true) {
                        work.db.execute("INSERT OR IGNORE INTO directory_tasks(job_id,source_id,path,root,depth) VALUES(?1,?2,?3,?4,?5)",params![work.job.id,source,local::path_bytes(&child),local::path_bytes(&root),depth+1])?;
                    } else {
                        excluded_entry(work, &child, &source, "non_recursive_scope")?;
                    }
                } else if metadata.is_file() {
                    process_file(
                        work, &child, &root, &source, &namespace, &filters, &excluded,
                    )?;
                } else {
                    excluded_entry(
                        work,
                        &child,
                        &source,
                        if metadata.file_type().is_symlink() {
                            "symlink"
                        } else {
                            "special_entry"
                        },
                    )?;
                }
            }
            // Confirm that the root path still resolves to the directory observed
            // at open time; never infer deletions from a replaced or offline root.
            let after = local::open_secure(&path)?.metadata()?;
            let a = Revision::of(&meta);
            let b = Revision::of(&after);
            if a.device != b.device || a.inode != b.inode || a.birth != b.birth {
                return Err(Error::new(
                    ErrorCode::SourceChanged,
                    "discovery",
                    "Directory identity changed during enumeration",
                ));
            }
            Ok(())
        })();
        if let Err(error) = result {
            if matches!(
                error.code,
                ErrorCode::Cancelled
                    | ErrorCode::OutputClosed
                    | ErrorCode::BudgetExhausted
                    | ErrorCode::StorageError
            ) {
                return Err(error);
            }
            work.job.source_partial = true;
            work.file_error(error.for_file(&source, None))?;
        }
        work.db
            .execute("UPDATE directory_tasks SET done=1 WHERE id=?1", [task_id])?;
        work.checkpoint()?;
    }
    refresh_counts(work)?;
    Ok(())
}

fn excluded_entry(work: &mut Work<'_>, path: &Path, source: &str, reason: &str) -> Result<()> {
    let fid = format!("entry_{}", profile::digest_hex(local::path_bytes(path)));
    let payload = json!({"file_id":fid,"state":"excluded","reason":reason,"locator":local::locator(path),"vector_id":null,"profile_id":null,"bytes":null});
    save_observation(work, path, source, &fid, &payload, false)?;
    refresh_counts(work)?;
    Ok(())
}

fn process_file(
    work: &mut Work<'_>,
    path: &Path,
    root: &Path,
    source: &str,
    namespace: &str,
    filters: &FilterSet,
    excluded: &[PathBuf],
) -> Result<()> {
    if local::excluded_artifact(path, excluded) {
        return excluded_entry(work, path, source, "filetwin_artifact");
    }
    let mut file = match local::open_secure(path) {
        Ok(f) => f,
        Err(e) => {
            work.job.source_partial = true;
            work.file_error(e.for_file(source, None))?;
            return Ok(());
        }
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        work.job.source_partial = true;
        work.file_error(
            Error::new(
                ErrorCode::SourceChanged,
                "discovery",
                "Entry is no longer a regular file",
            )
            .for_file(source, None),
        )?;
        return Ok(());
    }
    let revision = Revision::of(&meta);
    let fid = revision.file_id(namespace);
    let relative = if path == root {
        path.file_name().map(Path::new).unwrap_or(path)
    } else {
        path.strip_prefix(root).unwrap_or(path)
    };
    match filters.matches(relative, revision.bytes) {
        Ok(true) => (),
        Ok(false) => return excluded_entry(work, path, source, "excluded_by_filter"),
        Err(e) => {
            work.job.source_partial = true;
            work.file_error(e.for_file(source, Some(&fid)))?;
            return failed_observation(work, path, source, &fid, &revision, "failed", None);
        }
    }
    let prior = work
        .db
        .query_row(
            "SELECT payload FROM job_files WHERE job_id=?1 AND file_id=?2",
            params![work.job.id, fid],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if let Some(prior) = prior {
        let payload: Value = serde_json::from_str(&prior)?;
        if payload.get("revision_key").and_then(Value::as_str) == Some(&revision.key()) {
            save_observation(work, path, source, &fid, &payload, false)?;
            refresh_counts(work)?;
            return work.checkpoint();
        }
    }
    let cache = work.job.request.cache.clone().expect("Resolved cache");
    if cache.validation == Validation::Strict {
        work.job.source_partial = true;
        work.file_error(
            Error::new(
                ErrorCode::StrictConsistencyUnavailable,
                "validation",
                "The local preview adapter cannot bind reads to an immutable filesystem revision",
            )
            .for_file(source, Some(&fid)),
        )?;
        return failed_observation(work, path, source, &fid, &revision, "failed", None);
    }
    let compute = work.job.request.exact_duplicates == Some(ExactDuplicates::Compute);
    // Whole-file evidence belongs to the source revision, independently of the
    // selected encoder. Changing profiles or refreshing vectors must not erase
    // an already known digest for the same observed revision.
    let known_digest: Option<String> = work
        .db
        .query_row(
            "SELECT digest FROM files WHERE file_id=?1 AND revision=?2",
            params![fid, revision.key()],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let old = if cache.mode == CacheMode::Reuse {
        work.db.query_row("SELECT vector_id,digest,payload FROM files WHERE file_id=?1 AND revision=?2 AND vector_id IS NOT NULL",params![fid,revision.key()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?))).optional()?
    } else {
        None
    };
    if let Some((vector, mut digest, old_payload)) = old {
        let mut payload: Value = serde_json::from_str(&old_payload)?;
        if payload
            .get("family")
            .and_then(Value::as_str)
            .and_then(|family| work.job.request.profiles.as_ref()?.get(family))
            .map(String::as_str)
            == payload.get("profile_id").and_then(Value::as_str)
        {
            // Verify cached payload integrity before publishing a binding.
            verify_vector(&work.db, &vector)?;
            if compute && digest.is_none() {
                digest = Some(read_digest(work, &mut file)?);
            }
            if !still_current(&file, path, &revision) {
                return changed(work, path, source, &fid, &revision);
            }
            payload["locator"] = local::locator(path);
            payload["sha256"] = json!(digest);
            payload["cache"] = json!("hit");
            payload["observed_at"] = json!(store::now());
            save_observation(work, path, source, &fid, &payload, true)?;
            work.job.counts.cache_hits += 1;
            refresh_counts(work)?;
            return work.checkpoint();
        }
    }
    let mut prefix = [0u8; 512];
    let prefix_bytes = file.read(&mut prefix)?;
    work.job.counts.bytes_read += prefix_bytes as u64;
    file.seek(SeekFrom::Start(0))?;
    let (family, format) = detect_format(&prefix[..prefix_bytes], path);
    let selected = work
        .job
        .request
        .profiles
        .as_ref()
        .and_then(|p| {
            if format == "media" {
                p.get("video").or_else(|| p.get("audio"))
            } else {
                p.get(family)
            }
        })
        .cloned();
    let unsupported = family == "unknown"
        || (format == "heif_avif"
            && selected.as_deref() == Some(&profile::image_profile_v1().profile_id))
        || (family == "text"
            && format != "utf8"
            && selected.as_deref() == Some(&profile::text_profile().profile_id));
    if selected.is_none() || unsupported {
        let digest = if compute {
            Some(read_digest(work, &mut file)?)
        } else {
            known_digest.clone()
        };
        if !still_current(&file, path, &revision) {
            return changed(work, path, source, &fid, &revision);
        }
        if unsupported {
            work.job.source_partial = true;
            work.file_error(
                Error::new(
                    ErrorCode::UnsupportedFormat,
                    "encoding",
                    "No reader for this format under the selected profiles",
                )
                .for_file(source, Some(&fid)),
            )?;
        }
        let payload = json!({"file_id":fid,"family":family,"format":format,"locator":local::locator(path),"bytes":revision.bytes,"state":if unsupported{"unsupported"}else{"excluded"},"reason":if unsupported{"unsupported_format"}else{"unselected_family"},"profile_id":null,"vector_id":null,"sha256":digest,"revision":revision,"revision_key":revision.key(),"identity_evidence":revision.identity_kind(),"freshness":"fast_metadata_heuristic","observed_at":store::now()});
        save_observation(work, path, source, &fid, &payload, true)?;
        refresh_counts(work)?;
        return work.checkpoint();
    }
    let p = profile::find(selected.as_deref().expect("Selected profile"))?;
    let encoded = if format == "utf8" {
        text::encode(&mut file, compute, &|| work.check()).map(|e| {
            work.job.counts.bytes_read += e.bytes_read;
            if compute { work.job.counts.bytes_hashed += e.bytes_read; }
            (Encoded { family: "text".into(), format: "text/plain; charset=utf-8".into(), vector: e.vector,
                extraction: json!({"coverage":"complete_source_text","characters":e.characters}) }, e.digest)
        })
    } else {
        native::encode(work, &mut file, format, &p.profile_id, compute)
    };
    let encoded = match encoded {
        Ok(value) => value,
        Err(error) => {
            work.job.counts.bytes_read += error
                .details
                .get("bytes_read")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            work.job.counts.bytes_hashed += error
                .details
                .get("bytes_hashed")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if matches!(
                error.code,
                ErrorCode::Cancelled | ErrorCode::OutputClosed | ErrorCode::BudgetExhausted
            ) {
                return Err(error);
            }
            if !still_current(&file, path, &revision) {
                return changed(work, path, source, &fid, &revision);
            }
            let mut digest = error
                .details
                .get("digest")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| known_digest.clone());
            if compute && digest.is_none() {
                file.seek(SeekFrom::Start(0))?;
                digest = Some(read_digest(work, &mut file)?);
                if !still_current(&file, path, &revision) {
                    return changed(work, path, source, &fid, &revision);
                }
            }
            let state = if error.code == ErrorCode::InsufficientContent {
                "insufficient_content"
            } else if error.code == ErrorCode::UnsupportedFormat {
                "unsupported"
            } else if error.code == ErrorCode::UnselectedFamily {
                "excluded"
            } else {
                "failed"
            };
            let actual_family = error.details["family"]
                .as_str()
                .unwrap_or(family)
                .to_owned();
            if state != "excluded" {
                work.job.source_partial = true;
                work.file_error(error.for_file(source, Some(&fid)))?;
            }
            let payload = json!({"file_id":fid,"family":actual_family,"format":format,"locator":local::locator(path),"bytes":revision.bytes,"state":state,"reason":if state=="excluded"{Some("unselected_family")}else{None},"profile_id":null,"vector_id":null,"sha256":digest,"revision":revision,"revision_key":revision.key(),"observed_at":store::now()});
            save_observation(work, path, source, &fid, &payload, true)?;
            refresh_counts(work)?;
            return work.checkpoint();
        }
    };
    let (encoded, digest) = encoded;
    let digest = digest.or(known_digest);
    let p = profile::find(
        work.job
            .request
            .profiles
            .as_ref()
            .and_then(|p| p.get(&encoded.family))
            .expect("Validated result family"),
    )?;
    if !still_current(&file, path, &revision) {
        return changed(work, path, source, &fid, &revision);
    }
    // Re-open through the no-follow path to ensure the location still names the
    // opened object before publishing it; an open descriptor survives renames.
    match local::open_secure(path).and_then(|f| Ok(Revision::of(&f.metadata()?))) {
        Ok(r) if r.key() == revision.key() => (),
        _ => return changed(work, path, source, &fid, &revision),
    }
    let bytes = profile::vector_bytes(&encoded.vector);
    let checksum = profile::digest_hex(&bytes);
    let vector_id = format!(
        "vector_{}",
        profile::digest_hex(format!("{}:{checksum}", p.profile_id).as_bytes())
    );
    let payload = json!({"file_id":fid,"family":encoded.family,"format":encoded.format,"locator":local::locator(path),"bytes":revision.bytes,"state":"ready","profile_id":p.profile_id,"vector_id":vector_id,"sha256":digest,"revision":revision,"revision_key":revision.key(),"identity_evidence":revision.identity_kind(),"freshness":"fast_metadata_heuristic","observed_at":store::now(),"extraction":encoded.extraction,"cache":"encoded"});
    // The payload is immutable and may be orphaned by interruption before the
    // binding commit. Such an orphan is never a ready file and is safe to retain.
    work.db.execute(
        "INSERT OR IGNORE INTO vectors(id,profile_id,payload,checksum) VALUES(?1,?2,?3,?4)",
        params![vector_id, p.profile_id, bytes, checksum],
    )?;
    save_observation(work, path, source, &fid, &payload, true)?;
    work.job.counts.vectors_encoded += 1;
    refresh_counts(work)?;
    work.checkpoint()
}

fn still_current(file: &File, path: &Path, revision: &Revision) -> bool {
    file.metadata()
        .is_ok_and(|m| Revision::of(&m).key() == revision.key())
        && local::open_secure(path)
            .and_then(|f| Ok(Revision::of(&f.metadata()?)))
            .is_ok_and(|r| r.key() == revision.key())
}

fn changed(
    work: &mut Work<'_>,
    path: &Path,
    source: &str,
    fid: &str,
    revision: &Revision,
) -> Result<()> {
    work.job.source_partial = true;
    work.file_error(
        Error::new(
            ErrorCode::SourceChanged,
            "encoding",
            "Source revision or location changed; the staged vector was discarded",
        )
        .for_file(source, Some(fid)),
    )?;
    failed_observation(work, path, source, fid, revision, "stale", None)
}
fn failed_observation(
    work: &mut Work<'_>,
    path: &Path,
    source: &str,
    fid: &str,
    revision: &Revision,
    state: &str,
    digest: Option<&str>,
) -> Result<()> {
    let payload = json!({"file_id":fid,"family":"text","locator":local::locator(path),"bytes":revision.bytes,"state":state,"profile_id":null,"vector_id":null,"sha256":digest,"revision":revision,"revision_key":revision.key(),"observed_at":store::now()});
    save_observation(work, path, source, fid, &payload, true)?;
    refresh_counts(work)?;
    work.checkpoint()
}

fn save_observation(
    work: &mut Work<'_>,
    path: &Path,
    source: &str,
    fid: &str,
    payload: &Value,
    current: bool,
) -> Result<()> {
    let vector = payload.get("vector_id").and_then(Value::as_str);
    let digest = payload.get("sha256").and_then(Value::as_str);
    let lid = local::location_id(path);
    let location = json!({"location_id":lid,"file_id":fid,"source_id":source,"locator":local::locator(path),"availability":"observed"});
    let payload_text = serde_json::to_string(payload)?;
    let location_text = serde_json::to_string(&location)?;
    work.charge((payload_text.len() + location_text.len()) as u64)?;
    let tx = work.db.unchecked_transaction()?;
    tx.execute("INSERT INTO job_files(job_id,file_id,vector_id,digest,payload) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(job_id,file_id) DO UPDATE SET vector_id=excluded.vector_id,digest=excluded.digest,payload=excluded.payload",params![work.job.id,fid,vector,digest,payload_text])?;
    tx.execute("INSERT OR REPLACE INTO job_locations(job_id,location_id,source_id,file_id,path,payload) VALUES(?1,?2,?3,?4,?5,?6)",params![work.job.id,lid,source,fid,local::path_bytes(path),location_text])?;
    if current {
        tx.execute("INSERT INTO files(file_id,revision,vector_id,digest,payload) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(file_id) DO UPDATE SET revision=excluded.revision,vector_id=excluded.vector_id,digest=excluded.digest,payload=excluded.payload",params![fid,payload.get("revision_key").and_then(Value::as_str).unwrap_or(""),vector,digest,payload_text])?;
        tx.execute("INSERT INTO locations(location_id,path,file_id,active) VALUES(?1,?2,?3,1) ON CONFLICT(location_id) DO UPDATE SET file_id=excluded.file_id,active=1",params![lid,local::path_bytes(path),fid])?;
    }
    store::save_job(&tx, &work.job)?;
    tx.commit()?;
    Ok(())
}

fn refresh_counts(work: &mut Work<'_>) -> Result<()> {
    let counts=work.db.query_row("SELECT count(*),coalesce(sum(vector_id IS NOT NULL),0),coalesce(sum(json_extract(payload,'$.state')='excluded'),0),coalesce(sum(json_extract(payload,'$.state') IN ('failed','unsupported','insufficient_content','stale')),0) FROM job_files WHERE job_id=?1",[&work.job.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    work.job.counts.files_discovered = counts.0;
    work.job.counts.files_ready = counts.1;
    work.job.counts.files_excluded = counts.2;
    work.job.counts.files_failed = counts.3;
    work.job.counts.locations = work.db.query_row(
        "SELECT count(DISTINCT location_id) FROM job_locations WHERE job_id=?1",
        [&work.job.id],
        |r| r.get(0),
    )?;
    Ok(())
}

pub(crate) fn verify_vector(db: &rusqlite::Connection, id: &str) -> Result<Vec<f32>> {
    let (bytes, checksum, profile_id) = db.query_row(
        "SELECT payload,checksum,profile_id FROM vectors WHERE id=?1",
        [id],
        |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    if profile::digest_hex(&bytes) != checksum {
        return Err(Error::new(
            ErrorCode::DatabaseCorrupt,
            "vector",
            "Cached vector checksum mismatch",
        ));
    }
    profile::decode_vector(&bytes, profile::find(&profile_id)?.dimensions)
}

fn hash_file(file: &mut File, check: &dyn Fn() -> Result<()>) -> Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut bytes = 0;
    loop {
        if let Err(mut error) = check() {
            error.details = Box::new(json!({"bytes_read":bytes,"bytes_hashed":bytes}));
            return Err(error);
        }
        let n = match file.read(&mut buffer) {
            Ok(n) => n,
            Err(e) => {
                let mut error = Error::from(e);
                error.details = Box::new(json!({"bytes_read":bytes,"bytes_hashed":bytes}));
                return Err(error);
            }
        };
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        bytes += n as u64;
    }
    Ok((format!("{:x}", hash.finalize()), bytes))
}

fn read_digest(work: &mut Work<'_>, file: &mut File) -> Result<String> {
    match hash_file(file, &|| work.check()) {
        Ok((sha, bytes)) => {
            work.job.counts.bytes_read += bytes;
            work.job.counts.bytes_hashed += bytes;
            Ok(sha)
        }
        Err(error) => {
            work.job.counts.bytes_read += error.details["bytes_read"].as_u64().unwrap_or(0);
            work.job.counts.bytes_hashed += error.details["bytes_hashed"].as_u64().unwrap_or(0);
            Err(error)
        }
    }
}
fn detect_format(bytes: &[u8], path: &Path) -> (&'static str, &'static str) {
    if bytes.starts_with(b"%PDF-") {
        ("text", "pdf")
    } else if bytes.starts_with(b"PK\x03\x04") {
        ("text", "docx") // The bounded package reader verifies this is DOCX.
    } else if bytes.starts_with(b"\x1f\x8b")
        || bytes.starts_with(b"7z\xbc\xaf\x27\x1c")
        || bytes.starts_with(b"Rar!")
    {
        ("unknown", "archive")
    } else if bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"GIF8")
        || bytes.starts_with(b"II*\0")
        || bytes.starts_with(b"MM\0*")
        || bytes.starts_with(b"II+\0")
        || bytes.starts_with(b"MM\0+")
        || bytes.starts_with(b"BM")
        || bytes.starts_with(b"\0\0\x01\0")
        || bytes.starts_with(b"qoif")
        || bytes.starts_with(b"DDS ")
        || bytes.starts_with(b"v/1\x01")
        || bytes.starts_with(b"#?RADIANCE")
        || bytes.starts_with(b"#?RGBE")
        || bytes.starts_with(b"farbfeld")
        || (bytes.first() == Some(&b'P') && bytes.get(1).is_some_and(|b| (b'1'..=b'7').contains(b)))
    {
        ("image", "raster")
    } else if bytes.starts_with(b"RIFF") {
        match bytes.get(8..12) {
            Some(b"WEBP") => ("image", "raster"),
            Some(b"WAVE") => ("audio", "media"),
            Some(b"AVI ") => ("video", "media"),
            _ => ("unknown", "riff"),
        }
    } else if bytes.starts_with(b"ID3")
        || bytes.starts_with(b"fLaC")
        || bytes.starts_with(b"caff")
        || bytes.starts_with(b"MAC ")
        || bytes.starts_with(b"wvpk")
        || bytes.starts_with(b"#!AMR")
        || bytes.starts_with(b".snd")
        || bytes.starts_with(b"MPCK")
        || bytes.starts_with(b"FORM")
        || bytes.starts_with(b"RF64")
        || (bytes.first() == Some(&0xff) && bytes.get(1).is_some_and(|b| b & 0xe0 == 0xe0))
    {
        ("audio", "media")
    } else if bytes.starts_with(b"OggS") {
        if bytes.windows(6).any(|w| w == b"theora") {
            ("video", "media")
        } else {
            ("audio", "media")
        }
    } else if bytes.get(4..8) == Some(b"ftyp") {
        // Brands are four-byte entries: the major brand at byte 8 and
        // compatible brands after the minor version. MIAF files can put the
        // decisive HEIF/AVIF brand only in their compatible-brand list.
        let box_end = bytes
            .get(..4)
            .map(|n| u32::from_be_bytes(n.try_into().unwrap()) as usize)
            .unwrap_or(0)
            .min(bytes.len());
        let image_brand = |brand: &[u8]| {
            matches!(
                brand,
                b"avif" | b"avis" | b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1"
            )
        };
        if bytes.get(8..12).is_some_and(image_brand)
            || bytes.get(16..box_end).is_some_and(|brands| {
                brands
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|brand| image_brand(brand))
            })
        {
            return ("image", "heif_avif");
        }
        match bytes.get(8..12) {
            Some(b"M4A " | b"M4B " | b"M4P ") => ("audio", "media"),
            _ => ("video", "media"),
        }
    } else if bytes.starts_with(b"\x1aE\xdf\xa3")
        || bytes.starts_with(b"FLV")
        || bytes.starts_with(b".RMF")
        || bytes.starts_with(b"\x30\x26\xb2\x75\x8e\x66\xcf\x11")
        || bytes.starts_with(b"\0\0\x01\xba")
        || bytes.get(4..8) == Some(b"moov")
        || (bytes.first() == Some(&0x47)
            && bytes.get(188) == Some(&0x47)
            && bytes.get(376) == Some(&0x47))
    {
        ("video", "media")
    } else {
        // Formats without a reliable short magic get a decoder hint, followed
        // by strict content validation. Extensions never bypass the decoder.
        match path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("tga") => ("image", "tga"),
            Some("heic" | "heif" | "hif" | "avif" | "avifs") => ("image", "heif_avif"),
            Some(
                "mts" | "m2ts" | "mpeg" | "mpg" | "mkv" | "webm" | "mov" | "mp4" | "avi" | "wmv"
                | "asf" | "flv" | "m4v" | "3gp" | "3g2" | "mxf" | "vob" | "ogv" | "rm" | "rmvb"
                | "m2v",
            ) => ("video", "media"),
            Some(
                "m4a" | "mka" | "mp3" | "aac" | "wav" | "flac" | "aiff" | "opus" | "ogg" | "oga"
                | "mp2" | "wma" | "ape" | "wv" | "caf" | "au" | "amr" | "ac3" | "eac3" | "dts"
                | "aif" | "m4b" | "ra",
            ) => ("audio", "media"),
            _ => ("text", "utf8"),
        }
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn image_brands_and_media_contents_override_extension_hints() {
        assert_eq!(
            detect_format(b"\0\0\0\x18ftypmiaf\0\0\0\0avifmif1", Path::new("copy.bin")),
            ("image", "heif_avif")
        );
        assert_eq!(
            detect_format(b"\0\0\0\x18ftyphevx\0\0\0\0mif1heic", Path::new("copy.mp4")),
            ("image", "heif_avif")
        );
        assert_eq!(
            detect_format(b"\0\0\0\x18ftypM4A \0\0\0\0isommp42", Path::new("copy.bin")),
            ("audio", "media")
        );
        assert_eq!(
            detect_format(b"\x89PNG\r\n\x1a\n", Path::new("copy.heic")),
            ("image", "raster")
        );
        assert_eq!(
            detect_format(b"const value = 42;", Path::new("code.ts")),
            ("text", "utf8")
        );
    }
}
