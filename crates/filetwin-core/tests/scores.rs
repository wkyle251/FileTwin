use filetwin_core::{Catalog, Engine, ErrorCode, HostServices, api::*, profile};
use rusqlite::{Connection, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

fn fixture() -> (tempfile::TempDir, PathBuf, Engine, Catalog) {
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
    let catalog = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    (tmp, root, engine, catalog)
}

fn grouping(run: &str, threshold: f64) -> JobRequest {
    JobRequest::group_scores(
        run,
        BTreeMap::from([(profile::text_profile().profile_id, threshold)]),
    )
}

fn scores(run: &str) -> ResultsQuery {
    ResultsQuery {
        run_id: Some(run.into()),
        kind: "scores".into(),
        ..ResultsQuery::default()
    }
}

#[test]
fn every_score_survives_and_groups_reuse_evidence_without_vectors() {
    let (tmp, root, engine, catalog) = fixture();
    let saved = engine
        .submit(JobRequest::text_scores([root.clone()]))
        .unwrap()
        .wait()
        .unwrap();
    let run = saved.run_id.as_deref().unwrap();
    assert_eq!(
        (
            saved.counts.pairs_compared,
            saved.counts.scores_retained,
            saved.counts.similar_pairs,
            saved.counts.groups
        ),
        (3, 3, 0, 0)
    );
    let all = catalog.results(scores(run)).unwrap();
    assert_eq!(all.items.len(), 3);
    assert!(all.items.iter().any(|p| p["score"].as_f64().unwrap() < 0.6));
    let matrix = catalog.matrix(MatrixQuery::new(run)).unwrap();
    assert_eq!(matrix.total_files, 3);
    for i in 0..3 {
        assert_eq!(matrix.scores[i][i], Some(1.0));
        for j in 0..3 {
            assert_eq!(matrix.scores[i][j], matrix.scores[j][i]);
        }
    }
    let block = catalog
        .matrix(MatrixQuery {
            row_offset: 1,
            column_offset: 2,
            row_limit: 1,
            column_limit: 1,
            ..MatrixQuery::new(run)
        })
        .unwrap();
    assert_eq!(block.scores, vec![vec![matrix.scores[1][2]]]);
    assert_eq!(block.next_row_offset, Some(2));
    assert_eq!(block.next_column_offset, None);
    let mut query = scores(run);
    query.min_score = Some(0.6);
    query.page_size = Some(1);
    let first = catalog.results(query.clone()).unwrap();
    query.cursor = first.next_cursor;
    assert!(query.cursor.is_some());
    let second = catalog.results(query.clone()).unwrap();
    assert_eq!(second.items.len(), 1);
    assert_ne!(first.items, second.items);
    query.min_score = Some(0.7);
    assert_eq!(
        catalog.results(query).unwrap_err().code,
        ErrorCode::InvalidCursor
    );

    // A vector comparison would now fail its checksum. Grouping and matrices
    // must use the retained evidence and work after the originals disappear.
    fs::remove_dir_all(root).unwrap();
    let db = Connection::open(tmp.path().join("data/index.sqlite3")).unwrap();
    db.execute(
        "UPDATE vectors SET checksum='deliberately-invalid-test-checksum'",
        [],
    )
    .unwrap();
    for (threshold, pairs, groups) in [(0.95, 0, 0), (0.6, 2, 1), (-1.0, 3, 1)] {
        let grouped = engine
            .submit(grouping(run, threshold))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(grouped.status, "completed", "{grouped:?}");
        assert_eq!(
            (
                grouped.counts.pairs_compared,
                grouped.counts.vectors_encoded,
                grouped.counts.bytes_read
            ),
            (0, 0, 0)
        );
        assert_eq!(grouped.counts.scores_reused, 3);
        assert_eq!(
            (grouped.counts.similar_pairs, grouped.counts.groups),
            (pairs, groups)
        );
        if threshold == 0.6 {
            let headers = catalog
                .results(ResultsQuery {
                    run_id: grouped.run_id,
                    kind: "groups".into(),
                    ..ResultsQuery::default()
                })
                .unwrap();
            assert_eq!(headers.items[0]["member_count"], 2); // A-B-C is not a clique.
        }
    }
    assert_eq!(catalog.results(scores(run)).unwrap().items, all.items);
    assert_eq!(
        catalog.matrix(MatrixQuery::new(run)).unwrap().scores,
        matrix.scores
    );
    for format in ["json", "jsonl", "csv"] {
        let manifest = catalog
            .export(ExportRequest {
                schema_version: 1,
                request_id: None,
                run_id: Some(run.into()),
                snapshot_id: None,
                result_revision: None,
                format: format.into(),
                directory: tmp.path().join(format),
            })
            .unwrap();
        let artifact = manifest["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["kind"] == "scores")
            .unwrap();
        assert_eq!(artifact["records"], 3);
    }
}

#[test]
fn zero_and_negative_scores_are_numbers_and_partial_scores_are_null() {
    let (tmp, root, engine, catalog) = fixture();
    let indexed = engine
        .submit(JobRequest::text_index([root]))
        .unwrap()
        .wait()
        .unwrap();
    let sid = indexed.snapshot_id.unwrap();
    let db = Connection::open(tmp.path().join("data/index.sqlite3")).unwrap();
    let ids: Vec<String> = db
        .prepare("SELECT vector_id FROM snapshot_files WHERE snapshot_id=?1 ORDER BY ordinal")
        .unwrap()
        .query_map([&sid], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (id, coordinates) in ids.iter().zip([[1.0f32, 0.0], [0.0, 1.0], [-1.0, 0.0]]) {
        let mut vector = vec![0.0f32; 4096];
        vector[..2].copy_from_slice(&coordinates);
        let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        db.execute(
            "UPDATE vectors SET payload=?2,checksum=?3 WHERE id=?1",
            params![id, bytes, checksum],
        )
        .unwrap();
    }
    let request = JobRequest {
        operation: Operation::Compare,
        snapshot_id: Some(sid),
        matching: Some(Matching::all_scores()),
        ..JobRequest::default()
    };
    let complete = engine.submit(request.clone()).unwrap().wait().unwrap();
    let matrix = catalog
        .matrix(MatrixQuery::new(complete.run_id.unwrap()))
        .unwrap();
    assert_eq!(
        matrix.scores,
        vec![
            vec![Some(1.0), Some(0.0), Some(-1.0)],
            vec![Some(0.0), Some(1.0), Some(0.0)],
            vec![Some(-1.0), Some(0.0), Some(1.0)]
        ]
    );
    assert!(
        matrix
            .unavailable_reasons
            .iter()
            .flatten()
            .all(Option::is_none)
    );
    let partial = engine
        .submit(JobRequest {
            limits: Some(Limits {
                result_bytes: Some(1),
                ..Limits::default()
            }),
            ..request
        })
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(partial.exit_code(), 3);
    let run = partial.run_id.unwrap();
    let matrix = catalog.matrix(MatrixQuery::new(&run)).unwrap();
    assert_eq!(matrix.scores[0][1], None);
    assert_eq!(
        matrix.unavailable_reasons[0][1],
        Some(MatrixUnavailable::NotComputed)
    );
    assert_eq!(matrix.completeness.comparison_coverage, "partial");
    assert!(engine.submit(grouping(&run, 0.6)).is_err());
}

#[test]
fn unavailable_files_and_scope_exclusions_are_not_zero_similarity() {
    let (tmp, root, engine, catalog) = fixture();
    let bad = root.join("opaque.bin");
    fs::write(&bad, b"\0\xffinvalid").unwrap();
    let mut request = JobRequest::text_scores([root.join("a.txt"), root.join("b.txt"), bad]);
    request.pair_scope = Some(PairScope::WithinEachSource);
    request.exact_duplicates = Some(ExactDuplicates::Compute);
    let saved = engine.submit(request).unwrap().wait().unwrap();
    assert_eq!(saved.counts.files_ready, 2);
    assert_eq!(saved.counts.scores_retained, 0);
    let run = saved.run_id.unwrap();
    let matrix = catalog.matrix(MatrixQuery::new(&run)).unwrap();
    let failed = matrix
        .rows
        .iter()
        .position(|f| f.vector_id.is_none())
        .unwrap();
    assert_eq!(matrix.scores[failed][failed], None);
    assert_eq!(
        matrix.unavailable_reasons[failed][failed],
        Some(MatrixUnavailable::FileUnavailable)
    );
    let ready: Vec<_> = matrix
        .rows
        .iter()
        .enumerate()
        .filter(|(_, f)| f.vector_id.is_some())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        matrix.unavailable_reasons[ready[0]][ready[1]],
        Some(MatrixUnavailable::OutsideScope)
    );
    let mut wrong = grouping(&run, 0.6);
    wrong.pair_scope = Some(PairScope::AllSelected);
    assert!(engine.submit(wrong).is_err());
    let grouped = engine.submit(grouping(&run, 0.6)).unwrap().wait().unwrap();
    assert_eq!(grouped.counts.files_failed, saved.counts.files_failed);
    assert_eq!(grouped.exit_code(), 3);
    assert_eq!(grouped.counts.groups, 0);
    assert!(
        !catalog
            .results(ResultsQuery {
                run_id: grouped.run_id,
                kind: "errors".into(),
                ..ResultsQuery::default()
            })
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        catalog
            .matrix(MatrixQuery {
                row_limit: 257,
                ..MatrixQuery::new(&run)
            })
            .is_err()
    );
    assert!(
        catalog
            .matrix(MatrixQuery {
                column_offset: 4,
                ..MatrixQuery::new(&run)
            })
            .is_err()
    );
    assert_eq!(
        catalog
            .matrix_with_cancel(MatrixQuery::new(&run), &|| true)
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
    drop(tmp);
}

#[test]
fn exact_duplicates_remain_separate_from_scores() {
    let (_tmp, root, engine, catalog) = fixture();
    fs::copy(root.join("a.txt"), root.join("copy.txt")).unwrap();
    let mut request = JobRequest::text_scores([root]);
    request.exact_duplicates = Some(ExactDuplicates::Compute);
    // Retaining all scores can also accompany the existing immediate grouping.
    request.matching = Some(Matching {
        score_retention: ScoreRetention::All,
        threshold_overrides: BTreeMap::from([(profile::text_profile().profile_id, 0.95)]),
        ..Matching::default()
    });
    let saved = engine.submit(request).unwrap().wait().unwrap();
    assert_eq!(
        (saved.counts.scores_retained, saved.counts.exact_pairs),
        (6, 1)
    );
    let run = saved.run_id.unwrap();
    assert_eq!(catalog.results(scores(&run)).unwrap().items.len(), 6);
    let grouped = engine.submit(grouping(&run, 1.0)).unwrap().wait().unwrap();
    assert_eq!(
        (
            grouped.counts.similar_pairs,
            grouped.counts.exact_pairs,
            grouped.counts.scores_reused
        ),
        (1, 1, 6)
    );
    let pairs = catalog
        .results(ResultsQuery {
            run_id: grouped.run_id,
            kind: "pairs".into(),
            ..ResultsQuery::default()
        })
        .unwrap();
    assert!(
        pairs
            .items
            .iter()
            .any(|p| p["match_kind"] == "byte_identical" && p["score"].is_null())
    );
}

#[test]
fn corrupt_saved_scores_do_not_silently_become_nonmatches() {
    let (tmp, root, engine, catalog) = fixture();
    let saved = engine
        .submit(JobRequest::text_scores([root]))
        .unwrap()
        .wait()
        .unwrap();
    let run = saved.run_id.unwrap();
    let db = Connection::open(tmp.path().join("data/index.sqlite3")).unwrap();
    db.execute("UPDATE records SET payload=json_set(payload,'$.score',NULL) WHERE owner=?1 AND kind='scores'", [&run]).unwrap();
    let grouped = engine.submit(grouping(&run, 0.6)).unwrap().wait().unwrap();
    assert_eq!(grouped.status, "failed");
    assert_eq!(grouped.error.unwrap().code, ErrorCode::DatabaseCorrupt);
    assert!(catalog.matrix(MatrixQuery::new(run)).is_err());
}

#[test]
fn old_indexes_upgrade_without_changing_existing_results() {
    let (tmp, root, engine, catalog) = fixture();
    let saved = engine
        .submit(JobRequest::text_scan([root], 0.6))
        .unwrap()
        .wait()
        .unwrap();
    engine.shutdown().unwrap();
    let old_groups = catalog
        .results(ResultsQuery {
            run_id: saved.run_id.clone(),
            kind: "groups".into(),
            ..ResultsQuery::default()
        })
        .unwrap()
        .items;
    let db = Connection::open(tmp.path().join("data/index.sqlite3")).unwrap();
    // Reproduce v1's schema and missing optional serialized fields.
    db.execute_batch("ALTER TABLE jobs DROP COLUMN score_cursor; PRAGMA user_version=1;")
        .unwrap();
    let mut request: Value = serde_json::from_str(
        &db.query_row(
            "SELECT request FROM jobs WHERE id=?1",
            [&saved.job_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap(),
    )
    .unwrap();
    request["matching"]
        .as_object_mut()
        .unwrap()
        .remove("score_retention");
    let mut counts = serde_json::to_value(&saved.counts).unwrap();
    counts.as_object_mut().unwrap().remove("scores_retained");
    counts.as_object_mut().unwrap().remove("scores_reused");
    db.execute(
        "UPDATE jobs SET request=?2,counts=?3 WHERE id=?1",
        params![saved.job_id, request.to_string(), counts.to_string()],
    )
    .unwrap();
    let reader = Catalog::open_read_only(tmp.path().join("data")).unwrap();
    assert_eq!(
        reader
            .status(StatusQuery {
                schema_version: 1,
                job_id: saved.job_id,
                request_id: None
            })
            .unwrap()["counts"]["scores_retained"],
        0
    );
    assert_eq!(
        reader
            .matrix(MatrixQuery::new(saved.run_id.clone().unwrap()))
            .unwrap_err()
            .code,
        ErrorCode::UnsupportedCapability
    );
    let engine = Engine::open(
        EngineConfig::new(tmp.path().join("data")),
        HostServices::default(),
    )
    .unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let scored = engine
        .submit(JobRequest {
            operation: Operation::Compare,
            snapshot_id: saved.snapshot_id,
            matching: Some(Matching::all_scores()),
            ..JobRequest::default()
        })
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(
        (scored.counts.scores_retained, scored.counts.bytes_read),
        (3, 0)
    );
    assert_eq!(
        reader
            .results(ResultsQuery {
                run_id: saved.run_id,
                kind: "groups".into(),
                ..ResultsQuery::default()
            })
            .unwrap()
            .items,
        old_groups
    );
}

#[test]
fn cancelled_scoring_and_grouping_resume_without_duplicate_evidence() {
    let (tmp, root, engine, catalog) = fixture();
    for i in 0..117 {
        fs::write(
            root.join(format!("extra-{i}.txt")),
            format!("different saved input {i}"),
        )
        .unwrap();
    }
    let indexed = engine
        .submit(JobRequest::text_index([root]))
        .unwrap()
        .wait()
        .unwrap();
    let scoring = engine
        .submit(JobRequest {
            operation: Operation::Compare,
            snapshot_id: indexed.snapshot_id,
            matching: Some(Matching::all_scores()),
            ..JobRequest::default()
        })
        .unwrap();
    scoring.cancel();
    let stopped = scoring.wait().unwrap();
    assert_eq!(stopped.status, "cancelled");
    let old_query = MatrixQuery {
        result_revision: stopped.result_revision,
        row_limit: 2,
        column_limit: 2,
        ..MatrixQuery::new(stopped.run_id.clone().unwrap())
    };
    let old = catalog.matrix(old_query.clone()).unwrap().scores;
    let complete = engine.resume(scoring.id()).unwrap().wait().unwrap();
    let total = 120 * 119 / 2;
    assert_eq!(complete.counts.scores_retained, total);
    assert_eq!(catalog.matrix(old_query).unwrap().scores, old);
    let group = engine
        .submit(grouping(complete.run_id.as_deref().unwrap(), 1.0))
        .unwrap();
    let start = Instant::now();
    loop {
        let status = catalog
            .status(StatusQuery {
                schema_version: 1,
                job_id: group.id().into(),
                request_id: None,
            })
            .unwrap();
        if status["counts"]["scores_reused"].as_u64().unwrap() >= 64 {
            group.cancel();
            break;
        }
        assert!(!group.is_finished());
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(1));
    }
    let interrupted = group.wait().unwrap();
    assert_eq!(interrupted.status, "cancelled");
    assert!(interrupted.counts.scores_reused > 0 && interrupted.counts.scores_reused < total);
    let resumed = engine.resume(group.id()).unwrap().wait().unwrap();
    assert_eq!(resumed.status, "completed");
    assert_eq!(resumed.counts.scores_reused, total);
    assert_eq!(resumed.counts.pairs_compared, 0);
    assert!(resumed.result_bytes_used >= interrupted.result_bytes_used);
    drop(tmp);
}
