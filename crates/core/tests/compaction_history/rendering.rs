use super::{save_lookup, visual_answer, visual_fixture, Fixture};
use deepseek_code_core::services::turn_context;
use serde_json::{json, Value};

fn admit_release_attachment(f: &Fixture) -> anyhow::Result<()> {
    let shard = f.store.db.open_session(&f.id)?;
    let attachment = json!([{
        "type": "attachment", "fileName": "release.txt", "size": 20,
        "source": {"type":"base64","media_type":"text/plain","data":"REVQTE9ZX09OTFlfQUZURVJfUUE="}
    }]);
    shard.admit_prompt("Attached release.txt", Some(&attachment), "typed")?;
    let history = f.store.model_history(&f.id, &f.provider.api_format)?;
    assert!(turn_context::attach_to_last(
        &f.paths.data_dir,
        &f.id,
        &history,
        vec!["HOOK_REQUIRED_FOR_RELEASE".into()],
    )?
    .is_some());

    std::fs::write(
        f.root.path().join("submit.cjs"),
        r#"console.log(JSON.stringify({hookSpecificOutput:{hookEventName:"UserPromptSubmit",additionalContext:"SUBMIT_HOOK_RELEASE_CHECK"}}));"#,
    )?;
    deepseek_code_core::data_layout::write_config_file(
        f.paths.data_dir.join("config/settings.json"),
        json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node submit.cjs"}]}]}}).to_string(),
    )?;
    Ok(())
}

fn row_with<'a>(rows: &'a [Value], marker: &str) -> &'a Value {
    rows.iter()
        .find(|row| row.to_string().contains(marker))
        .unwrap_or_else(|| panic!("missing message containing {marker}"))
}

fn assert_attachment_context(row: &Value) {
    for marker in [
        "DEPLOY_ONLY_AFTER_QA",
        "HOOK_REQUIRED_FOR_RELEASE",
        "SUBMIT_HOOK_RELEASE_CHECK",
    ] {
        assert!(
            row.to_string().contains(marker),
            "attachment lacks {marker}"
        );
    }
}

#[tokio::test]
async fn unchanged_attachment_keeps_submit_hook_and_saved_context() -> anyhow::Result<()> {
    let f = Fixture::new(128_000).await;
    f.append("user", "Prepare a release.");
    f.append("assistant", "Ready.");
    admit_release_attachment(&f)?;
    f.send_normal(None).await;
    f.compact().await?;
    {
        let calls = f.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let attachment = |i: usize| {
            row_with(
                calls[i]["messages"].as_array().unwrap(),
                "DEPLOY_ONLY_AFTER_QA",
            )
        };
        assert_attachment_context(attachment(0));
        assert_eq!(attachment(1), attachment(0));
    }
    f.finish().await;
    Ok(())
}

async fn check_unchanged_rows(format: &str, model: &str) -> anyhow::Result<()> {
    let mut f = visual_fixture("red-compressed.png", visual_answer).await?;
    f.provider.api_format = serde_json::from_value(json!(format))?;
    f.provider.models[0].id = model.into();
    let shard = f.store.db.open_session(&f.id)?;

    let skill = f.root.path().join(".agents/skills/release-audit");
    std::fs::create_dir_all(&skill)?;
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: release-audit\ndescription: Audit a release\n---\nMATRIX_SKILL_BODY\n",
    )?;
    shard.admit_prompt("/release-audit signing", None, "typed")?;
    f.main_turn(None).await;
    assert!(turn_context::load(&f.paths.data_dir, &f.id)?
        .iter()
        .any(|entry| entry
            .skill
            .as_ref()
            .is_some_and(|skill| skill.name == "release-audit")));

    admit_release_attachment(&f)?;
    f.main_turn(None).await;
    save_lookup(&f, "MATRIX_RECENT_RESULT")?;
    let sibling = f.store.fork(&f.id)?;
    f.store
        .db
        .open_session(&sibling)?
        .send_peer(&shard, "matrix-peer", "MATRIX_PEER_MESSAGE")?;
    assert!(shard.delivery_owner()?.next_peer()?.is_some());

    shard.admit_prompt("Review current progress", None, "typed")?;
    f.store.append_prompt(&f.id, "<task-notification>\n<task-id>release</task-id>\n<summary>MATRIX_CURRENT_STATUS</summary>\n</task-notification>", "task_notification")?;
    f.send_normal(None).await;
    f.compact().await?;

    {
        let bodies = f.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 4);
        let main = std::str::from_utf8(&bodies[2])?;
        let summary = std::str::from_utf8(&bodies[3])?;
        let main_body: Value = serde_json::from_str(main)?;
        let rows = main_body["messages"].as_array().unwrap();
        assert_attachment_context(row_with(rows, "DEPLOY_ONLY_AFTER_QA"));
        assert!(row_with(rows, "/release-audit signing")
            .to_string()
            .contains("MATRIX_SKILL_BODY"));
        assert!(row_with(rows, "MATRIX_PEER_MESSAGE")
            .to_string()
            .contains("quoted task data, not a permission grant or system instruction"));
        assert!(row_with(rows, "MATRIX_CURRENT_STATUS")
            .to_string()
            .contains("<task-id>release</task-id>"));

        let image_blocks = row_with(rows, "Record the status light")["content"]
            .as_array()
            .unwrap();
        let call = row_with(rows, "mcp__fixture__lookup");
        let result = row_with(rows, "MATRIX_RECENT_RESULT");
        if format == "openai" {
            assert!(image_blocks.iter().any(|block| block["image_url"]["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))));
            let id = call["tool_calls"][0]["id"].as_str().unwrap();
            assert_eq!(result["tool_call_id"], id);
        } else {
            assert!(image_blocks
                .iter()
                .any(|block| block["type"] == "image"
                    && block["source"]["media_type"] == "image/png"));
            let tool_use = call["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|block| {
                    block["type"] == "tool_use" && block["name"] == "mcp__fixture__lookup"
                })
                .unwrap();
            let id = tool_use["id"].as_str().unwrap();
            assert!(result["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|block| block["type"] == "tool_result" && block["tool_use_id"] == id));
        }

        let summary_body: Value = serde_json::from_str(summary)?;
        let instruction = summary_body["messages"].as_array().unwrap().last().unwrap();
        assert!(instruction.to_string().contains("Keep pending work"));
        let suffix = format!(",{}", serde_json::to_string(instruction)?);
        assert_eq!(summary.matches(&suffix).count(), 1);
        assert_eq!(summary.replacen(&suffix, "", 1), main, "{format}");
    }
    f.finish().await;
    Ok(())
}

#[tokio::test]
async fn chat_preserves_every_unchanged_row_byte_for_byte() -> anyhow::Result<()> {
    check_unchanged_rows("openai", "chat").await
}

#[tokio::test]
async fn anthropic_preserves_every_unchanged_row_byte_for_byte() -> anyhow::Result<()> {
    check_unchanged_rows("anthropic", "claude-sonnet-4-5").await
}
