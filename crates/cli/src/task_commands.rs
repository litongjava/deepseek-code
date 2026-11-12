//! Explicit maintenance commands for the session task store.
//!
//! These commands never run automatically: nothing imports legacy plans on
//! startup, resume, listing or reading. They reuse the reviewed core importer
//! instead of duplicating its transaction and validation rules.
use crate::args::TasksCommands;
use crate::services;
use anyhow::{bail, Result};
use deepseek_code_core::runtime::RuntimePaths;
use deepseek_code_core::tools::plan_import::{self, ImportRequest, ImportSelector};

/// Exit convention for `dcl tasks import-plan`:
/// `0` created or unchanged (including `--dry-run`), `1` conflict or error,
/// `2` invalid arguments (clap).
pub fn run(paths: &RuntimePaths, action: &TasksCommands) -> Result<()> {
    match action {
        TasksCommands::ImportPlan {
            workspace,
            session,
            plan,
            current,
            dry_run,
        } => {
            // `--plan` and `--current` are a required, mutually exclusive group
            // in the parser; this guard keeps the invariant explicit if the
            // command is ever called from another entry point.
            let selector = match (plan.as_deref(), *current) {
                (Some(id), false) => ImportSelector::Plan(id.to_owned()),
                (None, true) => ImportSelector::Current,
                (Some(_), true) => bail!("Use either --plan <uuid> or --current, not both"),
                (None, false) => bail!("Choose exactly one of --plan <uuid> or --current"),
            };
            let report = plan_import::import(ImportRequest {
                data_dir: &paths.data_dir,
                workspace,
                session,
                selector,
                dry_run: *dry_run,
            })?;
            services::print(&report)?;
            if report.status == "conflict" {
                // A conflict is not success: the report says `conflict`, and the
                // process exits nonzero so automation cannot mistake it for an
                // applied import.
                bail!(
                    "Import conflict; nothing was written: {}",
                    report.conflicts.join("; ")
                );
            }
            Ok(())
        }
    }
}
