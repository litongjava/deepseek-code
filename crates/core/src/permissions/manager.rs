use super::audit::{AuditAction, AuditEntry, AuditLoggerRef};
use super::rules::{PermissionAction, PermissionRule, PermissionRuleset, DANGEROUS_TOOLS};
use super::{
    DecisionOrigin, PermissionChecker, PermissionContext, PermissionDecision, PermissionResult,
    ToolPermission,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum PermissionMode {
    AskPermissions,
    AcceptEdits,
    PlanMode,
    BypassPermissions,
    DontAsk,
    Auto,
}

impl Default for PermissionMode {
    fn default() -> Self {
        PermissionMode::AcceptEdits
    }
}

impl PermissionMode {
    /// Shared strict parser for CLI, SDK, persisted settings and Desktop.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "default" | "ask_permissions" | "ask" | "ask_permission" => Self::AskPermissions,
            "acceptEdits" | "accept-edits" | "accept_edits" | "edits" => Self::AcceptEdits,
            "plan_mode" | "plan" => Self::PlanMode,
            "bypassPermissions" | "bypass_permissions" | "bypass" | "always_allow"
            | "alwaysallow" | "zh" => Self::BypassPermissions,
            "dontAsk" | "dont-ask" | "dont_ask" => Self::DontAsk,
            "auto" => Self::Auto,
            _ => anyhow::bail!("Unknown permission mode: {s}"),
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::AskPermissions => "ask_permissions",
            PermissionMode::AcceptEdits => "accept_edits",
            PermissionMode::PlanMode => "plan_mode",
            PermissionMode::BypassPermissions => "bypass_permissions",
            PermissionMode::DontAsk => "dont_ask",
            PermissionMode::Auto => "auto",
        }
    }
}

impl TryFrom<String> for PermissionMode {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value).map_err(|error| error.to_string())
    }
}
impl From<PermissionMode> for String {
    fn from(mode: PermissionMode) -> Self {
        mode.as_str().into()
    }
}

pub type AlwaysAllowCheckerFn = Box<dyn Fn(&str, &str) -> bool + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeSnapshot {
    pub mode: PermissionMode,
    revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicySnapshot {
    pub mode: ModeSnapshot,
    revision: u64,
}

pub struct PermissionManager {
    bypass_available: std::sync::atomic::AtomicBool,
    full_access_available: std::sync::atomic::AtomicBool,
    policy_revision: Mutex<u64>,
    bash_sessions: Mutex<std::collections::HashSet<String>>,
    edit_sessions: Mutex<std::collections::HashSet<String>>,
    auto_stats: Mutex<HashMap<String, super::auto_config::AutoStats>>,
    ruleset: Mutex<PermissionRuleset>,
    tool_permissions: Mutex<HashMap<String, ToolPermission>>,
    audit_logger: AuditLoggerRef,
    confirmations: Mutex<HashMap<String, bool>>,
    mode: Mutex<ModeSnapshot>,
    session_rules: Mutex<HashMap<String, HashMap<String, PermissionAction>>>,
    runtime_rules: Mutex<HashMap<String, Vec<HashMap<String, PermissionAction>>>>,
    launch_rules: Mutex<HashMap<String, HashMap<String, PermissionAction>>>,
    session_directories: Mutex<HashMap<String, Vec<PathBuf>>>,
    always_allow_checker: Mutex<Option<Arc<dyn Fn(&str, &str) -> bool + Send + Sync>>>,
}

impl PermissionManager {
    /// In-memory equality guard for resuming an owned executor; never persisted or logged.
    pub(crate) fn delegation_policy_snapshot(&self, session: &str) -> serde_json::Value {
        let _revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let callback = self
            .always_allow_checker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|callback| Arc::as_ptr(callback) as *const () as usize);
        let mode = self.mode_snapshot();
        serde_json::json!({
            "mode":mode.mode.as_str(),
            "mode_revision":mode.revision,
            "rules":*self.ruleset.lock().unwrap_or_else(|e|e.into_inner()),
            "tools":*self.tool_permissions.lock().unwrap_or_else(|e|e.into_inner()),
            "session":self.session_rules.lock().unwrap_or_else(|e|e.into_inner()).get(session),
            "runtime":self.runtime_rules.lock().unwrap_or_else(|e|e.into_inner()).get(session),
            "launch":self.launch_rules.lock().unwrap_or_else(|e|e.into_inner()).get(session),
            "callback_identity":callback
        })
    }

    pub fn launch_rules(&self, conversation: &str) -> HashMap<String, PermissionAction> {
        self.launch_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(conversation)
            .cloned()
            .unwrap_or_default()
    }

    /// Rules supplied by the launching host; SDK session patches cannot remove them.
    pub fn set_launch_rules(
        &self,
        conversation: &str,
        rules: HashMap<String, PermissionAction>,
    ) -> anyhow::Result<()> {
        for pattern in rules.keys() {
            super::patterns::validate(pattern)?;
        }
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut current = self.launch_rules.lock().unwrap_or_else(|e| e.into_inner());
        if current
            .get(conversation)
            .is_some_and(|value| *value == rules)
            || (rules.is_empty() && !current.contains_key(conversation))
        {
            return Ok(());
        }
        if rules.is_empty() {
            current.remove(conversation);
        } else {
            current.insert(conversation.into(), rules);
        }
        *revision = revision.wrapping_add(1);
        Ok(())
    }

    /// Ephemeral host rules, independent of persisted session configuration.
    pub fn set_runtime_rules(
        &self,
        conversation: &str,
        rules: Vec<HashMap<String, PermissionAction>>,
    ) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        self.runtime_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conversation.to_owned(), rules);
    }
    /// Validate or atomically publish a session patch against the latest runtime rules.
    pub(crate) fn update_runtime_rules(
        &self,
        conversation: &str,
        publish: bool,
        update: impl FnOnce(&mut Vec<HashMap<String, PermissionAction>>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut runtime = self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = runtime
            .get(conversation)
            .cloned()
            .unwrap_or_else(|| vec![HashMap::new(); 3]);
        next.resize_with(3, HashMap::new);
        update(&mut next)?;
        if publish {
            runtime.insert(conversation.into(), next);
            *revision = revision.wrapping_add(1);
        }
        Ok(())
    }
    pub fn clear_runtime_rules(&self, conversation: &str) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        self.runtime_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(conversation);
    }
    pub fn set_session_rules(&self, conversation: &str, rules: HashMap<String, PermissionAction>) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        self.session_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conversation.to_owned(), rules);
    }
    /// Adds one allow rule to a session's rules; deny and ask rules still win.
    pub(crate) fn allow_in_session(&self, conversation: &str, rule: &str) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        self.session_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(conversation.to_owned())
            .or_default()
            .insert(rule.to_owned(), PermissionAction::Allow);
    }
    pub fn bash_session_allowed(&self, session: &str) -> bool {
        self.bash_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session)
    }
    pub fn edit_session_allowed(&self, session: &str) -> bool {
        self.edit_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session)
    }
    pub fn set_edit_session_allowed(&self, session: &str, allowed: bool) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut sessions = self.edit_sessions.lock().unwrap_or_else(|e| e.into_inner());
        let changed = if allowed {
            sessions.insert(session.into())
        } else {
            sessions.remove(session)
        };
        if changed {
            *revision = revision.wrapping_add(1);
        }
    }
    pub fn set_bash_session_allowed(&self, session: &str, allowed: bool) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut sessions = self.bash_sessions.lock().unwrap_or_else(|e| e.into_inner());
        let changed = if allowed {
            sessions.insert(session.into())
        } else {
            sessions.remove(session)
        };
        if changed {
            *revision = revision.wrapping_add(1);
        }
    }
    pub fn session_directories(&self, conversation: &str) -> Vec<PathBuf> {
        self.session_directories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(conversation)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_session_directories(&self, conversation: &str, directories: Vec<PathBuf>) {
        let mut current = self
            .session_directories
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if directories.is_empty() {
            current.remove(conversation);
        } else {
            current.insert(conversation.to_owned(), directories);
        }
    }

    #[cfg(test)]
    pub(crate) fn inherit_session_rules(&self, parent: &str, child: &str) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut sessions = self.session_rules.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rules) = sessions.get(parent).cloned() {
            sessions.insert(child.to_owned(), rules);
        }
        let mut runtime = self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rules) = runtime.get(parent).cloned() {
            runtime.insert(child.to_owned(), rules);
        }
        let mut launch = self.launch_rules.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rules) = launch.get(parent).cloned() {
            launch.insert(child.to_owned(), rules);
        }
        let mut directories = self
            .session_directories
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entries) = directories.get(parent).cloned() {
            directories.insert(child.to_owned(), entries);
        }
    }

    /// Independent mutable authority for a delegated session; read-only policy
    /// callback and audit sink may be shared, but mode/approvals/rules may not.
    pub(crate) fn fork_session(&self, parent: &str, child: &str) -> Self {
        let _revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let rules = self
            .session_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(parent)
            .cloned()
            .unwrap_or_default();
        Self {
            bypass_available: std::sync::atomic::AtomicBool::new(false),
            full_access_available: std::sync::atomic::AtomicBool::new(false),
            policy_revision: Mutex::new(0),
            bash_sessions: Mutex::new(Default::default()),
            edit_sessions: Mutex::new(Default::default()),
            auto_stats: Mutex::new(HashMap::new()),
            ruleset: Mutex::new(
                self.ruleset
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            tool_permissions: Mutex::new(
                self.tool_permissions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            audit_logger: self.audit_logger.clone(),
            confirmations: Mutex::new(HashMap::new()),
            mode: Mutex::new(ModeSnapshot {
                mode: self.get_mode(),
                revision: 0,
            }),
            session_rules: Mutex::new(HashMap::from([(child.to_owned(), rules)])),
            runtime_rules: Mutex::new(HashMap::from([(
                child.to_owned(),
                self.runtime_rules
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(parent)
                    .cloned()
                    .unwrap_or_default(),
            )])),
            launch_rules: Mutex::new(HashMap::from([(
                child.to_owned(),
                self.launch_rules
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(parent)
                    .cloned()
                    .unwrap_or_default(),
            )])),
            session_directories: Mutex::new(HashMap::from([(
                child.to_owned(),
                self.session_directories
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(parent)
                    .cloned()
                    .unwrap_or_default(),
            )])),
            always_allow_checker: Mutex::new(
                self.always_allow_checker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
        }
    }

    pub fn new(audit_logger: AuditLoggerRef) -> Self {
        Self {
            bypass_available: std::sync::atomic::AtomicBool::new(false),
            full_access_available: std::sync::atomic::AtomicBool::new(false),
            policy_revision: Mutex::new(0),
            bash_sessions: Mutex::new(Default::default()),
            edit_sessions: Mutex::new(Default::default()),
            auto_stats: Mutex::new(HashMap::new()),
            ruleset: Mutex::new(PermissionRuleset::new()),
            tool_permissions: Mutex::new(HashMap::new()),
            audit_logger,
            confirmations: Mutex::new(HashMap::new()),
            mode: Mutex::new(ModeSnapshot {
                mode: PermissionMode::default(),
                revision: 0,
            }),
            session_rules: Mutex::new(HashMap::new()),
            runtime_rules: Mutex::new(HashMap::new()),
            launch_rules: Mutex::new(HashMap::new()),
            session_directories: Mutex::new(HashMap::new()),
            always_allow_checker: Mutex::new(None),
        }
    }

    pub fn set_always_allow_checker(&self, checker: AlwaysAllowCheckerFn) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut guard = self
            .always_allow_checker
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(Arc::from(checker));
        tracing::info!(module = "Permission", "Always allow checker registered");
    }

    /// Launch authority is host-owned and never inferred from tool arguments.
    pub fn set_plan_launch_permissions(&self, available: bool, full_access: bool) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.bypass_available
            .store(available, std::sync::atomic::Ordering::Relaxed);
        self.full_access_available
            .store(full_access, std::sync::atomic::Ordering::Relaxed);
        *revision = revision.wrapping_add(1);
    }

    pub fn full_access_available(&self) -> bool {
        self.full_access_available
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn bypass_available(&self) -> bool {
        self.bypass_available
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        let mut policy_revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if m.mode != mode {
            m.mode = mode;
            m.revision = m.revision.wrapping_add(1);
            *policy_revision = policy_revision.wrapping_add(1);
        }
        tracing::info!(module = "Permission", "Mode changed to: {}", mode.as_str());
    }

    pub fn get_mode(&self) -> PermissionMode {
        self.mode.lock().unwrap_or_else(|e| e.into_inner()).mode
    }

    pub fn auto_stats(&self, session: &str) -> super::auto_config::AutoStats {
        self.auto_stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session)
            .cloned()
            .unwrap_or_default()
    }
    pub fn update_auto_stats(
        &self,
        session: &str,
        update: impl FnOnce(&mut super::auto_config::AutoStats),
    ) -> super::auto_config::AutoStats {
        let mut sessions = self.auto_stats.lock().unwrap_or_else(|e| e.into_inner());
        let stats = sessions.entry(session.into()).or_default();
        update(stats);
        stats.clone()
    }
    pub fn record_tool_success(&self, session: &str) {
        if self.get_mode() == PermissionMode::Auto {
            self.update_auto_stats(session, |stats| stats.denied_consecutive = 0);
        }
    }

    pub fn policy_snapshot(&self) -> PolicySnapshot {
        let revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        PolicySnapshot {
            mode: self.mode_snapshot(),
            revision: *revision,
        }
    }

    /// Do not hold policy locks while calling saved-approval callbacks: they can
    /// read SQLite. Retry a concurrently changing snapshot, then fail closed.
    pub fn evaluate_stable(
        &self,
        context: &PermissionContext,
    ) -> Option<(PermissionDecision, PolicySnapshot)> {
        for _ in 0..3 {
            let before = self.policy_snapshot();
            let decision = self.evaluate_permission(context);
            if self.policy_snapshot() == before {
                return Some((decision, before));
            }
        }
        None
    }

    pub fn mode_snapshot(&self) -> ModeSnapshot {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Caller must acquire any DB lock BEFORE entering this method. The closure
    /// may commit an existing transaction but must not acquire DB/permission locks.
    /// Failed commits leave mode unchanged; intervening mode changes reject approval.
    pub fn commit_mode_transition<T>(
        &self,
        expected: ModeSnapshot,
        next: PermissionMode,
        commit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut policy_revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut current = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            *current == expected,
            "Permission mode changed during review; request approval again"
        );
        let result = commit()?;
        current.mode = next;
        current.revision = current.revision.wrapping_add(1);
        *policy_revision = policy_revision.wrapping_add(1);
        Ok(result)
    }
    /// Explicit host selection; same DB-before-mode lock order as plan approval.
    pub fn commit_mode_update<T>(
        &self,
        next: PermissionMode,
        commit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut policy_revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut current = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        let result = commit()?;
        if current.mode != next {
            current.mode = next;
            current.revision = current.revision.wrapping_add(1);
            *policy_revision = policy_revision.wrapping_add(1);
        }
        Ok(result)
    }

    pub fn add_rule(&self, rule: PermissionRule) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.add_rule(rule);
    }

    pub fn remove_rule(&self, rule_id: &str) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.rules.retain(|r| r.id != rule_id);
    }

    pub fn get_rules(&self) -> Vec<PermissionRule> {
        let ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.rules.clone()
    }

    pub fn set_tool_permission(&self, tool_name: &str, permission: ToolPermission) {
        let mut revision = self
            .policy_revision
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut permissions = self
            .tool_permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        permissions.insert(tool_name.to_string(), permission);
    }

    pub fn get_tool_permission(&self, tool_name: &str) -> Option<ToolPermission> {
        let permissions = self
            .tool_permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        permissions.get(tool_name).cloned()
    }

    fn broad_auto_grant(pattern: &str) -> bool {
        let name = pattern.split('(').next().unwrap_or(pattern);
        matches!(name, "Bash" | "PowerShell" | "Agent" | "Task" | "team")
            && (!pattern.contains('(') || pattern.contains('*'))
    }

    fn is_read_only_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "Read"
                | "ListDir"
                | "Grep"
                | "Glob"
                | "WebFetch"
                | "WebSearch"
                | "memory_search"
                | "TodoRead"
                | "Skill"
                | "SkillRead"
                | "TaskGet"
                | "TaskList"
                | "TaskOutput"
                | "ToolSearch"
                | "ProposeGoal"
                | "CompactContext"
                | "EnterPlanMode"
                | "ExitPlanMode"
                | "brief"
                | "SyntheticOutput"
                | "ListAgents"
                | "CronList"
                | "ReportFindings"
        )
    }

    /// Tools that write only this app's own data (memory store, todo list,
    /// task list), never project files. Like the reference CLI, which does not
    /// prompt for its todo and task tools or for writes to its memory
    /// directory, they run without asking.
    fn is_own_state_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "memory_ingest" | "TodoWrite" | "TaskCreate" | "TaskUpdate"
        )
    }

    pub fn is_file_edit_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "ApplyPatch"
        )
    }

    fn is_edit_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "Write"
                | "Edit"
                | "MultiEdit"
                | "ApplyPatch"
                | "SearchReplace"
                | "GlobReplace"
                | "Delete"
                | "memory_ingest"
                | "SkillCreate"
                | "SkillUpdate"
                | "NotebookEdit"
                | "TaskCreate"
                | "TaskUpdate"
                | "TaskStop"
                | "TodoWrite"
                | "SendMessage"
                | "team"
                | "cron"
                | "CronCreate"
                | "CronDelete"
                | "ScheduleWakeup"
                | "Config"
        )
    }

    pub fn check_permission(&self, context: &PermissionContext) -> PermissionResult {
        self.check_permission_inner(context, false)
    }

    /// Called only after the engine verifies the trusted session plan path.
    pub(crate) fn check_plan_document_permission(
        &self,
        context: &PermissionContext,
    ) -> PermissionResult {
        self.check_permission_inner(context, true)
    }

    fn check_permission_inner(
        &self,
        context: &PermissionContext,
        plan_document: bool,
    ) -> PermissionResult {
        self.evaluate_permission_inner(context, plan_document)
            .result
    }

    pub fn evaluate_permission(&self, context: &PermissionContext) -> PermissionDecision {
        self.evaluate_permission_inner(context, false)
    }

    fn evaluate_permission_inner(
        &self,
        context: &PermissionContext,
        plan_document: bool,
    ) -> PermissionDecision {
        let mode = self.get_mode();
        let decision = self
            .static_decision(context, mode, plan_document)
            .in_mode(mode);
        self.audit_logger.log(
            AuditEntry::new(
                match &decision.result {
                    PermissionResult::Granted => AuditAction::PermissionGranted,
                    PermissionResult::Denied(_) => AuditAction::PermissionDenied,
                    PermissionResult::RequiresConfirmation(_) => AuditAction::PermissionChecked,
                },
                &context.tool_name,
                &context.conversation_id,
            )
            .with_user_id(context.user_id.as_deref())
            .with_result(&format!("{:?}", decision.origin)),
        );
        decision
    }

    fn static_decision(
        &self,
        context: &PermissionContext,
        mode: PermissionMode,
        plan_document: bool,
    ) -> PermissionDecision {
        use DecisionOrigin::*;
        let decision = PermissionDecision::new;
        let local_fetch = context.tool_name == "WebFetch"
            && super::web_fetch::requires_local_approval(
                context.tool_input["url"].as_str().unwrap_or_default(),
            );
        // Full Access grants everything bypass mode would, deny rules included.
        if mode == PermissionMode::BypassPermissions && super::full_access() && !local_fetch {
            return decision(PermissionResult::Granted, Mode);
        }
        let auto = mode == PermissionMode::Auto;
        let action = |rules: &HashMap<String, PermissionAction>| {
            if !auto {
                return super::patterns::matching_action(rules, context);
            }
            let effective = rules
                .iter()
                .filter(|(pattern, action)| {
                    **action != PermissionAction::Allow || !Self::broad_auto_grant(pattern)
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            super::patterns::matching_action(&effective, context)
        };
        let explicit = self
            .session_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&context.conversation_id)
            .and_then(action);
        let runtime = self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner());
        let mut actions: Vec<_> = runtime
            .get(&context.conversation_id)
            .into_iter()
            .flatten()
            .filter_map(action)
            .chain(explicit)
            .collect();
        drop(runtime);
        actions.extend(
            self.launch_rules
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&context.conversation_id)
                .and_then(action),
        );
        // This ruleset contains explicit overrides only. Defaults are evaluated
        // below by operation, not as an unconditional global Confirm rule.
        for rule in self
            .ruleset
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_rules_for_tool(&context.tool_name)
        {
            // Unknown conditions cannot authorize an operation. Deny/ask remain
            // conservative rather than being bypassed by a permissive mode.
            if auto
                && rule.action == PermissionAction::Allow
                && (rule.tool_name.is_none()
                    || Self::broad_auto_grant(rule.tool_name.as_deref().unwrap_or("")))
            {
                continue;
            }
            if rule.action != PermissionAction::Allow
                || (rule.conditions.is_none()
                    && matches!(
                        rule.scope,
                        super::rules::PermissionScope::Global
                            | super::rules::PermissionScope::ToolSpecific
                    ))
            {
                actions.push(rule.action);
            }
        }
        if actions.contains(&PermissionAction::Deny) {
            return decision(
                PermissionResult::Denied("Explicit tool deny rule".into()),
                ExplicitRule,
            );
        }
        let setting = self.get_tool_permission(&context.tool_name);
        if setting.as_ref().is_some_and(|p| !p.allowed) {
            return decision(
                PermissionResult::Denied("Tool is explicitly disabled".into()),
                ToolSetting,
            );
        }
        if local_fetch {
            return decision(self.request_confirmation(context), NetworkBoundary);
        }
        if mode == PermissionMode::BypassPermissions {
            return decision(PermissionResult::Granted, Mode);
        }
        // Delegation inherits the parent authority; children check their own tools.
        // Auto still classifies the spawn itself, matching the native Agent tool.
        let delegation = context.tool_name == "Agent"
            || (context.tool_name == "team" && context.tool_input["operation"] == "run");
        let read_only = Self::is_read_only_tool(&context.tool_name)
            || (context.tool_name == "Config"
                && matches!(
                    context.tool_input["operation"].as_str(),
                    Some("get" | "list" | "merge" | "sources")
                ))
            // One tool, two authorities: listing and viewing GitHub data is a
            // read, while creating, commenting and merging are mutations. The
            // tool name alone cannot decide that.
            || (context.tool_name == "GitHub"
                && crate::tools::github::is_read_operation(
                    context.tool_input["operation"].as_str(),
                ));
        // Plan mode permits shell commands except those rejected by the shared
        // danger filter. Explicit ask/deny settings still take precedence.
        let shell = matches!(context.tool_name.as_str(), "Bash" | "PowerShell");
        if mode == PermissionMode::PlanMode && shell && context.tool_input["interaction"] == "pty" {
            return decision(
                PermissionResult::Denied(
                    "Plan mode: starting an interactive PTY is not allowed".into(),
                ),
                PlanBoundary,
            );
        }
        if mode == PermissionMode::PlanMode && shell && !plan_document {
            let command = context.tool_input["command"].as_str().unwrap_or_default();
            if crate::tools::is_dangerous_command(command) {
                return decision(
                    PermissionResult::Denied(
                        "Plan mode: dangerous shell command is not allowed".into(),
                    ),
                    PlanBoundary,
                );
            }
            if !actions.contains(&PermissionAction::Confirm)
                && !setting.as_ref().is_some_and(|p| p.requires_confirmation)
            {
                return decision(PermissionResult::Granted, PlanBoundary);
            }
            return decision(self.request_confirmation(context), PlanBoundary);
        }
        if mode == PermissionMode::PlanMode
            && !read_only
            && !context.mcp_read_only
            && !plan_document
            && !delegation
            && !(context.tool_name == "SendMessage"
                && matches!(
                    context.tool_input["operation"].as_str(),
                    None | Some(
                        "send" | "receive" | "peers" | "sent" | "pending_count" | "mark_read"
                    )
                ))
        {
            return decision(
                PermissionResult::Denied(if context.tool_name.starts_with("mcp__") {
                    "Plan mode: MCP tool has a false or invalid annotations.readOnlyHint".into()
                } else {
                    "Plan mode: write operations are not allowed".into()
                }),
                PlanBoundary,
            );
        }
        if actions.contains(&PermissionAction::Confirm) {
            return decision(self.request_confirmation(context), ExplicitRule);
        }
        if setting.as_ref().is_some_and(|p| p.requires_confirmation) {
            return decision(self.request_confirmation(context), ToolSetting);
        }
        if shell
            && context.tool_input["command"]
                .as_str()
                .is_some_and(crate::tools::is_dangerous_command)
        {
            return decision(self.request_confirmation(context), Mode);
        }
        if context.tool_name == "Bash"
            && self.bash_session_allowed(&context.conversation_id)
            && context.tool_input["command"].as_str().is_some()
        {
            return decision(PermissionResult::Granted, SavedApproval);
        }
        if auto && shell && context.tool_input["command"].as_str().is_some() {
            return decision(PermissionResult::Granted, Mode);
        }
        if actions.contains(&PermissionAction::Allow) {
            return decision(PermissionResult::Granted, ExplicitRule);
        }
        // As in the reference CLI, WebFetch asks once per host outside its
        // documentation list; "don't ask again" saves a WebFetch(domain:) rule.
        if context.tool_name == "WebFetch"
            && !matches!(
                mode,
                PermissionMode::BypassPermissions | PermissionMode::PlanMode
            )
            && !setting.as_ref().is_some_and(|p| p.allowed)
            && !super::web_fetch::preapproved(
                context.tool_input["url"].as_str().unwrap_or_default(),
            )
        {
            return decision(self.request_confirmation(context), DefaultPrompt);
        }
        if mode == PermissionMode::PlanMode {
            return decision(PermissionResult::Granted, PlanBoundary);
        }
        if setting.as_ref().is_some_and(|p| p.allowed)
            && !(auto && Self::broad_auto_grant(&context.tool_name))
        {
            return decision(PermissionResult::Granted, ToolSetting);
        }
        let checker = self
            .always_allow_checker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !(auto && Self::broad_auto_grant(&context.tool_name))
            && checker.is_some_and(|checker| {
                checker(
                    &context.tool_name,
                    &self.extract_action_from_context(context),
                )
            })
        {
            return decision(PermissionResult::Granted, SavedApproval);
        }
        if mode != PermissionMode::BypassPermissions && Self::is_edit_tool(&context.tool_name) {
            if let Some(path) = context.tool_input["file_path"]
                .as_str()
                .or_else(|| context.tool_input["notebook_path"].as_str())
                .or_else(|| context.tool_input["path"].as_str())
            {
                let cwd = std::path::Path::new(context.workspace_path.as_deref().unwrap_or("."));
                if super::protected_paths::requires_edit_approval(
                    path,
                    cwd,
                    dirs::home_dir().as_deref(),
                ) {
                    return decision(self.request_confirmation(context), DefaultPrompt);
                }
            }
        }
        if Self::is_file_edit_tool(&context.tool_name)
            && (auto || self.edit_session_allowed(&context.conversation_id))
        {
            return decision(PermissionResult::Granted, Mode);
        }
        if Self::is_own_state_tool(&context.tool_name) {
            return decision(PermissionResult::Granted, DefaultOwnState);
        }
        if mode == PermissionMode::BypassPermissions
            || (mode == PermissionMode::AcceptEdits && Self::is_edit_tool(&context.tool_name))
        {
            return decision(PermissionResult::Granted, Mode);
        }
        if delegation && !auto {
            return decision(PermissionResult::Granted, DefaultDelegation);
        }
        if read_only {
            return decision(PermissionResult::Granted, DefaultRead);
        }
        // As in the reference CLI's default mode, shell commands that only read
        // inside the workspace run without asking.
        if shell
            && !auto
            && matches!(
                mode,
                PermissionMode::AskPermissions | PermissionMode::AcceptEdits
            )
        {
            let command = context.tool_input["command"].as_str().unwrap_or_default();
            if let Some(workspace) = context.workspace_path.as_deref() {
                if super::shell_read_only::reads_only_inside(
                    command,
                    std::path::Path::new(workspace),
                ) {
                    return decision(PermissionResult::Granted, DefaultRead);
                }
            }
        }
        decision(self.request_confirmation(context), DefaultPrompt)
    }

    fn extract_action_from_context(&self, context: &PermissionContext) -> String {
        if let Some(cmd) = context.tool_input.get("command").and_then(|v| v.as_str()) {
            format!("command:{}", cmd.split_whitespace().next().unwrap_or(""))
        } else if let Some(path) = context.tool_input.get("path").and_then(|v| v.as_str()) {
            format!("path:{}", path)
        } else if let Some(content) = context.tool_input.get("content").and_then(|v| v.as_str()) {
            if content.len() > 50 {
                format!("content:{}", content.chars().take(50).collect::<String>())
            } else {
                format!("content:{}", content)
            }
        } else {
            "unknown".to_string()
        }
    }

    fn request_confirmation(&self, context: &PermissionContext) -> PermissionResult {
        let message = format!(
            "Tool '{}' requires user confirmation before execution",
            context.tool_name
        );
        PermissionResult::RequiresConfirmation(message)
    }

    /// Audit a one-call approval. Execution resumes via the original waiter;
    /// this must not authorize future calls with different inputs.
    pub fn confirm_permission(&self, conversation_id: &str, tool_name: &str) {
        let confirm_key = format!("{}-{}", conversation_id, tool_name);
        let mut confirmations = self.confirmations.lock().unwrap_or_else(|e| e.into_inner());
        confirmations.insert(confirm_key, true);

        self.audit_logger.log(
            AuditEntry::new(AuditAction::PermissionConfirmed, tool_name, conversation_id)
                .with_result("User confirmed"),
        );
    }

    pub fn clear_confirmation(&self, conversation_id: &str, tool_name: &str) {
        let confirm_key = format!("{}-{}", conversation_id, tool_name);
        let mut confirmations = self.confirmations.lock().unwrap_or_else(|e| e.into_inner());
        confirmations.remove(&confirm_key);
    }

    pub fn clear_all_confirmations(&self) {
        let mut confirmations = self.confirmations.lock().unwrap_or_else(|e| e.into_inner());
        confirmations.clear();
    }

    pub fn log_tool_execution(&self, context: &PermissionContext, success: bool) {
        let action = if success {
            AuditAction::ToolExecuted
        } else {
            AuditAction::ToolCancelled
        };
        let result = if success { "Success" } else { "Cancelled" };

        self.audit_logger.log(
            AuditEntry::new(action, &context.tool_name, &context.conversation_id)
                .with_user_id(context.user_id.as_deref())
                .with_result(result)
                .with_details(context.tool_input.clone()),
        );
    }

    pub fn is_tool_dangerous(&self, tool_name: &str) -> bool {
        DANGEROUS_TOOLS.contains(&tool_name)
    }
}

impl PermissionChecker for PermissionManager {
    fn check_permission(&self, context: &PermissionContext) -> PermissionResult {
        self.check_permission(context)
    }
}

pub type PermissionManagerRef = Arc<PermissionManager>;

#[cfg(test)]
mod session_rule_tests {
    use super::*;

    #[test]
    fn runtime_rules_fork_independently_and_preserve_plan_mode() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        manager.set_mode(PermissionMode::AskPermissions);
        manager.set_runtime_rules(
            "parent",
            vec![HashMap::from([("Write".into(), PermissionAction::Allow)])],
        );
        let child = manager.fork_session("parent", "child");
        manager.clear_runtime_rules("parent");
        let context = PermissionContext {
            mcp_read_only: false,
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"path":"test"}),
            conversation_id: "child".into(),
            user_id: None,
            workspace_path: None,
        };
        assert_eq!(child.check_permission(&context), PermissionResult::Granted);
        child.set_mode(PermissionMode::PlanMode);
        assert!(matches!(
            child.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        assert!(matches!(
            manager.check_permission(&PermissionContext {
                conversation_id: "parent".into(),
                ..context
            }),
            PermissionResult::RequiresConfirmation(_)
        ));
    }

    #[test]
    fn unrelated_maven_allow_does_not_prompt_for_compound_git_in_bypass() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        manager
            .set_launch_rules(
                "one",
                HashMap::from([(
                    "Bash(mvn -pl tio-boot -am compile)".into(),
                    PermissionAction::Allow,
                )]),
            )
            .unwrap();
        let context = PermissionContext {
            mcp_read_only: false,
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command":"cd repo && git ls-remote origin refs/heads/main 2>&1"}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        manager.set_mode(PermissionMode::BypassPermissions);
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        manager.set_mode(PermissionMode::AskPermissions);
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        manager.set_mode(PermissionMode::BypassPermissions);
        manager.set_session_rules(
            "one",
            HashMap::from([("Bash".into(), PermissionAction::Deny)]),
        );
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn pattern_rules_enforce_denies_and_complex_shell_confirmation() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        let mut context = PermissionContext {
            mcp_read_only: false,
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command":"git status"}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        manager.set_session_rules(
            "one",
            HashMap::from([
                ("Bash(git *)".into(), PermissionAction::Allow),
                ("Bash(git push*)".into(), PermissionAction::Deny),
            ]),
        );
        manager.set_mode(PermissionMode::BypassPermissions);
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        context.tool_input = serde_json::json!({"command":"git push origin main"});
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        manager.set_session_rules(
            "one",
            HashMap::from([("Bash(git *)".into(), PermissionAction::Allow)]),
        );
        context.tool_input = serde_json::json!({"command":"git status; evil"});
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        manager.set_mode(PermissionMode::AskPermissions);
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        manager.set_mode(PermissionMode::PlanMode);
        // Plan mode permits commands outside the read-only recognizer.
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        context.tool_input = serde_json::json!({"command":"git status"});
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        context.tool_name = "Read".into();
        manager.set_session_rules(
            "one",
            HashMap::from([("Read".into(), PermissionAction::Confirm)]),
        );
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
    }

    #[test]
    fn explicit_denies_survive_bypass_and_allow_cannot_override_plan() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        let context = PermissionContext {
            mcp_read_only: false,
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"path":"test"}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        manager.set_session_rules(
            "one",
            HashMap::from([("Write".into(), PermissionAction::Deny)]),
        );
        manager.set_mode(PermissionMode::BypassPermissions);
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        let other = PermissionContext {
            conversation_id: "two".into(),
            ..context.clone()
        };
        assert_eq!(manager.check_permission(&other), PermissionResult::Granted);
        manager.set_session_rules(
            "one",
            HashMap::from([("Write".into(), PermissionAction::Allow)]),
        );
        manager.set_mode(PermissionMode::PlanMode);
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        manager.set_mode(PermissionMode::AskPermissions);
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
    }

    #[test]
    fn plan_refuses_pty_launch_even_with_an_explicit_allow() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        manager.set_mode(PermissionMode::PlanMode);
        manager.set_session_rules(
            "one",
            HashMap::from([("Bash".into(), PermissionAction::Allow)]),
        );
        let mut context = PermissionContext {
            mcp_read_only: false,
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command":"cat", "interaction":"pty", "run_in_background":true}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        context.tool_input = serde_json::json!({"command":"pwd"});
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
    }

    #[test]
    fn monitor_uses_bash_rules() {
        use crate::permissions::patterns::canonical_tool_name;
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        // The engine canonicalizes the name before asking the manager, so this
        // context is what a Monitor call actually presents.
        let context = |tool: &str, command: &str| PermissionContext {
            mcp_read_only: false,
            tool_name: canonical_tool_name(tool).to_owned(),
            tool_input: serde_json::json!({"command":command}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        manager.set_session_rules(
            "one",
            HashMap::from([("Bash(rm:*)".into(), PermissionAction::Deny)]),
        );
        assert!(matches!(
            manager.check_permission(&context("Monitor", "rm -rf x")),
            PermissionResult::Denied(_)
        ));
        assert!(matches!(
            manager.check_permission(&context("Bash", "rm -rf x")),
            PermissionResult::Denied(_)
        ));
        manager.set_session_rules("one", HashMap::new());
        manager.set_mode(PermissionMode::AskPermissions);
        let monitor = manager.check_permission(&context("Monitor", "rm -rf x"));
        assert!(
            matches!(monitor, PermissionResult::RequiresConfirmation(_)),
            "{monitor:?}"
        );
        assert_eq!(monitor, manager.check_permission(&context("Bash", "rm -rf x")));
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    fn manager(mode: PermissionMode) -> PermissionManager {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(64)));
        manager.set_mode(mode);
        manager
    }
    fn context(tool: &str) -> PermissionContext {
        PermissionContext {
            mcp_read_only: false,
            tool_name: tool.into(),
            tool_input: serde_json::json!({"path":"fixture","content":"\u{4f60}\u{597d}\u{4e16}\u{754c}".repeat(40)}),
            conversation_id: "session".into(),
            user_id: None,
            workspace_path: None,
        }
    }
    #[test]
    fn legacy_task_output_keeps_task_get_read_only_authority() {
        for mode in [
            PermissionMode::AskPermissions,
            PermissionMode::AcceptEdits,
            PermissionMode::PlanMode,
        ] {
            let manager = manager(mode);
            let get = manager.check_permission(&context("TaskGet"));
            assert_eq!(get, PermissionResult::Granted, "{mode:?}");
            assert_eq!(
                manager.check_permission(&context("TaskOutput")),
                get,
                "{mode:?}"
            );
        }
    }
    #[test]
    fn task_input_is_a_mutating_operation_and_is_refused_in_plan_mode() {
        // It feeds an execution, so it must not inherit TaskGet's read-only
        // authority: plan mode denies it and the default modes ask first.
        let plan = manager(PermissionMode::PlanMode);
        assert!(matches!(
            plan.check_permission(&context("TaskInput")),
            PermissionResult::Denied(_)
        ));
        for mode in [PermissionMode::AskPermissions, PermissionMode::AcceptEdits] {
            assert!(
                matches!(
                    manager(mode).check_permission(&context("TaskInput")),
                    PermissionResult::RequiresConfirmation(_)
                ),
                "{mode:?}"
            );
        }
        assert_eq!(
            manager(PermissionMode::BypassPermissions).check_permission(&context("TaskInput")),
            PermissionResult::Granted
        );
    }
    #[test]
    fn github_reads_are_read_only_and_writes_need_approval() {
        // One tool covers both: the operation, not the tool name, decides.
        let plan = manager(PermissionMode::PlanMode);
        for operation in [
            "repo_view",
            "issue_list",
            "issue_view",
            "pr_list",
            "pr_view",
        ] {
            let mut call = context("GitHub");
            call.tool_input = serde_json::json!({"operation":operation});
            assert_eq!(
                plan.check_permission(&call),
                PermissionResult::Granted,
                "{operation}"
            );
            assert_eq!(
                manager(PermissionMode::AskPermissions).check_permission(&call),
                PermissionResult::Granted,
                "{operation}"
            );
        }
        for operation in [
            "issue_create",
            "issue_comment",
            "pr_create",
            "pr_comment",
            "pr_merge",
        ] {
            let mut call = context("GitHub");
            call.tool_input = serde_json::json!({"operation":operation});
            assert!(
                matches!(plan.check_permission(&call), PermissionResult::Denied(_)),
                "plan mode accepted {operation}"
            );
            for mode in [PermissionMode::AskPermissions, PermissionMode::AcceptEdits] {
                assert!(
                    matches!(
                        manager(mode).check_permission(&call),
                        PermissionResult::RequiresConfirmation(_)
                    ),
                    "{operation} in {mode:?}"
                );
            }
            assert_eq!(
                manager(PermissionMode::BypassPermissions).check_permission(&call),
                PermissionResult::Granted,
                "{operation}"
            );
        }
        // A missing operation is not silently a read.
        let mut call = context("GitHub");
        call.tool_input = serde_json::json!({});
        assert!(matches!(
            plan.check_permission(&call),
            PermissionResult::Denied(_)
        ));
    }
    #[test]
    fn plan_allows_messages_without_allowing_team_control() {
        let manager = manager(PermissionMode::PlanMode);
        let mut call = context("SendMessage");
        for operation in [
            "send",
            "receive",
            "peers",
            "sent",
            "pending_count",
            "mark_read",
        ] {
            call.tool_input = serde_json::json!({"operation":operation});
            assert_eq!(manager.check_permission(&call), PermissionResult::Granted);
        }
        call.tool_input = serde_json::json!({"operation":"shutdown_request"});
        assert!(matches!(
            manager.check_permission(&call),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn plan_allows_ordinary_commands_but_preserves_boundaries() {
        let manager = manager(PermissionMode::PlanMode);
        for tool in ["Bash", "PowerShell"] {
            for command in [
                "git status",
                "cargo test --locked",
                "npm install",
                "npm run build",
                "Get-Content README.md -Encoding UTF8",
                "echo result > result.txt",
                "codegraph init --yes \"D:/reference library\"",
                "codegraph index \"/srv/reference-library\"",
            ] {
                let mut input = context(tool);
                input.tool_input = serde_json::json!({"command":command});
                assert_eq!(
                    manager.check_permission(&input),
                    PermissionResult::Granted,
                    "{command}"
                );
            }
            for command in ["rm -rf /", "shutdown /s", "diskpart"] {
                let mut input = context(tool);
                input.tool_input = serde_json::json!({"command":command});
                assert!(matches!(
                    manager.check_permission(&input),
                    PermissionResult::Denied(_)
                ));
            }
        }
        assert!(matches!(
            manager.check_permission(&context("Edit")),
            PermissionResult::Denied(_)
        ));
        manager.set_session_rules(
            "session",
            HashMap::from([("Bash".into(), PermissionAction::Confirm)]),
        );
        let mut shell = context("Bash");
        shell.tool_input = serde_json::json!({"command":"cargo test"});
        assert!(matches!(
            manager.check_permission(&shell),
            PermissionResult::RequiresConfirmation(_)
        ));
        shell.tool_input =
            serde_json::json!({"command":"codegraph init --yes /srv/reference-library"});
        assert!(matches!(
            manager.check_permission(&shell),
            PermissionResult::RequiresConfirmation(_)
        ));
        manager.set_session_rules(
            "session",
            HashMap::from([("Bash".into(), PermissionAction::Deny)]),
        );
        assert!(matches!(
            manager.check_permission(&shell),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn mcp_read_only_is_host_owned_and_only_relaxes_plan_boundary() {
        let mut context = context("mcp__fixture__inspect");
        context.tool_input = serde_json::json!({"readOnlyHint":true,"mcp_read_only":true});
        let manager = manager(PermissionMode::PlanMode);
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        context.mcp_read_only = true;
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        for mode in [PermissionMode::AskPermissions, PermissionMode::Auto] {
            manager.set_mode(mode);
            assert!(matches!(
                manager.check_permission(&context),
                PermissionResult::RequiresConfirmation(_)
            ));
        }
        manager.set_mode(PermissionMode::PlanMode);
        for (action, denied) in [
            (PermissionAction::Deny, true),
            (PermissionAction::Confirm, false),
        ] {
            manager.set_session_rules(
                "session",
                HashMap::from([(context.tool_name.clone(), action)]),
            );
            let result = manager.check_permission(&context);
            assert_eq!(matches!(result, PermissionResult::Denied(_)), denied);
            assert_eq!(
                matches!(result, PermissionResult::RequiresConfirmation(_)),
                !denied
            );
        }
        manager.set_session_rules("session", HashMap::new());
        manager.set_tool_permission(
            &context.tool_name,
            ToolPermission {
                tool_name: context.tool_name.clone(),
                allowed: false,
                requires_confirmation: false,
                scope: super::super::PermissionScope::ToolSpecific,
                level: super::super::PermissionLevel::Execute,
            },
        );
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        let mut serialized = serde_json::to_value(&context).unwrap();
        serialized["mcp_read_only"] = serde_json::json!(true);
        assert!(
            !serde_json::from_value::<PermissionContext>(serialized)
                .unwrap()
                .mcp_read_only
        );
    }

    #[test]
    fn default_reads_and_unapproved_side_effects_follow_mode() {
        for mode in [
            PermissionMode::AskPermissions,
            PermissionMode::AcceptEdits,
            PermissionMode::PlanMode,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
        ] {
            let manager = manager(mode);
            assert_eq!(
                manager.check_permission(&context("Read")),
                PermissionResult::Granted
            );
            for tool in ["Write", "Bash", "LSP", "unknown"] {
                let result = manager.check_permission(&context(tool));
                match mode {
                    PermissionMode::BypassPermissions => {
                        assert_eq!(result, PermissionResult::Granted)
                    }
                    PermissionMode::AcceptEdits if tool == "Write" => {
                        assert_eq!(result, PermissionResult::Granted)
                    }
                    PermissionMode::PlanMode if tool == "Bash" => {
                        assert_eq!(result, PermissionResult::Granted)
                    }
                    PermissionMode::DontAsk | PermissionMode::PlanMode => {
                        assert!(matches!(result, PermissionResult::Denied(_)))
                    }
                    _ => assert!(matches!(result, PermissionResult::RequiresConfirmation(_))),
                }
            }
        }
    }
    #[test]
    fn workspace_reads_in_the_shell_run_without_asking_in_default_modes() {
        let shell = |command: &str| PermissionContext {
            mcp_read_only: false,
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": command}),
            conversation_id: "session".into(),
            user_id: None,
            workspace_path: Some("/work/project".into()),
        };
        for mode in [PermissionMode::AskPermissions, PermissionMode::AcceptEdits] {
            let manager = manager(mode);
            assert_eq!(
                manager.check_permission(&shell("ls -la src")),
                PermissionResult::Granted
            );
            assert!(matches!(
                manager.check_permission(&shell("cat /etc/passwd")),
                PermissionResult::RequiresConfirmation(_)
            ));
            assert!(matches!(
                manager.check_permission(&shell("npm install")),
                PermissionResult::RequiresConfirmation(_)
            ));
            manager.set_session_rules(
                "session",
                HashMap::from([("Bash(ls *)".into(), PermissionAction::Confirm)]),
            );
            assert!(matches!(
                manager.check_permission(&shell("ls -la src")),
                PermissionResult::RequiresConfirmation(_)
            ));
        }
        assert!(matches!(
            manager(PermissionMode::DontAsk).check_permission(&shell("ls")),
            PermissionResult::Denied(_)
        ));
    }
    #[test]
    fn local_fetch_requires_explicit_review_in_every_mode() {
        for mode in [
            PermissionMode::PlanMode,
            PermissionMode::Auto,
            PermissionMode::AskPermissions,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
        ] {
            let manager = manager(mode);
            manager.set_session_rules(
                "session",
                HashMap::from([("WebFetch".into(), PermissionAction::Allow)]),
            );
            for url in [
                "http://localhost:8080/",
                "http://127.0.0.1/",
                "http://192.168.1.2/",
                "http://[::1]/",
                "http://[::ffff:127.0.0.1]/",
                "http://intranet/",
            ] {
                let call = PermissionContext {
                    tool_input: serde_json::json!({"url":url}),
                    ..context("WebFetch")
                };
                let decision = manager.evaluate_permission(&call);
                assert!(
                    matches!(decision.result, PermissionResult::RequiresConfirmation(_)),
                    "{mode:?}: {url}"
                );
                assert!(!decision.classifier_eligible);
            }
            manager.set_session_rules(
                "session",
                HashMap::from([("WebFetch".into(), PermissionAction::Deny)]),
            );
            let call = PermissionContext {
                tool_input: serde_json::json!({"url":"http://localhost/"}),
                ..context("WebFetch")
            };
            assert!(matches!(
                manager.check_permission(&call),
                PermissionResult::Denied(_)
            ));
        }
    }

    #[test]
    fn plan_fetch_skips_host_prompt_but_keeps_explicit_policy() {
        let manager = manager(PermissionMode::PlanMode);
        let call = PermissionContext {
            tool_input: serde_json::json!({"url":"https://ifnotnil.com/topic"}),
            ..context("WebFetch")
        };
        assert_eq!(manager.check_permission(&call), PermissionResult::Granted);
        for action in [PermissionAction::Confirm, PermissionAction::Deny] {
            manager.set_session_rules(
                "session",
                HashMap::from([("WebFetch".into(), action.clone())]),
            );
            match action {
                PermissionAction::Confirm => assert!(matches!(
                    manager.check_permission(&call),
                    PermissionResult::RequiresConfirmation(_)
                )),
                _ => assert!(matches!(
                    manager.check_permission(&call),
                    PermissionResult::Denied(_)
                )),
            }
        }
    }

    #[test]
    fn auto_and_session_file_edits_preserve_boundaries_and_isolation() {
        for auto in [false, true] {
            let manager = manager(if auto {
                PermissionMode::Auto
            } else {
                PermissionMode::AskPermissions
            });
            if !auto {
                manager.set_edit_session_allowed("session", true);
            }
            for tool in ["Write", "Edit", "MultiEdit", "NotebookEdit"] {
                let mut call = context(tool);
                call.tool_input =
                    serde_json::json!({"file_path":"normal.txt", "notebook_path":"normal.ipynb"});
                let decision = manager.evaluate_permission(&call);
                assert_eq!(decision.result, PermissionResult::Granted);
                assert!(!decision.classifier_eligible);
                call.tool_input =
                    serde_json::json!({"file_path":".bashrc", "notebook_path":".bashrc"});
                assert!(matches!(
                    manager.check_permission(&call),
                    PermissionResult::RequiresConfirmation(_)
                ));
                for action in [PermissionAction::Confirm, PermissionAction::Deny] {
                    manager.set_session_rules("session", HashMap::from([(tool.into(), action)]));
                    assert_ne!(
                        manager.check_permission(&context(tool)),
                        PermissionResult::Granted
                    );
                }
                manager.set_session_rules("session", HashMap::new());
            }
            assert!(!manager.edit_session_allowed("other"));
            assert!(!manager
                .fork_session("session", "child")
                .edit_session_allowed("child"));
            assert!(!crate::runtime::default_permissions().edit_session_allowed("session"));
            manager.set_mode(PermissionMode::PlanMode);
            assert!(matches!(
                manager.check_permission(&context("Write")),
                PermissionResult::Denied(_)
            ));
            manager.set_mode(PermissionMode::AskPermissions);
            manager.set_edit_session_allowed("session", false);
            assert!(matches!(
                manager.check_permission(&context("Write")),
                PermissionResult::RequiresConfirmation(_)
            ));
        }
    }

    #[test]
    fn web_fetch_asks_for_new_hosts_unless_preapproved_or_allowed() {
        let fetch = |url: &str| PermissionContext {
            tool_input: serde_json::json!({"url": url, "prompt": "p"}),
            ..context("WebFetch")
        };
        for mode in [PermissionMode::AskPermissions, PermissionMode::AcceptEdits] {
            let manager = manager(mode);
            assert_eq!(
                manager.check_permission(&fetch("https://docs.python.org/3/")),
                PermissionResult::Granted,
                "{mode:?}"
            );
            assert!(
                matches!(
                    manager.check_permission(&fetch("https://example.com/")),
                    PermissionResult::RequiresConfirmation(_)
                ),
                "{mode:?}"
            );
            manager.set_session_rules(
                "session",
                HashMap::from([(
                    "WebFetch(domain:example.com)".into(),
                    PermissionAction::Allow,
                )]),
            );
            assert_eq!(
                manager.check_permission(&fetch("https://example.com/")),
                PermissionResult::Granted,
                "{mode:?}"
            );
            assert!(matches!(
                manager.check_permission(&fetch("https://example.org/")),
                PermissionResult::RequiresConfirmation(_)
            ));
        }
        // A research child may fetch anything; its parent still asks, and deny wins.
        let parent = manager(PermissionMode::AskPermissions);
        parent.set_session_rules(
            "session",
            HashMap::from([("WebFetch(domain:evil.test)".into(), PermissionAction::Deny)]),
        );
        let child = parent.fork_session("session", "child");
        child.allow_in_session("child", "WebFetch");
        let in_child = |url: &str| PermissionContext {
            conversation_id: "child".into(),
            ..fetch(url)
        };
        assert_eq!(
            child.check_permission(&in_child("https://example.com/")),
            PermissionResult::Granted
        );
        assert!(matches!(
            child.check_permission(&in_child("https://evil.test/")),
            PermissionResult::Denied(_)
        ));
        assert!(matches!(
            parent.check_permission(&fetch("https://example.com/")),
            PermissionResult::RequiresConfirmation(_)
        ));
        assert_eq!(
            manager(PermissionMode::BypassPermissions)
                .check_permission(&fetch("https://example.com/")),
            PermissionResult::Granted
        );
        assert!(matches!(
            manager(PermissionMode::DontAsk).check_permission(&fetch("https://example.com/")),
            PermissionResult::Denied(_)
        ));
    }
    #[test]
    fn own_state_tools_run_without_asking_unless_a_rule_or_plan_mode_says_otherwise() {
        for mode in [
            PermissionMode::AskPermissions,
            PermissionMode::AcceptEdits,
            PermissionMode::DontAsk,
        ] {
            let manager = manager(mode);
            for tool in ["memory_ingest", "TodoWrite", "TaskCreate", "TaskUpdate"] {
                let decision = manager.evaluate_permission(&context(tool));
                assert_eq!(
                    decision.result,
                    PermissionResult::Granted,
                    "{tool} {mode:?}"
                );
                assert_eq!(decision.origin, DecisionOrigin::DefaultOwnState);
            }
            manager.set_session_rules(
                "session",
                HashMap::from([
                    ("memory_ingest".into(), PermissionAction::Confirm),
                    ("TodoWrite".into(), PermissionAction::Deny),
                ]),
            );
            assert!(
                !matches!(
                    manager.check_permission(&context("memory_ingest")),
                    PermissionResult::Granted
                ),
                "{mode:?}"
            );
            assert!(
                matches!(
                    manager.check_permission(&context("TodoWrite")),
                    PermissionResult::Denied(_)
                ),
                "{mode:?}"
            );
        }
        // Other writes still ask, and plan mode still blocks these tools.
        assert!(matches!(
            manager(PermissionMode::AskPermissions).check_permission(&context("CronCreate")),
            PermissionResult::RequiresConfirmation(_)
        ));
        assert!(matches!(
            manager(PermissionMode::PlanMode).check_permission(&context("memory_ingest")),
            PermissionResult::Denied(_)
        ));
    }
    #[test]
    fn bypass_skips_ask_but_preserves_deny_and_other_modes_keep_ask() {
        for mode in [
            PermissionMode::AskPermissions,
            PermissionMode::AcceptEdits,
            PermissionMode::PlanMode,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
        ] {
            let manager = manager(mode);
            manager.set_always_allow_checker(Box::new(|_, _| true));
            manager.set_runtime_rules(
                "session",
                vec![HashMap::from([("Read".into(), PermissionAction::Allow)])],
            );
            manager.set_session_rules(
                "session",
                HashMap::from([("Read".into(), PermissionAction::Confirm)]),
            );
            let decision = manager.evaluate_permission(&context("Read"));
            assert_eq!(
                decision.origin,
                if mode == PermissionMode::BypassPermissions {
                    DecisionOrigin::Mode
                } else {
                    DecisionOrigin::ExplicitRule
                }
            );
            assert!(!decision.classifier_eligible);
            if mode == PermissionMode::BypassPermissions {
                assert_eq!(decision.result, PermissionResult::Granted);
            } else if mode == PermissionMode::DontAsk {
                assert!(matches!(decision.result, PermissionResult::Denied(_)));
            } else {
                assert!(matches!(
                    decision.result,
                    PermissionResult::RequiresConfirmation(_)
                ));
            }
            manager.set_session_rules(
                "session",
                HashMap::from([("Read".into(), PermissionAction::Deny)]),
            );
            assert!(matches!(
                manager.check_permission(&context("Read")),
                PermissionResult::Denied(_)
            ));
        }
    }
    #[test]
    fn dont_ask_preserves_approvals_and_fork_isolation() {
        let manager = manager(PermissionMode::DontAsk);
        manager.set_session_rules(
            "session",
            HashMap::from([("Write".into(), PermissionAction::Allow)]),
        );
        assert_eq!(
            manager.check_permission(&context("Write")),
            PermissionResult::Granted
        );
        let child = manager.fork_session("session", "child");
        child.set_mode(PermissionMode::AskPermissions);
        assert_eq!(manager.get_mode(), PermissionMode::DontAsk);
        assert_eq!(
            child.check_permission(&PermissionContext {
                conversation_id: "child".into(),
                ..context("Write")
            }),
            PermissionResult::Granted
        );
    }
    #[test]
    fn only_default_prompt_is_classifier_eligible_and_unicode_preview_is_safe() {
        let manager = manager(PermissionMode::AskPermissions);
        let mut input = context("Write");
        input.tool_input = serde_json::json!({"content":"\u{4e2d}\u{6587}".repeat(50)});
        manager.set_always_allow_checker(Box::new(|_, action| {
            assert!(action.starts_with("content:"));
            false
        }));
        let decision = manager.evaluate_permission(&input);
        assert_eq!(decision.origin, DecisionOrigin::DefaultPrompt);
        assert!(decision.classifier_eligible);
        manager.set_mode(PermissionMode::DontAsk);
        assert!(!manager.evaluate_permission(&input).classifier_eligible);
    }
    #[test]
    fn tool_settings_and_plan_boundaries_cannot_be_overridden_by_allow() {
        let manager = manager(PermissionMode::BypassPermissions);
        manager.set_session_rules(
            "session",
            HashMap::from([("Write".into(), PermissionAction::Allow)]),
        );
        let permission = ToolPermission {
            tool_name: "Write".into(),
            allowed: true,
            requires_confirmation: true,
            scope: super::super::PermissionScope::ToolSpecific,
            level: super::super::PermissionLevel::Write,
        };
        manager.set_tool_permission("Write", permission.clone());
        assert_eq!(
            manager.check_permission(&context("Write")),
            PermissionResult::Granted
        );
        manager.set_tool_permission(
            "Write",
            ToolPermission {
                allowed: false,
                ..permission.clone()
            },
        );
        assert!(matches!(
            manager.check_permission(&context("Write")),
            PermissionResult::Denied(_)
        ));
        manager.set_tool_permission(
            "Write",
            ToolPermission {
                requires_confirmation: false,
                ..permission
            },
        );
        manager.set_mode(PermissionMode::PlanMode);
        assert!(matches!(
            manager.check_permission(&context("Write")),
            PermissionResult::Denied(_)
        ));
        assert_eq!(
            manager.check_plan_document_permission(&context("Write")),
            PermissionResult::Granted
        );
        manager.set_session_rules(
            "session",
            HashMap::from([("Write".into(), PermissionAction::Deny)]),
        );
        assert!(matches!(
            manager.check_plan_document_permission(&context("Write")),
            PermissionResult::Denied(_)
        ));
    }
    #[test]
    fn unsupported_conditional_allow_does_not_authorize_default_prompt() {
        let manager = manager(PermissionMode::DontAsk);
        manager.add_rule(
            PermissionRule::new(
                "conditional",
                PermissionAction::Allow,
                super::super::PermissionScope::ToolSpecific,
                super::super::PermissionLevel::Execute,
            )
            .with_tool_name("Bash")
            .with_conditions(serde_json::json!({"unknown":true})),
        );
        assert!(matches!(
            manager.check_permission(&context("Bash")),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn delegation_defaults_preserve_inherited_authority_but_auto_classifies() {
        for mode in [
            PermissionMode::AskPermissions,
            PermissionMode::PlanMode,
            PermissionMode::DontAsk,
            PermissionMode::Auto,
        ] {
            let manager = manager(mode);
            for name in ["Agent"] {
                let decision = manager.evaluate_permission(&context(name));
                if mode == PermissionMode::Auto {
                    assert!(decision.classifier_eligible);
                } else {
                    assert_eq!(decision.result, PermissionResult::Granted);
                }
                manager.set_session_rules(
                    "session",
                    HashMap::from([(name.into(), PermissionAction::Deny)]),
                );
                assert!(matches!(
                    manager.check_permission(&context(name)),
                    PermissionResult::Denied(_)
                ));
                manager.set_session_rules(
                    "session",
                    HashMap::from([(name.into(), PermissionAction::Confirm)]),
                );
                let explicit = manager.evaluate_permission(&context(name));
                assert!(!explicit.classifier_eligible);
                assert_ne!(explicit.result, PermissionResult::Granted);
                manager.set_session_rules("session", HashMap::new());
            }
        }
    }

    #[test]
    fn auto_filters_broad_grants_without_destroying_saved_rules() {
        let manager = manager(PermissionMode::Auto);
        manager.set_session_rules(
            "session",
            HashMap::from([
                ("Bash".into(), PermissionAction::Allow),
                ("Agent".into(), PermissionAction::Allow),
            ]),
        );
        assert!(
            manager
                .evaluate_permission(&context("Agent"))
                .classifier_eligible
        );
        let mut shell = context("Bash");
        shell.tool_input = serde_json::json!({"command":"npm test"});
        assert_eq!(manager.check_permission(&shell), PermissionResult::Granted);
        manager.set_mode(PermissionMode::AskPermissions);
        for tool in ["Bash", "Agent"] {
            assert_eq!(
                manager.check_permission(&context(tool)),
                PermissionResult::Granted
            );
        }
        manager.set_mode(PermissionMode::Auto);
        manager.set_session_rules(
            "session",
            HashMap::from([("Bash".into(), PermissionAction::Deny)]),
        );
        assert!(matches!(
            manager.check_permission(&context("Bash")),
            PermissionResult::Denied(_)
        ));
    }

    #[test]
    fn auto_shell_skips_classifier_but_preserves_explicit_policy_and_danger() {
        let manager = manager(PermissionMode::Auto);
        for command in [
            "cd D:/repo && grep -n 'BridgeServer|shutdown' src/main.rs | head",
            "npm install",
            "python unknown.py",
            "custom-script --run",
            "echo hi > output.txt",
        ] {
            let mut input = context("Bash");
            input.tool_input = serde_json::json!({"command":command});
            let decision = manager.evaluate_permission(&input);
            assert_eq!(decision.result, PermissionResult::Granted, "{command}");
            assert!(!decision.classifier_eligible);
            manager.set_session_rules(
                "session",
                HashMap::from([("Bash".into(), PermissionAction::Confirm)]),
            );
            assert!(matches!(
                manager.check_permission(&input),
                PermissionResult::RequiresConfirmation(_)
            ));
            manager.set_session_rules(
                "session",
                HashMap::from([("Bash".into(), PermissionAction::Deny)]),
            );
            assert!(matches!(
                manager.check_permission(&input),
                PermissionResult::Denied(_)
            ));
            manager.set_session_rules("session", HashMap::new());
        }
        let mut input = context("Bash");
        input.tool_input = serde_json::json!({"command":"shutdown /s"});
        for mode in [
            PermissionMode::Auto,
            PermissionMode::AskPermissions,
            PermissionMode::AcceptEdits,
        ] {
            manager.set_mode(mode);
            manager.set_session_rules(
                "session",
                HashMap::from([("Bash".into(), PermissionAction::Allow)]),
            );
            let decision = manager.evaluate_permission(&input);
            assert!(matches!(
                decision.result,
                PermissionResult::RequiresConfirmation(_)
            ));
            assert!(!decision.classifier_eligible);
        }
    }

    #[test]
    fn bash_session_grant_is_runtime_only_scoped_revocable_and_excludes_danger() {
        let manager = manager(PermissionMode::AskPermissions);
        let mut input = context("Bash");
        input.tool_input = serde_json::json!({"command":"custom-script && npm test"});
        manager.set_bash_session_allowed("session", true);
        assert_eq!(manager.check_permission(&input), PermissionResult::Granted);
        assert!(matches!(
            manager.check_permission(&context("Write")),
            PermissionResult::RequiresConfirmation(_)
        ));
        let child = manager.fork_session("session", "child");
        assert!(!child.bash_session_allowed("child"));
        assert!(!manager.bash_session_allowed("other"));
        manager.inherit_session_rules("session", "inherited");
        assert!(!manager.bash_session_allowed("inherited"));
        assert!(!crate::runtime::default_permissions().bash_session_allowed("session"));
        input.tool_input = serde_json::json!({"command":"shutdown"});
        assert!(matches!(
            manager.check_permission(&input),
            PermissionResult::RequiresConfirmation(_)
        ));
        input.tool_input = serde_json::json!({"command":"custom-script && npm test"});
        for action in [PermissionAction::Deny, PermissionAction::Confirm] {
            manager.set_session_rules("session", HashMap::from([("Bash".into(), action)]));
            assert_ne!(manager.check_permission(&input), PermissionResult::Granted);
        }
        manager.set_session_rules("session", HashMap::new());
        manager.set_bash_session_allowed("session", false);
        assert!(matches!(
            manager.check_permission(&input),
            PermissionResult::RequiresConfirmation(_)
        ));
    }

    #[test]
    fn strict_mode_aliases_serde_and_unknown_rejection() {
        for (alias, mode) in [
            ("default", PermissionMode::AskPermissions),
            ("acceptEdits", PermissionMode::AcceptEdits),
            ("plan", PermissionMode::PlanMode),
            ("bypassPermissions", PermissionMode::BypassPermissions),
            ("dontAsk", PermissionMode::DontAsk),
            ("dont-ask", PermissionMode::DontAsk),
            ("dont_ask", PermissionMode::DontAsk),
        ] {
            assert_eq!(PermissionMode::parse(alias).unwrap(), mode);
            assert_eq!(
                serde_json::from_value::<PermissionMode>(serde_json::json!(alias)).unwrap(),
                mode
            );
            assert_eq!(
                serde_json::from_str::<PermissionMode>(&serde_json::to_string(&mode).unwrap())
                    .unwrap(),
                mode
            );
        }
        for value in ["automatic", "", "typo"] {
            assert!(PermissionMode::parse(value).is_err());
            assert!(serde_json::from_value::<PermissionMode>(serde_json::json!(value)).is_err());
        }
    }
}

#[cfg(test)]
#[path = "launch_rules_tests.rs"]
mod launch_rule_tests;
