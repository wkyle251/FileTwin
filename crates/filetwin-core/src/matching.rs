use crate::{
    Result,
    api::{ExactDuplicates, JobSummary, Operation, PairScope},
    engine::Work,
    profile, scan, store,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

struct Item {
    ordinal: i64,
    file_id: String,
    vector_id: Option<String>,
    digest: Option<String>,
    sources: Vec<String>,
    payload: Value,
}
fn items(db: &rusqlite::Connection, sid: &str, after: i64, limit: u32) -> Result<Vec<Item>> {
    let mut stmt=db.prepare("SELECT ordinal,file_id,vector_id,digest,sources,payload FROM snapshot_files WHERE snapshot_id=?1 AND ordinal>?2 AND (vector_id IS NOT NULL OR digest IS NOT NULL) ORDER BY ordinal LIMIT ?3")?;
    let rows = stmt.query_map(params![sid, after, limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    rows.map(|row| {
        let (ordinal, file_id, vector_id, digest, sources, payload) = row?;
        Ok(Item {
            ordinal,
            file_id,
            vector_id,
            digest,
            sources: serde_json::from_str(&sources)?,
            payload: serde_json::from_str(&payload)?,
        })
    })
    .collect()
}

pub(crate) fn compare(work: &mut Work<'_>) -> Result<()> {
    let sid = work.job.snapshot.clone().expect("Frozen snapshot");
    let run = work.job.run.clone().expect("Comparison run");
    let (request, _) = store::snapshot_request(&work.db, &sid)?;
    let report_exact = request.exact_duplicates != Some(ExactDuplicates::Off);
    if work.job.request.operation == Operation::Compare {
        let (n,ready,excluded,failed)=work.db.query_row("SELECT count(*),coalesce(sum(vector_id IS NOT NULL),0),coalesce(sum(json_extract(payload,'$.state')='excluded'),0),coalesce(sum(json_extract(payload,'$.state') IN ('failed','unsupported','insufficient_content','stale')),0) FROM snapshot_files WHERE snapshot_id=?1",[&sid],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,u64>(1)?,r.get::<_,u64>(2)?,r.get::<_,u64>(3)?)))?;
        work.job.counts.files_discovered = n;
        work.job.counts.files_ready = ready;
        work.job.counts.files_excluded = excluded;
        work.job.counts.files_failed = failed;
        work.job.counts.locations=work.db.query_row("SELECT count(DISTINCT json_extract(payload,'$.location_id')) FROM records WHERE owner=?1 AND revision=1 AND kind='locations'",[&sid],|r|r.get(0))?;
    }
    work.job.counts.files_processed = work.job.counts.files_discovered;
    work.job.counts.files_total = Some(work.job.counts.files_discovered);
    let candidates: u64 = work.db.query_row(
        "SELECT count(*) FROM snapshot_files WHERE snapshot_id=?1 AND (vector_id IS NOT NULL OR digest IS NOT NULL)",
        [&sid],
        |r| r.get(0),
    )?;
    work.job.counts.pairs_total = Some(
        u64::try_from(u128::from(candidates) * u128::from(candidates.saturating_sub(1)) / 2)
            .map_err(|_| {
                crate::Error::invalid("Candidate pair count exceeds the supported range")
            })?,
    );
    // Older jobs have no candidate-work counter. Their durable cursor identifies
    // exactly how many candidates precede the next pair, including skipped pairs.
    if work.job.counts.pairs_processed == 0 && (work.job.next_a > 1 || work.job.next_b > 1) {
        let (before_left, through_right): (u64, u64) = work.db.query_row(
            "SELECT coalesce(sum(ordinal<?2),0),coalesce(sum(ordinal<=?3),0) FROM snapshot_files WHERE snapshot_id=?1 AND (vector_id IS NOT NULL OR digest IS NOT NULL)",
            rusqlite::params![sid, work.job.next_a, work.job.next_b],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let previous_rows = u128::from(before_left)
            * (2 * u128::from(candidates) - u128::from(before_left)).saturating_sub(1)
            / 2;
        let current_row = through_right.saturating_sub(before_left + 1);
        work.job.counts.pairs_processed = u64::try_from(previous_rows + u128::from(current_row))
            .map_err(|_| {
                crate::Error::invalid("Candidate pair count exceeds the supported range")
            })?;
    }
    work.checkpoint()?;
    loop {
        work.check()?;
        let Some(left) = items(&work.db, &sid, work.job.next_a - 1, 1)?
            .into_iter()
            .next()
        else {
            break;
        };
        let left_vector = left
            .vector_id
            .as_deref()
            .map(|id| scan::verify_vector(&work.db, id))
            .transpose()?;
        let right = items(&work.db, &sid, work.job.next_b.max(left.ordinal), 64)?;
        if right.is_empty() {
            work.job.next_a = left.ordinal + 1;
            work.job.next_b = left.ordinal + 1;
            work.checkpoint()?;
            continue;
        }
        let mut pending = Vec::new();
        let mut stop = None;
        for right in right {
            if let Err(error) = work.check() {
                stop = Some(error);
                break;
            }
            let in_scope = work.job.request.pair_scope == Some(PairScope::AllSelected)
                || left.sources.iter().any(|s| right.sources.contains(s));
            if in_scope {
                let mut pair_records = Vec::new();
                let mut compared = 0;
                if let (Some(a), Some(right_id)) = (&left_vector, &right.vector_id)
                    && left.payload["family"] == right.payload["family"]
                    && left.payload["profile_id"] == right.payload["profile_id"]
                {
                    let profile_id = left.payload["profile_id"].as_str().expect("Ready profile");
                    let threshold = work
                        .job
                        .request
                        .matching
                        .as_ref()
                        .filter(|m| m.needs_thresholds())
                        .map(|_| work.job.request.threshold(profile_id))
                        .transpose()?;
                    let score = if left.vector_id == right.vector_id {
                        1.0
                    } else {
                        profile::cosine(a, &scan::verify_vector(&work.db, right_id)?)
                    };
                    compared = 1;
                    if work.job.request.retains_scores() {
                        pair_records.push(("similarity_score", Some(score),
                            json!({"file_a":left.file_id,"file_b":right.file_id,"vector_a":left.vector_id,"vector_b":right.vector_id,"match_kind":"similarity_score","family":left.payload["family"],"profile_id":profile_id,"metric":"cosine","scorer":"filetwin_cosine_f64_v1","score":score,"calibration_status":"uncalibrated"})));
                    }
                    if threshold.is_some_and(|t| score >= t) {
                        let value = json!({"file_a":left.file_id,"file_b":right.file_id,"vector_a":left.vector_id,"vector_b":right.vector_id,"match_kind":"similar_content","family":left.payload["family"],"profile_id":profile_id,"metric":"cosine","score":score,"threshold":threshold,"calibration_status":"uncalibrated"});
                        pair_records.push(("similar_content", Some(score), value));
                    }
                }
                if report_exact
                    && left.digest.is_some()
                    && left.digest == right.digest
                    && left.payload.get("bytes") == right.payload.get("bytes")
                {
                    pair_records.push(("byte_identical",None,json!({"file_a":left.file_id,"file_b":right.file_id,"match_kind":"byte_identical","sha256":left.digest,"bytes":left.payload.get("bytes"),"profile_id":null,"metric":null,"score":null,"threshold":null})));
                }
                let mut bytes = 0;
                for (_, _, value) in &pair_records {
                    bytes += serde_json::to_vec(value)?.len() as u64;
                }
                if let Err(e) = work.charge(bytes) {
                    stop = Some(e);
                    break;
                }
                work.job.counts.pairs_compared += compared;
                for (kind, score, value) in pair_records {
                    if kind == "similarity_score" {
                        work.job.counts.scores_retained += 1;
                    } else if kind == "similar_content" {
                        work.job.counts.similar_pairs += 1;
                    } else {
                        work.job.counts.exact_pairs += 1;
                    }
                    pending.push((
                        left.file_id.clone(),
                        right.file_id.clone(),
                        kind,
                        score,
                        serde_json::to_string(&value)?,
                    ));
                }
            }
            work.job.counts.pairs_processed += 1;
            work.job.next_a = left.ordinal;
            work.job.next_b = right.ordinal;
        }
        {
            let tx = work.db.unchecked_transaction()?;
            for (a, b, kind, score, value) in pending {
                tx.execute("INSERT OR IGNORE INTO work_pairs(run_id,a,b,kind,score,payload) VALUES(?1,?2,?3,?4,?5,?6)",params![run,a,b,kind,score,value])?;
            }
            work.job.elapsed = work.elapsed();
            store::save_job(&tx, &work.job)?;
            tx.commit()?;
        }
        work.checkpoint()?;
        if let Some(e) = stop {
            return Err(e);
        }
    }
    work.job.stage = if work
        .job
        .request
        .matching
        .as_ref()
        .is_some_and(|m| m.grouping == "none")
    {
        "scored"
    } else {
        "grouping"
    }
    .into();
    work.checkpoint()
}

/// Filter immutable score records without loading vectors, models, or originals.
/// Records, counters, budget consumption and the key cursor commit together.
pub(crate) fn filter_scores(work: &mut Work<'_>) -> Result<()> {
    let source = store::score_run(
        &work.db,
        work.job
            .request
            .source_run_id
            .as_deref()
            .expect("Score run"),
        work.job.request.source_revision,
    )?;
    let source_run = work.job.request.source_run_id.clone().expect("Score run");
    let run = work.job.run.clone().expect("Group run");
    work.job.counts.files_discovered = source.summary.counts.files_discovered;
    work.job.counts.files_processed = source.summary.counts.files_discovered;
    work.job.counts.files_total = Some(source.summary.counts.files_discovered);
    work.job.counts.files_ready = source.summary.counts.files_ready;
    work.job.counts.files_failed = source.summary.counts.files_failed;
    work.job.counts.files_excluded = source.summary.counts.files_excluded;
    work.job.counts.locations = source.summary.counts.locations;
    work.job.counts.scores_total = Some(work.db.query_row(
        "SELECT count(*) FROM records WHERE owner=?1 AND revision=?2 AND kind='scores'",
        params![source_run, source.revision],
        |r| r.get(0),
    )?);
    work.job.stage = "filtering".into();
    work.checkpoint()?;
    loop {
        work.check()?;
        let rows = {
            let mut stmt = work.db.prepare("SELECT key,payload FROM records WHERE owner=?1 AND revision=?2 AND kind='scores' AND key>?3 UNION ALL SELECT key,payload FROM records WHERE owner=?1 AND revision=?2 AND kind='pairs' AND key>?3 AND json_extract(payload,'$.match_kind')='byte_identical' ORDER BY key LIMIT 64")?;
            stmt.query_map(
                params![source_run, source.revision, work.job.score_cursor],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }
        let mut pending = Vec::new();
        let mut stop = None;
        for (key, raw) in rows {
            if let Err(error) = work.check() {
                stop = Some(error);
                break;
            }
            let mut value: Value = serde_json::from_str(&raw)?;
            let scored = value["match_kind"] == "similarity_score";
            let keep = if scored {
                let score = value["score"]
                    .as_f64()
                    .filter(|score| score.is_finite() && (-1.0..=1.0).contains(score))
                    .ok_or_else(|| {
                        crate::Error::new(
                            crate::ErrorCode::DatabaseCorrupt,
                            "scores",
                            "Saved score is missing or is not a finite cosine value",
                        )
                    })?;
                let threshold = work.job.request.threshold(
                    value["profile_id"]
                        .as_str()
                        .ok_or_else(|| crate::Error::invalid("Saved score has no profile"))?,
                )?;
                value["match_kind"] = json!("similar_content");
                value["threshold"] = json!(threshold);
                score >= threshold
            } else {
                true
            };
            if keep {
                let payload = serde_json::to_string(&value)?;
                if let Err(error) = work.charge(payload.len() as u64) {
                    stop = Some(error);
                    break;
                }
                if scored {
                    work.job.counts.similar_pairs += 1;
                } else {
                    work.job.counts.exact_pairs += 1;
                }
                pending.push((value, payload));
            }
            if scored {
                work.job.counts.scores_reused += 1;
            }
            work.job.score_cursor = key;
        }
        let tx = work.db.unchecked_transaction()?;
        for (value, payload) in pending {
            tx.execute("INSERT OR IGNORE INTO work_pairs(run_id,a,b,kind,score,payload) VALUES(?1,?2,?3,?4,?5,?6)",
                params![run,value["file_a"].as_str(),value["file_b"].as_str(),value["match_kind"].as_str(),value["score"].as_f64(),payload])?;
        }
        work.job.elapsed = work.elapsed();
        store::save_job(&tx, &work.job)?;
        tx.commit()?;
        work.checkpoint()?;
        if let Some(error) = stop {
            return Err(error);
        }
    }
    work.job.stage = "grouping".into();
    work.checkpoint()
}

pub(crate) fn group(work: &mut Work<'_>) -> Result<()> {
    let run = work.job.run.clone().expect("Run");
    work.db
        .execute("DELETE FROM work_members WHERE run_id=?1", [&run])?;
    work.db
        .execute("DELETE FROM work_groups WHERE run_id=?1", [&run])?;
    for kind in ["similar_content", "byte_identical"] {
        let mut last = String::new();
        let mut ordinal = 0i64;
        loop {
            work.check()?;
            let files = {
                let mut stmt=work.db.prepare("SELECT file_id FROM (SELECT a AS file_id FROM work_pairs WHERE run_id=?1 AND kind=?2 UNION SELECT b AS file_id FROM work_pairs WHERE run_id=?1 AND kind=?2) WHERE file_id>?3 ORDER BY file_id LIMIT 64")?;
                stmt.query_map(params![run, kind, last], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            if files.is_empty() {
                break;
            }
            for fid in files {
                work.check()?;
                last = fid.clone();
                // Every edge must be present, so both cutoff and pair_scope are
                // enforced. The first admissible group is selected deterministically.
                let candidate=work.db.query_row("SELECT g.group_id,g.minimum FROM work_groups g WHERE g.run_id=?1 AND g.kind=?2 AND NOT EXISTS (SELECT 1 FROM work_members m WHERE m.run_id=g.run_id AND m.kind=g.kind AND m.group_id=g.group_id AND NOT EXISTS (SELECT 1 FROM work_pairs p WHERE p.run_id=g.run_id AND p.kind=g.kind AND p.a=min(m.file_id,?3) AND p.b=max(m.file_id,?3))) ORDER BY g.ordinal LIMIT 1",params![run,kind,fid],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<f64>>(1)?))).optional()?;
                work.charge((fid.len() + 160) as u64)?;
                let group_id = if let Some((gid, minimum)) = candidate {
                    let score:Option<f64>=work.db.query_row("SELECT min(p.score) FROM work_pairs p JOIN work_members m ON m.run_id=p.run_id AND m.kind=p.kind AND m.group_id=?4 AND ((p.a=?3 AND p.b=m.file_id) OR (p.b=?3 AND p.a=m.file_id)) WHERE p.run_id=?1 AND p.kind=?2",params![run,kind,fid,gid],|r|r.get(0))?;
                    let minimum = match (minimum, score) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    work.db.execute("UPDATE work_groups SET minimum=?4 WHERE run_id=?1 AND kind=?2 AND group_id=?3",params![run,kind,gid,minimum])?;
                    gid
                } else {
                    work.charge(512)?;
                    ordinal += 1;
                    let gid = format!(
                        "group_{}",
                        profile::digest_hex(format!("{run}:{kind}:{fid}").as_bytes())
                    );
                    work.db.execute("INSERT INTO work_groups(run_id,kind,group_id,ordinal,minimum) VALUES(?1,?2,?3,?4,NULL)",params![run,kind,gid,ordinal])?;
                    gid
                };
                work.db.execute(
                    "INSERT INTO work_members(run_id,kind,group_id,file_id) VALUES(?1,?2,?3,?4)",
                    params![run, kind, group_id, fid],
                )?;
            }
            work.checkpoint()?;
        }
    }
    work.job.counts.groups=work.db.query_row("SELECT count(*) FROM (SELECT group_id FROM work_members WHERE run_id=?1 GROUP BY kind,group_id HAVING count(*)>=2)",[&run],|r|r.get(0))?;
    work.checkpoint()
}

pub(crate) fn publish(work: &Work<'_>, summary: &JobSummary) -> Result<()> {
    let run = summary.run_id.as_ref().expect("Run");
    let sid = summary.snapshot_id.as_ref().expect("Snapshot");
    let revision = summary.result_revision.expect("Revision");
    let tx = work.db.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO publications(run_id,revision,summary) VALUES(?1,?2,?3)",
        params![run, revision, serde_json::to_string(summary)?],
    )?;
    tx.execute("INSERT INTO records(owner,revision,kind,key,filter_id,payload) SELECT run_id,?2,CASE WHEN kind='similarity_score' THEN 'scores' ELSE 'pairs' END,a||'/'||b||'/'||kind,NULL,payload FROM work_pairs WHERE run_id=?1",params![run,revision])?;
    if summary.status == "completed" {
        let mut stmt=tx.prepare("SELECT g.group_id,g.kind,g.minimum,count(m.file_id),min(m.file_id) FROM work_groups g JOIN work_members m ON m.run_id=g.run_id AND m.kind=g.kind AND m.group_id=g.group_id WHERE g.run_id=?1 GROUP BY g.group_id,g.kind,g.minimum HAVING count(m.file_id)>=2 ORDER BY g.kind,g.ordinal")?;
        let mut rows = stmt.query([run])?;
        while let Some(row) = rows.next()? {
            let gid: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let minimum: Option<f64> = row.get(2)?;
            let count: u64 = row.get(3)?;
            let representative: String = row.get(4)?;
            let similar = kind == "similar_content";
            let representative_payload: String = tx.query_row(
                "SELECT payload FROM snapshot_files WHERE snapshot_id=?1 AND file_id=?2",
                params![sid, representative],
                |r| r.get(0),
            )?;
            let representative_payload: Value = serde_json::from_str(&representative_payload)?;
            let family = similar
                .then(|| representative_payload["family"].as_str())
                .flatten();
            let profile_id = similar
                .then(|| representative_payload["profile_id"].as_str())
                .flatten();
            let threshold = profile_id
                .map(|id| work.job.request.threshold(id))
                .transpose()?;
            let payload = json!({"group_id":gid,"match_kind":kind,"family":family,"profile_id":profile_id,"metric":if similar{Some("cosine")}else{None},"member_count":count,"representative_file_id":representative,"minimum_score":minimum,"threshold":threshold});
            store::record(&tx, run, revision, "groups", &gid, None, &payload)?;
            let mut members=tx.prepare("SELECT m.file_id,f.payload FROM work_members m JOIN snapshot_files f ON f.snapshot_id=?4 AND f.file_id=m.file_id WHERE m.run_id=?1 AND m.kind=?2 AND m.group_id=?3 ORDER BY m.file_id")?;
            let mut member_rows = members.query(params![run, kind, gid, sid])?;
            while let Some(member) = member_rows.next()? {
                let fid: String = member.get(0)?;
                let mut value: Value = serde_json::from_str(&member.get::<_, String>(1)?)?;
                value["group_id"] = json!(gid);
                store::record(
                    &tx,
                    run,
                    revision,
                    "members",
                    &format!("{gid}/{fid}"),
                    Some(&gid),
                    &value,
                )?;
            }
        }
    }
    if work.job.request.operation.uses_saved_input() {
        tx.execute("INSERT INTO records(owner,revision,kind,key,filter_id,payload) SELECT ?1,?2,'errors','snapshot/'||key,NULL,payload FROM records WHERE owner=?3 AND revision=1 AND kind='errors'",params![run,revision,sid])?;
    }
    store::copy_errors(&tx, &work.job, run, revision)?;
    store::record(
        &tx,
        run,
        revision,
        "summary",
        "summary",
        None,
        &serde_json::to_value(summary)?,
    )?;
    tx.commit()?;
    Ok(())
}
