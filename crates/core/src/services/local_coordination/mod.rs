//! Local coordination: private domain paths and opt-in user settings.
//!
//! This stage resolves the coordination root independently of the conversation
//! data directory. Reads never create state; only
//! [`CoordinationPaths::prepare_private`] creates the coordination subtree.

mod owner;
mod paths;
pub(crate) mod process_identity;
mod registry;
mod settings;
mod store;
mod types;
mod workspace;

#[allow(unused_imports)] // Queue state uses it from later rounds.
pub(crate) use paths::private_file;
pub use owner::{probe_owner, OwnerLease, OwnerProof};
pub use paths::{CoordinationPaths, COORDINATION_DIR_ENV};
pub use registry::{
    registry_snapshot, InstanceView, RegistrationInput, RegistryGuard, RegistrySnapshot,
    Responsiveness, SessionGroup,
};
pub use settings::{load_settings, CoordinationSettings};
pub use store::Store;
pub use types::{
    Activity, CoordResult, CoordinationError, Lifecycle, NativePath, OwnerRecord, ParticipantKind,
    ParticipantRole, Registration,
};
pub use workspace::{data_namespace, escaped_native, observe_workspace, WorkspaceIdentity};

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::path::{Path, PathBuf};

    fn entries(root: &Path) -> Result<Vec<PathBuf>> {
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut entries = std::fs::read_dir(root)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        Ok(entries)
    }

    #[test]
    fn coordination_disabled_creates_nothing() -> Result<()> {
        let temp = tempfile::tempdir()?;

        // Missing settings mean disabled (and `registry` stays enabled by
        // default), and the data directory is not created.
        let ghost_data = temp.path().join("ghost-data");
        assert_eq!(
            load_settings(&ghost_data)?,
            CoordinationSettings {
                enabled: false,
                registry: true
            }
        );
        assert!(!ghost_data.exists());

        // An explicit enabled:false is disabled as well.
        let data = temp.path().join("data");
        std::fs::create_dir_all(&data)?;
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&data).config_file("settings.json"),
            r#"{"localCoordination":{"enabled":false,"registry":true}}"#,
        )?;
        let settings = load_settings(&data)?;
        assert!(!settings.enabled);
        assert!(settings.registry);

        // Resolving and inspecting a nonexistent root creates no files.
        let before = entries(temp.path())?;
        let root = temp.path().join("never-created");
        let paths = CoordinationPaths::resolve(None, Some(root.as_os_str()), None, temp.path())?;
        assert_eq!(
            paths.root,
            temp.path().canonicalize()?.join("never-created")
        );
        assert!(!paths.root.exists());
        assert!(!paths.database().exists());
        assert_eq!(entries(temp.path())?, before);
        Ok(())
    }
}
