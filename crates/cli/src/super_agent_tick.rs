//! The host-owned Super Agent tick for the interactive CLI.
//!
//! The Router model only appends events and reports. This module runs the
//! deterministic orchestrator between turns, so tasks are prepared once,
//! Coder plan approval is requested from the configured policy, and the same
//! Coder is resumed only after that approval. Desktop runs the same core tick
//! from its own host loop.
use crate::session::Session;
use anyhow::Result;
use deepseek_code_core::super_agent::config::SuperAgentConfig;
use deepseek_code_core::super_agent::orchestrator::Orchestrator;

pub async fn tick(session: &Session) -> Result<()> {
    let role = deepseek_code_core::super_agent::role::load(&session.store.db, &session.id)?;
    if !role.is_router() {
        return Ok(());
    }
    let context = crate::services::context(&session.paths)?;
    let config = SuperAgentConfig::load(&context)?;
    let provider = session
        .engine
        .resolved_provider(&session.provider_id, &session.model)
        .await?;
    let orchestrator = Orchestrator::new(
        context,
        config,
        provider,
        session.permission_mode().to_owned(),
        None,
    );
    let report = orchestrator.tick_watched(&session.id).await?;
    if report.is_empty() {
        return Ok(());
    }
    for line in &report.launched {
        command_eprintln!("Super Agent: started {line}");
    }
    for approval in &report.approvals {
        command_eprintln!(
            "Super Agent: plan approval {approval} is waiting for you. Reply here or open /crp."
        );
    }
    for warning in &report.warnings {
        command_eprintln!("Super Agent: {warning}");
    }
    for blocked in &report.blocked {
        command_eprintln!("Super Agent: task blocked: {blocked}");
    }
    Ok(())
}
