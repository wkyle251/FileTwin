use crate::{Error, ErrorCode, Result, api::*, local, profile, store};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
};

/// Read-only catalog connections are Send, but not Sync. Open a separate catalog
/// on each querying thread. Queries never migrate or initialize an index.
pub struct Catalog {
    pub(crate) db: Connection,
    directory: PathBuf,
}
struct Target {
    owner: String,
    revision: u64,
    snapshot: String,
    run: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    namespace: String,
    owner: String,
    revision: u64,
    kind: String,
    group: Option<String>,
    file: Option<String>,
    last: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_score: Option<f64>,
}

impl Catalog {
    /// Encoding profiles frozen into an immutable snapshot; this never loads a model.
    pub fn snapshot_profiles(
        &self,
        snapshot_id: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        Ok(store::snapshot_request(&self.db, snapshot_id)?
            .0
            .profiles
            .unwrap_or_default())
    }

    pub fn score_run_profiles(
        &self,
        run_id: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let source = store::score_run(&self.db, run_id, None)?;
        self.snapshot_profiles(&source.snapshot)
    }

    pub fn open_read_only(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        if !directory.is_absolute() {
            return Err(Error::invalid("Catalog directory must be absolute"));
        }
        Ok(Self {
            db: store::open_reader(directory)?,
            directory: directory.to_owned(),
        })
    }

    pub fn status(&self, query: StatusQuery) -> Result<Value> {
        check_schema(query.schema_version)?;
        let job = store::load_job(&self.db, &query.job_id)?;
        let owner = owner_active(&self.directory)?;
        let status = if ["queued", "running"].contains(&job.status.as_str()) && !owner {
            "interrupted"
        } else {
            &job.status
        };
        let summary: Option<String> =
            self.db
                .query_row("SELECT summary FROM jobs WHERE id=?1", [&job.id], |r| {
                    r.get(0)
                })?;
        let resumable = ["cancelled", "interrupted"].contains(&status)
            && !job
                .request
                .limits
                .as_ref()
                .and_then(|l| l.wall_time_seconds)
                .is_some_and(|limit| job.elapsed >= limit as f64);
        Ok(
            json!({"job_id":job.id,"run_id":job.run,"attempt_id":job.attempt,"status":status,"owner_active":owner,"resumable":resumable,"stage":job.stage,"snapshot_id":job.snapshot,"elapsed_seconds":job.elapsed,"counts":job.counts,"summary":summary.map(|s|serde_json::from_str::<Value>(&s)).transpose()?}),
        )
    }

    fn target(
        &self,
        run: Option<&str>,
        snapshot: Option<&str>,
        revision: Option<u64>,
    ) -> Result<Target> {
        if revision == Some(0) {
            return Err(Error::invalid("result_revision must be positive"));
        }
        match (run, snapshot) {
            (Some(run), None) => {
                let sid = self
                    .db
                    .query_row("SELECT snapshot_id FROM runs WHERE id=?1", [run], |r| {
                        r.get::<_, String>(0)
                    })
                    .optional()?
                    .ok_or_else(|| Error::new(ErrorCode::NotFound, "results", "Unknown run ID"))?;
                let rev: Option<u64> = self.db.query_row(
                    "SELECT max(revision) FROM publications WHERE run_id=?1",
                    [run],
                    |r| r.get(0),
                )?;
                let latest = rev.ok_or_else(|| {
                    Error::new(
                        ErrorCode::ResultsNotReady,
                        "results",
                        "This run has no published revision",
                    )
                })?;
                let rev = revision.unwrap_or(latest);
                let exists: bool = self.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM publications WHERE run_id=?1 AND revision=?2)",
                    params![run, rev],
                    |r| r.get(0),
                )?;
                if !exists {
                    return Err(Error::new(
                        ErrorCode::ResultExpired,
                        "results",
                        "Requested result revision is unavailable",
                    ));
                }
                Ok(Target {
                    owner: run.into(),
                    revision: rev,
                    snapshot: sid,
                    run: Some(run.into()),
                })
            }
            (None, Some(sid)) => {
                store::snapshot_request(&self.db, sid)?;
                if revision.is_some_and(|r| r != 1) {
                    return Err(Error::new(
                        ErrorCode::ResultExpired,
                        "results",
                        "Snapshot revision is unavailable",
                    ));
                }
                Ok(Target {
                    owner: sid.into(),
                    revision: 1,
                    snapshot: sid.into(),
                    run: None,
                })
            }
            _ => Err(Error::invalid("Select exactly one run_id or snapshot_id")),
        }
    }

    pub fn results(&self, query: ResultsQuery) -> Result<ResultPage> {
        check_schema(query.schema_version)?;
        let kinds = [
            "summary",
            "groups",
            "members",
            "pairs",
            "scores",
            "files",
            "locations",
            "errors",
        ];
        if !kinds.contains(&query.kind.as_str()) {
            return Err(Error::invalid("Unknown result kind"));
        }
        if query.snapshot_id.is_some()
            && !["files", "locations", "errors"].contains(&query.kind.as_str())
        {
            return Err(Error::invalid(
                "Snapshot queries support files, locations, and errors",
            ));
        }
        if (query.kind == "members") != query.group_id.is_some()
            || (query.kind == "locations") != query.file_id.is_some()
        {
            return Err(Error::invalid(
                "members requires only group_id; locations requires only file_id",
            ));
        }
        if query.kind == "summary" && (query.cursor.is_some() || query.page_size.is_some()) {
            return Err(Error::invalid("summary rejects cursor and page_size"));
        }
        if let Some(score) = query.min_score
            && (query.kind != "scores" || !score.is_finite() || !(-1.0..=1.0).contains(&score))
        {
            return Err(Error::invalid(
                "min_score is a finite cutoff in [-1, 1] for scores queries only",
            ));
        }
        let size = query.page_size.unwrap_or(100);
        if !(1..=1000).contains(&size) {
            return Err(Error::invalid("page_size must be in 1..1000"));
        }
        let cursor = if let Some(token) = &query.cursor {
            if token.len() > 4096 {
                return Err(Error::new(
                    ErrorCode::InvalidCursor,
                    "results",
                    "Cursor is too large",
                ));
            }
            let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| {
                Error::new(
                    ErrorCode::InvalidCursor,
                    "results",
                    "Invalid cursor encoding",
                )
            })?;
            Some(serde_json::from_slice::<Cursor>(&bytes).map_err(|_| {
                Error::new(ErrorCode::InvalidCursor, "results", "Invalid cursor record")
            })?)
        } else {
            None
        };
        let target = self.target(
            query.run_id.as_deref(),
            query.snapshot_id.as_deref(),
            query
                .result_revision
                .or(cursor.as_ref().map(|c| c.revision)),
        )?;
        let namespace = store::namespace(&self.db)?;
        if let Some(c) = &cursor
            && (c.namespace != namespace
                || c.owner != target.owner
                || c.revision != target.revision
                || c.kind != query.kind
                || c.group != query.group_id
                || c.file != query.file_id
                || c.min_score != query.min_score)
        {
            return Err(Error::new(
                ErrorCode::InvalidCursor,
                "results",
                "Cursor belongs to another query or revision",
            ));
        }
        if query.kind == "scores" {
            store::score_run(
                &self.db,
                target.run.as_deref().expect("Run query"),
                Some(target.revision),
            )?;
        }
        if let Some(gid) = &query.group_id {
            let exists:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM records WHERE owner=?1 AND revision=?2 AND kind='groups' AND key=?3)",params![target.owner,target.revision,gid],|r|r.get(0))?;
            if !exists {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    "results",
                    "Unknown group in this revision",
                ));
            }
        }
        if let Some(fid) = &query.file_id {
            let exists: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM snapshot_files WHERE snapshot_id=?1 AND file_id=?2)",
                params![target.snapshot, fid],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    "results",
                    "Unknown file in this snapshot",
                ));
            }
        }
        let (owner, revision) = record_owner(&target, &query.kind);
        let filter = query.group_id.as_ref().or(query.file_id.as_ref());
        let after = cursor.as_ref().map(|c| c.last.as_str()).unwrap_or("");
        let mut stmt=self.db.prepare("SELECT key,payload FROM records WHERE owner=?1 AND revision=?2 AND kind=?3 AND (?4 IS NULL OR filter_id=?4) AND key>?5 AND (?7 IS NULL OR json_extract(payload,'$.score')>=?7) ORDER BY key LIMIT ?6")?;
        let mut rows = stmt.query(params![
            owner,
            revision,
            query.kind,
            filter,
            after,
            size + 1,
            query.min_score
        ])?;
        let (mut items, mut bytes, mut last, mut more) = (Vec::new(), 0usize, String::new(), false);
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let payload: String = row.get(1)?;
            if items.len() >= size as usize {
                more = true;
                break;
            }
            if bytes + payload.len() > MAX_PAGE_BYTES - 32768 {
                if items.is_empty() {
                    return Err(Error::new(
                        ErrorCode::RecordTooLarge,
                        "results",
                        "Record exceeds the page-byte limit; use streaming export",
                    ));
                }
                more = true;
                break;
            }
            bytes += payload.len();
            last = key;
            items.push(serde_json::from_str(&payload)?);
        }
        let next_cursor = if more {
            Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor {
                namespace,
                owner: target.owner,
                revision: target.revision,
                kind: query.kind.clone(),
                group: query.group_id,
                file: query.file_id,
                last,
                min_score: query.min_score,
            })?))
        } else {
            None
        };
        Ok(ResultPage {
            snapshot_id: target.snapshot,
            run_id: target.run,
            result_revision: target.revision,
            kind: query.kind,
            items,
            next_cursor,
        })
    }

    pub fn export(&self, request: ExportRequest) -> Result<Value> {
        self.export_with_cancel(request, &|| false)
    }

    pub fn export_with_cancel(
        &self,
        request: ExportRequest,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Value> {
        check_schema(request.schema_version)?;
        if !["json", "jsonl", "csv"].contains(&request.format.as_str()) {
            return Err(Error::invalid("Report format must be json, jsonl, or csv"));
        }
        if !request.directory.is_absolute() {
            return Err(Error::invalid("Export directory must be absolute"));
        }
        if std::fs::symlink_metadata(&request.directory).is_ok() {
            return Err(Error::invalid("Export destination already exists"));
        }
        let target = self.target(
            request.run_id.as_deref(),
            request.snapshot_id.as_deref(),
            request.result_revision,
        )?;
        let parent = request
            .directory
            .parent()
            .ok_or_else(|| Error::invalid("Export destination needs a parent directory"))?;
        let stage = tempfile::Builder::new()
            .prefix(".filetwin-export-")
            .tempdir_in(parent)?;
        let mut kinds: Vec<&str> = if target.run.is_some() {
            vec![
                "summary",
                "groups",
                "members",
                "pairs",
                "files",
                "locations",
                "errors",
            ]
        } else {
            vec!["files", "locations", "errors"]
        };
        if let Some(run) = &target.run {
            let raw: String = self.db.query_row(
                "SELECT j.request FROM jobs j JOIN runs r ON r.job_id=j.id WHERE r.id=?1",
                [run],
                |r| r.get(0),
            )?;
            if serde_json::from_str::<JobRequest>(&raw)?.retains_scores() {
                kinds.push("scores");
            }
        }
        let mut artifacts = Vec::new();
        for kind in kinds {
            if cancel() {
                return Err(Error::new(
                    ErrorCode::Cancelled,
                    "export",
                    "Export cancelled",
                ));
            }
            let filename = format!("{kind}.{}", request.format);
            let path = stage.path().join(&filename);
            let (owner, revision) = record_owner(&target, kind);
            let mut stmt=self.db.prepare("SELECT payload FROM records WHERE owner=?1 AND revision=?2 AND kind=?3 ORDER BY key")?;
            let mut rows = stmt.query(params![owner, revision, kind])?;
            let mut writer = BufWriter::with_capacity(65536, File::create(&path)?);
            let mut count = 0u64;
            if request.format == "csv" {
                let columns = csv_columns(kind);
                let mut csv = csv::Writer::from_writer(&mut writer);
                csv.write_record(&columns).map_err(export_error)?;
                while let Some(row) = rows.next()? {
                    if cancel() {
                        return Err(Error::new(
                            ErrorCode::Cancelled,
                            "export",
                            "Export cancelled",
                        ));
                    }
                    let raw: String = row.get(0)?;
                    let value: Value = serde_json::from_str(&raw)?;
                    let fields: Vec<String> = columns
                        .iter()
                        .map(|k| {
                            if *k == "record_json" {
                                raw.clone()
                            } else {
                                match value.get(*k) {
                                    None | Some(Value::Null) => String::new(),
                                    Some(Value::String(s)) => s.clone(),
                                    Some(v) => v.to_string(),
                                }
                            }
                        })
                        .collect();
                    csv.write_record(fields).map_err(export_error)?;
                    count += 1;
                }
                csv.flush()?;
            } else {
                if request.format == "json" {
                    writer.write_all(b"[")?;
                }
                while let Some(row) = rows.next()? {
                    if cancel() {
                        return Err(Error::new(
                            ErrorCode::Cancelled,
                            "export",
                            "Export cancelled",
                        ));
                    }
                    let raw: String = row.get(0)?;
                    if request.format == "json" && count > 0 {
                        writer.write_all(b",")?;
                    }
                    writer.write_all(raw.as_bytes())?;
                    if request.format == "jsonl" {
                        writer.write_all(b"\n")?;
                    }
                    count += 1;
                }
                if request.format == "json" {
                    writer.write_all(b"]\n")?;
                }
            }
            writer.flush()?;
            writer.get_ref().sync_all()?;
            drop(writer);
            let mut hash = Sha256::new();
            let mut file = File::open(&path)?;
            let mut buf = [0; 65536];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
            artifacts.push(json!({"file":filename,"kind":kind,"records":count,"bytes":file.metadata()?.len(),"sha256":format!("{:x}",hash.finalize())}));
        }
        let completeness = if let Some(run) = &target.run {
            let s: String = self.db.query_row(
                "SELECT summary FROM publications WHERE run_id=?1 AND revision=?2",
                params![run, target.revision],
                |r| r.get(0),
            )?;
            serde_json::from_str::<Value>(&s)?["completeness"].clone()
        } else {
            let (_, partial) = store::snapshot_request(&self.db, &target.snapshot)?;
            json!({"source_coverage":if partial{"partial"}else{"complete_for_requested_profiles"},"comparison_coverage":"not_run"})
        };
        let manifest = json!({"schema_version":1,"product":"filetwin","artifact_kind":"report","run_id":target.run,"snapshot_id":target.snapshot,"result_revision":target.revision,"report_format":request.format,"artifacts":artifacts,"completeness":completeness});
        let mut manifest_file = File::create(stage.path().join("filetwin-export.json"))?;
        serde_json::to_writer_pretty(&mut manifest_file, &manifest)?;
        manifest_file.write_all(b"\n")?;
        manifest_file.sync_all()?;
        File::open(stage.path())?.sync_all()?;
        if cancel() {
            return Err(Error::new(
                ErrorCode::Cancelled,
                "export",
                "Export cancelled",
            ));
        }
        local::rename_new(stage.path(), &request.directory)?;
        File::open(parent)?.sync_all()?;
        Ok(manifest)
    }

    pub fn storage_stats(&self) -> Result<Value> {
        let vectors: u64 = self
            .db
            .query_row("SELECT count(*) FROM vectors", [], |r| r.get(0))?;
        let snapshots: u64 = self
            .db
            .query_row("SELECT count(*) FROM snapshots", [], |r| r.get(0))?;
        Ok(
            json!({"vectors":vectors,"snapshots":snapshots,"database_bytes":self.directory.join("index.sqlite3").metadata()?.len()}),
        )
    }
}

fn record_owner<'a>(target: &'a Target, kind: &str) -> (&'a str, u64) {
    if ["files", "locations"].contains(&kind) {
        (&target.snapshot, 1)
    } else {
        (&target.owner, target.revision)
    }
}
fn check_schema(version: u32) -> Result<()> {
    if version != 1 {
        Err(Error::new(
            ErrorCode::UnsupportedSchemaVersion,
            "validation",
            "Only schema_version 1 is supported",
        ))
    } else {
        Ok(())
    }
}
fn export_error(e: csv::Error) -> Error {
    Error::new(ErrorCode::IoError, "export", e.to_string())
}
fn csv_columns(kind: &str) -> Vec<&'static str> {
    let mut columns = match kind {
        "files" | "members" => vec![
            "file_id",
            "group_id",
            "state",
            "family",
            "bytes",
            "locator",
            "source_ids",
            "location_count",
            "profile_id",
            "sha256",
        ],
        "groups" => vec![
            "group_id",
            "match_kind",
            "member_count",
            "minimum_score",
            "threshold",
            "representative_file_id",
        ],
        "pairs" | "scores" => vec![
            "file_a",
            "file_b",
            "match_kind",
            "score",
            "threshold",
            "sha256",
        ],
        "locations" => vec![
            "location_id",
            "file_id",
            "source_id",
            "locator",
            "availability",
        ],
        "errors" => vec!["code", "stage", "source_id", "file_id", "message"],
        _ => vec!["job_id", "run_id", "status", "snapshot_id"],
    };
    columns.push("record_json");
    columns
}

pub(crate) fn owner_active(dir: &Path) -> Result<bool> {
    let file = File::open(dir.join("owner.lock"))?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            FileExt::unlock(&file)?;
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(e) => Err(e.into()),
    }
}

/// Diagnostic inspection has no database/model initialization side effects.
pub fn capabilities() -> Value {
    json!({"app_version":env!("CARGO_PKG_VERSION"),"protocol_versions":[1],"database_schema_version":store::DB_VERSION,
        "platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"build_kind":"developer_preview",
        "default_families":[],"available_families":["text","image","audio","video"],"profiles":profile::profiles(),
        "formats":{"text":["strict UTF-8 source text","DOCX main body","text PDF"],
            "image":["JPEG","PNG","GIF (first frame)","WebP","TIFF (first image)","BMP","ICO","PNM","TGA","DDS","QOI","farbfeld","HDR","OpenEXR","HEIC/HEIF (SDR primary item or tile grid)","AVIF (SDR primary image)"],
            "audio":"Recognized FFmpeg 9 audio containers/codecs; first audio stream; depends on runtime build",
            "video":"Recognized FFmpeg 9 video containers/codecs; first non-attached video stream; depends on runtime build"},
        "format_validation":{"suite":"scripts/format-smoke.py","qualification":"local developer fixtures; not every codec variant or platform",
            "audio_containers":["WAV","MP3","FLAC","AAC","M4A","AIFF/AIF","Opus","Ogg/OGA","MP2","WMA","WavPack","CAF","AU","AC3","EAC3","MKA","audio-only MP4"],
            "video_containers":["MP4","MOV","MKV","AVI","WebM","MPEG-PS","MPEG-TS/TS/MTS/M2TS","FLV","WMV","3GP"],
            "arbitrary_readable_bytes":"SHA-256 exact-copy evidence with exact_duplicates=compute; unsupported content has no similarity vector"},
        "runtime_policy":{"worker":"isolated per file, model reused across video frames","network_during_processing":false,
            "native_assets":"explicit provisioning, pinned SHA-256 checks","media_classification":"probe streams; ignore cover-art video streams",
            "file_deadline_seconds":crate::worker_protocol::MAX_FILE_SECONDS,"source_access":"bounded private staging from an opened no-follow source",
            "staging":"one full source file plus 2 MiB protocol reserve","memory":"bounded buffers/admission; Linux worker address-space limit; macOS has no hard RSS limit"},
        "scorer":"filetwin_cosine_f64_v1","sqlite_version":rusqlite::version(),"strict_consistency_available":false,"docker_required":false,
        "score_outputs":{"retention_modes":["matches","all"],"group_from_saved_scores":true,"matrix_default_axis_limit":128,"matrix_max_axis_limit":256,"matrix_max_response_bytes":MAX_PAGE_BYTES,"null_scores_have_reasons":true},
        "limitations":["Experimental, uncalibrated profiles; explicit thresholds required for grouping, optional for all-scores mode","Local macOS/Linux files only",
            "OCR, legacy Office, slides/spreadsheets, generic archives and SVG rendering are not implemented",
            "PQ/HLG HDR HEIF/AVIF and video require a separate tone-mapping profile; auxiliary HEIF depth/alpha items are not composed",
            "Document text excludes layout and auxiliary DOCX parts; blank/scanned PDF pages require separate handling",
            "Audio/video use bounded timeline samples; video ignores sound and temporal order",
            "Scalar reference matching; no BLAS or incremental score reuse across snapshots; group reuses a saved score run","Result budget counts records, not total managed disk storage",
            "Sidecars, maintenance, and 10 TB qualification remain later milestones"]})
}
