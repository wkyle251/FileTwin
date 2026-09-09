use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum Format {
    Human,
    Json,
    Jsonl,
}

#[derive(Parser, Debug)]
#[command(
    name = "filetwin",
    version,
    about = "Local text, document, image, audio and video similarity (experimental)"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub model_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub temp_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    pub worker_path: Option<PathBuf>,
    #[arg(long, global = true)]
    pub ffmpeg_path: Option<PathBuf>,
    #[arg(long, global = true)]
    pub ffprobe_path: Option<PathBuf>,
    #[arg(long, global = true)]
    pub onnxruntime_path: Option<PathBuf>,
    #[arg(long, global = true)]
    pub pdfium_path: Option<PathBuf>,
    #[arg(long, global = true, value_enum)]
    pub format: Option<Format>,
    #[arg(long, global = true)]
    pub request_id: Option<String>,
    #[arg(long, global = true)]
    pub non_interactive: bool,
    #[arg(long,global=true,default_value="warn",value_parser=["error","warn","info","debug"])]
    pub log_level: String,
    #[arg(long,global=true,default_value_t=1000,value_parser=clap::value_parser!(u64).range(1..))]
    pub progress_interval_ms: u64,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Discover and index files, then compare their saved vectors.
    Scan(ScanArgs),
    /// Create an immutable vector snapshot without comparing files.
    Index(InputArgs),
    /// Compare a saved snapshot without reading the originals.
    Compare(CompareArgs),
    /// Apply a cutoff and group saved scores without comparing vectors again.
    Group(GroupArgs),
    /// Read a bounded similarity matrix from a run created with --all-scores.
    Matrix(MatrixArgs),
    /// Read a versioned JSON processing request (use - for stdin).
    Run {
        #[arg(long)]
        request: PathBuf,
    },
    /// Resume a cancelled or interrupted job with its frozen settings.
    Resume {
        #[arg(long)]
        job: String,
    },
    /// Inspect durable job state.
    Status {
        #[arg(long)]
        job: String,
    },
    /// Read a bounded page of published records.
    Results(ResultsArgs),
    /// Export published results into a new directory.
    Export(ExportArgs),
    /// Inspect available encoding profiles.
    Profiles {
        #[command(subcommand)]
        command: ProfilesCommand,
    },
    /// Report capabilities without initializing a database or downloading models.
    Doctor,
}
#[derive(Debug, Subcommand)]
pub enum ProfilesCommand {
    List,
}

#[derive(Debug, Args)]
pub struct ScanArgs {
    #[command(flatten)]
    pub input: InputArgs,
    #[command(flatten)]
    pub matching: MatchingArgs,
}

#[derive(Debug, Args)]
pub struct InputArgs {
    #[arg(required=true,num_args=1..)]
    pub paths: Vec<PathBuf>,
    /// Explicitly select the built-in experimental text profile.
    #[arg(long, conflicts_with = "profiles")]
    pub experimental_text: bool,
    /// Select experimental encoders for all families, or those in --families.
    #[arg(long, conflicts_with_all = ["experimental_text", "profiles"])]
    pub experimental: bool,
    #[arg(long = "profile", value_name = "FAMILY=PROFILE_ID")]
    pub profiles: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    pub families: Option<Vec<String>>,
    #[arg(long, conflicts_with = "no_recursive")]
    pub recursive: bool,
    #[arg(long, conflicts_with = "recursive")]
    pub no_recursive: bool,
    #[arg(long = "include")]
    pub include_globs: Vec<String>,
    #[arg(long = "exclude")]
    pub exclude_globs: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    pub extensions: Option<Vec<String>>,
    #[arg(long, conflicts_with = "exclude_hidden")]
    pub include_hidden: bool,
    #[arg(long, conflicts_with = "include_hidden")]
    pub exclude_hidden: bool,
    #[arg(long)]
    pub min_bytes: Option<u64>,
    #[arg(long)]
    pub max_bytes: Option<u64>,
    #[arg(long,value_parser=["reuse","refresh"])]
    pub cache_mode: Option<String>,
    #[arg(long,value_parser=["fast","strict"])]
    pub validation: Option<String>,
    #[arg(long,value_parser=["reuse_known","compute","off"])]
    pub exact_duplicates: Option<String>,
    #[arg(long, conflicts_with = "no_import_sidecars")]
    pub import_sidecars: bool,
    #[arg(long, conflicts_with = "import_sidecars")]
    pub no_import_sidecars: bool,
    #[arg(long, conflicts_with = "no_export_sidecars")]
    pub export_sidecars: bool,
    #[arg(long, conflicts_with = "export_sidecars")]
    pub no_export_sidecars: bool,
    #[command(flatten)]
    pub limits: CommonLimits,
    #[arg(long)]
    pub staging_bytes: Option<u64>,
    #[arg(long)]
    pub io_workers: Option<u32>,
    #[arg(long)]
    pub inference_workers: Option<u32>,
    #[arg(long, value_name = "N|none")]
    pub download_bytes: Option<String>,
    #[arg(long, value_name = "N|none")]
    pub bandwidth_bytes_per_second: Option<String>,
}

#[derive(Debug, Args)]
pub struct CommonLimits {
    #[arg(long)]
    pub memory_bytes: Option<u64>,
    #[arg(long)]
    pub result_bytes: Option<u64>,
    #[arg(long, value_name = "N|none")]
    pub max_runtime_seconds: Option<String>,
}

#[derive(Debug, Args)]
pub struct MatchingArgs {
    /// Retain every compatible score. Without --threshold, skip grouping.
    #[arg(long)]
    pub all_scores: bool,
    /// Explicit cosine cutoff. A bare SCORE applies to every selected profile.
    #[arg(
        long,
        value_name = "[FAMILY|PROFILE_ID=]SCORE",
        allow_hyphen_values = true
    )]
    pub threshold: Vec<String>,
    #[arg(long,value_parser=["all_selected","within_each_source"])]
    pub pair_scope: Option<String>,
    #[arg(long,value_parser=["exact"])]
    pub retrieval: Option<String>,
}

#[derive(Debug, Args)]
pub struct CompareArgs {
    #[arg(long)]
    pub snapshot: String,
    #[command(flatten)]
    pub matching: MatchingArgs,
    #[command(flatten)]
    pub limits: CommonLimits,
}

#[derive(Debug, Args)]
pub struct GroupArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub revision: Option<u64>,
    #[arg(
        long,
        value_name = "[FAMILY|PROFILE_ID=]SCORE",
        allow_hyphen_values = true
    )]
    pub threshold: Vec<String>,
    #[command(flatten)]
    pub limits: CommonLimits,
}

#[derive(Debug, Args)]
pub struct MatrixArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long)]
    pub revision: Option<u64>,
    #[arg(long, default_value_t = 0)]
    pub row_offset: u64,
    #[arg(long, default_value_t = 0)]
    pub column_offset: u64,
    #[arg(long, default_value_t = 128)]
    pub row_limit: u32,
    #[arg(long, default_value_t = 128)]
    pub column_limit: u32,
}

#[derive(Debug, Args)]
pub struct TargetArgs {
    #[arg(
        long,
        required_unless_present = "snapshot",
        conflicts_with = "snapshot"
    )]
    pub run: Option<String>,
    #[arg(long, required_unless_present = "run", conflicts_with = "run")]
    pub snapshot: Option<String>,
    #[arg(long)]
    pub revision: Option<u64>,
}
#[derive(Debug, Args)]
pub struct ResultsArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long,value_parser=["summary","groups","members","pairs","scores","files","locations","errors"])]
    pub kind: String,
    #[arg(long)]
    pub group: Option<String>,
    #[arg(long)]
    pub file: Option<String>,
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long)]
    pub page_size: Option<u32>,
    /// Filter saved scores without recalculation; valid only with --kind scores.
    #[arg(long, allow_hyphen_values = true)]
    pub min_score: Option<f64>,
}
#[derive(Debug, Args)]
pub struct ExportArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[arg(long,value_parser=["json","jsonl","csv"])]
    pub report_format: String,
    #[arg(long)]
    pub report_dir: PathBuf,
}
