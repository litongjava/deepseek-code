use deepseek_desktop_tauri_lib::tools::execute_tool;
use serde_json::json;

#[test]
fn grep_honors_glob_and_resolves_relative_paths_in_project() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/main.rs"), "// 中文 needle\n").unwrap();
    std::fs::write(temp.path().join("src/notes.txt"), "needle\n").unwrap();
    let cwd = temp.path().to_str().unwrap();
    for include in ["*.rs", "**/*.rs"] {
        let result = execute_tool("Grep", json!({"pattern":"needle", "path":"src", "glob":include, "output_mode":"content"}), cwd).unwrap();
        assert_eq!(result["count"], 1);
        assert!(result["content"].as_str().unwrap().ends_with(":1:// 中文 needle"), "{result}");
    }
    let file = execute_tool("Grep", json!({"pattern":"needle", "path":"src/main.rs", "glob":"*.rs"}), cwd).unwrap();
    assert_eq!(file["count"], 1);
    assert!(execute_tool("Grep", json!({"pattern":"needle", "glob":"["}), cwd).is_err());
}

#[test]
fn glob_matches_project_relative_patterns_and_only_returns_files() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("src/nested")).unwrap();
    std::fs::create_dir(temp.path().join("src/directory.rs")).unwrap();
    std::fs::write(temp.path().join("src/main.rs"), "").unwrap();
    std::fs::write(temp.path().join("src/nested/lib.rs"), "").unwrap();
    let cwd = temp.path().to_str().unwrap();
    let result = execute_tool("Glob", json!({"pattern":"src/**/*.rs"}), cwd).unwrap();
    assert_eq!(result["count"], 2);
    let scoped = execute_tool("Glob", json!({"pattern":"**/*.rs", "path":"src"}), cwd).unwrap();
    assert_eq!(scoped["count"], 2);
    assert!(execute_tool("Glob", json!({"pattern":"["}), cwd).is_err());
}
