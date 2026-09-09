//! Generate the transport contracts from the Rust types, with JSON-level
//! presence constraints that Option<T> cannot express on its own.
use filetwin_core::{Error, api::*, profile::Profile};
use schemars::JsonSchema;
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("Schema is serializable")
}
fn absent(keys: &[&str]) -> Value {
    json!({"not":{"anyOf":keys.iter().map(|k|json!({"required":[k]})).collect::<Vec<_>>()}})
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "schemas".into()));
    let check = args.next().is_some_and(|s| s == "--check");
    let mut request = schema::<JobRequest>();
    request["properties"]["schema_version"]["const"] = json!(1);
    request["allOf"] = json!([
        {"if":{"required":["operation"],"properties":{"operation":{"const":"compare"}}},
         "then":{"required":["snapshot_id"],"properties":{"snapshot_id":{"type":"string","minLength":1},"limits":{"properties":{"staging_bytes":false,"io_workers":false,"inference_workers":false,"download_bytes":false,"download_bytes_per_second":false}}},"allOf":[absent(&["sources","families","profiles","filters","cache","recursive","exact_duplicates"])]},
         "else":{"required":["sources"],"properties":{"sources":{"type":"array","minItems":1}},"allOf":[absent(&["snapshot_id"])]}},
        {"if":{"required":["operation"],"properties":{"operation":{"const":"index"}}},"then":absent(&["matching","pair_scope"])}
    ]);
    request["$defs"]["Source"]["oneOf"] = json!([
        {"required":["root"],"properties":{"root":{"type":"string","minLength":1}},"not":{"required":["local_path"]}},
        {"required":["local_path"],"properties":{"local_path":{"type":"object"}},"not":{"required":["root"]}}
    ]);
    let mut envelope = schema::<Envelope>();
    envelope["additionalProperties"] = json!(false);
    envelope["properties"]["schema_version"]["const"] = json!(1);
    envelope["properties"]["sequence"]["minimum"] = json!(1);
    envelope["properties"]["data"] = json!({"type":"object"});
    envelope["properties"]["type"] = json!({"enum":["accepted","progress","error","summary","status","page","export","profiles","capabilities"]});
    let entries = [
        ("job-request", request),
        ("envelope", envelope),
        ("job-summary", schema::<JobSummary>()),
        ("results-query", schema::<ResultsQuery>()),
        ("status-query", schema::<StatusQuery>()),
        ("export-request", schema::<ExportRequest>()),
        ("result-page", schema::<ResultPage>()),
        ("error", schema::<Error>()),
        ("profile", schema::<Profile>()),
    ];
    if !check {
        fs::create_dir_all(&dir)?;
    }
    for (name, value) in entries {
        let path = dir.join(format!("{name}.schema.json"));
        let bytes = serde_json::to_string_pretty(&value)? + "\n";
        if check {
            if fs::read_to_string(&path)? != bytes {
                return Err(format!("Schema needs regeneration: {}", path.display()).into());
            }
        } else {
            fs::write(path, bytes)?;
        }
    }
    Ok(())
}
