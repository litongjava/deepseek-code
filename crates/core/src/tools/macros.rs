use anyhow::Result;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub struct MacroToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub fn get_macro_definitions() -> Vec<MacroToolDefinition> {
    vec![
        MacroToolDefinition {
            name: "smart_project_scan".to_string(),
            description: "Scan a project directory structure, read key config files, and check git status in one operation.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Project root directory" }
                },
                "required": ["path"]
            }),
        },
    ]
}

pub fn execute_smart_project_scan(input: Value, cwd: &str) -> Result<Value> {
    let project_path = input["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("path required"))?;
    let resolved_path = super::resolve_path(project_path, cwd)?;

    let mut structure: Vec<Value> = Vec::new();

    for entry in walkdir::WalkDir::new(&resolved_path)
        .max_depth(3)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.')
                && name != "node_modules"
                && name != "target"
                && name != "dist"
                && name != "build"
        })
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let depth = entry.depth();
        let is_dir = entry.file_type().is_dir();

        structure.push(json!({
            "path": path.to_string_lossy(),
            "depth": depth,
            "is_dir": is_dir
        }));
    }

    let config_files = ["package.json", "Cargo.toml", "pyproject.toml", "go.mod"];
    let mut configs = serde_json::Map::new();

    for config_name in &config_files {
        let config_path = Path::new(&resolved_path).join(config_name);
        if config_path.exists() {
            if let Ok(content) = fs::read_to_string(&config_path) {
                configs.insert(config_name.to_string(), json!(content));
            }
        }
    }

    let git_status = match super::run_git_command(&["status", "--porcelain"], &resolved_path) {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let files: Vec<Value> = stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(|line| {
                    let status = &line[..2.min(line.len())];
                    let file_path = if line.len() > 3 { &line[3..] } else { "" };
                    json!({
                        "status": status.trim(),
                        "file": file_path
                    })
                })
                .collect();
            json!({"files": files, "count": files.len()})
        }
        Err(e) => json!({"error": e.to_string()}),
    };

    Ok(json!({
        "structure": structure,
        "configs": Value::Object(configs),
        "git_status": git_status
    }))
}
