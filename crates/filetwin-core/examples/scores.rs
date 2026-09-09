//! Score once, inspect a matrix and filtered pairs, then group without rescoring.
//! This text-only example requires no native model setup.
use filetwin_core::{Catalog, Engine, HostServices, api::*};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let data = PathBuf::from(
        args.next()
            .ok_or("Usage: scores ABSOLUTE_DATA_DIR ABSOLUTE_SOURCE")?,
    );
    let source = PathBuf::from(args.next().ok_or("Missing absolute source path")?);
    let engine = Engine::open(EngineConfig::new(&data), HostServices::default())?;
    let summary = engine.submit(JobRequest::text_scores([source]))?.wait()?;
    if summary.exit_code() != 0 {
        return Err(format!("Scoring was incomplete: {}", summary.status).into());
    }
    let run_id = summary.run_id.ok_or("Scoring did not publish a run")?;
    let catalog = Catalog::open_read_only(&data)?;
    let matrix = catalog.matrix(MatrixQuery::new(&run_id))?;
    let filtered = catalog.results(ResultsQuery {
        run_id: Some(run_id.clone()),
        kind: "scores".into(),
        min_score: Some(0.8),
        ..ResultsQuery::default()
    })?;
    let thresholds = catalog
        .score_run_profiles(&run_id)?
        .into_values()
        .map(|id| (id, 0.8))
        .collect();
    let grouped = engine
        .submit(JobRequest::group_scores(run_id, thresholds))?
        .wait()?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"matrix":matrix,"filtered_scores":filtered,"group_summary":grouped})
        )?
    );
    engine.shutdown()?;
    if grouped.exit_code() != 0 {
        return Err("Grouping did not complete".into());
    }
    Ok(())
}
