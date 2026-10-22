//! Session-local permission mode storage shared by CLI and its worker.
use crate::{
    db::DbManager,
    permissions::{PermissionManager, PermissionMode},
};
use anyhow::Result;
use rusqlite::OptionalExtension;
pub fn parse(value: &str) -> Result<PermissionMode> {
    PermissionMode::parse(value)
}
pub fn name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::AskPermissions => "ask",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::PlanMode => "plan",
        PermissionMode::BypassPermissions => "bypass",
        PermissionMode::DontAsk => "dont-ask",
        PermissionMode::Auto => "auto",
    }
}
pub fn load(db: &DbManager, session: &str) -> Result<Option<PermissionMode>> {
    let value: Option<String> = db.with_conn(|conn| {
        conn.query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("cli.permission-mode.{session}")],
            |row| row.get(0),
        )
        .optional()
    })??;
    value.map(|value| parse(&value)).transpose()
}
pub fn set(
    db: &DbManager,
    session: &str,
    manager: &PermissionManager,
    mode: PermissionMode,
) -> Result<()> {
    db.with_conn(|conn| -> Result<()> {
        let tx = crate::db::write_transaction(&conn)?;
        tx.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [format!("cli.permission-mode.{session}"), name(mode).into()])?;
        manager.commit_mode_update(mode, || { tx.commit()?; Ok(()) })
    })??;
    Ok(())
}
