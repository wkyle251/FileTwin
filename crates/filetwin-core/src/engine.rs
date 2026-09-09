use crate::{
    Error, ErrorCode, Result,
    api::*,
    local, matching, profile, scan,
    store::{self, Job},
};
use crossbeam_channel::{Receiver, Sender, bounded};
use fs2::FileExt;
use rusqlite::Connection;
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type DiagnosticSink = Arc<dyn Fn(&Error) + Send + Sync>;
type Completion = Arc<(Mutex<Option<Result<JobSummary>>>, Condvar)>;

#[derive(Default, Clone)]
pub struct HostServices {
    pub diagnostics: Option<DiagnosticSink>,
}

#[derive(Default)]
pub(crate) struct Cancellation {
    stopped: AtomicBool,
    reason: Mutex<Option<Error>>,
}
impl Cancellation {
    pub fn stop(&self, error: Option<Error>) {
        if let Some(error) = error {
            let mut reason = self.reason.lock().expect("Cancellation lock");
            if reason.is_none() {
                *reason = Some(error);
            }
        }
        self.stopped.store(true, Ordering::Release);
    }
    pub fn check(&self) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            Err(self
                .reason
                .lock()
                .expect("Cancellation lock")
                .clone()
                .unwrap_or_else(|| {
                    Error::new(ErrorCode::Cancelled, "processing", "Cancellation requested")
                }))
        } else {
            Ok(())
        }
    }
}

pub struct JobHandle {
    id: String,
    events: Receiver<JobEvent>,
    completion: Completion,
    cancellation: Arc<Cancellation>,
}
impl JobHandle {
    pub fn id(&self) -> &str {
        &self.id
    }
    /// A bounded receiver. Cloned receivers compete for events; they do not broadcast.
    /// `wait` and the catalog remain authoritative if progress events are coalesced.
    pub fn events(&self) -> Receiver<JobEvent> {
        self.events.clone()
    }
    pub fn cancel(&self) {
        self.cancellation.stop(None);
    }
    pub fn cancel_for_output_failure(&self) {
        self.cancellation.stop(Some(Error::new(
            ErrorCode::OutputClosed,
            "output",
            "Caller could not deliver processing output",
        )));
    }
    pub fn is_finished(&self) -> bool {
        self.completion.0.lock().expect("Completion lock").is_some()
    }
    pub fn wait(&self) -> Result<JobSummary> {
        let (lock, ready) = &*self.completion;
        let mut value = lock.lock().expect("Completion lock");
        while value.is_none() {
            value = ready.wait(value).expect("Completion lock");
        }
        value.as_ref().expect("Completed result").clone()
    }
}

struct Worker {
    thread: JoinHandle<()>,
    cancellation: Arc<Cancellation>,
    completion: Completion,
}

/// One processing owner per data directory; one active job per engine. Methods
/// are thread-safe. The engine owns its worker, even after a handle is dropped.
pub struct Engine {
    config: EngineConfig,
    host: HostServices,
    owner: Arc<File>,
    worker: Mutex<Option<Worker>>,
    lifecycle: AtomicU8,
}
impl Engine {
    pub fn open(mut config: EngineConfig, host: HostServices) -> Result<Self> {
        for p in [&config.data_dir, &config.model_dir, &config.temp_dir] {
            if !p.is_absolute() {
                return Err(Error::invalid("Engine directories must be absolute"));
            }
        }
        std::fs::create_dir_all(&config.data_dir)?;
        config.data_dir = std::fs::canonicalize(&config.data_dir)?;
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(config.data_dir.join("owner.lock"))?;
        FileExt::try_lock_exclusive(&owner).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                Error::new(
                    ErrorCode::CacheBusy,
                    "ownership",
                    "Another engine owns this data directory",
                )
            } else {
                e.into()
            }
        })?;
        let db = store::open_writer(&config.data_dir)?;
        db.execute(
            "UPDATE jobs SET status='interrupted' WHERE status IN ('queued','running')",
            [],
        )?;
        Ok(Self {
            config,
            host,
            owner: Arc::new(owner),
            worker: Mutex::new(None),
            lifecycle: AtomicU8::new(0),
        })
    }

    pub fn submit(&self, request: JobRequest) -> Result<JobHandle> {
        let mut slot = self.worker.lock().expect("Worker lock");
        self.prepare_slot(&mut slot)?;
        let mut request = request.resolve()?;
        if let Some(sources) = &mut request.sources {
            for source in sources {
                let path = local::resolve_root(&source.path()?)?;
                let file = local::open_secure(&path)?;
                let meta = file.metadata()?;
                if !meta.is_file() && !meta.is_dir() {
                    return Err(Error::invalid(
                        "Explicit source roots must be regular files or directories",
                    ));
                }
                let resolved = Source::local(path);
                source.root = resolved.root;
                source.local_path = resolved.local_path;
            }
        }
        let db = store::open_writer(&self.config.data_dir)?;
        if let Some(sid) = &request.snapshot_id {
            let (snapshot_request, _) = store::snapshot_request(&db, sid)?;
            request.validate_thresholds(
                snapshot_request
                    .profiles
                    .as_ref()
                    .ok_or_else(|| Error::invalid("Snapshot has no profiles"))?,
            )?;
        }
        let job = store::insert_job(&db, request)?;
        self.start(&mut slot, job)
    }

    pub fn resume(&self, id: &str) -> Result<JobHandle> {
        let mut slot = self.worker.lock().expect("Worker lock");
        self.prepare_slot(&mut slot)?;
        let db = store::open_writer(&self.config.data_dir)?;
        let mut job = store::load_job(&db, id)?;
        job.request.clone().resolve()?;
        if let Some(sid) = &job.request.snapshot_id {
            let (request, _) = store::snapshot_request(&db, sid)?;
            job.request.validate_thresholds(
                request
                    .profiles
                    .as_ref()
                    .ok_or_else(|| Error::invalid("Snapshot has no profiles"))?,
            )?;
        }
        if !["cancelled", "interrupted"].contains(&job.status.as_str()) {
            return Err(Error::invalid(
                "Only cancelled/interrupted jobs can resume; exhausted fixed budgets require a new request",
            ));
        }
        if job
            .request
            .limits
            .as_ref()
            .and_then(|l| l.wall_time_seconds)
            .is_some_and(|s| job.elapsed >= s as f64)
        {
            return Err(Error::invalid(
                "This job exhausted its accumulated wall-time allowance",
            ));
        }
        job.attempt += 1;
        job.status = "queued".into();
        store::save_job(&db, &job)?;
        self.start(&mut slot, job)
    }

    fn prepare_slot(&self, slot: &mut Option<Worker>) -> Result<()> {
        if self.lifecycle.load(Ordering::Acquire) != 0 {
            return Err(Error::new(
                ErrorCode::EngineBusy,
                "ownership",
                "This engine is shutting down or already shut down",
            ));
        }
        if slot
            .as_ref()
            .is_some_and(|w| w.completion.0.lock().expect("Completion lock").is_none())
        {
            return Err(Error::new(
                ErrorCode::EngineBusy,
                "ownership",
                "The engine already has an active job",
            ));
        }
        if let Some(worker) = slot.take() {
            worker.thread.join().map_err(|_| {
                Error::new(
                    ErrorCode::InternalError,
                    "worker",
                    "Previous worker panicked",
                )
            })?;
        }
        Ok(())
    }

    fn start(&self, slot: &mut Option<Worker>, job: Job) -> Result<JobHandle> {
        let (send, receive) = bounded(64);
        let completion: Completion = Arc::new((Mutex::new(None), Condvar::new()));
        let cancel = Arc::new(Cancellation::default());
        let handle = JobHandle {
            id: job.id.clone(),
            events: receive,
            completion: completion.clone(),
            cancellation: cancel.clone(),
        };
        let cfg = self.config.clone();
        let host = self.host.clone();
        let work_cancel = cancel.clone();
        let owner = self.owner.clone();
        let worker_completion = completion.clone();
        send.try_send(JobEvent::Accepted{job_id:job.id.clone(),run_id:job.run.clone(),data:json!({"attempt_id":job.attempt,"status":"queued","resolved_request":job.request,"provenance":{"app_version":env!("CARGO_PKG_VERSION"),"scorer":"filetwin_cosine_f64_v1","encoder_workers":1,"max_native_workers":1,"native_worker_policy":"isolated_per_file","platform":std::env::consts::OS,"architecture":std::env::consts::ARCH}})}).expect("Empty event queue");
        let job_id = job.id.clone();
        let fallback_job = job.clone();
        let fallback_config = cfg.clone();
        let thread = thread::Builder::new()
            .name("filetwin-engine".into())
            .spawn(move || {
                let _owner_guard = owner;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(cfg, host, job, &work_cancel, &send)
                }))
                .unwrap_or_else(|_| {
                    Err(Error::new(
                        ErrorCode::InternalError,
                        "worker",
                        "The processing worker panicked; committed work remains recoverable",
                    ))
                });
                let summary = result
                    .unwrap_or_else(|error| failed_terminal(&fallback_config, fallback_job, error));
                let _ = send.try_send(JobEvent::Summary(Box::new(summary.clone())));
                *completion.0.lock().expect("Completion lock") = Some(Ok(summary));
                completion.1.notify_all();
            })
            .map_err(|e| {
                let mut error: Error = e.into();
                error.details = Box::new(json!({"job_id":job_id}));
                error
            })?;
        *slot = Some(Worker {
            thread,
            cancellation: cancel,
            completion: worker_completion,
        });
        Ok(handle)
    }

    pub fn shutdown(&self) -> Result<()> {
        match self
            .lifecycle
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => (),
            Err(2) => return Ok(()),
            Err(_) => {
                return Err(Error::new(
                    ErrorCode::EngineBusy,
                    "shutdown",
                    "Shutdown is already in progress",
                ));
            }
        }
        let worker = self.worker.lock().expect("Worker lock").take();
        let mut result = Ok(());
        if let Some(worker) = worker {
            worker.cancellation.stop(None);
            if worker.thread.thread().id() == thread::current().id() {
                *self.worker.lock().expect("Worker lock") = Some(worker);
                self.lifecycle.store(0, Ordering::Release);
                return Err(Error::new(
                    ErrorCode::EngineBusy,
                    "shutdown",
                    "Cancellation requested; a worker callback cannot synchronously join itself",
                ));
            }
            result = worker.thread.join().map_err(|_| {
                Error::new(ErrorCode::InternalError, "worker", "Worker shutdown failed")
            });
        }
        let unlock = FileExt::unlock(self.owner.as_ref()).map_err(Error::from);
        self.lifecycle.store(2, Ordering::Release);
        result.and(unlock)
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Even a publication failure produces a terminal outcome for a live caller.
/// Null result IDs mean no newly published result is promised. If persistence
/// itself fails, keep that fact explicit and leave crash recovery to the catalog.
fn failed_terminal(config: &EngineConfig, mut job: Job, mut error: Error) -> JobSummary {
    let db = store::open_writer(&config.data_dir);
    if let Ok(db) = &db
        && let Ok(saved) = store::load_job(db, &job.id)
    {
        job = saved;
    }
    job.status = "failed".into();
    let mut summary = JobSummary {
        job_id: job.id.clone(),
        run_id: None,
        attempt_id: job.attempt,
        status: "failed".into(),
        resumable: false,
        snapshot_id: None,
        result_revision: None,
        scope_summary: json!({"operation":job.request.operation,"families":job.request.families,"source_count":job.request.sources.as_ref().map(Vec::len)}),
        started_at: job.started_at.clone(),
        finished_at: store::now(),
        elapsed_seconds: job.elapsed,
        result_bytes_used: job.used,
        counts: job.counts.clone(),
        completeness: Completeness {
            source_coverage: "partial".into(),
            comparison_coverage: if job.run.is_some() {
                "partial"
            } else {
                "not_run"
            }
            .into(),
            retrieval_mode: job.run.as_ref().map(|_| "exact".into()),
            limits_reached: vec![],
            freshness: "unpublished".into(),
        },
        error: Some(error.clone()),
    };
    let persisted = db.and_then(|db| store::save_summary(&db, &job, &summary));
    if let Err(persistence) = persisted {
        error.details = Box::new(
            json!({"terminal_state_persisted":false,"persistence_error":persistence.message}),
        );
        summary.error = Some(error);
    }
    summary
}

pub(crate) struct Work<'a> {
    pub db: Connection,
    pub config: EngineConfig,
    pub job: Job,
    pub cancel: &'a Cancellation,
    events: &'a Sender<JobEvent>,
    host: HostServices,
    start: Instant,
    previous_elapsed: f64,
    last_progress: Instant,
    last_heartbeat: std::cell::Cell<Instant>,
}
impl Work<'_> {
    pub fn check(&self) -> Result<()> {
        self.cancel.check()?;
        if self.last_heartbeat.get().elapsed() >= Duration::from_millis(250) {
            self.db.execute(
                "UPDATE jobs SET elapsed=?2,updated_at=?3 WHERE id=?1",
                rusqlite::params![self.job.id, self.elapsed(), store::now()],
            )?;
            let _=self.events.try_send(JobEvent::Progress{job_id:self.job.id.clone(),run_id:self.job.run.clone(),data:json!({"attempt_id":self.job.attempt,"stage":self.job.stage,"counts":self.job.counts,"elapsed_seconds":self.elapsed(),"bytes_transferred":0,"eta_seconds":null})});
            self.last_heartbeat.set(Instant::now());
        }
        if self
            .job
            .request
            .limits
            .as_ref()
            .and_then(|l| l.wall_time_seconds)
            .is_some_and(|limit| self.elapsed() >= limit as f64)
        {
            return Err(Error::new(
                ErrorCode::BudgetExhausted,
                "processing",
                "Accumulated wall-time allowance exhausted; start a new job to change limits",
            ));
        }
        Ok(())
    }
    pub fn elapsed(&self) -> f64 {
        self.previous_elapsed + self.start.elapsed().as_secs_f64()
    }
    pub fn charge(&mut self, bytes: u64) -> Result<()> {
        let limit = self
            .job
            .request
            .limits
            .as_ref()
            .and_then(|l| l.result_bytes)
            .expect("Resolved result budget");
        if bytes > limit.saturating_sub(self.job.used) {
            return Err(Error::new(
                ErrorCode::BudgetExhausted,
                "results",
                "Encoded result-record budget exhausted; start a new job to change limits",
            ));
        }
        self.job.used += bytes;
        Ok(())
    }
    pub fn checkpoint(&mut self) -> Result<()> {
        self.job.elapsed = self.elapsed();
        store::save_job(&self.db, &self.job)?;
        if self.last_progress.elapsed() >= Duration::from_millis(250) {
            let _=self.events.try_send(JobEvent::Progress{job_id:self.job.id.clone(),run_id:self.job.run.clone(),data:json!({"attempt_id":self.job.attempt,"stage":self.job.stage,"counts":self.job.counts,"elapsed_seconds":self.job.elapsed,"bytes_transferred":0,"eta_seconds":null})});
            self.last_progress = Instant::now();
        }
        Ok(())
    }
    pub fn file_error(&mut self, error: Error) -> Result<()> {
        store::error(&self.db, &self.job, &error)?;
        if let Some(sink) = &self.host.diagnostics {
            sink(&error);
        }
        let _ = self.events.try_send(JobEvent::Error {
            job_id: self.job.id.clone(),
            run_id: self.job.run.clone(),
            error,
        });
        Ok(())
    }
}

fn run(
    config: EngineConfig,
    host: HostServices,
    job: Job,
    cancel: &Cancellation,
    events: &Sender<JobEvent>,
) -> Result<JobSummary> {
    let previous_elapsed = job.elapsed;
    let mut work = Work {
        db: store::open_writer(&config.data_dir)?,
        config,
        job,
        cancel,
        events,
        host,
        start: Instant::now(),
        previous_elapsed,
        last_progress: Instant::now(),
        last_heartbeat: std::cell::Cell::new(Instant::now()),
    };
    work.job.status = "running".into();
    work.checkpoint()?;
    let outcome = (|| -> Result<()> {
        if work.job.request.operation == Operation::Compare && work.job.snapshot.is_none() {
            work.job.snapshot = work.job.request.snapshot_id.clone();
            let (_, partial) = store::snapshot_request(
                &work.db,
                work.job.snapshot.as_ref().expect("Compare snapshot"),
            )?;
            work.job.source_partial = partial;
            store::create_run(&work.db, &mut work.job)?;
        }
        work.check()?;
        if work.job.stage == "discovery" {
            scan::discover(&mut work)?;
            store::publish_snapshot(&work.db, &mut work.job)?;
            if work.job.request.operation != Operation::Index {
                store::create_run(&work.db, &mut work.job)?;
            }
        }
        if work.job.run.is_some() {
            matching::compare(&mut work)?;
            matching::group(&mut work)?;
        }
        Ok(())
    })();
    let error = outcome.err();
    work.job.status = match error.as_ref().map(|e| e.code) {
        None => "completed",
        Some(ErrorCode::Cancelled | ErrorCode::OutputClosed) => "cancelled",
        Some(ErrorCode::BudgetExhausted) => "checkpointed",
        Some(_) => "failed",
    }
    .into();
    if let Some(e) = &error {
        store::error(&work.db, &work.job, e)?;
    }
    if work.job.stage == "discovery"
        && work.job.status != "completed"
        && work.job.request.operation != Operation::Compare
    {
        work.job.source_partial = true;
        store::publish_snapshot(&work.db, &mut work.job)?;
    }
    work.job.elapsed = work.elapsed();
    let revision = if let Some(run) = &work.job.run {
        work.db.query_row(
            "SELECT coalesce(max(revision),0)+1 FROM publications WHERE run_id=?1",
            [run],
            |r| r.get::<_, u64>(0),
        )?
    } else {
        1
    };
    let selected = if let Some(sid) = &work.job.request.snapshot_id {
        store::snapshot_request(&work.db, sid)?
            .0
            .profiles
            .unwrap_or_default()
    } else {
        work.job.request.profiles.clone().unwrap_or_default()
    };
    let selected_profiles: Vec<_> = selected.values().map(|id| profile::find(id).map(|p|
        json!({"profile_id":p.profile_id,"family":p.family,"status":p.status,"calibration_status":p.calibration_status}))).collect::<Result<_>>()?;
    let summary = JobSummary {
        job_id: work.job.id.clone(),
        run_id: work.job.run.clone(),
        attempt_id: work.job.attempt,
        status: work.job.status.clone(),
        resumable: work.job.status == "cancelled"
            && !work
                .job
                .request
                .limits
                .as_ref()
                .and_then(|l| l.wall_time_seconds)
                .is_some_and(|limit| work.job.elapsed >= limit as f64),
        snapshot_id: work.job.snapshot.clone(),
        result_revision: work.job.snapshot.as_ref().map(|_| revision),
        scope_summary: json!({"families":selected.keys().collect::<Vec<_>>(),"profiles":selected_profiles,"pair_scope":work.job.request.pair_scope}),
        started_at: work.job.started_at.clone(),
        finished_at: store::now(),
        elapsed_seconds: work.job.elapsed,
        result_bytes_used: work.job.used,
        counts: work.job.counts.clone(),
        completeness: Completeness {
            source_coverage: if work.job.source_partial {
                "partial"
            } else {
                "complete_for_requested_profiles"
            }
            .into(),
            comparison_coverage: if work.job.run.is_none() {
                "not_run"
            } else if error.is_none() {
                "exhaustive_for_snapshot"
            } else {
                "partial"
            }
            .into(),
            retrieval_mode: work.job.run.as_ref().map(|_| "exact".into()),
            limits_reached: if error
                .as_ref()
                .is_some_and(|e| e.code == ErrorCode::BudgetExhausted)
            {
                vec![error.as_ref().expect("Budget error").stage.clone()]
            } else {
                vec![]
            },
            freshness: if work.job.request.operation == Operation::Compare {
                "snapshot_observation"
            } else {
                "fast_metadata_heuristic"
            }
            .into(),
        },
        error,
    };
    if work.job.run.is_some() {
        matching::publish(&work, &summary)?;
    }
    store::save_summary(&work.db, &work.job, &summary)?;
    Ok(summary)
}
