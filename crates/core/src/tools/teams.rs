//! Team identity tools. `TeamSpawn` is executed by the engine because it needs
//! to build owned child executors; this module only exposes its contract.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn definitions() -> Vec<super::ToolDefinition> {
    if !crate::services::teams::enabled() {
        return Vec::new();
    }
    vec![
        super::ToolDefinition {
            summary: Some("Create a named team led by this session".into()),
            name: "TeamCreate".into(),
            description: "Create a named team for coordinating multiple agents. Only one team may be led per session; teammates join through TeamSpawn.".into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "team_name":{"type":"string","description":"Name for the new team to create."},
                    "description":{"type":"string","description":"Team description/purpose."},
                    "agent_type":{"type":"string","description":"Type/role of the team lead."}
                },
                "required":["team_name"],
                "additionalProperties":false
            }),
        },
        super::ToolDefinition {
            summary: Some("Disband the team led by this session".into()),
            name: "TeamDelete".into(),
            description: "Disband the team led by this session. Fails while teammates are active; ask them to shut down first.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        super::ToolDefinition {
            summary: Some("Run named teammates concurrently and wait for all of them".into()),
            name: "TeamSpawn".into(),
            description: "Run named teammates concurrently. Each member gets an independent child session with the leader's tool permissions. Waits for every member; cancellation stops them all. Only the team leader may spawn.".into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "team_name":{"type":"string"},
                    "members":{"type":"array","minItems":1,"maxItems":8,"items":{
                        "type":"object",
                        "properties":{
                            "name":{"type":"string","description":"Unique teammate name; messages route by this name."},
                            "prompt":{"type":"string"},
                            "agent_type":{"type":"string","default":"general"}
                        },
                        "required":["name","prompt"],
                        "additionalProperties":false
                    }}
                },
                "required":["team_name","members"],
                "additionalProperties":false
            }),
        },
    ]
}

pub fn execute(
    name: &str,
    input: &Value,
    root: &Path,
    session: &str,
    cwd: &Path,
    model: Option<&str>,
) -> Result<Value> {
    use crate::services::teams;
    match name {
        "TeamCreate" => {
            let team_name = input["team_name"]
                .as_str()
                .context("team_name is required for TeamCreate")?;
            let output = teams::create(
                root,
                session,
                team_name,
                input["description"].as_str(),
                input["agent_type"].as_str(),
                model,
                cwd,
            )?;
            Ok(json!({
                "team_name": output.team.name,
                "team_file_path": output.team_file_path.to_string_lossy(),
                "lead_agent_id": output.team.lead_agent_id
            }))
        }
        "TeamDelete" => {
            let output = teams::delete(root, session)?;
            let mut value = json!({"success":output.success,"message":output.message});
            if let Some(team_name) = output.team_name {
                value["team_name"] = json!(team_name);
            }
            Ok(value)
        }
        _ => anyhow::bail!("Unknown team tool: {name}"),
    }
}
