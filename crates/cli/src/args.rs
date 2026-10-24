//! Terminal commands migrated from the former CLI, now backed by claude-core.
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "dcl",
    version,
    args_override_self = true,
    disable_help_subcommand = true,
    about = "DeepSeek Code — standalone CLI using the Desktop engine"
)]
pub struct Cli {
    /// Run initialization hooks before starting the session.
    #[arg(long, conflicts_with = "maintenance")]
    pub init: bool,
    /// Run initialization hooks and exit without a model request.
    #[arg(long, conflicts_with = "maintenance")]
    pub init_only: bool,
    /// Run maintenance hooks before starting the session.
    #[arg(long)]
    pub maintenance: bool,
    /// Initial prompt; piped stdin is appended in non-interactive mode.
    pub prompt: Option<String>,
    #[command(subcommand)]
    pub command: Option<Commands>,
    /// Project directory (defaults to the current directory).
    #[arg(short = 'C', long, global = true)]
    pub path: Option<PathBuf>,
    /// Data directory shared with Desktop; defaults to ~/.deepseek-code.
    #[arg(long, env = "DEEPSEEK_CODE_DATA_DIR", global = true)]
    pub data_dir: Option<PathBuf>,
    /// Exact model ID, legacy alias, or provider::model.
    #[arg(short, long, env = "CLAUDE_MODEL", global = true)]
    pub model: Option<String>,
    #[arg(long, global = true)]
    pub provider: Option<String>,
    /// Allow the explicitly configured lightweight-model router for a new conversation.
    #[arg(long, conflicts_with_all=["provider", "model", "resume", "continue_session"])]
    pub auto_route: bool,
    /// API endpoint for a temporary provider; never saved automatically.
    #[arg(long, env = "API_BASE_URL", global = true)]
    pub base_url: Option<String>,
    /// Environment variable containing the API key (avoid secrets in arguments).
    #[arg(long, global = true)]
    pub api_key_env: Option<String>,
    #[arg(long, value_enum, global = true)]
    pub api_format: Option<ApiFormatArg>,
    /// Run once, stream the answer to stdout, then exit.
    #[arg(long, global = true)]
    pub print: bool,
    /// Run once (short form of --print, before any subcommand).
    #[arg(short = 'p')]
    pub print_short: bool,
    #[arg(long, global = true)]
    pub no_interactive: bool,
    /// Non-interactive mode (short form, before any subcommand).
    #[arg(short = 'n')]
    pub no_interactive_short: bool,
    #[arg(long, value_enum, default_value = "text", global = true)]
    pub output_format: OutputFormat,
    /// Read plain text or newline-delimited SDK user messages from stdin.
    #[arg(long, value_enum, default_value = "text", global = true)]
    pub input_format: InputFormat,
    /// Echo accepted SDK user envelopes, including their UUIDs.
    #[arg(long, global = true)]
    pub replay_user_messages: bool,
    /// Add reference `stream_event` partial-message frames to stream-json output.
    #[arg(long, global = true)]
    pub include_partial_messages: bool,
    /// Route tool approvals to the SDK host; only `stdio` is supported.
    #[arg(long, global = true, value_parser = ["stdio"])]
    pub permission_prompt_tool: Option<String>,
    /// Maximum model/tool-loop iterations for each submitted user message.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), global = true)]
    pub max_turns: Option<u32>,
    /// Require a provider-enforced JSON result matching this JSON Schema string.
    #[arg(long, global = true)]
    pub json_schema: Option<String>,
    #[arg(long, value_enum, default_value = "ask", global = true)]
    pub permission_mode: PermissionModeArg,
    #[arg(skip)]
    pub permission_mode_explicit: bool,
    /// Bypass interactive permission prompts; explicit deny rules still apply.
    #[arg(long, global = true, conflicts_with = "permission_mode")]
    pub dangerously_skip_permissions: bool,
    /// Allow matching tools without confirmation for this launch; deny and Plan still apply.
    #[arg(long, visible_alias = "allowedTools", num_args = 1.., global = true)]
    pub allowed_tools: Vec<String>,
    /// Deny matching tools for this launch, including in bypass mode.
    #[arg(long, visible_alias = "disallowedTools", num_args = 1.., global = true)]
    pub disallowed_tools: Vec<String>,
    #[arg(short = 'r', long, global = true)]
    pub resume: Option<String>,
    /// Continue the most recently updated conversation in this project.
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    pub continue_session: bool,
    /// Copy the resumed conversation into a new session before continuing.
    #[arg(long, global = true)]
    pub fork_session: bool,
    /// UUID for a new conversation; existing IDs must be resumed explicitly.
    #[arg(long, conflicts_with_all=["resume", "continue_session", "fork_session"], value_parser=parse_session_id)]
    pub session_id: Option<String>,
    /// Enable experimental agent teams (TeamCreate/TeamDelete/TeamSpawn).
    #[arg(long = "agent-teams", global = true)]
    pub agent_teams: bool,
    /// Create an isolated managed Git worktree; omit NAME to generate one.
    #[arg(short='w',long,num_args=0..=1,default_missing_value="",conflicts_with_all=["resume","continue_session","fork_session"])]
    pub worktree: Option<String>,
    /// Human-readable title saved with this conversation.
    #[arg(long)]
    pub name: Option<String>,
    /// Additional trusted working directory. May be repeated.
    #[arg(long)]
    pub add_dir: Vec<PathBuf>,
    /// Connect automatically when exactly one editor extension matches the workspace.
    #[arg(long)]
    pub ide: bool,
    /// Flat terminal output without borders, animation, mouse capture or fullscreen.
    #[arg(long)]
    pub ax_screen_reader: bool,
    /// Enable diagnostics; optionally select categories (api,hooks,mcp,lsp,skills,permissions,cli).
    #[arg(short='d',long,num_args=0..=1,default_missing_value="")]
    pub debug: Option<String>,
    /// Append redacted diagnostics to this UTF-8 log file.
    #[arg(long)]
    pub debug_file: Option<PathBuf>,
    #[arg(long, value_parser = ["off", "low", "medium", "high", "xhigh", "max"], global = true)]
    pub effort: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), global = true)]
    pub max_tokens: Option<u32>,
    #[arg(long, global = true)]
    pub system_prompt: Option<String>,
    /// Read a UTF-8 system prompt from a file.
    #[arg(long, global = true, conflicts_with = "system_prompt")]
    pub system_prompt_file: Option<PathBuf>,
    /// Append instructions to the supplied system prompt.
    #[arg(long, global = true)]
    pub append_system_prompt: Option<String>,
    /// Append UTF-8 instructions from a file after the supplied system prompt.
    #[arg(long, global = true, conflicts_with = "append_system_prompt")]
    pub append_system_prompt_file: Option<PathBuf>,
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[arg(long)]
    pub info: bool,
}

fn parse_session_id(value: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(value).map(|id|id.to_string()).map_err(|_|"Session ID must be a UUID".into())
}

impl Cli {
    pub fn try_parse_with_mode_source<I, T>(args: I) -> Result<Self, clap::Error>
    where I: IntoIterator<Item=T>, T: Into<std::ffi::OsString> + Clone {
        use clap::{CommandFactory, FromArgMatches};
        let matches = Self::command().try_get_matches_from(args)?;
        let mut cli = Self::from_arg_matches(&matches)?;
        cli.permission_mode_explicit = matches.value_source("permission_mode") == Some(clap::parser::ValueSource::CommandLine);
        Ok(cli)
    }
    pub fn parse_with_mode_source() -> Self {
        Self::try_parse_with_mode_source(std::env::args_os()).unwrap_or_else(|error|error.exit())
    }
    pub fn normalize(&mut self) {
        if self.dangerously_skip_permissions {
            self.permission_mode = PermissionModeArg::Bypass;
        }
    }
    pub fn response_schema(&self) -> anyhow::Result<Option<serde_json::Value>> {
        use anyhow::Context;
        self.json_schema.as_ref().map(|text| {
            let schema = serde_json::from_str(text).context("--json-schema must be a valid JSON Schema string")?;
            claude_core::structured_output::validate_schema(&schema)?;
            Ok(schema)
        }).transpose()
    }

    pub fn system_prompt_text(&self) -> anyhow::Result<String> {
        use anyhow::Context;
        let mut prompt = match &self.system_prompt_file {
            Some(path) => std::fs::read_to_string(path)
                .with_context(|| format!("Cannot read UTF-8 system prompt: {}", path.display()))?,
            None => self.system_prompt.clone().unwrap_or_default(),
        };
        if let Some(append) = &self.append_system_prompt {
            if !prompt.is_empty() && !append.is_empty() {
                prompt.push_str("\n\n");
            }
            prompt.push_str(append);
        }
        if let Some(path) = &self.append_system_prompt_file {
            let append = std::fs::read_to_string(path)
                .with_context(|| format!("Cannot read UTF-8 appended system prompt: {}", path.display()))?;
            if !prompt.is_empty() && !append.is_empty() { prompt.push_str("\n\n"); }
            prompt.push_str(&append);
        }
        Ok(prompt)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InputFormat {
    Text,
    StreamJson,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApiFormatArg {
    Anthropic,
    Openai,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PermissionModeArg {
    #[value(alias = "default", alias = "ask_permissions", alias = "ask_permission")]
    Ask,
    #[value(alias = "acceptEdits", alias = "accept_edits", alias = "edits")]
    AcceptEdits,
    #[value(alias = "plan_mode")]
    Plan,
    #[value(alias = "bypassPermissions", alias = "bypass_permissions", alias = "always_allow", alias = "alwaysallow", alias = "zh")]
    Bypass,
    #[value(alias = "dontAsk", alias = "dont_ask")]
    DontAsk,
    /// Rust extension: requires an explicitly configured automatic classifier.
    Auto,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Manage this application's Anthropic account without provider setup.
    Auth { #[command(subcommand)] action: AuthCommands },
    Learning { #[command(subcommand)] action: crate::migration::LearningCommands },
    Performance { #[command(subcommand)] action: crate::migration::PerformanceCommands },
    Project { #[command(subcommand)] action: crate::migration::ProjectCommands },
    Cron { #[command(subcommand)] action: crate::migration::CronCommands },
    Ssh { config: PathBuf, operation: PathBuf, #[arg(long)] yes: bool },
    Remote { request: PathBuf, #[arg(long)] yes: bool },
    /// Show language server status or install one: lsp [status] | lsp install LANGUAGE [--yes].
    Lsp { action: Option<String>, language: Option<String>, #[arg(long)] yes: bool },
    Help { #[arg(long)] topic:Option<String>, command:Vec<String> },
    /// Install and run versioned Node.js/Python plugin bundles.
    Plugin { #[command(subcommand)] action: PluginCommands },
    /// Check a real update manifest; does not install or execute downloads.
    Update { #[arg(long, env="CLAUDE_UPDATE_MANIFEST")] url:String },
    /// Bounded HTTP diagnostics, using a temporary local server by default.
    StressTest {
        #[arg(short='c',long,default_value_t=4)] concurrency:usize,
        #[arg(short='i',long,default_value_t=10)] iterations:usize,
        #[arg(long)] endpoint:Option<String>,
        #[arg(long)] allow_remote:bool,
    },
    /// Run a named agent through the shared session, tools, and permissions.
    Agent {
        #[arg(long, default_value="general")] agent_type:String,
        #[arg(short, long)] prompt:String,
    },
    Agents { #[command(subcommand)] action:AgentCommands },
    MagicDocs {
        #[command(subcommand)] action: MagicCommands,
    },
    /// Manage personal memory shared with Desktop.
    Memory {
        #[command(subcommand)]
        action: MemoryCommands,
    },
    /// Capture real microphone audio or transcribe a recording with the configured backend.
    Voice {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(short = 'p', long)]
        push_to_talk: bool,
        #[arg(long)]
        continuous: bool,
        /// Send each transcript through the current shared engine session.
        #[arg(long)]
        send: bool,
    },
    VoiceConfig {
        endpoint: String,
        #[arg(long, default_value = "whisper-1")]
        model: String,
        #[arg(long, default_value = "VOICE_API_KEY")]
        credential: String,
        #[arg(long)]
        language: Option<String>,
    },
    TeamSync {
        #[command(subcommand)]
        action: TeamCommands,
    },
    Services {
        #[command(subcommand)]
        action: ServiceCommands,
    },
    /// Start an interactive session; `rust` remains a legacy alias.
    #[command(alias = "rust")]
    Repl {
        #[arg(short, long)]
        prompt: Option<String>,
    },
    /// Execute one coding task with the shared tool loop.
    Query {
        #[arg(short, long)]
        prompt: Option<String>,
    },
    Config {
        #[command(subcommand)]
        action: ConfigCommands,
    },
    Mcp {
        #[command(subcommand)]
        action: McpCommands,
    },
    Skills {
        #[command(subcommand)]
        action: SkillsCommands,
    },
    /// List or inspect persistent sessions shared with Desktop.
    Sessions {
        #[command(subcommand)]
        action: SessionCommands,
    },
    /// Create project instruction directories without overwriting files.
    Init {
        #[arg(short, long)]
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommands {
    Login,
    Logout,
    Status,
}

#[derive(Debug, Subcommand)]
pub enum MemoryCommands {
    /// Consolidate new local memories using the selected model, preserving originals.
    Dream,
    /// Force one consolidation, ignoring automatic time/count gates.
    AutoDream,
    DreamStatus,
    /// Configure automatic consolidation using a saved provider/model.
    DreamConfig {
        #[arg(long)] enabled: bool,
        #[arg(long, default_value_t = 24)] min_hours: u64,
        #[arg(long, default_value_t = 5)] min_memories: usize,
    },
    Status,
    /// Clear personal memory only. Requires explicit confirmation.
    Clear { #[arg(long)] yes: bool },
    /// Export all local memories; never overwrite an existing file.
    Export { output: PathBuf },
    /// Import a versioned archive or an old MemoryEntry JSON array.
    Import { input: PathBuf, #[arg(long)] preview: bool },
}

#[derive(Debug, Subcommand)]
pub enum MagicCommands {
    ContextConfig { project_id: String, #[arg(long)] enabled: bool },
    ImportLegacy {source:PathBuf,#[arg(long)] preview:bool},
    List,
    Check { file: PathBuf },
    /// Generate a persisted preview; leaves the source file unchanged.
    Update { id: String, #[arg(long)] context: String },
    Previews,
    Apply { id: String, #[arg(long)] yes: bool },
    /// Remove tracking and previews only, never document files.
    Clear,
    /// Opt one tracked document into automatic updates from matching personal memory.
    Auto { id: String, #[arg(long)] enabled: bool },
    /// Foreground host for this project; services stop magic-docs stops it.
    Start { #[arg(long, default_value_t=3600)] interval: u64 },
}

#[derive(Debug, Subcommand)]
pub enum AgentCommands {
    /// Preview or import one historical AgentDefinition under an explicit custom ID.
    ImportLegacy {
        source: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        preview: bool,
    },
    List,
    /// Load a custom JSON definition; source is not modified.
    Import {
        source: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum TeamCommands {
    /// Allow this project to send shared team memories to its selected model.
    Context { #[arg(long,action=clap::ArgAction::Set)] enabled:bool },
    Status,
    Auth {
        team_id: String,
        #[arg(long, env = "TEAM_SYNC_URL")]
        endpoint: String,
        #[arg(long, default_value = "TEAM_SYNC_TOKEN")]
        credential: String,
    },
    Sync,
    List {
        #[arg(long)]
        include_deleted: bool,
    },
    Create {
        title: String,
        #[arg(short, long)]
        content: String,
        #[arg(short, long)]
        tags: Option<String>,
    },
    Update {
        id: String,
        title: String,
        #[arg(short, long)]
        content: String,
        #[arg(short, long)]
        tags: Option<String>,
    },
    Delete {
        id: String,
    },
    Conflicts,
    Resolve {
        id: String,
        #[arg(long, value_parser = ["local", "remote"])]
        keep: String,
    },
    /// Import an explicit legacy .claude-code/team directory; source and tokens remain untouched.
    ImportLegacy {
        source: PathBuf,
        #[arg(long)]
        preview: bool,
    },
    /// Start a standalone protocol server. Operator supplies a token-to-team JSON map in an environment variable.
    Serve {
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: std::net::SocketAddr,
        #[arg(long, default_value = "TEAM_SERVER_TOKENS")]
        tokens_env: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommands {
    Status,
    /// Run a foreground host until Ctrl+C or a stop command from another process.
    Start {
        #[arg(default_value="team-sync",value_parser=["team-sync", "auto-dream"])]
        name: String,
        #[arg(long, default_value_t = 60)]
        interval: u64,
    },
    Stop {
        #[arg(default_value = "team-sync")]
        name: String,
    },
    TeamSync,
    Voice,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Show effective saved configuration with credentials redacted.
    Show,
    /// Set model, base_url, api_format, api_key, max_tokens, or permissions.auto for the CLI provider.
    Set { key: String, value: String },
    /// Remove only the CLI-managed provider and CLI preferences.
    Reset,
    /// Import an old .claude-code/settings.json; the source is read-only.
    ImportLegacy { source: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum McpCommands {
    /// Import old settings.json MCP definitions without starting processes.
    ImportLegacy {
        source: PathBuf,
        #[arg(long)]
        preview: bool,
    },
    Hosts,
    Restart { name:String, #[arg(long)] host:Option<String> },
    List,
    Add {
        name: String,
        command: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillsCommands {
    ImportLegacy { source: PathBuf, #[arg(long)] preview: bool },
    List,
    Search {
        query: String,
    },
    Help {
        skill: String,
    },
    Execute {
        skill: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCommands {
    /// Stop an idle persistent worker without opening its session.
    ShutdownWorker { id: String },
    /// Request cancellation of one task owned by this saved session.
    StopTask { id: String, task_id: String },
    /// Restore trust for one saved session; explicitly list every project root.
    Trust {
        id: String,
        #[arg(long = "folder", required = true)]
        folders: Vec<PathBuf>,
    },
    ImportLegacy {source:PathBuf,#[arg(long)] preview:bool},
    List,
    Show { id: String },
    /// Summarize a saved conversation while preserving its original transcript.
    Compact { id: String, #[arg(long, default_value = "")] instructions: String },
}

#[derive(Debug, Subcommand)]
pub enum PluginCommands {
    ImportLegacy { source: String, #[arg(long)] preview: bool },
    List,
    Install { source: String },
    Update { id: String, source: String },
    Enable { id: String },
    Disable { id: String },
    Search { query:String, #[arg(long,env="CLAUDE_PLUGIN_REGISTRY")] registry:String },
    Remove { id: String },
    /// Execute with the current user's permissions in the selected project.
    Run { id: String, command: String, #[arg(long,default_value="null")] input: String, #[arg(long)] yes: bool },
}

#[cfg(test)]
mod tests {
    #[test]
    fn permission_mode_tracks_explicit_ask_without_confusing_default() {
        assert!(!super::Cli::try_parse_with_mode_source(["dcl"]).unwrap().permission_mode_explicit);
        assert!(super::Cli::try_parse_with_mode_source(["dcl", "--permission-mode", "ask"]).unwrap().permission_mode_explicit);
    }
    use super::*;
    #[test]
    fn original_permission_flags_are_explicit_and_unambiguous() {
        let mut cli = Cli::try_parse_from(["dcl", "--dangerously-skip-permissions"]).unwrap();
        cli.normalize();
        assert!(matches!(cli.permission_mode, PermissionModeArg::Bypass));
        assert!(Cli::try_parse_from(["dcl", "--dangerously-skip-permissions", "--permission-mode", "plan"]).is_err());
        assert!(matches!(Cli::try_parse_from(["dcl", "--permission-mode", "acceptEdits"]).unwrap().permission_mode, PermissionModeArg::AcceptEdits));
        assert!(matches!(Cli::try_parse_from(["dcl", "--permission-mode", "bypassPermissions"]).unwrap().permission_mode, PermissionModeArg::Bypass));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append.md");
        std::fs::write(&path, "UTF-8 中文").unwrap();
        let cli = Cli::try_parse_from(["dcl", "--system-prompt", "Base", "--append-system-prompt-file", path.to_str().unwrap()]).unwrap();
        assert_eq!(cli.system_prompt_text().unwrap(), "Base\n\nUTF-8 中文");
    }
    #[test]
    fn sdk_input_and_turn_budget_preserve_short_print_prompt() {
        let cli = Cli::try_parse_from(["dcl", "-p", "initial", "--input-format", "stream-json", "--max-turns", "2"]).unwrap();
        assert_eq!(cli.prompt.as_deref(), Some("initial"));
        assert!(cli.print_short);
        assert_eq!(cli.input_format, InputFormat::StreamJson);
        assert_eq!(cli.max_turns, Some(2));
        assert!(Cli::try_parse_from(["dcl", "--max-turns", "0"]).is_err());
        assert!(Cli::try_parse_from(["dcl", "--input-format", "invalid"]).is_err());
    }

    #[test]
    fn prompt_sources_compose_and_reject_ambiguous_input() {
        let cli = Cli::try_parse_from([
            "dcl",
            "--system-prompt",
            "Base",
            "--append-system-prompt",
            "Extra",
        ])
        .unwrap();
        assert_eq!(cli.system_prompt_text().unwrap(), "Base\n\nExtra");
        assert!(Cli::try_parse_from([
            "dcl",
            "--system-prompt",
            "Base",
            "--system-prompt-file",
            "prompt.txt",
        ])
        .is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.txt");
        std::fs::write(&path, "中文 instructions").unwrap();
        let cli = Cli::try_parse_from([
            "dcl",
            "--system-prompt-file",
            path.to_str().unwrap(),
            "--append-system-prompt",
            "Extra",
        ])
        .unwrap();
        assert_eq!(
            cli.system_prompt_text().unwrap(),
            "中文 instructions\n\nExtra"
        );
        std::fs::write(&path, [0xff]).unwrap();
        assert!(cli.system_prompt_text().is_err());
        std::fs::remove_file(path).unwrap();
        assert!(cli.system_prompt_text().is_err());
    }

    #[test]
    fn continuation_is_exclusive_with_explicit_resume_and_auto_route() {
        assert!(
            Cli::try_parse_from(["dcl", "-c"])
                .unwrap()
                .continue_session
        );
        assert!(Cli::try_parse_from(["dcl", "-c", "--resume", "id"]).is_err());
        assert!(Cli::try_parse_from(["dcl", "-c", "--auto-route"]).is_err());
    }

    #[test]
    fn root_short_flags_do_not_shadow_subcommand_flags() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        assert!(
            Cli::try_parse_from(["dcl", "-p", "hello"])
                .unwrap()
                .print_short
        );
        assert!(
            matches!(Cli::try_parse_from(["dcl", "query", "-p", "hello"]).unwrap().command,
            Some(Commands::Query { prompt: Some(value) }) if value == "hello")
        );
        assert!(matches!(
            Cli::try_parse_from(["dcl", "stress-test", "-c", "2"])
                .unwrap()
                .command,
            Some(Commands::StressTest { concurrency: 2, .. })
        ));
    }
}
