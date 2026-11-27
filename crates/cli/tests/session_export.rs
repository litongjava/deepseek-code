//! Read the public wire format independently of the exporter.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use deepseek_code_core::db::{
    message_repo::ToolCallRow,
    session_store::{Branch, CreateOptions, ProjectSelection, Stores},
    turn_repo::TurnStatus,
};
use ring::digest::{digest, SHA256};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Command, Stdio},
};

fn hash(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|n| format!("{n:02x}"))
        .collect()
}

fn export(root: &Path, id: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dcl"));
    c.arg("--data-dir")
        .arg(root)
        .args(["sessions", "export", id])
        .stdin(Stdio::null());
    c
}

fn stdout(root: &Path, id: &str) -> Result<Vec<u8>> {
    let out = export(root, id).output()?;
    ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

fn decode(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut counts = BTreeMap::<String, usize>::new();
    let mut records = Vec::new();
    let mut parts = HashMap::<(String, String), Vec<Vec<u8>>>::new();
    let mut offset = 0;
    let mut ended = false;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        ensure!(line.ends_with(b"\n") && !ended, "invalid record boundary");
        let row: Value = serde_json::from_slice(line)?;
        let kind = row["type"].as_str().context("record type")?;
        if offset == 0 {
            ensure!(
                kind == "header" && row["format"] == "deepseek-code.session" && row["version"] == 1,
                "invalid header"
            );
        }
        if kind == "end" {
            ensure!(
                row["counts"] == json!(counts) && row["sha256"] == hash(&bytes[..offset]),
                "invalid footer"
            );
            ended = true;
        } else {
            *counts.entry(kind.into()).or_default() += 1;
            if kind == "chunk" {
                let key = (
                    row["owner"].as_str().unwrap().into(),
                    row["field"].as_str().unwrap().into(),
                );
                let list = parts.entry(key).or_default();
                ensure!(row["index"] == list.len(), "out of order chunk");
                list.push(STANDARD.decode(row["data"].as_str().unwrap())?);
                // The final descriptor must digest to the same value as every chunk tag.
                let descriptor = records
                    .iter()
                    .rev()
                    .find_map(|r: &Value| {
                        let field = &r[row["field"].as_str().unwrap()];
                        (field["owner"] == row["owner"]).then_some(field)
                    })
                    .context("unresolved chunk owner")?;
                ensure!(descriptor["sha256"] == row["sha256"], "wrong chunk digest");
            } else {
                records.push(row);
            }
        }
        offset += line.len();
    }
    ensure!(ended, "missing end record");
    for row in &mut records {
        for (name, field) in row.as_object_mut().unwrap() {
            if let Some(owner) = field["owner"].as_str() {
                let chunks = parts
                    .remove(&(owner.into(), name.clone()))
                    .context("missing chunks")?;
                ensure!(field["chunks"] == chunks.len(), "missing chunk");
                let data = chunks.concat();
                ensure!(
                    field["bytes"] == data.len() && field["sha256"] == hash(&data),
                    "bad field digest"
                );
                *field = match field["encoding"].as_str().unwrap() {
                    "utf8" => json!(String::from_utf8(data)?),
                    "binary" => json!({"encoding":"base64","data":STANDARD.encode(data)}),
                    other => anyhow::bail!("unknown encoding {other}"),
                };
            }
        }
    }
    ensure!(parts.is_empty(), "orphan chunks");
    Ok(records)
}

fn of<'a>(rows: &'a [Value], kind: &str) -> Vec<&'a Value> {
    rows.iter().filter(|r| r["type"] == kind).collect()
}

fn links(rows: &[Value]) {
    let id = &rows[0]["session_id"];
    for r in &rows[1..] {
        if let Some(session) = r.get("conversation_id") {
            assert_eq!(session, id);
        }
        for (field, kind, key) in [
            ("message_id", "message", "id"),
            ("turn_id", "turn", "id"),
            ("tool_call_id", "tool_call", "id"),
            ("parent_tool_call_id", "tool_call", "id"),
            ("tool_use_id", "tool_call", "id"),
            ("request_id", "model_request", "id"),
        ] {
            if field == "tool_use_id" && r["occurrence_pending"] == true {
                assert!(!of(rows, kind).iter().any(|v| v[key] == r[field]));
            } else if !r[field].is_null() {
                assert!(
                    of(rows, kind).iter().any(|v| v[key] == r[field]),
                    "unresolved {field} on {r}"
                );
            }
        }
    }
}

#[test]
fn reconstruct_history_related_records_and_fork_after_rewind() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let session = db.create(
        "source",
        CreateOptions {
            title: Some("export title".into()),
            model: Some("model".into()),
            provider: Some("provider".into()),
            project: Some(ProjectSelection::Workspace(root.path().into())),
            ..Default::default()
        },
    )?;
    let typed = json!([{"type":"text","text":"one\ntwo"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]);
    let large = "界🙂\n".repeat(32000);
    session.admit_user("user", &typed, "typed")?;
    let message = session.write(|w| {
        let turn = w.start_turn()?;
        let message = w.segment(&turn)?;
        w.message_content(&message, &large)?;
        w.snapshots(
            &message,
            &json!([{"id":"r1","systemPrompt":large},{"id":"r2","systemPrompt":large}]),
        )?;
        for (i, name) in ["parent", "child"].iter().enumerate() {
            let parent = format!("{message}:parent");
            w.tool(
                &ToolCallRow {
                    id: format!("{message}:{name}"),
                    message_id: message.clone(),
                    name: name.to_string(),
                    input: Some("input".into()),
                    output: Some("output".into()),
                    is_error: false,
                    sort_order: i as i64,
                },
                (i == 1).then_some((parent.as_str(), 1)),
            )?;
        }
        let tool = format!("{message}:child");
        w.tool_note(&tool, "note")?;
        w.tool_duration(&tool, 52)?;
        w.attachment(
            "file",
            &message,
            "external.txt",
            "file",
            "text/plain",
            17,
            "/external/file",
        )?;
        w.approval(&serde_json::from_value(json!({"id":"a","conversation_id":"source","message_id":message,
            "tool_name":"Write","action":"save","risk_level":"medium","status":"pending","created_at":"2026-01-01"}))?)?;
        w.decide("a","approved",Some("allow"),Some("fixture decision"),"2026-01-02")?;
        w.diff(&serde_json::from_value(json!({"id":"d","conversation_id":"source","message_id":message,
            "file_path":"f","original_content":"old","modified_content":"new","diff_text":"change","status":"pending","created_at":"2026-01-01"}))?)?;
        w.set_goal("finish export")?;
        w.set_message_metadata(&message, &json!({"fixture":true}))?;
        w.set_request_metadata("r1", &json!({"fixture":1}))?;
        w.bind_agent("agent definition")?;
        w.set_git_branch(Some("topic"))?;
        w.finish_turn(&turn, TurnStatus::Failed, 15)?;
        let cancelled = w.start_turn()?;
        w.segment(&cancelled)?;
        w.finish_turn(&cancelled, TurnStatus::Cancelled, 5)?;
        let tail = w.append("assistant", "retired")?;
        w.snapshots(&tail, &json!([{"id":"gone","systemPrompt":"unreferenced"}]))?;
        w.rewind_from(&tail)?;
        Ok(message)
    })?;
    session.approve_plan("approved plan")?;
    session.save_mcp(
        &message,
        "child",
        &json!({"content":[{"type":"text","text":"result"}],"_meta":{"trace":"ok"}}),
        false,
    )?;
    let history = db.executor_history("source")?;
    history.add_segment_input(
        &message,
        "child",
        &deepseek_code_core::services::input_queue::DeliveredInput {
            text: "queued".into(),
            content: json!("queued"),
            origin: "user".into(),
            prompt_origin: Some("typed".into()),
            message_id: None,
        },
    )?;
    history.save_segment(
        &message,
        deepseek_code_core::db::session_store::SegmentUpdate {
            thinking: Some("exact\nthought"),
            ..Default::default()
        },
    )?;
    let fork = session.branch("fork", Branch::Full)?;
    let (running, segment) = session.write(|w| {
        let turn = w.start_turn()?;
        let segment = w.segment(&turn)?;
        w.message_content(&segment, "committed prefix")?;
        Ok((turn, segment))
    })?;
    session.set_archived(true)?;
    let projection = session.read(|r| r.projection())?;
    let source = decode(&stdout(root.path(), "source")?)?;
    let copied = decode(&stdout(root.path(), "fork")?)?;
    for rows in [&source, &copied] {
        links(rows);
        let origins = of(rows, "tool_origin");
        assert_eq!(origins.len(), 1);
        let tools = of(rows, "tool_call");
        for (name, field) in [("parent", "parent_tool_call_id"), ("child", "tool_call_id")] {
            assert_eq!(
                origins[0][field],
                tools.iter().find(|t| t["name"] == name).unwrap()["id"]
            );
        }
        let messages = of(rows, "message");
        let user = messages.iter().find(|r| r["role"] == "user").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(user["user_content_json"].as_str().unwrap())?,
            session.read(|r| r.user_content("user"))?
        );
        let assistant = messages.iter().find(|r| r["content"] == large).unwrap();
        assert_eq!(assistant["thinking"], "exact\nthought");
        assert!(messages
            .windows(2)
            .all(|m| m[0]["sort_order"].as_i64() <= m[1]["sort_order"].as_i64()));
        assert!(messages.iter().all(|m| m["content"] != "retired"));
        let blobs = of(rows, "request_blob");
        assert_eq!(blobs.len(), 1);
        let body = blobs[0]["body"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<String>(body)?, large);
        assert_eq!(blobs[0]["hash"], hash(body.as_bytes()));
        let snapshots: Vec<Value> = serde_json::from_str(
            of(rows, "request_snapshot")[0]["requests_json"]
                .as_str()
                .unwrap(),
        )?;
        assert_eq!(snapshots.len(), 2);
        let requests = of(rows, "model_request");
        for r in snapshots {
            assert_eq!(r["systemPromptRef"], blobs[0]["hash"]);
            assert!(requests
                .iter()
                .any(|n| n["id"] == r["id"] && n["message_id"] == assistant["id"]));
        }
        assert_eq!(of(rows, "tool_note")[0]["note"], "note");
        assert_eq!(of(rows, "tool_duration")[0]["duration_ms"], 52);
        assert!(of(rows, "delivered_inputs")[0]["inputs_json"]
            .as_str()
            .unwrap()
            .contains("queued"));
        assert!(of(rows, "mcp_output")[0]["content_json"]
            .as_str()
            .unwrap()
            .contains("result"));
        assert_eq!(of(rows, "attachment")[0]["source"], "/external/file");
        let turns = of(rows, "turn");
        for status in ["failed", "cancelled"] {
            assert!(turns
                .iter()
                .any(|r| r["status"] == status && r["ended_at"].is_string()));
        }
    }
    assert_eq!(of(&source, "diff")[0]["original_content"], "old");
    assert_eq!(
        of(&source, "approval")[0]["decision_reason"],
        "fixture decision"
    );
    assert!(of(&source, "local_setting")
        .iter()
        .any(|r| r["value"].as_str().unwrap().contains("finish export")));
    assert!(of(&source, "permission_authority")
        .iter()
        .any(|r| r["source"] == "approved_plan"));
    let plan = &of(&source, "approved_plan_boundary")[0]["message_id"];
    assert!(of(&source, "message")
        .iter()
        .any(|r| r["id"] == *plan && r["is_compact_boundary"] == true));
    let h = &source[0];
    assert_eq!(h["status"], "running");
    assert_eq!(h["archived"], true);
    assert_eq!(h["incarnation"], session.incarnation());
    assert_eq!(h["content_revision"], projection.revision);
    assert_eq!(h["message_count"], of(&source, "message").len());
    assert_eq!(h["title"], "export title");
    assert_eq!(h["provider"], "provider");
    assert_eq!(h["model"], "model");
    assert!(h["metadata_revision"].is_i64() && h["project_id"].is_string());
    let turn = of(&source, "turn")
        .into_iter()
        .find(|t| t["id"] == running)
        .unwrap();
    assert_eq!(turn["status"], "running");
    assert!(turn["ended_at"].is_null() && turn["elapsed_ms"].is_null());
    assert!(of(&source, "message").iter().any(|m| m["id"] == segment
        && m["turn_id"] == running
        && m["content"] == "committed prefix"));
    assert_eq!(copied[0]["incarnation"], fork.incarnation());
    for t in of(&copied, "turn") {
        assert!(of(&source, "turn")
            .iter()
            .any(|p| p["id"] == t["source_turn_id"] && p["id"] != t["id"]));
    }
    assert_eq!(of(&source, "agent")[0]["definition"], "agent definition");
    Ok(())
}

#[test]
fn pipe_pins_committed_prefix_and_interruption_has_no_footer() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let session = stores.create("pipe", Default::default())?;
    let payload = "bounded output 界\n".repeat(100000);
    session.write(|w| w.append("assistant", &payload))?;
    let expected = stdout(root.path(), "pipe")?;
    let mut child = export(root.path(), "pipe").stdout(Stdio::piped()).spawn()?;
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut copy = Vec::new();
    reader.read_until(b'\n', &mut copy)?;
    let header: Value = serde_json::from_slice(&copy)?;
    assert_eq!(header["type"], "header");
    // Output exceeds pipe capacity. This separate writer commits while the
    // exporter is blocked writing the old snapshot to our pipe.
    session.write(|w| w.append("user", "later append"))?;
    reader.read_to_end(&mut copy)?;
    assert!(child.wait()?.success());
    assert_eq!(copy, expected);
    let prior = decode(&copy)?;
    assert_eq!(of(&prior, "message").len(), 1);
    assert_eq!(of(&prior, "message")[0]["content"], payload);
    assert_eq!(
        of(&decode(&stdout(root.path(), "pipe")?)?, "message").len(),
        2
    );

    let mut child = export(root.path(), "pipe")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut truncated = Vec::new();
    reader.read_until(b'\n', &mut truncated)?;
    drop(reader);
    let output = child.wait_with_output()?;
    assert!(!output.status.success());
    assert_eq!(
        decode(&truncated).unwrap_err().to_string(),
        "missing end record"
    );
    let mut damaged = expected;
    let pos = damaged.windows(9).position(|s| s == b"assistant").unwrap();
    damaged[pos] = b'A';
    assert_eq!(decode(&damaged).unwrap_err().to_string(), "invalid footer");
    session.delete()?;
    Ok(())
}

#[test]
fn publish_named_file_once_and_clean_up_after_failure() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    db.create("file", Default::default())?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("backup.jsonl");
    let named = |id| {
        export(root.path(), id)
            .current_dir(dir.path())
            .args(["--output", "backup.jsonl"])
            .output()
    };
    let out = named("file")?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    assert_eq!(
        decode(&fs::read(&path)?)?,
        decode(&stdout(root.path(), "file")?)?
    );
    fs::write(&path, b"already here")?;
    let collision = named("file")?;
    assert!(
        !collision.status.success() && collision.stdout.is_empty() && !collision.stderr.is_empty()
    );
    assert_eq!(fs::read(&path)?, b"already here");
    fs::remove_file(&path)?;
    let missing = named("absent")?;
    assert!(!missing.status.success() && missing.stdout.is_empty() && !missing.stderr.is_empty());
    assert_eq!(fs::read_dir(dir.path())?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn export_sessions_generated_by_both_streaming_protocols() -> Result<()> {
    use axum::{routing::post, Router};
    for protocol in ["openai", "anthropic"] {
        let root = tempfile::tempdir()?;
        let data = root.path().join("data");
        fs::create_dir(&data)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let (route, frames) = if protocol == "anthropic" {
            (
                "/v1/messages",
                vec![
                    json!({"type":"message_start","message":{"type":"message","id":"resp","role":"assistant","model":"model","content":[]}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello from fixture"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
                    json!({"type":"message_stop"}),
                ],
            )
        } else {
            (
                "/v1/chat/completions",
                vec![
                    json!({"id":"resp","model":"model","choices":[{"index":0,"delta":{"content":"hello from fixture"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":3}}),
                ],
            )
        };
        let mut body = frames
            .iter()
            .map(|f| format!("data: {f}\n\n"))
            .collect::<String>();
        if protocol == "openai" {
            body.push_str("data: [DONE]\n\n");
        }
        let app = Router::new().route(
            route,
            post(move || async move { ([("content-type", "text/event-stream")], body) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        deepseek_code_core::data_layout::write_config_file(data.join("config/providers.json"),json!([{"id":"fixture","name":"fixture","api_key":"excluded-provider-secret","base_url":url,
            "api_format":protocol,"enabled":true,"models":[{"id":"model","name":"model","max_tokens":4096,"supports_web_search":false,"supports_vision":false,"enabled":true}]}]).to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        for (key, _) in std::env::vars().filter(|(k, _)| {
            k.starts_with("DEEPSEEK_CODE")
                || k.starts_with("ANTHROPIC_")
                || k.starts_with("OPENAI_")
        }) {
            cmd.env_remove(key);
        }
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(45),
            cmd.arg("--data-dir")
                .arg(&data)
                .arg("--path")
                .arg(root.path())
                .args([
                    "--print",
                    "--provider",
                    "fixture",
                    "--model",
                    "model",
                    "--session-id",
                    &id,
                    "export this",
                ])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        server.abort();
        let _ = server.await;
        let out = out??;
        ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bytes = stdout(&data, &id)?;
        assert!(!std::str::from_utf8(&bytes)?.contains("excluded-provider-secret"));
        let rows = decode(&bytes)?;
        links(&rows);
        assert!(of(&rows, "message")
            .iter()
            .any(|m| m["role"] == "assistant" && m["content"] == "hello from fixture"));
        assert!(of(&rows, "turn").iter().any(|t| t["status"] == "completed"));
        let req = of(&rows, "model_request");
        assert!(
            req.iter().any(|r| r["api_format"] == protocol),
            "missing normalized {protocol} request"
        );
        assert!(!of(&rows, "request_snapshot").is_empty());
    }
    Ok(())
}

fn directory_names(dir: &Path) -> Result<std::collections::BTreeSet<String>> {
    fs::read_dir(dir)?
        .map(|entry| Ok(entry?.file_name().into_string().unwrap()))
        .collect()
}

fn assert_layout(dir: &Path, required: &[&str], optional: &[&str]) -> Result<()> {
    let names = directory_names(dir)?;
    for name in required {
        ensure!(
            names.contains(*name),
            "missing {name} in {dir:?}: {names:?}"
        );
    }
    for name in names {
        ensure!(
            required.contains(&name.as_str()) || optional.contains(&name.as_str()),
            "unexpected {name} in {dir:?}"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn watch_legacy_files(paths: &[std::path::PathBuf]) -> Result<fs::File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    ensure!(fd >= 0, std::io::Error::last_os_error());
    let events = unsafe { fs::File::from_raw_fd(fd) };
    for path in paths {
        let name = CString::new(path.as_os_str().as_bytes())?;
        let watch = unsafe {
            libc::inotify_add_watch(events.as_raw_fd(), name.as_ptr(), libc::IN_ALL_EVENTS)
        };
        ensure!(watch >= 0, std::io::Error::last_os_error());
    }
    Ok(events)
}

#[test]
fn fresh_start_and_export_leave_all_three_legacy_files_untouched() -> Result<()> {
    let root = tempfile::tempdir()?;
    let names = [
        "deepseek_code.db",
        "deepseek_code.db-wal",
        "deepseek_code.db-shm",
    ];
    let paths = names.map(|s| root.path().join(s));
    let mut mtimes = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        fs::write(path, [0xff, i as u8, 0, 0x80])?;
        mtimes.push(fs::metadata(path)?.modified()?);
    }
    #[cfg(target_os = "linux")]
    let mut events = watch_legacy_files(&paths)?;
    let started = Command::new(env!("CARGO_BIN_EXE_dcl"))
        .arg("--data-dir")
        .arg(root.path())
        .args(["sessions", "list"])
        .output()?;
    ensure!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert_eq!(serde_json::from_slice::<Value>(&started.stdout)?, json!([]));
    let id = uuid::Uuid::new_v4().to_string();
    let db = Stores::open(root.path())?;
    let s = db.create(&id, Default::default())?;
    s.write(|w| w.append("user", "new-layout history"))?;
    drop((s, db));
    let bytes = stdout(root.path(), &id)?;
    assert_eq!(
        of(&decode(&bytes)?, "message")[0]["content"],
        "new-layout history"
    );
    #[cfg(target_os = "linux")]
    {
        let mut buf = [0; 4096];
        assert_eq!(
            events.read(&mut buf).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        // Confirm the same watcher detects an actual open before checking bytes.
        drop(fs::File::open(&paths[0])?);
        assert!(events.read(&mut buf)? >= std::mem::size_of::<libc::inotify_event>());
    }
    for (i, path) in paths.iter().enumerate() {
        assert_eq!(fs::metadata(path)?.modified()?, mtimes[i]);
        assert_eq!(fs::read(path)?, [0xff, i as u8, 0, 0x80]);
    }
    Ok(())
}

#[test]
fn fresh_install_creates_only_directory_shard_and_lifecycle_files() -> Result<()> {
    let root = tempfile::tempdir()?;
    let id = uuid::Uuid::new_v4().to_string();
    let data = root.path().join("new");
    let db = Stores::open(&data)?;
    db.create(&id, Default::default())?;
    drop(db);
    let result = export(&data, &id)
        .env("DCL_RESOURCE_SAMPLING", "0")
        .output()?;
    ensure!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(decode(&result.stdout)?[0]["session_id"], id);
    assert_layout(
        &data,
        &["README.md", "data", "global.db", "layout.json", "state"],
        &["global.db-shm", "global.db-wal"],
    )?;
    assert_layout(
        &data.join("state/locks/session"),
        &[".directory", ".initialized", ".orphan-sweep", &id],
        &[],
    )?;
    let sessions = data.join("data/sessions");
    assert_layout(&sessions, &[".staging", ".trash", ".quarantine", &id], &[])?;
    for dir in [".staging", ".trash", ".quarantine"] {
        assert_layout(&sessions.join(dir), &[], &[])?;
    }
    assert_layout(
        &sessions.join(id),
        &["session.db", "session.db.admission"],
        &["session.db-wal", "session.db-shm"],
    )?;
    Ok(())
}

#[path = "session_export/race.rs"]
mod race;
fn check_bad_blob(missing: bool) -> Result<()> {
    let root = tempfile::tempdir()?;
    let dest = tempfile::tempdir()?;
    let store = Stores::open(root.path())?;
    let id = "6217631e-49bd-41f9-b922-dbb42d16c3d1";
    let s = store.create(id, Default::default())?;
    s.write(|w| {
        let msg = w.append("assistant", "response")?;
        let request = if missing {
            json!({"toolsRef":"a".repeat(64)})
        } else {
            json!({"systemPrompt":"original"})
        };
        w.snapshots(&msg, &json!([request]))
    })?;
    if !missing {
        let db = rusqlite::Connection::open(
            root.path()
                .join("data/sessions")
                .join(id)
                .join("session.db"),
        )?;
        assert_eq!(
            db.execute("UPDATE request_blobs SET body=?1", ["wrong body"])?,
            1
        );
    }
    let out = export(root.path(), id)
        .arg("--output")
        .arg(dest.path().join("backup"))
        .output()?;
    assert!(!out.status.success());
    let reason = if missing {
        "Missing request blob"
    } else {
        "Request blob digest mismatch"
    };
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(reason),
        "{out:?}"
    );
    assert_eq!(fs::read_dir(dest.path())?.count(), 0);
    Ok(())
}
#[test]
fn reject_missing_request_blob() -> Result<()> {
    check_bad_blob(true)
}
#[test]
fn reject_mismatched_request_blob() -> Result<()> {
    check_bad_blob(false)
}
#[test]
fn mark_transport_result_waiting_for_occurrence() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Stores::open(root.path())?;
    let s = store.create("early", Default::default())?;
    let msg = s.write(|w| {
        let turn = w.start_turn()?;
        w.segment(&turn)
    })?;
    s.save_mcp(
        &msg,
        "soon",
        &json!({"content":[{"type":"text","text":"early"}]}),
        false,
    )?;
    let rows = decode(&stdout(root.path(), "early")?)?;
    let result = of(&rows, "mcp_output")[0];
    assert_eq!(result["tool_use_id"], format!("{msg}:soon"));
    assert_eq!(result["occurrence_pending"], true);
    assert!(of(&rows, "tool_call").is_empty());
    links(&rows);
    s.write(|w| {
        w.tool(
            &ToolCallRow {
                id: format!("{msg}:soon"),
                message_id: msg.clone(),
                name: "early".into(),
                input: None,
                output: None,
                is_error: false,
                sort_order: 0,
            },
            None,
        )
    })?;
    let linked = decode(&stdout(root.path(), "early")?)?;
    links(&linked);
    assert!(of(&linked, "mcp_output")[0]
        .get("occurrence_pending")
        .is_none());
    Ok(())
}
#[test]
fn export_leaves_unknown_roots_and_sessions_untouched() -> Result<()> {
    let empty = tempfile::tempdir()?;
    for root in [empty.path().join("absent"), empty.path().to_owned()] {
        assert!(!export(&root, "absent").output()?.status.success());
        assert_eq!(fs::read_dir(empty.path())?.count(), 0);
    }
    let store = Stores::open(empty.path())?;
    store.create("known", Default::default())?;
    let locks = empty.path().join("state/locks/session");
    let sessions = empty.path().join("data/sessions");
    let before = (directory_names(&locks)?, directory_names(&sessions)?);
    assert!(!export(empty.path(), "unknown").output()?.status.success());
    assert_eq!(
        before,
        (directory_names(&locks)?, directory_names(&sessions)?)
    );
    Ok(())
}
#[test]
fn leave_pending_delete_for_host_recovery() -> Result<()> {
    use deepseek_code_core::db::session_store::Phase;
    let root = tempfile::tempdir()?;
    let store = Stores::open(root.path())?;
    let id = "6217631e-49bd-41f9-b922-dbb42d16c3d1";
    store.create(id, Default::default())?;
    let fault = store.with_fault_hook(|phase| {
        if phase == Phase::DeleteIntent {
            anyhow::bail!("stop after intent");
        }
        Ok(())
    });
    assert!(fault.open_session(id)?.delete().is_err());
    let db = rusqlite::Connection::open(root.path().join("global.db"))?;
    let intent = || {
        db.query_row("SELECT operation FROM lifecycle WHERE id=?1", [id], |r| {
            r.get::<_, String>(0)
        })
    };
    let pending = intent()?;
    let shard = root
        .path()
        .join("data/sessions")
        .join(id)
        .join("session.db");
    assert!(shard.is_file());
    let out = export(root.path(), id).output()?;
    assert!(
        !out.status.success()
            && String::from_utf8_lossy(&out.stderr).contains("Session unavailable")
    );
    assert!(shard.is_file());
    assert_eq!(intent()?, pending);
    Ok(())
}

#[test]
fn export_closed_databases_without_creating_sqlite_sidecars() -> Result<()> {
    let root = tempfile::tempdir()?;
    let id = "6217631e-49bd-41f9-b922-dbb42d16c3d1";
    let store = Stores::open(root.path())?;
    store.create(id, Default::default())?;
    drop(store);
    let shard = root.path().join("data/sessions").join(id);
    for path in [root.path().join("global.db"), shard.join("session.db")] {
        // Close the last writer to checkpoint the fixture and remove its WAL.
        rusqlite::Connection::open(&path)?.execute_batch("BEGIN IMMEDIATE; COMMIT")?;
        assert!(!path.with_extension("db-wal").exists());
    }
    let names = || -> Result<_> { Ok((directory_names(root.path())?, directory_names(&shard)?)) };
    let before = names()?;
    assert_eq!(decode(&stdout(root.path(), id)?)?[0]["session_id"], id);
    assert_eq!(before, names()?);
    Ok(())
}
#[cfg(unix)]
#[test]
fn killed_cold_reader_leaves_no_private_snapshot_files() -> Result<()> {
    let root = tempfile::tempdir()?;
    let scratch = tempfile::tempdir()?;
    let id = "6217631e-49bd-41f9-b922-dbb42d16c3d1";
    let store = Stores::open(root.path())?;
    store
        .create(id, Default::default())?
        .write(|w| w.append("assistant", &"x".repeat(1_000_000)))?;
    drop(store);
    let shard = root
        .path()
        .join("data/sessions")
        .join(id)
        .join("session.db");
    rusqlite::Connection::open(&shard)?.execute_batch("BEGIN IMMEDIATE; COMMIT")?;
    assert!(!shard.with_extension("db-wal").exists());
    let mut child = export(root.path(), id)
        .env("TMPDIR", scratch.path())
        .env("SQLITE_TMPDIR", scratch.path())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut header = Vec::new();
    let mut pipe = BufReader::new(child.stdout.take().unwrap());
    let received = pipe.read_until(b'\n', &mut header);
    let killed = child.kill();
    let status = child.wait()?;
    killed?;
    assert!(received? > 0 && !status.success());
    assert_eq!(fs::read_dir(scratch.path())?.count(), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn interrupt_named_export_and_reclaim_abandoned_staging() -> Result<()> {
    use std::time::{Duration, Instant};
    let root = tempfile::tempdir()?;
    let store = Stores::open(root.path())?;
    let id = "6217631e-49bd-41f9-b922-dbb42d16c3d1";
    store.create(id, Default::default())?;
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGKILL] {
        let out = tempfile::tempdir()?;
        let path = out.path().join("backup.jsonl");
        let guard = fs::File::open(root.path().join("state/locks/session").join(id))?;
        guard.lock()?;
        let mut child = export(root.path(), id).arg("--output").arg(&path).spawn()?;
        let stage = out
            .path()
            .join(format!(".backup.jsonl.export-{}.tmp", child.id()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while fs::read_dir(out.path())?.count() == 0
            && Instant::now() < deadline
            && child.try_wait()?.is_none()
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let signalled = unsafe { libc::kill(child.id() as i32, signal) } == 0;
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait()?.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let exited = child.try_wait()?.is_some();
        drop(guard);
        let status = child.wait()?;
        assert!(
            signalled && exited && !status.success() && !path.exists(),
            "signal {signal}: {status}"
        );
        if signal == libc::SIGKILL {
            assert!(stage.is_file());
            let live = out
                .path()
                .join(format!(".backup.jsonl.export-{}.tmp", std::process::id()));
            fs::write(&live, b"live")?;
            let other = out.path().join(".other.export-10000000.tmp");
            fs::write(&other, b"other")?;
            assert!(export(root.path(), id)
                .arg("--output")
                .arg(&path)
                .status()?
                .success());
            decode(&fs::read(&path)?)?;
            assert!(!stage.exists() && live.exists() && other.exists());
        } else {
            assert_eq!(fs::read_dir(out.path())?.count(), 0);
        }
    }
    Ok(())
}
#[test]
fn oversized_timestamp_in_header() -> Result<()> {
    let id = "header";
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("root");
    let store = Stores::open(&root)?;
    let timestamp = format!("2099-01-01T00:00:01.{}Z", "0".repeat(70000));
    let source = dir.path().join("import.json");
    fs::write(
        &source,
        serde_json::to_vec(
            &json!({"id":id,"name":"import","created_at":"2025-01-01T00:00:01Z","updated_at":timestamp,"messages":[]}),
        )?,
    )?;
    store.import_json(&source, dir.path(), false)?;
    let bytes = stdout(&root, id)?;
    let first = bytes.split(|b| *b == b'\n').next().unwrap();
    let header: Value = serde_json::from_slice(first)?;
    assert_eq!(decode(&bytes)?[0]["updated_at"], timestamp);
    assert!(first.len() < 32 * 1024);
    assert_eq!(header["updated_at"]["chunks"], 3);
    Ok(())
}
