use filetwin_core::{Catalog, Engine, ErrorCode, HostServices, api::*};
use std::{fs, path::PathBuf};

fn fixture() -> (tempfile::TempDir, PathBuf, Engine) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("input");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("a.txt"),
        "The little boat crossed the quiet lake at sunrise.",
    )
    .unwrap();
    fs::write(
        root.join("b.txt"),
        "The little boat crossed the quiet lake at sunrise!",
    )
    .unwrap();
    fs::write(
        root.join("c.md"),
        "Database ownership requires an exclusive lock.",
    )
    .unwrap();
    let engine = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    (tmp, root, engine)
}

#[test]
fn scan_cache_compare_and_frozen_results() {
    let (tmp, root, engine) = fixture();
    let a = engine
        .submit(JobRequest::text_scan([root.clone()], 0.7))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(a.status, "completed");
    assert_eq!(a.counts.files_ready, 3);
    assert!(a.counts.similar_pairs >= 1);
    let b = engine
        .submit(JobRequest::text_scan([root.clone()], 0.7))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(b.counts.cache_hits, 3);
    assert_eq!(b.counts.bytes_read, 0);
    assert_eq!(b.counts.vectors_encoded, 0);
    fs::remove_dir_all(&root).unwrap();
    let request = JobRequest {
        operation: Operation::Compare,
        snapshot_id: a.snapshot_id.clone(),
        matching: JobRequest::text_scan([], 0.7).matching,
        ..JobRequest::default()
    };
    let c = engine.submit(request).unwrap().wait().unwrap();
    assert_eq!(c.counts.similar_pairs, a.counts.similar_pairs);
    assert_eq!(c.counts.bytes_read, 0);
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let page = catalog
        .results(ResultsQuery {
            run_id: a.run_id,
            kind: "groups".into(),
            ..ResultsQuery::default()
        })
        .unwrap();
    assert!(!page.items.is_empty());
    engine.shutdown().unwrap();
    let next = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    next.shutdown().unwrap();
}

#[test]
fn hardlinks_locations_scope_and_exact_evidence() {
    let (tmp, root, engine) = fixture();
    fs::hard_link(root.join("a.txt"), root.join("alias.txt")).unwrap();
    fs::copy(root.join("a.txt"), root.join("copy.txt")).unwrap();
    let mut request = JobRequest::text_scan([root.clone(), root.join("a.txt")], 0.7);
    request.exact_duplicates = Some(ExactDuplicates::Compute);
    let summary = engine.submit(request).unwrap().wait().unwrap();
    assert_eq!(summary.counts.files_ready, 4);
    assert_eq!(summary.counts.locations, 5);
    assert_eq!(summary.counts.exact_pairs, 1);
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let files = catalog
        .results(ResultsQuery {
            snapshot_id: summary.snapshot_id.clone(),
            ..ResultsQuery::default()
        })
        .unwrap();
    let aliased = files
        .items
        .iter()
        .find(|f| f["location_count"] == 2)
        .unwrap();
    let locations = catalog
        .results(ResultsQuery {
            snapshot_id: summary.snapshot_id,
            kind: "locations".into(),
            file_id: aliased["file_id"].as_str().map(str::to_owned),
            ..ResultsQuery::default()
        })
        .unwrap();
    assert_eq!(locations.items.len(), 3); // two physical paths, three source memberships
    let mut separated = JobRequest::text_scan([root.join("a.txt"), root.join("copy.txt")], 0.0);
    separated.pair_scope = Some(PairScope::WithinEachSource);
    separated.exact_duplicates = Some(ExactDuplicates::Compute);
    let separated = engine.submit(separated).unwrap().wait().unwrap();
    assert_eq!(separated.counts.similar_pairs, 0);
    assert_eq!(separated.counts.exact_pairs, 0);
}

#[test]
fn owner_lock_and_request_validation() {
    let (tmp, root, engine) = fixture();
    let error = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .err()
    .unwrap();
    assert_eq!(error.code, ErrorCode::CacheBusy);
    assert!(parse_json::<JobRequest>(br#"{"schema_version":1,"schema_version":1}"#).is_err());
    assert!(parse_json::<JobRequest>(br#"{"schema_version":1,"extra":true}"#).is_err());
    let r = JobRequest {
        sources: Some(vec![Source::local(root)]),
        ..JobRequest::default()
    };
    assert_eq!(
        engine.submit(r).err().unwrap().code,
        ErrorCode::ExperimentalProfileRequired
    );
}

#[test]
fn strict_mode_reports_unavailable_without_reading() {
    let (_tmp, root, engine) = fixture();
    let mut r = JobRequest::text_index([root]);
    r.cache = Some(Cache {
        validation: Validation::Strict,
        ..Cache::default()
    });
    let summary = engine.submit(r).unwrap().wait().unwrap();
    assert_eq!(summary.exit_code(), 3);
    assert_eq!(summary.counts.files_ready, 0);
    assert_eq!(summary.counts.bytes_read, 0);
}

#[test]
fn export_streams_all_records_and_refuses_overwrite() {
    let (tmp, root, engine) = fixture();
    let summary = engine
        .submit(JobRequest::text_scan([root], 0.7))
        .unwrap()
        .wait()
        .unwrap();
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    for format in ["json", "jsonl", "csv"] {
        let request = ExportRequest {
            schema_version: 1,
            request_id: None,
            run_id: summary.run_id.clone(),
            snapshot_id: None,
            result_revision: None,
            format: format.into(),
            directory: tmp.path().join(format),
        };
        let manifest = catalog.export(request.clone()).unwrap();
        assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 7);
        assert!(catalog.export(request).is_err());
    }
}

#[test]
fn content_edits_invalidate_cache_but_not_old_snapshots() {
    let (tmp, root, engine) = fixture();
    let first = engine
        .submit(JobRequest::text_index([root.clone()]))
        .unwrap()
        .wait()
        .unwrap();
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let query = ResultsQuery {
        snapshot_id: first.snapshot_id.clone(),
        ..ResultsQuery::default()
    };
    let before = serde_json::to_value(catalog.results(query.clone()).unwrap()).unwrap();
    fs::write(
        root.join("a.txt"),
        "A completely different sentence about volcanic minerals.",
    )
    .unwrap();
    let second = engine
        .submit(JobRequest::text_index([root]))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(second.counts.cache_hits, 2);
    assert_eq!(second.counts.vectors_encoded, 1);
    assert_eq!(
        serde_json::to_value(catalog.results(query).unwrap()).unwrap(),
        before
    );
}

#[test]
fn cursors_bind_to_the_query_and_page_without_duplicates() {
    let (tmp, root, engine) = fixture();
    let summary = engine
        .submit(JobRequest::text_index([root.clone()]))
        .unwrap()
        .wait()
        .unwrap();
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let mut query = ResultsQuery {
        snapshot_id: summary.snapshot_id,
        page_size: Some(1),
        ..ResultsQuery::default()
    };
    let mut ids = std::collections::BTreeSet::new();
    let first = catalog.results(query.clone()).unwrap();
    ids.insert(first.items[0]["file_id"].as_str().unwrap().to_owned());
    query.cursor = first.next_cursor.clone();
    let other = engine
        .submit(JobRequest::text_index([root]))
        .unwrap()
        .wait()
        .unwrap();
    let mut wrong = query.clone();
    wrong.snapshot_id = other.snapshot_id;
    assert_eq!(
        catalog.results(wrong).unwrap_err().code,
        ErrorCode::InvalidCursor
    );
    loop {
        let page = catalog.results(query.clone()).unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(ids.insert(page.items[0]["file_id"].as_str().unwrap().to_owned()));
        query.cursor = page.next_cursor;
        if query.cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 3);
}

#[test]
fn cancellation_releases_owner_and_resume_keeps_previous_snapshot() {
    let (tmp, root, engine) = fixture();
    fs::write(
        root.join("large.txt"),
        "Long running text input.\n".repeat(100_000),
    )
    .unwrap();
    let handle = engine
        .submit(JobRequest::text_index([root.clone()]))
        .unwrap();
    assert_eq!(
        engine
            .submit(JobRequest::text_index([root.clone()]))
            .err()
            .unwrap()
            .code,
        ErrorCode::EngineBusy
    );
    handle.cancel();
    let cancelled = handle.wait().unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert!(cancelled.resumable);
    engine.shutdown().unwrap();
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let query = ResultsQuery {
        snapshot_id: cancelled.snapshot_id.clone(),
        ..ResultsQuery::default()
    };
    let old = serde_json::to_value(catalog.results(query.clone()).unwrap()).unwrap();
    fs::write(
        root.join("large.txt"),
        "The replacement can be indexed on a fresh discovery attempt.",
    )
    .unwrap();
    let next = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    let resumed = next.resume(&cancelled.job_id).unwrap().wait().unwrap();
    assert_eq!(resumed.status, "completed");
    assert_eq!(resumed.attempt_id, 2);
    assert_eq!(resumed.counts.files_ready, 4);
    assert!(resumed.elapsed_seconds >= cancelled.elapsed_seconds);
    assert!(resumed.result_bytes_used >= cancelled.result_bytes_used);
    assert_ne!(resumed.snapshot_id, cancelled.snapshot_id);
    assert_eq!(
        serde_json::to_value(catalog.results(query).unwrap()).unwrap(),
        old
    );
}

#[test]
fn exhausted_budget_publishes_partial_state_without_silent_success() {
    let (tmp, root, engine) = fixture();
    let mut request = JobRequest::text_scan([root], 0.7);
    request.limits = Some(Limits {
        result_bytes: Some(1),
        ..Limits::default()
    });
    let summary = engine.submit(request).unwrap().wait().unwrap();
    assert_eq!(summary.status, "checkpointed");
    assert_eq!(summary.exit_code(), 3);
    assert!(!summary.resumable);
    assert!(summary.result_bytes_used <= 1);
    assert_eq!(summary.completeness.source_coverage, "partial");
    assert!(engine.resume(&summary.job_id).is_err());
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    assert!(
        catalog
            .results(ResultsQuery {
                snapshot_id: summary.snapshot_id,
                ..ResultsQuery::default()
            })
            .is_ok()
    );
}

#[test]
fn similarity_chains_do_not_become_invalid_groups() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("input");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "a".repeat(1000)).unwrap();
    fs::write(root.join("b.txt"), "a".repeat(1000) + &"b".repeat(1000)).unwrap();
    fs::write(root.join("c.txt"), "b".repeat(1000)).unwrap();
    let engine = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    let summary = engine
        .submit(JobRequest::text_scan([root], 0.6))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(summary.counts.similar_pairs, 2);
    assert_eq!(summary.counts.groups, 1);
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let groups = catalog
        .results(ResultsQuery {
            run_id: summary.run_id.clone(),
            kind: "groups".into(),
            ..ResultsQuery::default()
        })
        .unwrap();
    assert_eq!(groups.items[0]["member_count"], 2);
    let pairs = catalog
        .results(ResultsQuery {
            run_id: summary.run_id,
            kind: "pairs".into(),
            ..ResultsQuery::default()
        })
        .unwrap();
    assert_eq!(pairs.items.len(), 2); // including the edge outside the chosen clique
}

#[test]
fn overlapping_sources_do_not_bridge_the_within_source_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let x = tmp.path().join("x");
    let y = tmp.path().join("y");
    fs::create_dir(&x).unwrap();
    fs::create_dir(&y).unwrap();
    fs::write(x.join("a.txt"), "one text file").unwrap();
    fs::write(x.join("b.txt"), "second text file").unwrap();
    fs::hard_link(x.join("b.txt"), y.join("b.txt")).unwrap();
    fs::write(y.join("c.txt"), "third text file").unwrap();
    let engine = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    let mut request = JobRequest::text_scan([x, y], -1.0);
    request.pair_scope = Some(PairScope::WithinEachSource);
    let summary = engine.submit(request).unwrap().wait().unwrap();
    assert_eq!(summary.counts.files_ready, 3);
    assert_eq!(summary.counts.similar_pairs, 2);
    assert_eq!(summary.counts.groups, 1);
    let groups = Catalog::open_read_only(tmp.path().join("data"))
        .unwrap()
        .results(ResultsQuery {
            run_id: summary.run_id,
            kind: "groups".into(),
            ..ResultsQuery::default()
        })
        .unwrap();
    assert_eq!(groups.items[0]["member_count"], 2);
}

#[test]
fn nonunicode_paths_are_lossless_and_symlinks_are_excluded() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let (tmp, root, engine) = fixture();
    let name = std::ffi::OsString::from_vec(b"raw-\xff.txt".to_vec());
    let raw_name_created =
        match fs::write(root.join(name), "non Unicode name; valid Unicode content") {
            Ok(()) => true,
            Err(e) if cfg!(target_os = "macos") && e.raw_os_error() == Some(libc::EILSEQ) => false, // APFS rejects invalid UTF-8 filenames.
            Err(e) => panic!("Unexpected filename creation failure: {e}"),
        };
    symlink(root.join("a.txt"), root.join("link.txt")).unwrap();
    let summary = engine
        .submit(JobRequest::text_index([root.clone()]))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(summary.counts.files_ready, 3 + u64::from(raw_name_created));
    assert_eq!(summary.counts.files_excluded, 1);
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let files = catalog
        .results(ResultsQuery {
            snapshot_id: summary.snapshot_id,
            ..ResultsQuery::default()
        })
        .unwrap();
    assert_eq!(
        raw_name_created,
        files
            .items
            .iter()
            .any(|f| f["locator"]["local_path"]["encoding"] == "posix_bytes")
    );
    assert!(
        engine
            .submit(JobRequest::text_index([root.join("link.txt")]))
            .is_err()
    );
}

#[test]
fn compare_retains_extraction_errors_and_exports_are_excluded_from_scans() {
    let (tmp, root, engine) = fixture();
    fs::write(root.join("invalid.txt"), b"bad\x00text").unwrap();
    let first = engine
        .submit(JobRequest::text_scan([root.clone()], 0.7))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(first.exit_code(), 3);
    let request = JobRequest {
        operation: Operation::Compare,
        snapshot_id: first.snapshot_id.clone(),
        matching: JobRequest::text_scan([], 0.7).matching,
        ..JobRequest::default()
    };
    let compared = engine.submit(request).unwrap().wait().unwrap();
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let errors = catalog
        .results(ResultsQuery {
            run_id: compared.run_id,
            kind: "errors".into(),
            ..ResultsQuery::default()
        })
        .unwrap();
    assert_eq!(errors.items.len(), 1);
    catalog
        .export(ExportRequest {
            schema_version: 1,
            request_id: None,
            run_id: first.run_id,
            snapshot_id: None,
            result_revision: None,
            format: "json".into(),
            directory: root.join("report"),
        })
        .unwrap();
    let again = engine
        .submit(JobRequest::text_index([root]))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(again.counts.files_ready, 3);
    assert_eq!(again.counts.files_failed, 1);
    assert_eq!(again.counts.files_excluded, 1);
}

#[test]
fn callback_panics_produce_a_failed_terminal_state() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("invalid.txt");
    fs::write(&input, b"\x00").unwrap();
    let host = HostServices {
        diagnostics: Some(std::sync::Arc::new(|_| panic!("host callback failure"))),
    };
    let engine = Engine::open(EngineConfig::new(tmp.path().join("data")), host).unwrap();
    let summary = engine
        .submit(JobRequest::text_index([input]))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(summary.status, "failed");
    assert_eq!(
        summary.error.as_ref().unwrap().code,
        ErrorCode::InternalError
    );
    let status = Catalog::open_read_only(tmp.path().join("data"))
        .unwrap()
        .status(StatusQuery {
            schema_version: 1,
            job_id: summary.job_id,
            request_id: None,
        })
        .unwrap();
    assert_eq!(status["status"], "failed");
}

#[test]
fn comparison_resume_keeps_frozen_vectors_and_published_revisions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("input");
    fs::create_dir(&root).unwrap();
    for i in 0..100 {
        fs::write(
            root.join(format!("{i}.txt")),
            format!("Unique observation number {i}: reference comparison coverage."),
        )
        .unwrap();
    }
    let engine = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    let index = engine
        .submit(JobRequest::text_index([root.clone()]))
        .unwrap()
        .wait()
        .unwrap();
    let request = JobRequest {
        operation: Operation::Compare,
        snapshot_id: index.snapshot_id.clone(),
        matching: JobRequest::text_scan([], -1.0).matching,
        ..JobRequest::default()
    };
    let handle = engine.submit(request).unwrap();
    let events = handle.events();
    loop {
        match events
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
        {
            JobEvent::Progress { data, .. }
                if data["stage"] == "comparing"
                    && data["counts"]["pairs_compared"].as_u64().unwrap_or(0) > 0 =>
            {
                assert_eq!(data["counts"]["files_processed"], 100);
                assert_eq!(data["counts"]["files_total"], 100);
                assert_eq!(data["counts"]["pairs_total"], 4950);
                assert_eq!(
                    data["counts"]["pairs_processed"],
                    data["counts"]["pairs_compared"]
                );
                handle.cancel();
                break;
            }
            JobEvent::Summary(s) => panic!(
                "Fixture completed before checkpoint: {} pairs",
                s.counts.pairs_compared
            ),
            _ => (),
        }
    }
    let cancelled = handle.wait().unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(cancelled.result_revision, Some(1));
    assert_eq!(cancelled.counts.pairs_total, Some(4950));
    assert!(cancelled.counts.pairs_processed > 0 && cancelled.counts.pairs_processed < 4950);
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    let query = ResultsQuery {
        run_id: cancelled.run_id.clone(),
        result_revision: Some(1),
        kind: "pairs".into(),
        ..ResultsQuery::default()
    };
    let prior = serde_json::to_value(catalog.results(query.clone()).unwrap()).unwrap();
    // A pre-progress-counter checkpoint still resumes with the correct candidate
    // numerator reconstructed from its saved comparison cursor.
    let db = rusqlite::Connection::open(tmp.path().join("data/index.sqlite3")).unwrap();
    db.execute(
        "UPDATE jobs SET counts=json_remove(counts,'$.files_processed','$.files_total','$.pairs_processed','$.pairs_total','$.scores_total') WHERE id=?1",
        [&cancelled.job_id],
    ).unwrap();
    fs::remove_dir_all(root).unwrap();
    let finished = engine.resume(&cancelled.job_id).unwrap().wait().unwrap();
    assert_eq!(finished.status, "completed");
    assert_eq!(finished.run_id, cancelled.run_id);
    assert_eq!(finished.snapshot_id, index.snapshot_id);
    assert_eq!(finished.result_revision, Some(2));
    assert_eq!(finished.counts.pairs_compared, 4950);
    assert_eq!(finished.counts.files_processed, 100);
    assert_eq!(finished.counts.files_total, Some(100));
    assert_eq!(finished.counts.pairs_processed, 4950);
    assert_eq!(finished.counts.pairs_total, Some(4950));
    assert_eq!(finished.counts.similar_pairs, 4950);
    assert_eq!(finished.counts.bytes_read, 0);
    assert_eq!(finished.counts.groups, 1);
    assert_eq!(
        serde_json::to_value(catalog.results(query).unwrap()).unwrap(),
        prior
    );
}

#[test]
fn immediately_cancelled_compare_retains_its_snapshot() {
    let (_tmp, root, engine) = fixture();
    let indexed = engine
        .submit(JobRequest::text_index([root.clone()]))
        .unwrap()
        .wait()
        .unwrap();
    let request = JobRequest {
        operation: Operation::Compare,
        snapshot_id: indexed.snapshot_id.clone(),
        matching: JobRequest::text_scan([], -1.0).matching,
        limits: Some(Limits::default()),
        ..JobRequest::default()
    };
    let request = JobRequest::from_json(&serde_json::to_vec(&request).unwrap()).unwrap();
    let handle = engine.submit(request).unwrap();
    handle.cancel();
    let cancelled = handle.wait().unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(cancelled.snapshot_id, indexed.snapshot_id);
    fs::remove_dir_all(root).unwrap();
    let resumed = engine.resume(&cancelled.job_id).unwrap().wait().unwrap();
    assert_eq!(resumed.status, "completed");
    assert_eq!(resumed.counts.similar_pairs, 3);
}

#[test]
fn byte_identity_includes_empty_and_invalid_text_files() {
    let (_tmp, root, engine) = fixture();
    fs::write(root.join("empty1"), b"").unwrap();
    fs::write(root.join("empty2"), b"").unwrap();
    fs::write(root.join("invalid1"), b"\xff\x00").unwrap();
    fs::write(root.join("invalid2"), b"\xff\x00").unwrap();
    let mut request = JobRequest::text_scan([root], 0.7);
    request.exact_duplicates = Some(ExactDuplicates::Compute);
    let summary = engine.submit(request).unwrap().wait().unwrap();
    assert_eq!(summary.counts.exact_pairs, 2);
    assert_eq!(summary.counts.files_failed, 4);
    assert_eq!(summary.exit_code(), 3);
}

#[test]
fn raw_path_serialization_preserves_bytes() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = b"/absolute/non-unicode-\xff.txt";
    let request = JobRequest::text_index([std::ffi::OsString::from_vec(bytes.to_vec()).into()]);
    let encoded = serde_json::to_vec(&request).unwrap();
    let decoded = JobRequest::from_json(&encoded).unwrap();
    assert_eq!(
        decoded.sources.unwrap()[0]
            .path()
            .unwrap()
            .as_os_str()
            .as_bytes(),
        bytes
    );
}
