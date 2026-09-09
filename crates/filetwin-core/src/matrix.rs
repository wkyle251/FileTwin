use crate::{Catalog, Error, ErrorCode, Result, api::*, store};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

struct Axis {
    file: MatrixFile,
    sources: Vec<String>,
}

fn axis(db: &Connection, snapshot: &str, offset: u64, limit: u32) -> Result<Vec<Axis>> {
    let mut stmt = db.prepare("SELECT payload,sources,vector_id FROM snapshot_files WHERE snapshot_id=?1 ORDER BY ordinal LIMIT ?2 OFFSET ?3")?;
    let rows = stmt.query_map(params![snapshot, limit, offset], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (payload, sources, vector_id) = row?;
        let value: Value = serde_json::from_str(&payload)?;
        Ok(Axis {
            file: MatrixFile {
                file_id: serde_json::from_value(value["file_id"].clone())?,
                vector_id,
                locator: serde_json::from_value(value["locator"].clone())?,
                family: value["family"].as_str().map(str::to_owned),
                profile_id: value["profile_id"].as_str().map(str::to_owned),
                state: serde_json::from_value(value["state"].clone())?,
            },
            sources: serde_json::from_str(&sources)?,
        })
    })
    .collect()
}

impl Catalog {
    /// Read a matrix block from saved scores. Missing/incompatible scores are
    /// null with reasons; this never loads vectors or substitutes zero.
    pub fn matrix(&self, query: MatrixQuery) -> Result<MatrixPage> {
        self.matrix_with_cancel(query, &|| false)
    }

    pub fn matrix_with_cancel(
        &self,
        query: MatrixQuery,
        cancel: &dyn Fn() -> bool,
    ) -> Result<MatrixPage> {
        if query.schema_version != SCHEMA_VERSION {
            return Err(Error::new(
                ErrorCode::UnsupportedSchemaVersion,
                "matrix",
                "Only schema_version 1 is supported",
            ));
        }
        if !(1..=256).contains(&query.row_limit) || !(1..=256).contains(&query.column_limit) {
            return Err(Error::invalid(
                "Matrix row_limit and column_limit must be in 1..256",
            ));
        }
        let source = store::score_run(&self.db, &query.run_id, query.result_revision)?;
        let total: u64 = self.db.query_row(
            "SELECT count(*) FROM snapshot_files WHERE snapshot_id=?1",
            [&source.snapshot],
            |r| r.get(0),
        )?;
        if query.row_offset > total || query.column_offset > total {
            return Err(Error::invalid("Matrix offsets must not exceed total_files"));
        }
        let rows = axis(
            &self.db,
            &source.snapshot,
            query.row_offset,
            query.row_limit,
        )?;
        let columns = axis(
            &self.db,
            &source.snapshot,
            query.column_offset,
            query.column_limit,
        )?;
        let mut lookup = self.db.prepare("SELECT json_extract(payload,'$.score') FROM records WHERE owner=?1 AND revision=?2 AND kind='scores' AND key=?3")?;
        let mut scores = Vec::with_capacity(rows.len());
        let mut reasons = Vec::with_capacity(rows.len());
        for a in &rows {
            if cancel() {
                return Err(Error::new(
                    ErrorCode::Cancelled,
                    "matrix",
                    "Matrix query cancelled",
                ));
            }
            let mut score_row = Vec::with_capacity(columns.len());
            let mut reason_row = Vec::with_capacity(columns.len());
            for b in &columns {
                let unavailable = if a.file.vector_id.is_none() || b.file.vector_id.is_none() {
                    Some(MatrixUnavailable::FileUnavailable)
                } else if a.file.family != b.file.family || a.file.profile_id != b.file.profile_id {
                    Some(MatrixUnavailable::IncompatibleProfile)
                } else if source.request.pair_scope == Some(PairScope::WithinEachSource)
                    && !a.sources.iter().any(|s| b.sources.contains(s))
                {
                    Some(MatrixUnavailable::OutsideScope)
                } else {
                    None
                };
                let (score, reason) = if let Some(reason) = unavailable {
                    (None, Some(reason))
                } else if a.file.file_id == b.file.file_id {
                    (Some(1.0), None)
                } else {
                    let (left, right) = if a.file.file_id < b.file.file_id {
                        (&a.file.file_id, &b.file.file_id)
                    } else {
                        (&b.file.file_id, &a.file.file_id)
                    };
                    let key = format!("{left}/{right}/similarity_score");
                    let score = lookup
                        .query_row(params![query.run_id, source.revision, key], |r| {
                            r.get::<_, f64>(0)
                        })
                        .optional()?;
                    if score.is_some_and(|s| !s.is_finite() || !(-1.0..=1.0).contains(&s)) {
                        return Err(Error::new(
                            ErrorCode::StorageError,
                            "matrix",
                            "Saved score is not a finite cosine value",
                        ));
                    }
                    (
                        score,
                        score.is_none().then_some(MatrixUnavailable::NotComputed),
                    )
                };
                score_row.push(score);
                reason_row.push(reason);
            }
            scores.push(score_row);
            reasons.push(reason_row);
        }
        let row_end = query.row_offset + rows.len() as u64;
        let column_end = query.column_offset + columns.len() as u64;
        let page = MatrixPage {
            snapshot_id: source.snapshot,
            run_id: query.run_id,
            result_revision: source.revision,
            metric: "cosine".into(),
            score_range: [-1.0, 1.0],
            total_files: total,
            row_offset: query.row_offset,
            column_offset: query.column_offset,
            rows: rows.into_iter().map(|a| a.file).collect(),
            columns: columns.into_iter().map(|a| a.file).collect(),
            scores,
            unavailable_reasons: reasons,
            next_row_offset: (row_end < total).then_some(row_end),
            next_column_offset: (column_end < total).then_some(column_end),
            completeness: source.summary.completeness,
        };
        if serde_json::to_vec(&page)?.len() > MAX_PAGE_BYTES - 32768 {
            return Err(Error::new(
                ErrorCode::RecordTooLarge,
                "matrix",
                "Matrix block exceeds the 4 MiB response limit; request fewer rows or columns",
            ));
        }
        Ok(page)
    }
}
