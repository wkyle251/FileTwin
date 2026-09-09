use crate::{
    Error, ErrorCode, Result,
    api::{Counts, JobRequest, JobSummary},
    profile,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

pub(crate) const DB_VERSION: i64 = 1;
const APPLICATION_ID: i64 = 0x4654574e;

pub(crate) fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC timestamp")
}
pub(crate) fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4())
}

pub(crate) fn open_writer(dir: &Path) -> Result<Connection> {
    let db = Connection::open(dir.join("index.sqlite3"))?;
    db.busy_timeout(Duration::from_secs(2))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if version > DB_VERSION || (version != 0 && app != APPLICATION_ID) {
        return Err(Error::new(
            ErrorCode::DatabaseVersionUnsupported,
            "storage",
            "Unrecognized database or newer schema; refusing to modify it",
        ));
    }
    if version == 0 {
        let tables: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if tables != 0 {
            return Err(Error::new(
                ErrorCode::DatabaseVersionUnsupported,
                "storage",
                "Refusing to initialize a nonempty unrecognized database",
            ));
        }
    }
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA temp_store=FILE; PRAGMA cache_size=-8192;")?;
    if version == 0 {
        let tx = db.unchecked_transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", DB_VERSION)?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('namespace',?1)",
            [id("index")],
        )?;
        tx.commit()?;
    }
    let mode: String = db.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    let sync: i64 = db.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    let fk: i64 = db.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    if mode != "wal" || sync != 2 || fk != 1 {
        return Err(Error::new(
            ErrorCode::StorageError,
            "storage",
            "Required SQLite WAL/FULL/foreign-key settings are unavailable",
        ));
    }
    for p in profile::profiles() {
        db.execute(
            "INSERT OR IGNORE INTO profiles(id,manifest) VALUES(?1,?2)",
            params![p.profile_id, serde_json::to_string(&p)?],
        )?;
    }
    Ok(db)
}

pub(crate) fn open_reader(dir: &Path) -> Result<Connection> {
    if !dir.join("index.sqlite3").is_file() {
        return Err(Error::new(
            ErrorCode::NotFound,
            "storage",
            "No FileTwin index exists in this data directory",
        ));
    }
    let db = Connection::open_with_flags(
        dir.join("index.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA cache_size=-8192;")?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if version != DB_VERSION || app != APPLICATION_ID {
        return Err(Error::new(
            ErrorCode::DatabaseVersionUnsupported,
            "storage",
            "Unsupported FileTwin database version",
        ));
    }
    Ok(db)
}

pub(crate) fn namespace(db: &Connection) -> Result<String> {
    Ok(
        db.query_row("SELECT value FROM meta WHERE key='namespace'", [], |r| {
            r.get(0)
        })?,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct Job {
    pub id: String,
    pub request: JobRequest,
    pub attempt: u64,
    pub status: String,
    pub stage: String,
    pub started_at: String,
    pub elapsed: f64,
    pub used: u64,
    pub counts: Counts,
    pub snapshot: Option<String>,
    pub run: Option<String>,
    pub next_a: i64,
    pub next_b: i64,
    pub source_partial: bool,
}

pub(crate) fn insert_job(db: &Connection, request: JobRequest) -> Result<Job> {
    let job = Job {
        id: id("job"),
        request,
        attempt: 1,
        status: "queued".into(),
        stage: "discovery".into(),
        started_at: now(),
        elapsed: 0.0,
        used: 0,
        counts: Counts::default(),
        snapshot: None,
        run: None,
        next_a: 1,
        next_b: 1,
        source_partial: false,
    };
    db.execute("INSERT INTO jobs(id,request,attempt,status,stage,started_at,updated_at,elapsed,used,counts,next_a,next_b,source_partial) VALUES(?1,?2,1,'queued','discovery',?3,?3,0,0,?4,1,1,0)",params![job.id,serde_json::to_string(&job.request)?,job.started_at,serde_json::to_string(&job.counts)?])?;
    Ok(job)
}

pub(crate) fn load_job(db: &Connection, id: &str) -> Result<Job> {
    let row=db.query_row("SELECT request,attempt,status,stage,started_at,elapsed,used,counts,snapshot_id,run_id,next_a,next_b,source_partial FROM jobs WHERE id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,f64>(5)?,r.get::<_,u64>(6)?,r.get::<_,String>(7)?,r.get::<_,Option<String>>(8)?,r.get::<_,Option<String>>(9)?,r.get::<_,i64>(10)?,r.get::<_,i64>(11)?,r.get::<_,bool>(12)?))).optional()?.ok_or_else(||Error::new(ErrorCode::NotFound,"status","Unknown job ID"))?;
    Ok(Job {
        id: id.into(),
        request: serde_json::from_str(&row.0)?,
        attempt: row.1,
        status: row.2,
        stage: row.3,
        started_at: row.4,
        elapsed: row.5,
        used: row.6,
        counts: serde_json::from_str(&row.7)?,
        snapshot: row.8,
        run: row.9,
        next_a: row.10,
        next_b: row.11,
        source_partial: row.12,
    })
}

pub(crate) fn save_job(db: &Connection, job: &Job) -> Result<()> {
    db.execute("UPDATE jobs SET attempt=?2,status=?3,stage=?4,updated_at=?5,elapsed=?6,used=?7,counts=?8,snapshot_id=?9,run_id=?10,next_a=?11,next_b=?12,source_partial=?13 WHERE id=?1",params![job.id,job.attempt,job.status,job.stage,now(),job.elapsed,job.used,serde_json::to_string(&job.counts)?,job.snapshot,job.run,job.next_a,job.next_b,job.source_partial])?;
    Ok(())
}

pub(crate) fn save_summary(db: &Connection, job: &Job, summary: &JobSummary) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    save_job(&tx, job)?;
    tx.execute(
        "UPDATE jobs SET summary=?2 WHERE id=?1",
        params![job.id, serde_json::to_string(summary)?],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn error(db: &Connection, job: &Job, error: &Error) -> Result<()> {
    db.execute(
        "INSERT INTO job_errors(job_id,attempt,payload) VALUES(?1,?2,?3)",
        params![job.id, job.attempt, serde_json::to_string(error)?],
    )?;
    Ok(())
}

pub(crate) fn record(
    db: &Connection,
    owner: &str,
    revision: u64,
    kind: &str,
    key: &str,
    filter: Option<&str>,
    value: &Value,
) -> Result<()> {
    db.execute(
        "INSERT INTO records(owner,revision,kind,key,filter_id,payload) VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            owner,
            revision,
            kind,
            key,
            filter,
            serde_json::to_string(value)?
        ],
    )?;
    Ok(())
}

pub(crate) fn publish_snapshot(db: &Connection, job: &mut Job) -> Result<String> {
    let sid = id("snapshot");
    let tx = db.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO snapshots(id,job_id,request,source_partial,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![
            sid,
            job.id,
            serde_json::to_string(&job.request)?,
            job.source_partial,
            now()
        ],
    )?;
    {
        let mut stmt=tx.prepare("SELECT file_id,vector_id,digest,payload FROM job_files WHERE job_id=?1 ORDER BY file_id")?;
        let mut rows = stmt.query([&job.id])?;
        let mut ordinal = 0i64;
        while let Some(row) = rows.next()? {
            let fid: String = row.get(0)?;
            let vector: Option<String> = row.get(1)?;
            let digest: Option<String> = row.get(2)?;
            let mut payload: Value = serde_json::from_str(&row.get::<_, String>(3)?)?;
            let mut sources=tx.prepare("SELECT DISTINCT source_id FROM job_locations WHERE job_id=?1 AND file_id=?2 ORDER BY source_id")?;
            let memberships = sources
                .query_map(params![job.id, fid], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let locations:u64=tx.query_row("SELECT count(DISTINCT location_id) FROM job_locations WHERE job_id=?1 AND file_id=?2",params![job.id,fid],|r|r.get(0))?;
            payload["source_ids"] = json!(memberships);
            payload["location_count"] = json!(locations);
            ordinal += 1;
            tx.execute("INSERT INTO snapshot_files(snapshot_id,ordinal,file_id,vector_id,digest,sources,payload) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![sid,ordinal,fid,vector,digest,serde_json::to_string(&memberships)?,serde_json::to_string(&payload)?])?;
            record(&tx, &sid, 1, "files", &fid, None, &payload)?;
        }
    }
    {
        let mut stmt=tx.prepare("SELECT location_id,source_id,file_id,payload FROM job_locations WHERE job_id=?1 ORDER BY location_id,source_id")?;
        let mut rows = stmt.query([&job.id])?;
        while let Some(row) = rows.next()? {
            let lid: String = row.get(0)?;
            let source: String = row.get(1)?;
            let fid: String = row.get(2)?;
            let payload: Value = serde_json::from_str(&row.get::<_, String>(3)?)?;
            record(
                &tx,
                &sid,
                1,
                "locations",
                &format!("{lid}/{source}"),
                Some(&fid),
                &payload,
            )?;
        }
    }
    copy_errors(&tx, job, &sid, 1)?;
    job.snapshot = Some(sid.clone());
    save_job(&tx, job)?;
    tx.commit()?;
    Ok(sid)
}

pub(crate) fn copy_errors(db: &Connection, job: &Job, owner: &str, revision: u64) -> Result<()> {
    let mut stmt =
        db.prepare("SELECT id,payload FROM job_errors WHERE job_id=?1 AND attempt=?2 ORDER BY id")?;
    let mut rows = stmt.query(params![job.id, job.attempt])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let value: Value = serde_json::from_str(&row.get::<_, String>(1)?)?;
        record(
            db,
            owner,
            revision,
            "errors",
            &format!("{id:020}"),
            None,
            &value,
        )?;
    }
    Ok(())
}

pub(crate) fn snapshot_request(db: &Connection, id: &str) -> Result<(JobRequest, bool)> {
    let result = db
        .query_row(
            "SELECT request,source_partial FROM snapshots WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
        )
        .optional()?
        .ok_or_else(|| Error::new(ErrorCode::NotFound, "compare", "Unknown snapshot ID"))?;
    Ok((serde_json::from_str(&result.0)?, result.1))
}

pub(crate) fn create_run(db: &Connection, job: &mut Job) -> Result<()> {
    let run = id("run");
    let sid = job.snapshot.as_ref().ok_or_else(|| {
        Error::new(
            ErrorCode::InternalError,
            "compare",
            "No snapshot to compare",
        )
    })?;
    db.execute(
        "INSERT INTO runs(id,job_id,snapshot_id) VALUES(?1,?2,?3)",
        params![run, job.id, sid],
    )?;
    job.run = Some(run);
    job.stage = "comparing".into();
    job.next_a = 1;
    job.next_b = 1;
    save_job(db, job)
}

const SCHEMA: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE profiles(id TEXT PRIMARY KEY,manifest TEXT NOT NULL);
CREATE TABLE vectors(id TEXT PRIMARY KEY,profile_id TEXT NOT NULL REFERENCES profiles(id),payload BLOB NOT NULL,checksum TEXT NOT NULL);
CREATE TABLE files(file_id TEXT PRIMARY KEY,revision TEXT NOT NULL,vector_id TEXT REFERENCES vectors(id),digest TEXT,payload TEXT NOT NULL);
CREATE TABLE locations(location_id TEXT PRIMARY KEY,path BLOB NOT NULL,file_id TEXT NOT NULL,active INTEGER NOT NULL DEFAULT 1);
CREATE TABLE jobs(id TEXT PRIMARY KEY,request TEXT NOT NULL,attempt INTEGER NOT NULL,status TEXT NOT NULL,stage TEXT NOT NULL,started_at TEXT NOT NULL,updated_at TEXT NOT NULL,elapsed REAL NOT NULL,used INTEGER NOT NULL,counts TEXT NOT NULL,snapshot_id TEXT,run_id TEXT,next_a INTEGER NOT NULL,next_b INTEGER NOT NULL,source_partial INTEGER NOT NULL,summary TEXT);
CREATE TABLE job_files(job_id TEXT NOT NULL REFERENCES jobs(id),file_id TEXT NOT NULL,vector_id TEXT REFERENCES vectors(id),digest TEXT,payload TEXT NOT NULL,PRIMARY KEY(job_id,file_id));
CREATE TABLE job_locations(job_id TEXT NOT NULL REFERENCES jobs(id),location_id TEXT NOT NULL,source_id TEXT NOT NULL,file_id TEXT NOT NULL,path BLOB NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(job_id,location_id,source_id));
CREATE TABLE job_errors(id INTEGER PRIMARY KEY,job_id TEXT NOT NULL REFERENCES jobs(id),attempt INTEGER NOT NULL,payload TEXT NOT NULL);
CREATE TABLE directory_tasks(id INTEGER PRIMARY KEY,job_id TEXT NOT NULL REFERENCES jobs(id),source_id TEXT NOT NULL,path BLOB NOT NULL,root BLOB NOT NULL,depth INTEGER NOT NULL,done INTEGER NOT NULL DEFAULT 0,UNIQUE(job_id,source_id,path));
CREATE TABLE snapshots(id TEXT PRIMARY KEY,job_id TEXT NOT NULL REFERENCES jobs(id),request TEXT NOT NULL,source_partial INTEGER NOT NULL,created_at TEXT NOT NULL);
CREATE TABLE snapshot_files(snapshot_id TEXT NOT NULL REFERENCES snapshots(id),ordinal INTEGER NOT NULL,file_id TEXT NOT NULL,vector_id TEXT REFERENCES vectors(id),digest TEXT,sources TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(snapshot_id,file_id),UNIQUE(snapshot_id,ordinal));
CREATE TABLE runs(id TEXT PRIMARY KEY,job_id TEXT NOT NULL REFERENCES jobs(id),snapshot_id TEXT NOT NULL REFERENCES snapshots(id));
CREATE TABLE work_pairs(run_id TEXT NOT NULL REFERENCES runs(id),a TEXT NOT NULL,b TEXT NOT NULL,kind TEXT NOT NULL,score REAL,payload TEXT NOT NULL,PRIMARY KEY(run_id,kind,a,b));
CREATE TABLE work_groups(run_id TEXT NOT NULL REFERENCES runs(id),kind TEXT NOT NULL,group_id TEXT NOT NULL,ordinal INTEGER NOT NULL,minimum REAL,PRIMARY KEY(run_id,kind,group_id));
CREATE TABLE work_members(run_id TEXT NOT NULL,kind TEXT NOT NULL,group_id TEXT NOT NULL,file_id TEXT NOT NULL,PRIMARY KEY(run_id,kind,file_id));
CREATE INDEX work_members_group ON work_members(run_id,kind,group_id);
CREATE TABLE publications(run_id TEXT NOT NULL REFERENCES runs(id),revision INTEGER NOT NULL,summary TEXT NOT NULL,PRIMARY KEY(run_id,revision));
CREATE TABLE records(owner TEXT NOT NULL,revision INTEGER NOT NULL,kind TEXT NOT NULL,key TEXT NOT NULL,filter_id TEXT,payload TEXT NOT NULL,PRIMARY KEY(owner,revision,kind,key));
CREATE INDEX records_filter ON records(owner,revision,kind,filter_id,key);
CREATE INDEX job_locations_file ON job_locations(job_id,file_id);
"#;
