//! Process-owned Desktop permission grants, independent of saved UI preferences.
use crate::permissions::{PermissionManager, PermissionMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchPermissions {
    pub allow_bypass: bool,
    pub start_in_bypass: bool,
}

impl Default for LaunchPermissions {
    fn default() -> Self {
        Self {
            allow_bypass: true,
            start_in_bypass: false,
        }
    }
}

impl LaunchPermissions {
    pub fn parse(args: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut permissions = Self::default();
        for arg in args {
            match arg.as_str() {
                "--allow-dangerously-skip-permissions" => permissions.allow_bypass = true,
                "--dangerously-skip-permissions" => {
                    permissions.allow_bypass = true;
                    permissions.start_in_bypass = true;
                }
                _ => anyhow::bail!("Unknown Desktop argument: {arg}"),
            }
        }
        Ok(permissions)
    }

    pub fn apply(self, manager: &PermissionManager) {
        // Ordinary bypass never grants Full Access or removes explicit deny rules.
        manager.set_plan_launch_permissions(self.allow_bypass, false);
        if self.start_in_bypass {
            manager.set_mode(PermissionMode::BypassPermissions);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_flags_grant_only_this_process_and_preserve_initial_mode() {
        for (args, allowed, mode) in [
            (vec![], true, PermissionMode::default()),
            (
                vec!["--allow-dangerously-skip-permissions"],
                true,
                PermissionMode::default(),
            ),
            (
                vec!["--dangerously-skip-permissions"],
                true,
                PermissionMode::BypassPermissions,
            ),
        ] {
            let manager = crate::runtime::default_permissions();
            LaunchPermissions::parse(args.into_iter().map(String::from))
                .unwrap()
                .apply(&manager);
            assert_eq!(manager.bypass_available(), allowed);
            assert_eq!(manager.get_mode(), mode);
            assert!(!manager.full_access_available());
        }
        assert!(!crate::runtime::default_permissions().bypass_available());
        for arg in [
            "--yolo",
            "--full-access",
            "--allow-dangerously-skip-permission",
        ] {
            assert!(LaunchPermissions::parse([arg.into()]).is_err());
        }
    }
}
