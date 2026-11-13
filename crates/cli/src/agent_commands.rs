//! Shared implementation of `dcl agents model` and `/agents model`.
use anyhow::{ensure, Result};
use deepseek_code_core::{runtime::RuntimePaths, services::agents};
use std::collections::BTreeSet;

/// Show or set agent types' default models. Returns the text to print: one
/// `TYPE  MODEL` line per known type (or `TYPE  (parent model)`), or the line
/// of the type that was changed. The model is not validated against the
/// provider here; it is checked when the agent starts.
pub fn model_command(
    paths: &RuntimePaths,
    agent: Option<&str>,
    model: Option<&str>,
) -> Result<String> {
    let definitions = agents::list(&crate::services::context(paths)?)?;
    let known = |id: &str| definitions.iter().any(|definition| definition.id == id);
    let line = |id: &str| match agents::default_model(&paths.data_dir, id) {
        Some(model) => format!("{id}  {model}"),
        None => format!("{id}  (parent model)"),
    };
    let canonical = |id: &str| {
        let normalized = id.to_lowercase();
        agents::canonical(&normalized).to_owned()
    };
    match (agent, model) {
        (None, _) => {
            let mut ids: BTreeSet<String> =
                definitions.iter().map(|definition| definition.id.clone()).collect();
            ids.insert("fork".into());
            Ok(ids.iter().map(|id| line(id)).collect::<Vec<_>>().join("\n"))
        }
        (Some(agent), None) => {
            let id = canonical(agent);
            ensure!(id == "fork" || known(&id), "Unknown agent type {agent}");
            Ok(line(&id))
        }
        (Some(agent), Some(value)) => {
            let id = canonical(agent);
            ensure!(id == "fork" || known(&id), "Unknown agent type {agent}");
            let model = (value != "default").then_some(value);
            agents::set_model(&paths.data_dir, &id, model)?;
            Ok(line(&id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_model_command_lists_sets_and_clears() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();

        let listing = model_command(&paths, None, None).unwrap();
        for id in ["explore", "plan", "general", "fork"] {
            assert!(
                listing
                    .lines()
                    .any(|line| line == format!("{id}  (parent model)")),
                "{id} is missing from {listing}"
            );
        }

        let set = model_command(&paths, Some("explore"), Some("cheap-model")).unwrap();
        assert_eq!(set, "explore  cheap-model");
        let shown = model_command(&paths, Some("explore"), None).unwrap();
        assert_eq!(shown, "explore  cheap-model");
        let listing = model_command(&paths, None, None).unwrap();
        assert!(
            listing.lines().any(|line| line == "explore  cheap-model"),
            "{listing}"
        );
        assert!(
            listing.lines().any(|line| line == "plan  (parent model)"),
            "other types keep the parent's model: {listing}"
        );

        let cleared = model_command(&paths, Some("explore"), Some("default")).unwrap();
        assert_eq!(cleared, "explore  (parent model)");
        assert!(agents::default_model(&paths.data_dir, "explore").is_none());

        let error = model_command(&paths, Some("nope"), None)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Unknown agent type nope");

        let error = model_command(&paths, Some("fork"), Some("cheap-model"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("A fork always uses this session's model"),
            "{error}"
        );

        let error = model_command(&paths, Some("nope"), Some("cheap-model"))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Unknown agent type nope");
    }
}
