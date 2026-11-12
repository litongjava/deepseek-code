//! Model-visible ToolScript: one bounded JavaScript source that orchestrates
//! real tool calls through the executor's normal preparation and completion
//! path. Execution is owned by a durable job worker; this module only owns the
//! catalogue entry and the name checks.

pub const NAME: &str = "ToolScript";

pub fn is_toolscript(name: &str) -> bool {
    name == NAME
}

pub(super) fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: NAME.into(),
        description: "Run a bounded JavaScript program that calls real tools with \
`await tools.call(name, args)`. Use it when one turn should sequence many tool calls, \
branch on their results, or fan out independent reads with `Promise.all` instead of \
issuing them one request at a time.\n\n- Each `tools.call` runs the same tool the model \
could call directly, with the same permissions, hooks and history records.\n- Calls may \
be sequential, conditional, or grouped with `Promise.all`; mutations still run one at a \
time.\n- `args` is exposed to the script as `args`.\n- Set `run_in_background: true` to \
return a durable task id immediately; use TaskGet for its paged output and TaskStop to \
cancel it.\n\nA call that runs longer than the bounded response wait returns its task id \
instead of cancelling. The script cannot call ToolScript, Agent, Workflow or a managed \
background Bash."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "JavaScript source. Top-level await and `return` are supported; `args` is the JSON value passed alongside."
                },
                "args": {
                    "type": "object",
                    "description": "JSON value exposed to the script as `args`."
                },
                "run_in_background": {
                    "type": "boolean",
                    "default": false,
                    "description": "Return a durable task id after admission instead of waiting for the script."
                }
            },
            "required": ["source"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_definition_is_discoverable_and_requires_a_source() {
        let definition = definition();
        assert_eq!(definition.name, NAME);
        assert_eq!(definition.input_schema["required"], serde_json::json!(["source"]));
        assert_eq!(
            definition.input_schema["properties"]["run_in_background"]["default"],
            serde_json::json!(false)
        );
        assert!(is_toolscript("ToolScript"));
        assert!(!is_toolscript("toolscript"));
    }
}
