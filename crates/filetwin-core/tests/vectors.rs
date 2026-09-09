use filetwin_core::{Encoder, ErrorCode, api::*, profile, read_vectors, write_vectors};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::{ffi::OsStringExt, fs::symlink},
    path::{Path, PathBuf},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    encoder: Encoder,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("input");
        fs::create_dir(&source).unwrap();
        fs::create_dir(root.join("staging")).unwrap();
        let encoder = Encoder::new(EncoderConfig::new(
            root.join("models"),
            root.join("staging"),
        ))
        .unwrap();
        Self {
            _temp: temp,
            root,
            source,
            encoder,
        }
    }
    fn run(&self, cache: Option<&Path>) -> VectorFile {
        let mut request = EncodeRequest::new(&self.source);
        request.vectors_file = cache.map(Path::to_owned);
        self.encoder
            .encode(&request, &CancellationToken::default(), |_| {})
            .unwrap()
    }
}
fn id(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[test]
fn returns_vectors_and_content_ids_without_creating_a_database() {
    let f = Fixture::new();
    fs::create_dir(f.source.join("nested")).unwrap();
    fs::write(f.source.join("nested/a.txt"), "first complete text").unwrap();
    fs::write(f.source.join(".hidden"), "another complete text").unwrap();
    let result = f.run(None);
    assert!(result.complete);
    assert_eq!(result.exit_code(), 0);
    assert_eq!(result.files.len(), 2);
    for file in result.files {
        let original = fs::read(f.source.join(file.path.to_path_buf().unwrap())).unwrap();
        assert_eq!(file.file_id, Some(id(&original)));
        assert_eq!(file.vector.unwrap().len(), 4096);
    }
    assert_eq!(fs::read_dir(f.root.join("staging")).unwrap().count(), 0);
    let names: Vec<_> = fs::read_dir(&f.root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 2); // input + empty staging; no database/lock/catalog.
}

#[test]
fn portable_roundtrip_reuses_after_a_move_and_drops_deleted_paths() {
    let f = Fixture::new();
    fs::write(f.source.join("a.txt"), "a document with enough text").unwrap();
    fs::write(f.source.join("gone.txt"), "delete this file next time").unwrap();
    let first = f.run(None);
    let cache = f.root.join("vectors.json");
    write_vectors(&cache, &first).unwrap();
    let read = read_vectors(&cache).unwrap();
    assert_eq!(read.files[0].vector, first.files[0].vector);
    fs::rename(f.source.join("a.txt"), f.source.join("moved.txt")).unwrap();
    fs::remove_file(f.source.join("gone.txt")).unwrap();
    let next = f.run(Some(&cache));
    assert_eq!(next.files.len(), 1);
    assert_eq!(next.files[0].file_id, first.files[0].file_id);
    assert_eq!(next.files[0].vector, first.files[0].vector);
    assert_eq!(
        next.files[0].path.to_path_buf().unwrap(),
        Path::new("moved.txt")
    );
    assert_eq!(
        (
            next.summary.counts.cache_hits,
            next.summary.counts.vectors_encoded
        ),
        (1, 0)
    );
    assert!(next.summary.counts.bytes_hashed > 0);
}

#[test]
fn content_changes_invalidate_reuse_even_with_same_size_and_mtime() {
    let f = Fixture::new();
    let file = f.source.join("same.txt");
    fs::write(&file, "abc document").unwrap();
    let mtime = fs::metadata(&file).unwrap().modified().unwrap();
    let old = f.run(None);
    let cache = f.root.join("vectors.json");
    write_vectors(&cache, &old).unwrap();
    fs::write(&file, "xyz document").unwrap();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    let result = f.run(Some(&cache));
    assert_ne!(result.files[0].file_id, old.files[0].file_id);
    assert_ne!(result.files[0].vector, old.files[0].vector);
    assert_eq!(
        (
            result.summary.counts.cache_hits,
            result.summary.counts.vectors_encoded
        ),
        (0, 1)
    );
}

#[test]
fn copies_and_hardlinks_share_identity_without_losing_paths() {
    let f = Fixture::new();
    fs::write(f.source.join("a.txt"), "same original bytes").unwrap();
    fs::copy(f.source.join("a.txt"), f.source.join("b.txt")).unwrap();
    fs::hard_link(f.source.join("a.txt"), f.source.join("c.txt")).unwrap();
    let result = f.run(None);
    assert_eq!(result.files.len(), 3);
    assert!(
        result
            .files
            .iter()
            .all(|r| r.file_id == result.files[0].file_id)
    );
    assert_eq!(
        (
            result.summary.counts.vectors_encoded,
            result.summary.counts.cache_hits
        ),
        (1, 2)
    );
}

#[test]
fn output_inside_input_is_excluded_and_can_be_updated_in_place() {
    let f = Fixture::new();
    fs::write(f.source.join("a.txt"), "text for repeat scans").unwrap();
    let path = f.source.join("vectors.json");
    let mut request = EncodeRequest::new(&f.source);
    request.output_file = Some(path.clone());
    let first = f
        .encoder
        .encode(&request, &CancellationToken::default(), |_| {})
        .unwrap();
    write_vectors(&path, &first).unwrap();
    request.vectors_file = Some(path.clone());
    let second = f
        .encoder
        .encode(&request, &CancellationToken::default(), |_| {})
        .unwrap();
    assert_eq!(second.files.len(), 1);
    assert_eq!(second.summary.counts.cache_hits, 1);
    write_vectors(&path, &second).unwrap();
    assert_eq!(read_vectors(&path).unwrap().summary.counts.cache_hits, 1);
}

#[test]
fn invalid_and_unsupported_files_keep_hashes_but_never_fake_vectors() {
    let f = Fixture::new();
    fs::write(f.source.join("binary"), b"\0\xff invalid").unwrap();
    fs::write(f.source.join("archive.7z"), b"7z\xbc\xaf\x27\x1c opaque").unwrap();
    fs::write(f.source.join("empty"), []).unwrap();
    let result = f.run(None);
    assert!(result.complete);
    assert_eq!(result.exit_code(), 3);
    for file in &result.files {
        assert!(file.vector.is_none() && file.error.is_some() && file.file_id.is_some());
    }
    assert_eq!(result.summary.counts.files_failed, 3);
    assert_eq!(result.summary.counts.bytes_hashed, 10 + 13);
    let path = f.root.join("failed.json");
    write_vectors(&path, &result).unwrap();
    assert_eq!(read_vectors(&path).unwrap().files.len(), 3);
}

#[test]
fn rejects_corrupt_and_conflicting_cached_vectors_before_encoding() {
    let f = Fixture::new();
    fs::write(f.source.join("a.txt"), "a valid file to cache").unwrap();
    let first = f.run(None);
    let cache = f.root.join("bad.json");
    let mut broken = first.clone();
    broken.files[0].vector.as_mut().unwrap()[0] += 0.5;
    fs::write(&cache, serde_json::to_vec(&broken).unwrap()).unwrap();
    assert_eq!(
        read_vectors(&cache).unwrap_err().code,
        ErrorCode::InvalidVectorFile
    );
    let mut request = EncodeRequest::new(&f.source);
    request.vectors_file = Some(cache.clone());
    let mut progressed = false;
    assert!(
        f.encoder
            .encode(&request, &CancellationToken::default(), |_| progressed =
                true)
            .is_err()
    );
    assert!(!progressed);
    let mut conflicting = first.clone();
    let mut other = first.files[0].clone();
    other.path = FilePath::Utf8("other.txt".into());
    let vector = other.vector.as_mut().unwrap();
    vector.fill(0.0);
    vector[0] = 1.0;
    other.vector_sha256 = Some(format!(
        "{:x}",
        Sha256::digest(
            vector
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>()
        )
    ));
    conflicting.files.push(other);
    fs::write(&cache, serde_json::to_vec(&conflicting).unwrap()).unwrap();
    assert_eq!(
        read_vectors(&cache).unwrap_err().code,
        ErrorCode::InvalidVectorFile
    );
    let mut version = first.clone();
    version.schema_version = 99;
    fs::write(&cache, serde_json::to_vec(&version).unwrap()).unwrap();
    assert_eq!(
        read_vectors(&cache).unwrap_err().code,
        ErrorCode::UnsupportedSchemaVersion
    );
    let raw = serde_json::to_string(&first).unwrap().replacen(
        '{',
        "{\"format\":\"filetwin-vectors\",",
        1,
    );
    fs::write(&cache, raw).unwrap();
    assert_eq!(
        read_vectors(&cache).unwrap_err().code,
        ErrorCode::InvalidVectorFile
    );
}

#[test]
fn output_refuses_unrelated_files_and_symlinks() {
    let f = Fixture::new();
    fs::write(f.source.join("a.txt"), "original content").unwrap();
    let result = f.run(None);
    let unrelated = f.root.join("important.json");
    fs::write(&unrelated, "{\"important\":true}").unwrap();
    assert!(write_vectors(&unrelated, &result).is_err());
    assert_eq!(
        fs::read_to_string(&unrelated).unwrap(),
        "{\"important\":true}"
    );
    let link = f.root.join("symlink.json");
    symlink(&unrelated, &link).unwrap();
    assert!(write_vectors(&link, &result).is_err());
    assert!(read_vectors(&link).is_err());
}

#[test]
fn nonunicode_names_roundtrip_and_symlinks_are_not_followed() {
    let f = Fixture::new();
    // APFS rejects invalid UTF-8 names; Linux filesystems allow them.
    let raw_name = std::ffi::OsString::from_vec(b"text-\xff.txt".to_vec());
    let portable = FilePath::from_path(Path::new(&raw_name));
    let serialized = serde_json::to_vec(&portable).unwrap();
    assert_eq!(
        serde_json::from_slice::<FilePath>(&serialized)
            .unwrap()
            .to_path_buf()
            .unwrap()
            .as_os_str(),
        raw_name
    );
    let name = if cfg!(target_os = "macos") {
        "text-猫.txt".into()
    } else {
        raw_name
    };
    fs::write(f.source.join(&name), "valid source text").unwrap();
    symlink("missing", f.source.join("link")).unwrap();
    let result = f.run(None);
    assert_eq!(result.summary.counts.files_skipped, 1);
    assert!(
        result
            .files
            .iter()
            .any(|r| r.path.to_path_buf().unwrap().as_os_str() == name && r.vector.is_some())
    );
    let cache = f.root.join("vectors.json");
    write_vectors(&cache, &result).unwrap();
    assert_eq!(f.run(Some(&cache)).summary.counts.cache_hits, 1);
}

#[test]
fn cancellation_returns_a_reusable_partial_result_and_cleans_temporary_files() {
    let f = Fixture::new();
    fs::write(f.source.join("a.txt"), "a file").unwrap();
    let cancel = CancellationToken::default();
    let result = f
        .encoder
        .encode(&EncodeRequest::new(&f.source), &cancel, |p| {
            if p.stage == Stage::Encoding {
                cancel.cancel();
            }
        })
        .unwrap();
    assert!(!result.complete && result.summary.cancelled);
    assert_eq!(result.exit_code(), 130);
    assert_eq!(result.summary.counts.files_processed, 0);
    assert_eq!(result.summary.counts.files_total, Some(1));
    let saved = f.root.join("partial.json");
    write_vectors(&saved, &result).unwrap();
    assert_eq!(f.run(Some(&saved)).summary.counts.vectors_encoded, 1);
    assert_eq!(fs::read_dir(f.root.join("staging")).unwrap().count(), 0);
}

#[test]
fn progress_reports_counts_and_separate_discovery_and_encoding() {
    let f = Fixture::new();
    for i in 0..3 {
        fs::write(
            f.source.join(format!("{i}.txt")),
            format!("document number {i}"),
        )
        .unwrap();
    }
    let mut progress = Vec::new();
    let result = f
        .encoder
        .encode(
            &EncodeRequest::new(&f.source),
            &CancellationToken::default(),
            |p| {
                if p.stage == Stage::Completed {
                    assert_eq!(fs::read_dir(f.root.join("staging")).unwrap().count(), 0);
                }
                progress.push(p.clone());
            },
        )
        .unwrap();
    assert_eq!(progress[0].stage, Stage::Discovering);
    assert_eq!(progress[0].counts.files_total, None);
    assert!(
        progress
            .iter()
            .any(|p| p.stage == Stage::Encoding && p.counts.files_total == Some(3))
    );
    assert_eq!(progress.last().unwrap().stage, Stage::Completed);
    assert_eq!(progress.last().unwrap().counts.files_processed, 3);
    assert_eq!(result.summary.counts.vectors_encoded, 3);
}

#[test]
fn missing_temporary_parents_are_not_created() {
    let f = Fixture::new();
    let parent = f.root.join("missing/parent");
    let config = EncoderConfig::new(f.root.join("models"), &parent);
    assert!(matches!(Encoder::new(config), Err(e) if e.code == ErrorCode::InvalidRequest));
    assert!(!f.root.join("missing").exists());
}

#[test]
fn fatal_discovery_errors_also_clean_processing_data() {
    let f = Fixture::new();
    let result = f.encoder.encode(
        &EncodeRequest::new(&f.source),
        &CancellationToken::default(),
        |p| {
            if p.stage == Stage::Discovering {
                fs::remove_dir(&f.source).unwrap();
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read_dir(f.root.join("staging")).unwrap().count(), 0);
}

#[test]
fn cleanup_failures_are_reported_instead_of_silently_returning_success() {
    use std::os::unix::fs::PermissionsExt;
    // Root bypasses permissions, so it cannot exercise this failure mode.
    // SAFETY: geteuid only reads the current process's effective user ID.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new();
    fs::write(f.source.join("a.txt"), "complete source text").unwrap();
    let mut blocked = None;
    let error = f
        .encoder
        .encode(
            &EncodeRequest::new(&f.source),
            &CancellationToken::default(),
            |p| {
                if p.stage == Stage::Encoding && blocked.is_none() {
                    let run = fs::read_dir(f.root.join("staging"))
                        .unwrap()
                        .next()
                        .unwrap()
                        .unwrap()
                        .path();
                    let path = run.join("blocked");
                    fs::create_dir(&path).unwrap();
                    fs::write(path.join("cache"), "temporary data").unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                    blocked = Some(path);
                }
                assert_ne!(p.stage, Stage::Completed);
            },
        )
        .unwrap_err();
    let blocked = blocked.unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(blocked.parent().unwrap()).unwrap();
    assert_eq!(error.code, ErrorCode::IoError);
    assert_eq!(error.stage, "cleanup");
}

#[test]
fn encoder_configuration_is_validated_before_processing() {
    let f = Fixture::new();
    let mut config = EncoderConfig::new(f.root.join("models"), f.root.join("staging"));
    config.workers = 0;
    assert!(Encoder::new(config.clone()).is_err());
    config.workers = 2;
    config.runtime.cuda_device_id = -1;
    assert!(Encoder::new(config).is_err());
    assert!(
        f.encoder
            .encode(
                &EncodeRequest::new("relative"),
                &CancellationToken::default(),
                |_| {}
            )
            .is_err()
    );
}

#[test]
fn encoding_profiles_keep_their_previous_meaning() {
    assert_eq!(
        profile::image_profile_v2().profile_id,
        "sha256:88a7383952aabd98b9bc41e66b355fc569e9070697040026e4476db04408700b"
    );
    assert_eq!(
        profile::video_profile_v1().profile_id,
        "sha256:b9b098920072811527446fcf68330b717817561f3eae28012410dfbebb2fd93b"
    );
    let mut ids = std::collections::HashSet::new();
    for backend in [
        profile::Backend::Cpu,
        profile::Backend::Coreml,
        profile::Backend::Cuda,
    ] {
        for p in profile::experimental_profiles_for(backend)
            .into_iter()
            .filter(|p| p.requires_model)
        {
            assert!(ids.insert(p.profile_id.clone()));
            assert_eq!(profile::inference_backend(&p.profile_id).unwrap(), backend);
        }
    }
}
