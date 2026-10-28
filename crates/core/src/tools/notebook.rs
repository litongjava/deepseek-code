use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{io::Write, path::PathBuf};

pub(super) fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: "NotebookEdit".into(),
        description: "Replaces, inserts, or deletes a single cell in a Jupyter notebook (.ipynb file).\n\nUsage:\n- You must use the Read tool on the notebook in this conversation before editing — this tool will fail otherwise.\n- `notebook_path` must be an absolute path.\n- `cell_id` is the `id` attribute shown in the Read tool's `<cell id=\"...\">` output. It is required for `replace` and `delete`.\n- `edit_mode` defaults to `replace`. Use `insert` to add a new cell after the cell with the given `cell_id` (or at the beginning of the notebook if `cell_id` is omitted) — `cell_type` is required when inserting. Use `delete` to remove the cell.".into(),
        input_schema: json!({"type":"object","properties":{
            "notebook_path":{"description":"The absolute path to the Jupyter notebook file to edit (must be absolute, not relative)","type":"string"},
            "cell_id":{"description":"The ID of the cell to edit. When inserting a new cell, the new cell will be inserted after the cell with this ID, or at the beginning if not specified.","type":"string"},
            "new_source":{"description":"The new source for the cell","type":"string"},
            "cell_type":{"description":"The type of the cell (code or markdown). If not specified, it defaults to the current cell type. If using edit_mode=insert, this is required.","type":"string","enum":["code","markdown"]},
            "edit_mode":{"description":"The type of edit to make (replace, insert, delete). Defaults to replace.","type":"string","enum":["replace","insert","delete"]}
        },"required":["notebook_path","new_source"],"additionalProperties":false}),
    }
}

/// The reference shape addresses cells by the id Read shows (`cell-N` when a
/// cell has none) and maps onto the index-based operations below.
fn reference_edit(input: &Value, cwd: &str) -> Result<Value> {
    let notebook_path = input["notebook_path"].as_str().context("notebook_path is required")?;
    let path = PathBuf::from(super::resolve_path(notebook_path, cwd)?);
    let notebook: Value = serde_json::from_slice(&std::fs::read(&path)?).context("Invalid notebook JSON")?;
    let cells = notebook["cells"].as_array().context("Notebook cells must be an array")?;
    let position = |id: &str| cells.iter().enumerate().position(|(index, cell)| cell["id"].as_str().map(str::to_owned).unwrap_or_else(|| format!("cell-{index}")) == id);
    let cell_id = input["cell_id"].as_str().filter(|id| !id.is_empty());
    let index = match cell_id {
        Some(id) => Some(position(id).with_context(|| format!("Cell {id} not found"))?),
        None => None,
    };
    let lines: Vec<Value> = input["new_source"].as_str().unwrap_or_default().split_inclusive('\n').map(|line| json!(line)).collect();
    let mode = input["edit_mode"].as_str().unwrap_or("replace");
    let legacy = match mode {
        "replace" => json!({"path":notebook_path,"operation":"edit_cell","cell_index":index.context("cell_id is required for replace")?,"source":lines,"cell_type":input["cell_type"]}),
        "insert" => json!({"path":notebook_path,"operation":"add_cell","cell_index":index.map_or(0, |at| at + 1),"source":lines,"cell_type":input["cell_type"].as_str().context("cell_type is required when inserting")?}),
        "delete" => json!({"path":notebook_path,"operation":"delete_cell","cell_index":index.context("cell_id is required for delete")?}),
        other => bail!("Unknown edit_mode: {other}"),
    };
    execute(legacy, cwd)?;
    Ok(json!({"notebook_path":path,"cell_id":cell_id,"edit_mode":mode,"content":format!("Notebook {mode} succeeded for {}", cell_id.unwrap_or("the first position"))}))
}

pub(super) fn execute(input: Value, cwd: &str) -> Result<Value> {
    if input.get("notebook_path").is_some() {
        return reference_edit(&input, cwd);
    }
    let path = input["path"].as_str().context("path is required")?;
    let path = PathBuf::from(super::resolve_path(path, cwd)?);
    if path.extension().and_then(|p| p.to_str()) != Some("ipynb") {
        bail!("Notebook path must end in .ipynb");
    }
    let operation = input["operation"]
        .as_str()
        .context("operation is required")?;
    let writable = !matches!(operation, "read" | "list_cells");
    let parent = path.parent().context("Notebook parent is missing")?;
    let lock = if writable {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("ipynb.claude-lock"))?;
        file.try_lock()
            .context("Notebook is being edited by another process")?;
        Some(file)
    } else {
        None
    };
    let mut notebook = if operation == "create" {
        if path.exists() {
            bail!("Notebook already exists");
        }
        json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[]})
    } else {
        let bytes = std::fs::read(&path)?;
        if bytes.len() > 32 * 1024 * 1024 {
            bail!("Notebook exceeds 32 MiB");
        }
        serde_json::from_slice::<Value>(&bytes).context("Invalid notebook JSON")?
    };
    if notebook["nbformat"] != 4 {
        bail!("Only Jupyter notebook format 4 is supported");
    }
    let cells = notebook["cells"]
        .as_array_mut()
        .context("Notebook cells must be an array")?;
    let index = || -> Result<usize> {
        let index = input["cell_index"]
            .as_u64()
            .context("cell_index is required")?;
        usize::try_from(index).context("cell_index is too large")
    };
    match operation {
        "create" | "read" => {}
        "list_cells" => return Ok(json!({"path":path,"cells":cells})),
        "add_cell" => {
            let kind = input["cell_type"].as_str().unwrap_or("code");
            if !matches!(kind, "code" | "markdown" | "raw") {
                bail!("Invalid cell_type");
            }
            let source = source(&input)?;
            let metadata = input.get("metadata").cloned().unwrap_or(json!({}));
            if !metadata.is_object() {
                bail!("metadata must be an object");
            }
            let mut cell = json!({"id":uuid::Uuid::new_v4().to_string(),"cell_type":kind,"source":source,"metadata":metadata});
            if kind == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
            let at = if input.get("cell_index").is_some() {
                index()?
            } else {
                cells.len()
            };
            if at > cells.len() {
                bail!("cell_index is out of range");
            }
            cells.insert(at, cell);
        }
        "edit_cell" => {
            let cell = cells
                .get_mut(index()?)
                .context("cell_index is out of range")?;
            if input.get("source").is_some() {
                cell["source"] = source(&input)?;
            }
            if let Some(metadata) = input.get("metadata") {
                if !metadata.is_object() {
                    bail!("metadata must be an object");
                }
                cell["metadata"] = metadata.clone();
            }
            if let Some(kind) = input["cell_type"].as_str() {
                if !matches!(kind, "code" | "markdown" | "raw") {
                    bail!("Invalid cell_type");
                }
                if cell["cell_type"] != kind {
                    cell["cell_type"] = json!(kind);
                    if let Some(object) = cell.as_object_mut() {
                        if kind == "code" { object.insert("outputs".into(), json!([])); object.insert("execution_count".into(), Value::Null); }
                        else { object.remove("outputs"); object.remove("execution_count"); }
                    }
                }
            }
            // Results from the previous source are stale after editing code.
            if cell["cell_type"] == "code" && input.get("source").is_some() {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
        }
        "delete_cell" => {
            let at = index()?;
            if at >= cells.len() {
                bail!("cell_index is out of range");
            }
            cells.remove(at);
        }
        "move_cell" => {
            let from = index()?;
            let to = input
                .get("new_index")
                .or_else(|| input["metadata"].get("new_index"))
                .and_then(Value::as_u64)
                .context("new_index is required")? as usize;
            if from >= cells.len() || to >= cells.len() {
                bail!("Cell index is out of range");
            }
            let cell = cells.remove(from);
            cells.insert(to, cell);
        }
        "clear_outputs" => {
            let selected = if input.get("cell_index").is_some() {
                let at = index()?;
                if at >= cells.len() {
                    bail!("cell_index is out of range");
                }
                Some(at)
            } else {
                None
            };
            for (at, cell) in cells.iter_mut().enumerate() {
                if selected.is_none_or(|selected| selected == at) && cell["cell_type"] == "code" {
                    cell["outputs"] = json!([]);
                    cell["execution_count"] = Value::Null;
                }
            }
        }
        _ => bail!("Unknown notebook operation: {operation}"),
    }
    if writable {
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&serde_json::to_vec_pretty(&notebook)?)?;
        temporary.as_file().sync_all()?;
        if operation == "create" {
            temporary.persist_noclobber(&path)?;
        } else {
            temporary.persist(&path)?;
        }
    }
    drop(lock);
    Ok(json!({"path":path,"notebook":notebook}))
}

fn source(input: &Value) -> Result<Value> {
    let source = input["source"]
        .as_array()
        .context("source must be an array of strings")?;
    if source.iter().any(|line| !line.is_string()) {
        bail!("source must contain strings");
    }
    Ok(Value::Array(source.clone()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reference_edits_address_cells_by_read_ids() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join("n.ipynb"), serde_json::json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"a","cell_type":"code","source":["x = 1\n"],"metadata":{},"outputs":[{"text":"old"}],"execution_count":1},{"cell_type":"markdown","source":["# t"],"metadata":{}}]}).to_string()).unwrap();
        let edit = |input: serde_json::Value| super::execute(input, cwd);
        edit(serde_json::json!({"notebook_path":"n.ipynb","cell_id":"a","new_source":"x = 2\ny = 3"})).unwrap();
        edit(serde_json::json!({"notebook_path":"n.ipynb","cell_id":"cell-1","new_source":"print(1)","cell_type":"code"})).unwrap();
        edit(serde_json::json!({"notebook_path":"n.ipynb","new_source":"intro","cell_type":"markdown","edit_mode":"insert"})).unwrap();
        assert!(edit(serde_json::json!({"notebook_path":"n.ipynb","new_source":"x","edit_mode":"insert"})).is_err());
        assert!(edit(serde_json::json!({"notebook_path":"n.ipynb","cell_id":"zzz","new_source":""})).is_err());
        let notebook: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("n.ipynb")).unwrap()).unwrap();
        let cells = notebook["cells"].as_array().unwrap();
        assert_eq!(cells[0]["source"], serde_json::json!(["intro"]));
        assert_eq!(cells[1]["source"], serde_json::json!(["x = 2\n", "y = 3"]));
        assert_eq!(cells[1]["outputs"], serde_json::json!([]));
        assert_eq!((cells[2]["cell_type"].as_str(), cells[2]["outputs"].clone()), (Some("code"), serde_json::json!([])));
        edit(serde_json::json!({"notebook_path":"n.ipynb","cell_id":"a","new_source":"","edit_mode":"delete"})).unwrap();
        let notebook: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("n.ipynb")).unwrap()).unwrap();
        assert_eq!(notebook["cells"].as_array().unwrap().len(), 2);
    }

    use super::*;
    #[test]
    fn edits_preserve_unknown_metadata_and_invalidate_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let path = dir.path().join("test.ipynb");
        std::fs::write(&path, json!({"nbformat":4,"nbformat_minor":5,"metadata":{"custom":"preserve"},"vendor":"preserve","cells":[{"cell_type":"code","source":["old"],"metadata":{"tags":["keep"]},"outputs":[{"text":"stale"}],"execution_count":3}]}).to_string()).unwrap();
        execute(
            json!({"path":"test.ipynb","operation":"edit_cell","cell_index":0,"source":["new"]}),
            cwd,
        )
        .unwrap();
        let result = execute(json!({"path":"test.ipynb","operation":"read"}), cwd).unwrap();
        assert_eq!(result["notebook"]["vendor"], "preserve");
        assert_eq!(
            result["notebook"]["cells"][0]["metadata"]["tags"],
            json!(["keep"])
        );
        assert_eq!(result["notebook"]["cells"][0]["outputs"], json!([]));
        let before = std::fs::read(&path).unwrap();
        assert!(execute(
            json!({"path":"test.ipynb","operation":"delete_cell","cell_index":8}),
            cwd
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(execute(json!({"path":"test.ipynb","operation":"create"}), cwd).is_err());
    }
    #[test]
    fn create_add_move_delete_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        execute(json!({"path":"test.ipynb","operation":"create"}), cwd).unwrap();
        for kind in ["code", "markdown"] {
            execute(json!({"path":"test.ipynb","operation":"add_cell","cell_type":kind,"source":[kind]}),cwd).unwrap();
        }
        execute(json!({"path":"test.ipynb","operation":"move_cell","cell_index":1,"metadata":{"new_index":0}}),cwd).unwrap();
        let result = execute(json!({"path":"test.ipynb","operation":"list_cells"}), cwd).unwrap();
        assert_eq!(result["cells"][0]["cell_type"], "markdown");
        execute(
            json!({"path":"test.ipynb","operation":"clear_outputs"}),
            cwd,
        )
        .unwrap();
        execute(
            json!({"path":"test.ipynb","operation":"delete_cell","cell_index":0}),
            cwd,
        )
        .unwrap();
        let result = execute(json!({"path":"test.ipynb","operation":"list_cells"}), cwd).unwrap();
        assert_eq!(result["cells"].as_array().unwrap().len(), 1);
    }
}
