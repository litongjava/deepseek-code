use deepseek_code_core::mcp::{McpServerConfig, McpServerManager, McpToolRegistry};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn resource_tools_read_protocol_contents_and_reject_stopped_servers() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("resources.cjs");
    std::fs::write(&script, r#"require('readline').createInterface({input:process.stdin}).on('line',line=>{
      const r=JSON.parse(line); const result=r.method==='tools/list'?{tools:[]}:
      r.method==='resources/list'?{resources:[{uri:'fixture://data',name:'data'}]}:
      r.method==='resources/read'?{contents:[{uri:r.params.uri,text:'actual resource text',mimeType:'text/plain'},{uri:r.params.uri,blob:'AQID',mimeType:'application/octet-stream'}]}:
      {protocolVersion:'2024-11-05',capabilities:{resources:{}},serverInfo:{name:'fixture',version:'1'}};
      console.log(JSON.stringify({jsonrpc:'2.0',id:r.id,result}));
    });"#).unwrap();
    let manager = McpServerManager::new(temp.path().join("mcp.json"));
    manager.initialize().await.unwrap();
    manager
        .add_server(McpServerConfig {
            id: "fixture".into(),
            name: "fixture".into(),
            command: "node".into(),
            args: vec![script.to_string_lossy().into_owned()],
            env: Default::default(),
            cwd: None,
            enabled: true,
            ..Default::default()
        })
        .await
        .unwrap();
    manager.start_server("fixture").await.unwrap();
    let manager = Arc::new(manager);
    let registry = McpToolRegistry::new(manager.clone());
    let resources = registry
        .execute_tool("ListMcpResources", json!({"server_id":"fixture"}))
        .await
        .unwrap();
    assert_eq!(resources[0]["uri"], "fixture://data");
    let result = registry
        .execute_tool(
            "ReadMcpResource",
            json!({"server_id":"fixture","uri":"fixture://data"}),
        )
        .await
        .unwrap();
    assert!(result["content"]
        .as_str()
        .unwrap()
        .contains("actual resource text"));
    assert_eq!(result["metadata"]["contents"][1]["blob"], "AQID");
    manager.stop_server("fixture").await.unwrap();
    assert!(registry
        .execute_tool(
            "ReadMcpResource",
            json!({"server_id":"fixture","uri":"fixture://data"})
        )
        .await
        .is_err());
}

#[tokio::test]
async fn configured_server_tools_are_namespaced_for_dispatch_and_server_rules() {
    use deepseek_code_core::permissions::{
        rules::PermissionAction, AuditLogger, PermissionContext, PermissionManager,
        PermissionMode, PermissionResult,
    };
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("tools.cjs");
    std::fs::write(&script, r#"require('readline').createInterface({input:process.stdin}).on('line',line=>{
      const r=JSON.parse(line); if(r.id===undefined)return;
      const result=r.method==='tools/list'?{tools:[{name:'echo.v2',description:'Echo',inputSchema:{type:'object'}}]}:
      r.method==='tools/call'?{content:[{type:'text',text:process.argv[2]+':'+r.params.name}]}:
      r.method==='resources/list'?{resources:[]}:
      {protocolVersion:'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'fixture',version:'1'}};
      console.log(JSON.stringify({jsonrpc:'2.0',id:r.id,result}));
    });"#).unwrap();
    let manager = McpServerManager::new(temp.path().join("mcp.json"));
    manager.initialize().await.unwrap();
    for id in ["alpha", "beta.srv"] {
        manager
            .add_server(McpServerConfig {
                id: id.into(),
                name: id.into(),
                command: "node".into(),
                args: vec![script.to_string_lossy().into_owned(), id.into()],
                env: Default::default(),
                cwd: None,
                enabled: true,
                ..Default::default()
            })
            .await
            .unwrap();
        manager.start_server(id).await.unwrap();
    }
    let manager = Arc::new(manager);
    let registry = McpToolRegistry::new(manager.clone());
    let mut names = registry
        .get_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .filter(|name| name.starts_with("mcp__"))
        .collect::<Vec<_>>();
    names.sort();
    // Identical raw tool names on two servers no longer collide.
    assert_eq!(names, ["mcp__alpha__echo_v2", "mcp__beta_srv__echo_v2"]);
    for (name, expected) in [
        ("mcp__alpha__echo_v2", "alpha:echo.v2"),
        ("mcp__beta_srv__echo_v2", "beta.srv:echo.v2"),
    ] {
        assert!(registry.is_mcp_tool(name).await);
        let value = registry.execute_tool(name, json!({})).await.unwrap();
        assert!(value.to_string().contains(expected), "{name}: {value}");
    }
    assert!(!registry.is_mcp_tool("echo.v2").await);
    assert!(registry.execute_tool("echo.v2", json!({})).await.is_err());

    let permissions = PermissionManager::new(Arc::new(AuditLogger::new(16)));
    permissions.set_mode(PermissionMode::BypassPermissions);
    permissions
        .set_launch_rules(
            "session",
            std::collections::HashMap::from([("mcp__alpha".into(), PermissionAction::Deny)]),
        )
        .unwrap();
    for (tool, denied) in [("mcp__alpha__echo_v2", true), ("mcp__beta_srv__echo_v2", false)] {
        let result = permissions.check_permission(&PermissionContext {
            tool_name: tool.into(),
            tool_input: json!({}),
            conversation_id: "session".into(),
            user_id: None,
            workspace_path: None,
        });
        assert_eq!(matches!(result, PermissionResult::Denied(_)), denied, "{tool}");
    }
    for id in ["alpha", "beta.srv"] {
        manager.stop_server(id).await.unwrap();
    }
}
