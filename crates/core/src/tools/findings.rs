//! ReportFindings, as in the reference CLI: code-review findings as a typed
//! list the host renders, so the model does not also print them as text.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

pub fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        summary: Some("Record a code review findings as structured data".into()),
        name: "ReportFindings".into(),
        description: "Record a code review's findings as structured data. Call it once when the review instructions ask for findings through this tool, and give the verified findings most severe first; an empty array records that nothing survived verification. Each finding needs `file`, `summary` and `failure_scenario`, while `line` is optional. The result is a numbered list for the terminal, so do not repeat the findings in your reply. If the apply instructions ask for a follow-up report after fixes, set `outcome` on each finding.".into(),
        input_schema: json!({"type":"object","properties":{
            "level":{"description":"Review effort level to record with the report.","type":"string","enum":["low","medium","high","xhigh","max"]},
            "findings":{"description":"Verified findings, most severe first; at most 32, or empty when none survived.","maxItems":32,"type":"array","items":{"type":"object","properties":{
                "file":{"description":"Repository-relative path of the file containing the finding.","type":"string"},
                "line":{"description":"Line number the finding refers to.","type":"integer"},
                "summary":{"description":"One sentence naming the defect.","type":"string"},
                "short_summary":{"description":"Short label of at most 60 characters that states only the claim.","type":"string","maxLength":60},
                "failure_scenario":{"description":"Concrete input or state that produces the wrong result.","type":"string"},
                "category":{"description":"Short kebab-case kind of finding, up to 40 characters.","type":"string","maxLength":40},
                "verdict":{"description":"`CONFIRMED` or `PLAUSIBLE` when a verification pass ran.","type":"string","enum":["CONFIRMED","PLAUSIBLE"]},
                "outcome":{"description":"What happened after a fix: `fixed`, `skipped` or `no_change_needed`.","type":"string","enum":["fixed","skipped","no_change_needed"]}
            },"required":["file","summary","failure_scenario"],"additionalProperties":false}}
        },"required":["findings"],"additionalProperties":false}),
    }
}

/// Validates the findings and renders the list the user sees.
pub fn execute(input: &Value) -> Result<Value> {
    let findings = input["findings"]
        .as_array()
        .context("findings must be an array")?;
    ensure!(findings.len() <= 32, "At most 32 findings");
    let mut lines = Vec::new();
    for (index, finding) in findings.iter().enumerate() {
        let field = |key: &str| finding[key].as_str().filter(|text| !text.trim().is_empty());
        let file = field("file").context("Each finding needs a file")?;
        let summary = field("summary").context("Each finding needs a summary")?;
        let scenario =
            field("failure_scenario").context("Each finding needs a failure_scenario")?;
        let location = match finding["line"].as_i64() {
            Some(line) => format!("{file}:{line}"),
            None => file.to_owned(),
        };
        let tags: Vec<&str> = ["category", "verdict", "outcome"]
            .iter()
            .filter_map(|key| field(key))
            .collect();
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };
        lines.push(format!(
            "{}. {location}{tags}\n   {summary}\n   Failure: {scenario}",
            index + 1
        ));
    }
    let header = match findings.len() {
        0 => "No findings survived verification.".to_owned(),
        1 => "1 finding:".to_owned(),
        count => format!("{count} findings:"),
    };
    let content = if lines.is_empty() {
        header
    } else {
        format!("{header}\n{}", lines.join("\n"))
    };
    Ok(json!({"count":findings.len(),"level":input["level"],"content":content}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_render_in_order_and_require_core_fields() {
        let out = execute(&json!({"level":"high","findings":[
            {"file":"src/a.rs","line":12,"summary":"Off by one","failure_scenario":"len 0 panics","category":"correctness","verdict":"CONFIRMED"},
            {"file":"src/b.rs","summary":"Leak","failure_scenario":"handle never closed"}]})).unwrap();
        assert_eq!(out["count"], 2);
        assert_eq!(out["content"], "2 findings:\n1. src/a.rs:12 [correctness, CONFIRMED]\n   Off by one\n   Failure: len 0 panics\n2. src/b.rs\n   Leak\n   Failure: handle never closed");
        assert_eq!(
            execute(&json!({"findings":[]})).unwrap()["content"],
            "No findings survived verification."
        );
        assert!(execute(&json!({"findings":[{"file":"a","summary":"s"}]})).is_err());
    }
}
