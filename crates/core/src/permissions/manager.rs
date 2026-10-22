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
pub struct ModeSnapshot { pub mode: PermissionMode, revision: u64 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicySnapshot { pub mode: ModeSnapshot, revision: u64 }

pub struct PermissionManager {
    policy_revision: Mutex<u64>,
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
        let _revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        let callback = self.always_allow_checker.lock().unwrap_or_else(|e|e.into_inner()).as_ref().map(|callback| Arc::as_ptr(callback) as *const () as usize);
        let mode=self.mode_snapshot();
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
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        let mut current = self.launch_rules.lock().unwrap_or_else(|e| e.into_inner());
        if current.get(conversation).is_some_and(|value| *value == rules)
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
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        self.runtime_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conversation.to_owned(), rules);
    }
    /// Validate or atomically publish a session patch against the latest runtime rules.
    pub(crate) fn update_runtime_rules(
        &self, conversation: &str, publish: bool,
        update: impl FnOnce(&mut Vec<HashMap<String, PermissionAction>>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        let mut runtime = self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = runtime.get(conversation).cloned().unwrap_or_else(|| vec![HashMap::new(); 3]);
        next.resize_with(3, HashMap::new);
        update(&mut next)?;
        if publish {
            runtime.insert(conversation.into(), next);
            *revision = revision.wrapping_add(1);
        }
        Ok(())
    }
    pub fn clear_runtime_rules(&self, conversation: &str) {
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner()).remove(conversation);
    }
    pub fn set_session_rules(&self, conversation: &str, rules: HashMap<String, PermissionAction>) {
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        self.session_rules.lock().unwrap_or_else(|e| e.into_inner()).insert(conversation.to_owned(), rules);
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

    pub(crate) fn inherit_session_rules(&self, parent: &str, child: &str) {
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut sessions = self.session_rules.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rules) = sessions.get(parent).cloned() { sessions.insert(child.to_owned(), rules); }
        let mut runtime = self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rules) = runtime.get(parent).cloned() { runtime.insert(child.to_owned(), rules); }
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
        let _revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        let rules = self
            .session_rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(parent)
            .cloned()
            .unwrap_or_default();
        Self {
            policy_revision: Mutex::new(0),
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
            mode: Mutex::new(ModeSnapshot { mode: self.get_mode(), revision: 0 }),
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
            policy_revision: Mutex::new(0),
            auto_stats: Mutex::new(HashMap::new()),
            ruleset: Mutex::new(PermissionRuleset::new()),
            tool_permissions: Mutex::new(HashMap::new()),
            audit_logger,
            confirmations: Mutex::new(HashMap::new()),
            mode: Mutex::new(ModeSnapshot { mode: PermissionMode::default(), revision: 0 }),
            session_rules: Mutex::new(HashMap::new()),
            runtime_rules: Mutex::new(HashMap::new()),
            launch_rules: Mutex::new(HashMap::new()),
            session_directories: Mutex::new(HashMap::new()),
            always_allow_checker: Mutex::new(None),
        }
    }

    pub fn set_always_allow_checker(&self, checker: AlwaysAllowCheckerFn) {
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut guard = self.always_allow_checker.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Arc::from(checker));
        tracing::info!(module = "Permission", "Always allow checker registered");
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        let mut policy_revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if m.mode != mode { m.mode = mode; m.revision = m.revision.wrapping_add(1); *policy_revision = policy_revision.wrapping_add(1); }
        tracing::info!(module = "Permission", "Mode changed to: {}", mode.as_str());
    }

    pub fn get_mode(&self) -> PermissionMode {
        self.mode.lock().unwrap_or_else(|e| e.into_inner()).mode
    }

    pub fn auto_stats(&self, session: &str) -> super::auto_config::AutoStats {
        self.auto_stats.lock().unwrap_or_else(|e| e.into_inner()).get(session).cloned().unwrap_or_default()
    }
    pub fn update_auto_stats(&self, session: &str, update: impl FnOnce(&mut super::auto_config::AutoStats)) -> super::auto_config::AutoStats {
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
        let revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        PolicySnapshot { mode: self.mode_snapshot(), revision: *revision }
    }

    /// Do not hold policy locks while calling saved-approval callbacks: they can
    /// read SQLite. Retry a concurrently changing snapshot, then fail closed.
    pub fn evaluate_stable(&self, context: &PermissionContext) -> Option<(PermissionDecision, PolicySnapshot)> {
        for _ in 0..3 {
            let before = self.policy_snapshot();
            let decision = self.evaluate_permission(context);
            if self.policy_snapshot() == before { return Some((decision, before)); }
        }
        None
    }

    pub fn mode_snapshot(&self) -> ModeSnapshot {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Caller must acquire any DB lock BEFORE entering this method. The closure
    /// may commit an existing transaction but must not acquire DB/permission locks.
    /// Failed commits leave mode unchanged; intervening mode changes reject approval.
    pub fn commit_mode_transition<T>(&self, expected: ModeSnapshot, next: PermissionMode, commit: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut policy_revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        let mut current = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(*current == expected, "Permission mode changed during review; request approval again");
        let result = commit()?;
        current.mode = next;
        current.revision = current.revision.wrapping_add(1);
        *policy_revision = policy_revision.wrapping_add(1);
        Ok(result)
    }
    /// Explicit host selection; same DB-before-mode lock order as plan approval.
    pub fn commit_mode_update<T>(&self, next: PermissionMode, commit: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut policy_revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        let mut current = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        let result = commit()?;
        if current.mode != next { current.mode = next; current.revision = current.revision.wrapping_add(1); *policy_revision = policy_revision.wrapping_add(1); }
        Ok(result)
    }

    pub fn add_rule(&self, rule: PermissionRule) {
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.add_rule(rule);
    }

    pub fn remove_rule(&self, rule_id: &str) {
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.rules.retain(|r| r.id != rule_id);
    }

    pub fn get_rules(&self) -> Vec<PermissionRule> {
        let ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.rules.clone()
    }

    pub fn set_tool_permission(&self, tool_name: &str, permission: ToolPermission) {
        let mut revision = self.policy_revision.lock().unwrap_or_else(|e| e.into_inner());
        *revision = revision.wrapping_add(1);
        let mut permissions = self.tool_permissions.lock().unwrap_or_else(|e| e.into_inner());
        permissions.insert(tool_name.to_string(), permission);
    }

    pub fn get_tool_permission(&self, tool_name: &str) -> Option<ToolPermission> {
        let permissions = self.tool_permissions.lock().unwrap_or_else(|e| e.into_inner());
        permissions.get(tool_name).cloned()
    }

    fn broad_auto_grant(pattern: &str) -> bool {
        let name = pattern.split('(').next().unwrap_or(pattern);
        matches!(name, "Bash" | "PowerShell" | "Agent" | "Delegate" | "Worker" | "Task" | "team") && (!pattern.contains('(') || pattern.contains('*'))
    }

    fn is_read_only_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "Read" | "ListDir" | "Grep" | "Glob" | "WebFetch" | "WebSearch" | "memory_search"
                | "TodoRead" | "Skill" | "SkillRead" | "TaskGet" | "TaskList" | "TaskOutput" | "ToolSearch" | "ProposeGoal" | "plan_mode" | "EnterPlanMode" | "ExitPlanMode" | "brief" | "SyntheticOutput"
        )
    }

    fn is_edit_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "Write" | "Edit" | "SearchReplace" | "GlobReplace" | "Delete"
                | "memory_ingest" | "SkillCreate" | "SkillUpdate" | "NotebookEdit" | "TaskCreate" | "TaskUpdate" | "TaskStop" | "todo_write" | "TodoWrite" | "SendMessage" | "team" | "cron" | "Config"
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
        let auto = mode == PermissionMode::Auto;
        let action = |rules: &HashMap<String, PermissionAction>| {
            if !auto { return super::patterns::matching_action(rules, context); }
            let effective = rules.iter().filter(|(pattern, action)| **action != PermissionAction::Allow || !Self::broad_auto_grant(pattern)).map(|(key,value)| (key.clone(),value.clone())).collect();
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
            if auto && rule.action == PermissionAction::Allow && (rule.tool_name.is_none() || Self::broad_auto_grant(rule.tool_name.as_deref().unwrap_or(""))) { continue; }
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
        // Delegation inherits the parent authority; children check their own tools.
        // Auto still classifies the spawn itself, matching the native Agent tool.
        let delegation = matches!(context.tool_name.as_str(), "Agent" | "Delegate" | "Worker")
            || (context.tool_name == "team" && context.tool_input["operation"] == "run");
        let read_only = Self::is_read_only_tool(&context.tool_name)
            || (context.tool_name == "Config"
                && matches!(
                    context.tool_input["operation"].as_str(),
                    Some("get" | "list" | "merge" | "sources")
                ));
        // Plan mode may inspect the project with shell commands: read-only
        // ones run, any other command is the user's call instead of a denial.
        let shell = matches!(context.tool_name.as_str(), "Bash" | "PowerShell");
        if mode == PermissionMode::PlanMode && shell && !plan_document {
            let command = context.tool_input["command"].as_str().unwrap_or_default();
            if super::shell_read_only::is_read_only(command) && !actions.contains(&PermissionAction::Confirm) {
                return decision(PermissionResult::Granted, PlanBoundary);
            }
            return decision(self.request_confirmation(context), PlanBoundary);
        }
        if mode == PermissionMode::PlanMode && !read_only && !plan_document && !delegation {
            return decision(
                PermissionResult::Denied("Plan mode: write operations are not allowed".into()),
                PlanBoundary,
            );
        }
        if actions.contains(&PermissionAction::Confirm) {
            return decision(self.request_confirmation(context), ExplicitRule);
        }
        if setting.as_ref().is_some_and(|p| p.requires_confirmation) {
            return decision(self.request_confirmation(context), ToolSetting);
        }
        if actions.contains(&PermissionAction::Allow) {
            return decision(PermissionResult::Granted, ExplicitRule);
        }
        if mode == PermissionMode::PlanMode {
            return decision(PermissionResult::Granted, PlanBoundary);
        }
        if setting.as_ref().is_some_and(|p| p.allowed) && !(auto && Self::broad_auto_grant(&context.tool_name)) {
            return decision(PermissionResult::Granted, ToolSetting);
        }
        let checker = self
            .always_allow_checker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !(auto && Self::broad_auto_grant(&context.tool_name)) && checker.is_some_and(|checker| {
            checker(
                &context.tool_name,
                &self.extract_action_from_context(context),
            )
        }) {
            return decision(PermissionResult::Granted, SavedApproval);
        }
        if mode != PermissionMode::BypassPermissions && Self::is_edit_tool(&context.tool_name) {
            if let Some(path) = context.tool_input["file_path"]
                .as_str()
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
        let action = if success { AuditAction::ToolExecuted } else { AuditAction::ToolCancelled };
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
    fn pattern_rules_enforce_denies_and_complex_shell_confirmation() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        let mut context = PermissionContext {
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
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        manager.set_mode(PermissionMode::PlanMode);
        // Plan mode asks before a command it cannot prove read-only...
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        // ...and runs plain inspection commands.
        context.tool_input = serde_json::json!({"command":"git status"});
        assert_eq!(manager.check_permission(&context), PermissionResult::Granted);
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
            tool_name: tool.into(),
            tool_input: serde_json::json!({"path":"fixture","content":"\u{4f60}\u{597d}\u{4e16}\u{754c}".repeat(40)}),
            conversation_id: "session".into(),
            user_id: None,
            workspace_path: None,
        }
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
                        assert!(matches!(result, PermissionResult::RequiresConfirmation(_)))
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
    fn explicit_ask_and_deny_override_all_permissive_modes_and_saved_approvals() {
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
            assert_eq!(decision.origin, DecisionOrigin::ExplicitRule);
            assert!(!decision.classifier_eligible);
            if mode == PermissionMode::DontAsk {
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
        assert!(matches!(
            manager.check_permission(&context("Write")),
            PermissionResult::RequiresConfirmation(_)
        ));
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
        manager.add_rule(PermissionRule::new(
            "conditional", PermissionAction::Allow,
            super::super::PermissionScope::ToolSpecific,
            super::super::PermissionLevel::Execute,
        ).with_tool_name("Bash").with_conditions(serde_json::json!({"unknown":true})));
        assert!(matches!(manager.check_permission(&context("Bash")), PermissionResult::Denied(_)));
    }

    #[test]
    fn delegation_defaults_preserve_inherited_authority_but_auto_classifies() {
        for mode in [PermissionMode::AskPermissions, PermissionMode::PlanMode, PermissionMode::DontAsk, PermissionMode::Auto] {
            let manager = manager(mode);
            for name in ["Agent", "Delegate", "Worker"] {
                let decision = manager.evaluate_permission(&context(name));
                if mode == PermissionMode::Auto { assert!(decision.classifier_eligible); }
                else { assert_eq!(decision.result, PermissionResult::Granted); }
                manager.set_session_rules("session", HashMap::from([(name.into(), PermissionAction::Deny)]));
                assert!(matches!(manager.check_permission(&context(name)), PermissionResult::Denied(_)));
                manager.set_session_rules("session", HashMap::from([(name.into(), PermissionAction::Confirm)]));
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
        manager.set_session_rules("session", HashMap::from([("Bash".into(), PermissionAction::Allow), ("Agent".into(), PermissionAction::Allow)]));
        for tool in ["Bash", "Agent"] { assert!(manager.evaluate_permission(&context(tool)).classifier_eligible); }
        manager.set_mode(PermissionMode::AskPermissions);
        for tool in ["Bash", "Agent"] { assert_eq!(manager.check_permission(&context(tool)), PermissionResult::Granted); }
        manager.set_mode(PermissionMode::Auto);
        manager.set_session_rules("session", HashMap::from([("Bash".into(), PermissionAction::Deny)]));
        assert!(matches!(manager.check_permission(&context("Bash")), PermissionResult::Denied(_)));
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
