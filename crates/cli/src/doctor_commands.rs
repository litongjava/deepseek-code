//! Invoked explicitly; never called during discovery or startup.
use deepseek_code_core::{runtime::RuntimePaths, services::doctor};
use std::path::Path;

pub fn snapshot(workspace: &Path, paths: &RuntimePaths) -> String {
    let mut report = doctor::inspect(workspace, Some(paths)).render();
    let path = paths.data_dir.join("keybindings.json");
    if path.exists() {
        let valid = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.len() <= 65536)
            .and_then(|_| std::fs::read_to_string(&path).ok())
            .is_some_and(|text| crate::keybindings::Keybindings::parse(&text).is_ok());
        report.push_str(if valid {
            "CLI keybindings: ok — runtime parser accepted the application keybindings\n"
        } else {
            "CLI keybindings: error — unreadable/oversized file or unsupported context, key, action or reserved shortcut; values withheld\n"
        });
    }
    report
}
