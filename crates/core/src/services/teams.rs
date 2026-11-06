//! Persistent agent-team identity shared by hosts and the standalone CLI.
//!
//! Upstream stores `teams/<name>/config.json` under the config home with a
//! deterministic `name@team` agent ID per member. The leader is the session
//! that created the team; teammates are child sessions owned by this process.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const LEAD_NAME: &str = "team-lead";

/// Sessions currently executing in this process. Anything persisted as active
/// but absent here is stale after a crash or restart.
static LIVE: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn live() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    LIVE.lock().unwrap_or_else(|error| error.into_inner())
}

pub fn register_live(session: &str) {
    live()
        .get_or_insert_with(HashSet::new)
        .insert(session.into());
}

pub fn unregister_live(session: &str) {
    if let Some(set) = live().as_mut() {
        set.remove(session);
    }
}

pub fn is_live(session: &str) -> bool {
    live().as_ref().is_some_and(|set| set.contains(session))
}

/// Upstream also requires a `--agent-teams` flag and a remote killswitch. The
/// CLI flag sets this environment variable; there is no remote gate here.
pub fn enabled() -> bool {
    enabled_with(
        std::env::var("DEEPSEEK_CODE_EXPERIMENTAL_AGENT_TEAMS")
            .ok()
            .as_deref(),
    )
}

fn enabled_with(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn ensure_enabled() -> Result<()> {
    if enabled() {
        Ok(())
    } else {
        bail!("Agent teams are disabled; set DEEPSEEK_CODE_EXPERIMENTAL_AGENT_TEAMS=1 or pass --agent-teams")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TeamMember {
    pub agent_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub joined_at: i64,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub subscriptions: Vec<String>,
    /// False when idle, stopped or stale; absent/true while a turn is running.
    #[serde(default = "default_true")]
    pub is_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TeamFile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: i64,
    pub lead_agent_id: String,
    pub lead_session_id: String,
    #[serde(default)]
    pub members: Vec<TeamMember>,
}

pub fn sanitize_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    sanitized.trim_matches('-').to_string()
}

pub fn sanitize_agent_name(name: &str) -> String {
    name.replace('@', "-")
}

pub fn agent_id(name: &str, team: &str) -> String {
    format!("{}@{}", sanitize_agent_name(name), team)
}

pub fn team_dir(root: &Path, name: &str) -> PathBuf {
    root.join("teams").join(sanitize_name(name))
}

pub fn team_path(root: &Path, name: &str) -> PathBuf {
    team_dir(root, name).join("config.json")
}

fn global_lock(root: &Path) -> Result<std::fs::File> {
    let directory = root.join("teams");
    std::fs::create_dir_all(&directory)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".lock"))?;
    crate::fs::lock_file(&file)?;
    Ok(file)
}

fn lock(root: &Path, name: &str) -> Result<std::fs::File> {
    let directory = team_dir(root, name);
    std::fs::create_dir_all(&directory)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("config.lock"))?;
    crate::fs::lock_file(&file)?;
    Ok(file)
}

pub fn read_team(root: &Path, name: &str) -> Result<Option<TeamFile>> {
    match std::fs::read(team_path(root, name)) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("Invalid team file for {name}")
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_team(root: &Path, team: &TeamFile) -> Result<()> {
    let directory = team_dir(root, &team.name);
    std::fs::create_dir_all(&directory)?;
    let temp = directory.join(format!("config.json.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temp, serde_json::to_vec_pretty(team)?)?;
    std::fs::rename(&temp, team_path(root, &team.name))?;
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Marks persisted active members whose session no longer runs in this process
/// as stopped, so a restarted leader never reports live teammates.
fn recover(team: &mut TeamFile) -> bool {
    let mut changed = false;
    for member in &mut team.members {
        if member.name != LEAD_NAME
            && member.is_active
            && !member.session_id.as_deref().is_some_and(is_live)
        {
            member.is_active = false;
            member.status = Some("stale".into());
            changed = true;
        }
    }
    changed
}

pub fn read_recovered(root: &Path, name: &str) -> Result<Option<TeamFile>> {
    let _lock = lock(root, name)?;
    let Some(mut team) = read_team(root, name)? else {
        return Ok(None);
    };
    if recover(&mut team) {
        write_team(root, &team)?;
    }
    Ok(Some(team))
}

/// Scans for the team this session leads, recovering stale members first.
pub fn led_by(root: &Path, session: &str) -> Result<Option<TeamFile>> {
    let directory = root.join("teams");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(team) = read_recovered(root, &name)? else {
            continue;
        };
        if team.lead_session_id == session {
            return Ok(Some(team));
        }
    }
    Ok(None)
}

#[derive(Debug)]
pub struct CreateOutput {
    pub team: TeamFile,
    pub team_file_path: PathBuf,
}

pub fn create(
    root: &Path,
    session: &str,
    name: &str,
    description: Option<&str>,
    agent_type: Option<&str>,
    model: Option<&str>,
    cwd: &Path,
) -> Result<CreateOutput> {
    ensure_enabled()?;
    if name.trim().is_empty() {
        bail!("team_name is required for TeamCreate");
    }
    if sanitize_name(name).is_empty() {
        bail!("Team name must contain at least one letter or digit");
    }
    let _global = global_lock(root)?;
    if let Some(existing) = led_by(root, session)? {
        bail!(
            "Already leading team \"{}\". A leader can only manage one team at a time. Use TeamDelete to end the current team before creating a new one.",
            existing.name
        );
    }
    // Upstream generates a word slug on collision; a stable suffix keeps the
    // requested name recognizable.
    let mut final_name = sanitize_name(name);
    let mut attempt = 2;
    while team_path(root, &final_name).exists() {
        final_name = format!("{}-{attempt}", sanitize_name(name));
        attempt += 1;
        if attempt > 100 {
            bail!("Could not allocate a unique team name");
        }
    }
    let file = TeamFile {
        name: final_name.clone(),
        description: description.map(str::to_owned),
        created_at: now(),
        lead_agent_id: agent_id(LEAD_NAME, &final_name),
        lead_session_id: session.into(),
        members: vec![TeamMember {
            agent_id: agent_id(LEAD_NAME, &final_name),
            name: LEAD_NAME.into(),
            agent_type: Some(
                agent_type
                    .filter(|value| !value.is_empty())
                    .unwrap_or(LEAD_NAME)
                    .into(),
            ),
            model: model.map(str::to_owned),
            prompt: None,
            joined_at: now(),
            cwd: cwd.to_string_lossy().into_owned(),
            session_id: Some(session.into()),
            subscriptions: Vec::new(),
            is_active: false,
            status: Some("idle".into()),
            result: None,
        }],
    };
    let _lock = lock(root, &final_name)?;
    write_team(root, &file)?;
    Ok(CreateOutput {
        team_file_path: team_path(root, &final_name),
        team: file,
    })
}

#[derive(Debug)]
pub struct DeleteOutput {
    pub success: bool,
    pub message: String,
    pub team_name: Option<String>,
}

pub fn delete(root: &Path, session: &str) -> Result<DeleteOutput> {
    ensure_enabled()?;
    let _global = global_lock(root)?;
    let Some(mut team) = led_by(root, session)? else {
        return Ok(DeleteOutput {
            success: true,
            message: "No team name found, nothing to clean up".into(),
            team_name: None,
        });
    };
    let active: Vec<&str> = team
        .members
        .iter()
        .filter(|member| member.name != LEAD_NAME && member.is_active)
        .map(|member| member.name.as_str())
        .collect();
    if !active.is_empty() {
        return Ok(DeleteOutput {
            success: false,
            message: format!(
                "Cannot cleanup team with {} active member(s): {}. Use requestShutdown to gracefully terminate teammates first.",
                active.len(),
                active.join(", ")
            ),
            team_name: Some(team.name),
        });
    }
    // Persist the recovered state before deleting for crash forensics.
    let _lock = lock(root, &team.name)?;
    if recover(&mut team) {
        write_team(root, &team)?;
    }
    std::fs::remove_dir_all(team_dir(root, &team.name))?;
    Ok(DeleteOutput {
        success: true,
        message: format!(
            "Cleaned up directories and worktrees for team \"{}\"",
            team.name
        ),
        team_name: Some(team.name),
    })
}

pub struct MemberOutput {
    pub agent_id: String,
}

/// Registers a teammate before its first turn. The caller must own the team.
pub fn register_member(
    root: &Path,
    team_name: &str,
    session: &str,
    name: &str,
    agent_type: Option<&str>,
    model: Option<&str>,
    prompt: &str,
    cwd: &Path,
) -> Result<MemberOutput> {
    let _lock = lock(root, team_name)?;
    let mut team =
        read_team(root, team_name)?.with_context(|| format!("Team not found: {team_name}"))?;
    if team.lead_session_id != session {
        bail!("Only the team leader can spawn teammates");
    }
    if name.trim().is_empty() {
        bail!("Teammate name is required");
    }
    if name == LEAD_NAME {
        bail!("\"{LEAD_NAME}\" is reserved for the team leader");
    }
    let requested_id = agent_id(name, &team.name);
    if team
        .members
        .iter()
        .any(|member| member.name.eq_ignore_ascii_case(name) || member.agent_id == requested_id)
    {
        bail!("Teammate already exists: {name}");
    }
    let member = TeamMember {
        agent_id: agent_id(name, &team.name),
        name: name.into(),
        agent_type: agent_type.map(str::to_owned),
        model: model.map(str::to_owned),
        prompt: Some(prompt.into()),
        joined_at: now(),
        cwd: cwd.to_string_lossy().into_owned(),
        session_id: None,
        subscriptions: Vec::new(),
        is_active: true,
        status: Some("active".into()),
        result: None,
    };
    let agent = member.agent_id.clone();
    team.members.push(member);
    write_team(root, &team)?;
    Ok(MemberOutput { agent_id: agent })
}

/// Binds the child session ID once the executor exists.
pub fn bind_member_session(
    root: &Path,
    team_name: &str,
    agent: &str,
    session_id: &str,
) -> Result<()> {
    let _lock = lock(root, team_name)?;
    let mut team =
        read_team(root, team_name)?.with_context(|| format!("Team not found: {team_name}"))?;
    team.members
        .iter_mut()
        .find(|member| member.agent_id == agent)
        .with_context(|| format!("Teammate not found: {agent}"))?
        .session_id = Some(session_id.into());
    write_team(root, &team)?;
    Ok(())
}

pub fn finish_member(
    root: &Path,
    team_name: &str,
    agent: &str,
    status: &str,
    result: Option<&str>,
) -> Result<()> {
    let _lock = lock(root, team_name)?;
    let Some(mut team) = read_team(root, team_name)? else {
        return Ok(());
    };
    if let Some(member) = team
        .members
        .iter_mut()
        .find(|member| member.agent_id == agent)
    {
        member.is_active = false;
        member.status = Some(status.into());
        member.result = result.map(|value| {
            if value.len() > 4096 {
                format!("{}…", &value[..value.floor_char_boundary(4096)])
            } else {
                value.into()
            }
        });
        write_team(root, &team)?;
    }
    Ok(())
}

pub fn member(root: &Path, team_name: &str, name: &str) -> Result<Option<TeamMember>> {
    Ok(read_recovered(root, team_name)?.and_then(|team| {
        team.members
            .into_iter()
            .find(|member| member.name.eq_ignore_ascii_case(name))
    }))
}

/// Resolves the team a session belongs to, whether as leader or member.
pub fn team_for_session(root: &Path, session: &str) -> Result<Option<TeamFile>> {
    let directory = root.join("teams");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(team) = read_recovered(root, &name)? else {
            continue;
        };
        if team.lead_session_id == session
            || team
                .members
                .iter()
                .any(|member| member.session_id.as_deref() == Some(session))
        {
            return Ok(Some(team));
        }
    }
    Ok(None)
}

/// Adds `kind`, team and member status to `/list-agents` peers.
pub fn classify_peers(
    root: &Path,
    context: &crate::services::ServiceContext,
    peers: &mut [serde_json::Value],
) -> Result<()> {
    for peer in peers {
        let Some(id) = peer["id"].as_str().map(str::to_owned) else {
            continue;
        };
        if let Some(team) = team_for_session(root, &id)? {
            if let Some(member) = member_for_session(&team, &id) {
                peer["kind"] = serde_json::json!(if member.name == LEAD_NAME {
                    "team-lead"
                } else {
                    "teammate"
                });
                peer["team"] = serde_json::json!(team.name);
                peer["teammate_name"] = serde_json::json!(member.name);
                peer["member_status"] = serde_json::json!(member.status);
                peer["is_active"] = serde_json::json!(member.is_active);
                continue;
            }
        }
        if context
            .setting::<serde_json::Value>(&format!("delegation.{id}"))?
            .is_some()
        {
            peer["kind"] = serde_json::json!("subagent");
        } else {
            peer["kind"] = serde_json::json!("session");
        }
    }
    Ok(())
}

/// Returns `(team_name, teammate_name)` for a session that belongs to a team.
pub fn identity(root: &Path, session: &str) -> Result<Option<(String, String)>> {
    Ok(team_for_session(root, session)?.map(|team| {
        let name = member_for_session(&team, session)
            .map(|member| member.name.clone())
            .unwrap_or_else(|| LEAD_NAME.into());
        (team.name, name)
    }))
}

pub fn member_for_session<'a>(team: &'a TeamFile, session: &str) -> Option<&'a TeamMember> {
    team.members
        .iter()
        .find(|member| member.session_id.as_deref() == Some(session))
}

pub fn member_by_name<'a>(team: &'a TeamFile, name: &str) -> Option<&'a TeamMember> {
    team.members
        .iter()
        .find(|member| member.name.eq_ignore_ascii_case(name))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TeamRequest {
    pub id: String,
    pub kind: String,
    pub team: String,
    pub from: String,
    pub to: String,
    pub state: String,
    pub created_at: i64,
    pub deadline: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
}

fn requests_path(root: &Path, team: &str) -> PathBuf {
    team_dir(root, team).join("requests.json")
}

fn read_requests(root: &Path, team: &str) -> Result<Vec<TeamRequest>> {
    match std::fs::read(requests_path(root, team)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn write_requests(root: &Path, team: &str, requests: &[TeamRequest]) -> Result<()> {
    let directory = team_dir(root, team);
    std::fs::create_dir_all(&directory)?;
    let temp = directory.join(format!("requests.json.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temp, serde_json::to_vec_pretty(requests)?)?;
    std::fs::rename(&temp, requests_path(root, team))?;
    Ok(())
}

fn expire(requests: &mut [TeamRequest]) {
    let now = now();
    for request in requests {
        if request.state == "pending" && now > request.deadline {
            request.state = "timeout".into();
        }
    }
}

pub fn create_request(
    root: &Path,
    team: &str,
    kind: &str,
    from: &str,
    to: &str,
    timeout_ms: u64,
    reason: Option<&str>,
    plan: Option<&str>,
) -> Result<TeamRequest> {
    let _lock = lock(root, team)?;
    let mut requests = read_requests(root, team)?;
    expire(&mut requests);
    let created = now();
    let request = TeamRequest {
        id: uuid::Uuid::new_v4().to_string(),
        kind: kind.into(),
        team: team.into(),
        from: from.into(),
        to: to.into(),
        state: "pending".into(),
        created_at: created,
        deadline: created + timeout_ms as i64,
        reason: reason.map(str::to_owned),
        plan: plan.map(str::to_owned),
        feedback: None,
    };
    requests.push(request.clone());
    write_requests(root, team, &requests)?;
    Ok(request)
}

pub fn request(root: &Path, team: &str, id: &str) -> Result<Option<TeamRequest>> {
    let _lock = lock(root, team)?;
    let mut requests = read_requests(root, team)?;
    expire(&mut requests);
    write_requests(root, team, &requests)?;
    Ok(requests.into_iter().find(|request| request.id == id))
}

/// Only the addressed session may answer. Expired requests stay expired.
pub fn respond_request(
    root: &Path,
    team: &str,
    id: &str,
    responder: &str,
    approve: bool,
    feedback: Option<&str>,
) -> Result<TeamRequest> {
    let _lock = lock(root, team)?;
    let mut requests = read_requests(root, team)?;
    expire(&mut requests);
    let request = requests
        .iter_mut()
        .find(|request| request.id == id)
        .with_context(|| format!("Request not found: {id}"))?;
    if request.to != responder {
        bail!("Request {id} is addressed to another session");
    }
    match request.state.as_str() {
        "pending" => {}
        "timeout" => bail!("Request {id} timed out"),
        other => bail!("Request {id} was already {other}"),
    }
    request.state = if approve { "approved" } else { "rejected" }.into();
    request.feedback = feedback.map(str::to_owned);
    let result = request.clone();
    write_requests(root, team, &requests)?;
    if approve && result.kind == "shutdown" {
        // Update the member record under the same team lock.
        if let Some(mut file) = read_team(root, team)? {
            if let Some(member) = file
                .members
                .iter_mut()
                .find(|member| member.session_id.as_deref() == Some(responder))
            {
                member.is_active = false;
                member.status = Some("stopped".into());
                if let Some(reason) = &result.reason {
                    member.result = Some(reason.clone());
                }
                write_team(root, &file)?;
            }
        }
    }
    Ok(result)
}

pub enum PlanDecision {
    Approved { feedback: Option<String> },
    Rejected { feedback: Option<String> },
    Pending,
}

/// Consumes the latest plan decision addressed to a teammate.
pub fn plan_decision(root: &Path, team: &str, session: &str) -> Result<PlanDecision> {
    let _lock = lock(root, team)?;
    let mut requests = read_requests(root, team)?;
    expire(&mut requests);
    let latest = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request.kind == "plan_approval" && request.from == session)
        .max_by_key(|(_, request)| request.created_at)
        .map(|(index, request)| (index, request.clone()));
    let Some((index, request)) = latest else {
        return Ok(PlanDecision::Pending);
    };
    let decision = match request.state.as_str() {
        "approved" => PlanDecision::Approved {
            feedback: request.feedback.clone(),
        },
        "rejected" => PlanDecision::Rejected {
            feedback: request.feedback.clone(),
        },
        _ => return Ok(PlanDecision::Pending),
    };
    requests[index].state = "consumed".into();
    write_requests(root, team, &requests)?;
    Ok(decision)
}

/// Returns the latest unanswered plan approval request for a teammate.
pub fn pending_plan_request(root: &Path, team: &str, session: &str) -> Result<Option<TeamRequest>> {
    let _lock = lock(root, team)?;
    let mut requests = read_requests(root, team)?;
    expire(&mut requests);
    write_requests(root, team, &requests)?;
    Ok(requests
        .into_iter()
        .filter(|request| {
            request.kind == "plan_approval" && request.from == session && request.state == "pending"
        })
        .max_by_key(|request| request.created_at))
}

/// Marks a session stopped when a shutdown was approved or the child is gone.
pub fn stop_member(
    root: &Path,
    team: &str,
    session: &str,
    status: &str,
    note: Option<&str>,
) -> Result<()> {
    let _lock = lock(root, team)?;
    let Some(mut file) = read_team(root, team)? else {
        return Ok(());
    };
    if let Some(member) = file
        .members
        .iter_mut()
        .find(|member| member.session_id.as_deref() == Some(session))
    {
        member.is_active = false;
        member.status = Some(status.into());
        member.result = note.map(str::to_owned);
        write_team(root, &file)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
