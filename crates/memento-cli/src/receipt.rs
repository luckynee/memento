use std::env;
use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value};

pub(super) fn receipt_path() -> Result<PathBuf, String> {
    let root = match env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env::var_os("HOME").ok_or("HOME is not set")?).join(".config"),
    };
    Ok(root.join("memento/install.json"))
}

pub(super) fn read() -> Result<Map<String, Value>, String> {
    read_path(&receipt_path()?)
}

fn read_path(path: &std::path::Path) -> Result<Map<String, Value>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("cannot parse {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

pub(super) fn merge_setup(setup: Value) -> Result<(), String> {
    write(merge_value(read()?, setup))
}

pub(super) fn merge_fields(fields: Value) -> Result<(), String> {
    let fields = fields
        .as_object()
        .ok_or("receipt fields must be an object")?;
    write(merge_fields_value(read()?, fields.clone()))
}

fn merge_fields_value(
    mut receipt: Map<String, Value>,
    fields: Map<String, Value>,
) -> Map<String, Value> {
    receipt.extend(fields);
    receipt
}
fn merge_value(mut value: Map<String, Value>, setup: Value) -> Map<String, Value> {
    value.insert("setup".into(), setup);
    value
}

fn write(value: Map<String, Value>) -> Result<(), String> {
    let path = receipt_path()?;
    write_path(&path, value)
}

fn write_path(path: &std::path::Path, value: Map<String, Value>) -> Result<(), String> {
    let parent = path.parent().ok_or("invalid receipt path")?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(&Value::Object(value))
        .map_err(|error| format!("cannot serialize install receipt: {error}"))?;
    fs::write(path, bytes).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_existing_receipt_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("memento/install.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"method":"script","unknown":{"keep":true}}"#).unwrap();
        let merged = merge_value(
            read_path(&path).unwrap(),
            serde_json::json!({"skills": true}),
        );
        write_path(&path, merged).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["method"], "script");
        assert_eq!(value["unknown"]["keep"], true);
        assert_eq!(value["setup"]["skills"], true);
    }

    #[test]
    fn top_level_merge_preserves_unrelated_fields() {
        let merged = merge_fields_value(
            serde_json::from_value(serde_json::json!({"setup": {"skills": true}, "other": 1}))
                .unwrap(),
            serde_json::from_value(serde_json::json!({"method": "script", "version": "0.1.0"}))
                .unwrap(),
        );
        assert_eq!(merged["setup"]["skills"], true);
        assert_eq!(merged["other"], 1);
        assert_eq!(merged["method"], "script");
    }
}
