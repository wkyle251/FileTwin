use crate::args::*;
use filetwin_core::{
    Error, Result,
    api::{self, EngineConfig, JobRequest, RuntimeConfig, Source},
    profile,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    engine: FileEngine,
    defaults: Map<String, Value>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileEngine {
    data_dir: Option<PathBuf>,
    model_dir: Option<PathBuf>,
    temp_dir: Option<PathBuf>,
    runtime: RuntimeConfig,
}

pub struct Configuration {
    pub engine: EngineConfig,
    pub defaults: Map<String, Value>,
    pub cwd: PathBuf,
}

pub fn absolute(path: &Path, base: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                result.pop();
            }
            c => result.push(c.as_os_str()),
        }
    }
    result
}

impl Configuration {
    pub fn load(cli: &Cli) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let file_path = cli
            .config
            .clone()
            .or_else(|| std::env::var_os("FILETWIN_CONFIG").map(PathBuf::from));
        let (file, base) = if let Some(path) = file_path {
            let path = absolute(&path, &cwd);
            let mut contents = String::new();
            File::open(&path)?
                .take((api::MAX_REQUEST_BYTES + 1) as u64)
                .read_to_string(&mut contents)?;
            if contents.len() > api::MAX_REQUEST_BYTES {
                return Err(Error::invalid("Configuration exceeds 8 MiB"));
            }
            (
                toml::from_str::<ConfigFile>(&contents)
                    .map_err(|e| Error::invalid(e.to_string()))?,
                path.parent().expect("Absolute config path").to_owned(),
            )
        } else {
            (ConfigFile::default(), cwd.clone())
        };
        let allowed = [
            "recursive",
            "families",
            "profiles",
            "filters",
            "pair_scope",
            "matching",
            "cache",
            "exact_duplicates",
            "limits",
        ];
        if let Some(key) = file
            .defaults
            .keys()
            .find(|k| !allowed.contains(&k.as_str()))
        {
            return Err(Error::invalid(format!("Unknown processing default: {key}")));
        }
        let data_dir = resolve_path(
            cli.data_dir.clone(),
            "FILETWIN_DATA_DIR",
            file.engine.data_dir,
            &cwd,
            &base,
        )
        .map(Ok)
        .unwrap_or_else(|| default_data_dir(&cwd))?;
        let model_dir = resolve_path(
            cli.model_dir.clone(),
            "FILETWIN_MODEL_DIR",
            file.engine.model_dir,
            &cwd,
            &base,
        )
        .unwrap_or_else(|| data_dir.join("models"));
        let temp_dir = resolve_path(
            cli.temp_dir.clone(),
            "FILETWIN_TEMP_DIR",
            file.engine.temp_dir,
            &cwd,
            &base,
        )
        .unwrap_or_else(|| data_dir.join("tmp"));
        let mut runtime = file.engine.runtime;
        for (configured, flag) in [
            (&mut runtime.worker_path, &cli.worker_path),
            (&mut runtime.ffmpeg_path, &cli.ffmpeg_path),
            (&mut runtime.ffprobe_path, &cli.ffprobe_path),
            (&mut runtime.onnxruntime_path, &cli.onnxruntime_path),
            (&mut runtime.pdfium_path, &cli.pdfium_path),
        ] {
            *configured = flag
                .as_ref()
                .map(|p| absolute(p, &cwd))
                .or_else(|| configured.as_ref().map(|p| absolute(p, &base)));
        }
        runtime.worker_path = runtime.worker_path.or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|p| p.with_file_name("filetwin-worker"))
        });
        runtime.ffmpeg_path = runtime.ffmpeg_path.or_else(|| find_executable("ffmpeg"));
        runtime.ffprobe_path = runtime.ffprobe_path.or_else(|| find_executable("ffprobe"));
        let suffix = if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        runtime
            .onnxruntime_path
            .get_or_insert_with(|| model_dir.join(format!("runtime/libonnxruntime.{suffix}")));
        runtime
            .pdfium_path
            .get_or_insert_with(|| model_dir.join(format!("runtime/libpdfium.{suffix}")));
        Ok(Self {
            engine: EngineConfig {
                data_dir,
                model_dir,
                temp_dir,
                runtime,
            },
            defaults: file.defaults,
            cwd,
        })
    }

    pub fn defaults_for(&self, operation: &str) -> Value {
        let mut defaults = self.defaults.clone();
        if ["compare", "group"].contains(&operation) {
            defaults.retain(|key, _| ["matching", "pair_scope", "limits"].contains(&key.as_str()));
            if let Some(Value::Object(limits)) = defaults.get_mut("limits") {
                limits.retain(|key, _| {
                    ["memory_bytes", "result_bytes", "wall_time_seconds"].contains(&key.as_str())
                });
            }
            if operation == "group" {
                defaults.remove("pair_scope");
            }
        } else if operation == "index" {
            defaults.remove("matching");
            defaults.remove("pair_scope");
        }
        Value::Object(defaults)
    }

    pub fn flag_request(
        &self,
        input: Option<&InputArgs>,
        matching: Option<&MatchingArgs>,
        common: Option<&CommonLimits>,
        operation: &str,
        saved_input: Option<&str>,
        request_id: &str,
    ) -> Result<JobRequest> {
        let mut value = self.defaults_for(operation);
        let mut overrides =
            json!({"schema_version":1,"operation":operation,"request_id":request_id});
        if let Some(input) = input {
            overrides["sources"] = serde_json::to_value(
                input
                    .paths
                    .iter()
                    .map(|p| Source::local(absolute(p, &self.cwd)))
                    .collect::<Vec<_>>(),
            )?;
            if input.experimental_text {
                overrides["profiles"] = json!({"text":profile::text_profile().profile_id});
                overrides["families"] = json!(["text"]);
            } else if input.experimental {
                let selected: Map<String, Value> = profile::experimental_profiles()
                    .into_iter()
                    .filter(|p| {
                        input
                            .families
                            .as_ref()
                            .is_none_or(|f| f.contains(&p.family))
                    })
                    .map(|p| (p.family, json!(p.profile_id)))
                    .collect();
                overrides["profiles"] = json!(selected);
                overrides["families"] = json!(
                    overrides["profiles"]
                        .as_object()
                        .expect("Profiles")
                        .keys()
                        .collect::<Vec<_>>()
                );
            } else if !input.profiles.is_empty() {
                overrides["profiles"] = json!(string_map(&input.profiles)?);
                overrides["families"] = json!(
                    overrides["profiles"]
                        .as_object()
                        .expect("Profiles")
                        .keys()
                        .collect::<Vec<_>>()
                );
            }
            if let Some(f) = &input.families {
                overrides["families"] = json!(f);
            }
            if input.recursive || input.no_recursive {
                overrides["recursive"] = json!(input.recursive);
            }
            let mut filters = json!({});
            if !input.include_globs.is_empty() {
                filters["include_globs"] = json!(input.include_globs);
            }
            if !input.exclude_globs.is_empty() {
                filters["exclude_globs"] = json!(input.exclude_globs);
            }
            if let Some(e) = &input.extensions {
                filters["extensions"] = json!(e);
            }
            if input.include_hidden || input.exclude_hidden {
                filters["include_hidden"] = json!(input.include_hidden);
            }
            if let Some(n) = input.min_bytes {
                filters["min_bytes"] = json!(n);
            }
            if let Some(n) = input.max_bytes {
                filters["max_bytes"] = json!(n);
            }
            if !filters.as_object().expect("Filters object").is_empty() {
                overrides["filters"] = filters;
            }
            let mut cache = json!({});
            if let Some(mode) = &input.cache_mode {
                cache["mode"] = json!(mode);
            }
            if let Some(policy) = &input.validation {
                cache["validation"] = json!(policy);
            }
            if input.import_sidecars || input.no_import_sidecars {
                cache["import_sidecars"] = json!(input.import_sidecars);
            }
            if input.export_sidecars || input.no_export_sidecars {
                cache["export_sidecars"] = json!(input.export_sidecars);
            }
            if !cache.as_object().expect("Cache object").is_empty() {
                overrides["cache"] = cache;
            }
            if let Some(policy) = &input.exact_duplicates {
                overrides["exact_duplicates"] = json!(policy);
            }
            let mut limits = limits_value(&input.limits)?;
            if let Some(n) = input.staging_bytes {
                limits["staging_bytes"] = json!(n);
            }
            if let Some(n) = input.io_workers {
                limits["io_workers"] = json!(n);
            }
            if let Some(n) = input.inference_workers {
                limits["inference_workers"] = json!(n);
            }
            if let Some(s) = &input.download_bytes {
                limits["download_bytes"] = nullable(s)?;
            }
            if let Some(s) = &input.bandwidth_bytes_per_second {
                limits["download_bytes_per_second"] = nullable(s)?;
            }
            if !limits.as_object().expect("Limits object").is_empty() {
                overrides["limits"] = limits;
            }
        }
        if let Some(common) = common {
            let limits = limits_value(common)?;
            if !limits.as_object().expect("Limits object").is_empty() {
                overrides["limits"] = limits;
            }
        }
        if let Some(matching) = matching {
            if let Some(scope) = &matching.pair_scope {
                overrides["pair_scope"] = json!(scope);
            }
            let mut matching_value = json!({});
            if operation == "group" {
                matching_value["score_retention"] = json!("matches");
                matching_value["grouping"] = json!("all_pairs");
            }
            if matching.all_scores {
                matching_value["score_retention"] = json!("all");
                if matching.threshold.is_empty() {
                    matching_value["grouping"] = json!("none");
                    matching_value["threshold_overrides"] = json!({});
                }
            }
            if let Some(retrieval) = &matching.retrieval {
                matching_value["retrieval"] = json!(retrieval);
            }
            if !matching.threshold.is_empty() {
                matching_value["grouping"] = json!("all_pairs");
                let selected: std::collections::BTreeMap<String, String> = if let Some(sid) =
                    saved_input
                {
                    let catalog = filetwin_core::Catalog::open_read_only(&self.engine.data_dir)?;
                    if operation == "group" {
                        catalog.score_run_profiles(sid)?
                    } else {
                        catalog.snapshot_profiles(sid)?
                    }
                } else {
                    serde_json::from_value(
                        overrides
                            .get("profiles")
                            .or_else(|| value.get("profiles"))
                            .cloned()
                            .unwrap_or_else(|| json!({"text":profile::text_profile().profile_id})),
                    )?
                };
                let mut thresholds = Map::new();
                for item in &matching.threshold {
                    let (keys, value): (Vec<String>, &str) = match item.rsplit_once('=') {
                        Some((key, score)) => (
                            vec![selected.get(key).cloned().unwrap_or_else(|| key.to_owned())],
                            score,
                        ),
                        None => (selected.values().cloned().collect(), item),
                    };
                    let score = value
                        .parse::<f64>()
                        .map_err(|_| Error::invalid("Invalid threshold number"))?;
                    if !score.is_finite() {
                        return Err(Error::invalid("Threshold must be finite"));
                    }
                    for key in keys {
                        if thresholds.insert(key, json!(score)).is_some() {
                            return Err(Error::invalid("Repeated threshold profile key"));
                        }
                    }
                }
                matching_value["threshold_overrides"] = Value::Object(thresholds);
            }
            if !matching_value
                .as_object()
                .expect("Matching object")
                .is_empty()
            {
                overrides["matching"] = matching_value;
            }
        }
        if let Some(id) = saved_input {
            overrides[if operation == "group" {
                "source_run_id"
            } else {
                "snapshot_id"
            }] = json!(id);
        }
        merge(&mut value, overrides);
        Ok(serde_json::from_value(value)?)
    }

    pub fn json_request(&self, path: &Path, request_id: &str) -> Result<JobRequest> {
        let mut bytes = Vec::new();
        if path == Path::new("-") {
            std::io::stdin()
                .take((api::MAX_REQUEST_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
        } else {
            File::open(absolute(path, &self.cwd))?
                .take((api::MAX_REQUEST_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
        }
        let raw: Value = api::parse_json(&bytes)?;
        api::validate_request_shape(&raw)?;
        let object = raw
            .as_object()
            .ok_or_else(|| Error::invalid("Processing request must be one JSON object"))?;
        let operation = object
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("scan");
        let mut merged = self.defaults_for(operation);
        merge(&mut merged, raw);
        if merged.get("request_id").is_none() {
            merged["request_id"] = json!(request_id);
        }
        Ok(serde_json::from_value(merged)?)
    }
}

fn resolve_path(
    flag: Option<PathBuf>,
    variable: &str,
    config: Option<PathBuf>,
    cwd: &Path,
    config_base: &Path,
) -> Option<PathBuf> {
    flag.or_else(|| std::env::var_os(variable).map(PathBuf::from))
        .map(|p| absolute(&p, cwd))
        .or_else(|| config.map(|p| absolute(&p, config_base)))
}
fn find_executable(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .filter(|p| p.is_absolute())
            .map(|p| p.join(name))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    })
}
fn default_data_dir(cwd: &Path) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(xdg.join("filetwin"));
    }
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| {
            Error::invalid("No absolute HOME directory is available; supply --data-dir")
        })?;
    #[cfg(target_os = "macos")]
    let dir = user_home.join("Library/Application Support/FileTwin");
    #[cfg(target_os = "linux")]
    let dir = user_home.join(".local/share/filetwin");
    Ok(absolute(&dir, cwd))
}
fn string_map(items: &[String]) -> Result<Map<String, Value>> {
    let mut map = Map::new();
    for item in items {
        let (key, value) = item
            .split_once('=')
            .ok_or_else(|| Error::invalid("Expected FAMILY=PROFILE_ID"))?;
        if map.insert(key.into(), json!(value)).is_some() {
            return Err(Error::invalid("Repeated profile family"));
        }
    }
    Ok(map)
}
fn nullable(s: &str) -> Result<Value> {
    if s == "none" {
        Ok(Value::Null)
    } else {
        Ok(json!(s.parse::<u64>().map_err(|_| Error::invalid(
            "Expected an unsigned integer or none"
        ))?))
    }
}
fn limits_value(limits: &CommonLimits) -> Result<Value> {
    let mut value = json!({});
    if let Some(n) = limits.memory_bytes {
        value["memory_bytes"] = json!(n);
    }
    if let Some(n) = limits.result_bytes {
        value["result_bytes"] = json!(n);
    }
    if let Some(s) = &limits.max_runtime_seconds {
        value["wall_time_seconds"] = nullable(s)?;
    }
    Ok(value)
}
fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                if k == "profiles" || k == "threshold_overrides" {
                    a.insert(k, v);
                } else {
                    merge(a.entry(k).or_insert(Value::Null), v);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}
