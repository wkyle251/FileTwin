use filetwin_core::{Error, api::*, profile::Profile};
use schemars::JsonSchema;
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap()
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let directory = PathBuf::from(args.next().unwrap_or_else(|| "schemas".into()));
    let check = args.next().is_some_and(|s| s == "--check");
    let mut vectors = schema::<VectorFile>();
    vectors["properties"]["format"]["const"] = json!(VECTOR_FILE_FORMAT);
    vectors["properties"]["schema_version"]["const"] = json!(SCHEMA_VERSION);
    if !check {
        fs::create_dir_all(&directory)?;
    }
    for (name, schema) in [
        ("vector-file", vectors),
        ("progress", schema::<Progress>()),
        ("error", schema::<Error>()),
        ("profile", schema::<Profile>()),
    ] {
        let file = directory.join(format!("{name}.schema.json"));
        let contents = serde_json::to_string_pretty(&schema)? + "\n";
        if check {
            if fs::read_to_string(&file)? != contents {
                return Err(format!("Regenerate {}", file.display()).into());
            }
        } else {
            fs::write(file, contents)?;
        }
    }
    Ok(())
}
