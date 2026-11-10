use deepseek_code_core::services::customization::{self, ToolLoading};
use std::path::Path;

/// Declare tools required by fixtures that exercise execution rather than discovery.
/// Keep production defaults and all unrelated customization unchanged.
pub fn enable(data: &Path, tools: &[&str]) {
    let mut configuration = customization::load(data).unwrap();
    for name in tools {
        configuration.tool_loading.insert(
            (*name).to_owned(),
            ToolLoading {
                enabled: true,
                on_demand: true,
            },
        );
    }
    customization::save(data, &configuration, &[]).unwrap();
}
