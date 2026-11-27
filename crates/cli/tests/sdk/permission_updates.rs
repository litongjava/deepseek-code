use super::*;

#[tokio::test]
async fn sdk_updated_permissions_persist_user_settings_and_apply_to_the_session() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        deepseek_code_core::data_layout::write_config_file(
            data.join("config/settings.json"),
            json!({"theme":"dark","permissions":{"allow":["Read"]}}).to_string(),
        )
        .unwrap();
        let server = Server::start(anthropic, Mode::Approval).await;
        let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
        client.send(user("Write the fixture file")).await;
        let request = client.frame("control_request").await;
        assert_eq!(request["request"]["subtype"], "can_use_tool");
        client
            .send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{
                "behavior":"allow",
                "updatedInput":{"file_path":"approval.txt","content":"PERSISTED"},
                "updatedPermissions":[
                    {"type":"addRules","destination":"userSettings","behavior":"allow","rules":[{"toolName":"Write"},{"toolName":"Read"}]},
                    {"type":"addDirectories","destination":"userSettings","directories":[root.path().to_string_lossy()]},
                    {"type":"setMode","destination":"userSettings","mode":"acceptEdits"}
                ]
            }}}))
            .await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        assert!(root.path().join("approval.txt").exists());

        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(data.join("config/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["permissions"]["allow"], json!(["Read", "Write"]));
        assert_eq!(value["permissions"]["defaultMode"], "acceptEdits");
        assert!(value["permissions"]["additionalDirectories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == &json!(root.path().to_string_lossy())));

        // The persisted allow applies to the same process: the next write runs
        // without another host approval prompt.
        std::fs::remove_file(root.path().join("approval.txt")).unwrap();
        client.send(user("Write again")).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        assert!(root.path().join("approval.txt").exists(), "{result}");
        assert!(client.finish().await.0.success());
    }
}
