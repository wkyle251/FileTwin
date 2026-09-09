//! The embedding application supplies paths and owns presentation/lifetime.
use filetwin_core::{Catalog, Engine, HostServices, api::*};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let data = PathBuf::from(
        args.next()
            .ok_or("Usage: host ABSOLUTE_DATA_DIR ABSOLUTE_SOURCE")?,
    );
    let source = PathBuf::from(args.next().ok_or("Missing absolute source path")?);
    let engine = Engine::open(EngineConfig::new(&data), HostServices::default())?;
    let job = engine.submit(JobRequest::text_scan([source], 0.7))?;
    // An event-driven host can forward these counts to its UI and call job.cancel().
    // Keep presentation in the host: the core never writes to standard streams.
    for event in job.events() {
        if let JobEvent::Progress { data, .. } = event {
            eprintln!("progress: {data}");
        }
    }
    let summary = job.wait()?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if let Some(run) = &summary.run_id {
        let catalog = Catalog::open_read_only(&data)?;
        let page = catalog.results(ResultsQuery {
            run_id: Some(run.clone()),
            kind: "groups".into(),
            page_size: Some(20),
            ..ResultsQuery::default()
        })?;
        println!("{}", serde_json::to_string_pretty(&page)?);
    }
    engine.shutdown()?;
    if summary.exit_code() != 0 {
        return Err(format!(
            "Job ended with {} coverage",
            summary.completeness.source_coverage
        )
        .into());
    }
    Ok(())
}
