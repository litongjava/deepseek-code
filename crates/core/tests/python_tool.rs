use deepseek_code_core::tools::{execute_tool, execute_tool_async};
use serde_json::json;

#[tokio::test]
async fn python_tool_runs_files_env_saves_and_times_out() {
    fn interpreter_available() -> bool {
        #[cfg(windows)]
        if std::process::Command::new("py")
            .args(["-3", "--version"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return true;
        }
        ["python3", "python"].iter().any(|name| {
            std::process::Command::new(name)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }
    if !interpreter_available() {
        eprintln!("skipping Python integration checks: no interpreter found");
        return;
    }

    let data = tempfile::tempdir().unwrap();
    std::env::set_var(deepseek_code_core::runtime::DATA_DIR_ENV, data.path());
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_str().unwrap();

    let output = execute_tool_async("Python", json!({"code": "print(1)"}), cwd)
        .await
        .unwrap();
    assert_eq!(output["stdout"], "1\n", "{output}");
    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["shell"], "python");
    assert_eq!(output["script_saved"], false);
    assert!(output.get("script_path").is_none());
    assert!(output["interpreter"]
        .as_str()
        .is_some_and(|path| std::path::Path::new(path).is_absolute()));
    assert!(output["interpreter_source"].is_string());

    // A temporary file still has a real __file__, and a null stdin is EOF.
    let output = execute_tool_async(
        "Python",
        json!({
            "code": "import sys\nprint(__file__)\ntry:\n    input()\nexcept EOFError:\n    print('EOF')\nprint(repr(sys.stdin.read()))"
        }),
        cwd,
    )
    .await
    .unwrap();
    let stdout = output["stdout"].as_str().unwrap();
    let temp_script = std::path::PathBuf::from(stdout.lines().next().unwrap());
    assert!(
        temp_script
            .extension()
            .is_some_and(|extension| extension == "py"),
        "{stdout}"
    );
    assert!(stdout.contains("EOF"), "{stdout}");
    assert!(stdout.contains("''"), "{stdout}");
    assert!(
        !temp_script.exists(),
        "the temporary script should be deleted"
    );

    // Explicit env merges over the inherited environment, and the working
    // directory stays importable even after PYTHONPATH is supplied.
    std::fs::write(workspace.path().join("project_module.py"), "VALUE = 42\n").unwrap();
    let output = execute_tool_async(
        "Python",
        json!({
            "code": "import os, project_module; print(project_module.VALUE, os.environ['DCL_TEST'])",
            "env": {"DCL_TEST": "visible"}
        }),
        cwd,
    )
    .await
    .unwrap();
    assert_eq!(output["stdout"], "42 visible\n", "{output}");

    // A non-zero exit code and stderr survive verbatim.
    let output = execute_tool_async(
        "Python",
        json!({"code": "import sys\nsys.stderr.write('boom\\n')\nsys.exit(3)"}),
        cwd,
    )
    .await
    .unwrap();
    assert_eq!(output["exit_code"], 3);
    assert_eq!(output["stderr"], "boom\n");
    assert_eq!(output["success"], false);

    // save_name writes under <workspace>/temp, creates parents, and returns an
    // absolute path. A second call overwrites the same draft.
    let saved = execute_tool_async(
        "Python",
        json!({"code": "print(7)", "save_name": "python/script/001.py"}),
        cwd,
    )
    .await
    .unwrap();
    assert_eq!(saved["script_saved"], true);
    let script = workspace.path().join("temp/python/script/001.py");
    assert_eq!(std::fs::read_to_string(&script).unwrap(), "print(7)");
    let path = saved["script_path"].as_str().unwrap();
    assert!(std::path::Path::new(path).is_absolute());
    assert_eq!(
        std::path::Path::new(path).canonicalize().unwrap(),
        script.canonicalize().unwrap()
    );
    execute_tool_async(
        "Python",
        json!({"code": "print(8)", "save_name": "python/script/001.py"}),
        cwd,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&script).unwrap(), "print(8)");

    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        let link = workspace.path().join("temp/python/link.py");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside.py"), &link).unwrap();
        let error = execute_tool_async(
            "Python",
            json!({"code": "print(1)", "save_name": "python/link.py"}),
            cwd,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("symlink"), "{error}");
    }

    // A background timeout is not used here; a foreground timeout stops the
    // process and reports it through the shared shell result.
    let output = execute_tool_async(
        "Python",
        json!({"code": "import time; time.sleep(10)", "timeout": 100}),
        cwd,
    )
    .await
    .unwrap();
    assert_eq!(output["timed_out"], true, "{output}");
    assert_eq!(output["is_error"], true);

    // Output caps match the shared capture.
    let output = execute_tool_async("Python", json!({"code": "print('x' * 1200000)"}), cwd)
        .await
        .unwrap();
    assert_eq!(output["stdout_truncated"], true, "{output}");
    assert!(
        output["stdout"].as_str().unwrap().len() < 1_200_000,
        "stdout should be bounded"
    );

    // The sync entry point resolves and runs the same tool.
    let output = execute_tool("Python", json!({"code": "print('sync')"}), cwd).unwrap();
    assert_eq!(output["stdout"], "sync\n");

    // settings.python.path is authoritative: a stale path errors instead of
    // silently falling back to PATH.
    deepseek_code_core::data_layout::write_config_file(
        data.path().join("config/settings.json"),
        json!({"python": {"path": "/definitely/missing/python"}}).to_string(),
    )
    .unwrap();
    let error = execute_tool_async("Python", json!({"code": "print(1)"}), cwd)
        .await
        .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("python.path"), "{error}");
    assert!(error.contains("/definitely/missing/python"), "{error}");
    std::fs::remove_file(data.path().join("config/settings.json")).unwrap();
}
