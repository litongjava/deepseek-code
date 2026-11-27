//! The provider selected by a frontend can differ from the persistent host's
//! startup environment. Capture provider environment inputs per admission, including removals.
use crate::binding_store::{Observed, Write};
use anyhow::{ensure, Context, Result};
use deepseek_code_core::native_engine::provider_manager::ResolvedProvider;
use deepseek_code_core::services::session_worker::{self, Request};
use deepseek_code_core::{
    db::session_store::Stores, native_engine::session_binding::BindingSnapshot, runtime::RuntimePaths,
};
use std::{collections::BTreeMap, ffi::OsString, path::Path};
pub const SNAPSHOT_ENV: &str = "DEEPSEEK_CODE_SESSION_BINDING";

// Standalone provider selection in config::select and legacy_api_config.
// Nested independent CLIs must see the submitting frontend's public inputs.
const CLI_ENV: &[&str] = &[
    "CLAUDE_MODEL",
    "API_BASE_URL",
    "OPENAI_BASE_URL",
    "ANTHROPIC_BASE_URL",
    "DEEPSEEK_BASE_URL",
    "XAI_BASE_URL",
    "DASHSCOPE_BASE_URL",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "DEEPSEEK_API_KEY",
    "XAI_API_KEY",
    "DASHSCOPE_API_KEY",
];
type Environment = BTreeMap<String, OsString>;

fn variables(builtin: Option<&str>) -> &'static [&'static str] {
    match builtin {
        Some("amazon-bedrock") => &[
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_PROFILE",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_BEARER_TOKEN_BEDROCK",
        ],
        Some("google-vertex") => &[
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "GOOGLE_OAUTH_ACCESS_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_API_KEY",
        ],
        Some("azure-openai-responses") => &[
            "AZURE_OPENAI_BASE_URL",
            "AZURE_OPENAI_RESOURCE_NAME",
            "AZURE_OPENAI_API_VERSION",
        ],
        Some("cloudflare-ai-gateway" | "cloudflare-workers-ai") => {
            &["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"]
        }
        _ => &[],
    }
}

// A new submission has the same provider environment a newly spawned worker
// would inherit. Preserve still-exported adapter variables used by other tools,
// and remove variables the submitting frontend no longer exports.
fn public_variables() -> impl Iterator<Item = &'static str> {
    CLI_ENV.iter().copied().chain(
        [
            "amazon-bedrock",
            "google-vertex",
            "azure-openai-responses",
            "cloudflare-ai-gateway",
        ]
        .into_iter()
        .flat_map(|id| variables(Some(id)).iter().copied()),
    )
}

fn capture(builtin: Option<&str>) -> Environment {
    variables(builtin)
        .iter()
        .filter_map(|&name| {
            let value = std::env::var_os(name).or_else(|| match name {
                "AWS_SHARED_CREDENTIALS_FILE" => {
                    Some(dirs::home_dir()?.join(".aws/credentials").into_os_string())
                }
                "GOOGLE_APPLICATION_CREDENTIALS" => Some(
                    dirs::config_dir()?
                        .join("gcloud/application_default_credentials.json")
                        .into_os_string(),
                ),
                _ => None,
            })?;
            Some((name.to_owned(), value))
        })
        .collect()
}

pub fn snapshot(
    paths: &RuntimePaths,
    selection: crate::config::Selection,
    inherit_url: bool,
) -> Result<BindingSnapshot> {
    let config = paths.read_config()?;
    let mut environment = BTreeMap::new();
    let catalog_keys: Vec<_> = selection
        .providers
        .iter()
        .filter_map(|p| p.builtin_id.as_deref())
        .map(deepseek_code_core::native_engine::provider_runtime::credential_env)
        .collect();
    let mut names: Vec<&str> = public_variables().collect();
    names.extend(catalog_keys.iter().map(String::as_str));
    names.extend(
        config
            .providers
            .iter()
            .filter_map(|p| p.settings.get("apiKeyEnv")?.as_str()),
    );
    for name in names {
        if !name.is_empty() && !name.contains(['=', '\0']) {
            environment.insert(name.to_owned(), std::env::var_os(name));
        }
    }
    for id in ["amazon-bedrock", "google-vertex"] {
        environment.extend(capture(Some(id)).into_iter().map(|(k, v)| (k, Some(v))));
    }
    if !inherit_url {
        environment.insert("API_BASE_URL".into(), None);
    }
    let state = BindingSnapshot {
        generation: 0,
        observation_only: false,
        provider_id: selection.provider_id,
        model: selection.model,
        providers: selection.providers,
        environment,
    };
    state.active()?;
    Ok(state)
}

/// Observe never publishes. First try the local inputs against the durable
/// fingerprint, then the live worker. If neither has the private launch inputs,
/// the terminal can still observe the session but cannot use another account.
pub async fn observe(
    paths: &RuntimePaths,
    db: &Stores,
    id: &str,
    local: BindingSnapshot,
) -> Result<BindingSnapshot> {
    observe_row(paths, id, db.observed_binding(id)?, local).await
}

pub(super) async fn observe_row(
    paths: &RuntimePaths,
    id: &str,
    row: Observed,
    mut local: BindingSnapshot,
) -> Result<BindingSnapshot> {
    if row.pending.is_some() {
        eprintln!("Provider switch pending: the legacy worker continues its current job. Use /continue to observe.");
    }
    if row.provider.is_empty() || row.model.is_empty() {
        local.generation = row.generation;
        return Ok(local);
    }
    if let Some(state) = resident(paths, id, &row).await? {
        return Ok(state);
    }
    restore_observed(paths, row, local)
}

/// An unstamped job observes the latest revision without publishing. Sessions
/// predating binding revisions still take their transport from the job itself.
pub(crate) async fn observe_worker(
    paths: &RuntimePaths,
    db: &Stores,
    id: &str,
    host: Option<&deepseek_code_core::native_engine::session_binding::SessionBinding>,
    local: impl FnOnce() -> Result<BindingSnapshot>,
) -> Result<BindingSnapshot> {
    let row = db.observed_binding(id)?;
    ensure!(
        row.pending.is_none(),
        "Provider switch pending; use /continue to observe the legacy job"
    );
    if row.generation == 0 && row.digest.is_none() {
        let mut local = local()?;
        local.generation = row.generation;
        return Ok(local);
    }
    if let Some(host) = host {
        if let Some(state) = host.read() {
            if !state.observation_only && matches_revision(&row, &state)? {
                return Ok((*state).clone());
            }
        }
    } else if let Some(state) = resident(paths, id, &row).await? {
        return Ok(state);
    }
    let local = local().or_else(|error| {
        let saved = crate::interactive_config::select(paths, &row.provider, &row.model)
            .map_err(|_| error)?;
        snapshot(paths, saved, false)
    })?;
    restore_observed(paths, row, local)
}

fn restore_observed(
    paths: &RuntimePaths,
    row: Observed,
    mut local: BindingSnapshot,
) -> Result<BindingSnapshot> {
    local.generation = row.generation;
    if local.provider_id == row.provider
        && local.model == row.model
        && row.restore_options(&mut local).is_ok()
        && matches_revision(&row, &local)?
    {
        local.observation_only = false;
        return Ok(local);
    }
    if let Ok(selection) = crate::interactive_config::select(paths, &row.provider, &row.model) {
        let mut saved = snapshot(paths, selection, false)?;
        saved.generation = row.generation;
        if row.restore_options(&mut saved).is_ok() && matches_revision(&row, &saved)? {
            return Ok(saved);
        }
        local = saved;
    } else {
        local.provider_id = row.provider;
        local.model = row.model;
        local
            .active()
            .context("Saved session provider unavailable")?;
    }
    local.observation_only = true;
    Ok(local)
}

pub(crate) async fn resident(
    paths: &RuntimePaths,
    id: &str,
    row: &Observed,
) -> Result<Option<BindingSnapshot>> {
    if row.pending.is_some() || uuid::Uuid::parse_str(id).is_err() {
        return Ok(None);
    }
    let Some(worker) = session_worker::current(&paths.data_dir, id)? else {
        return Ok(None);
    };
    if worker.status != "stopped" {
        if let Ok(reply) = session_worker::call(
            &worker,
            &Request::GetBinding {
                generation: row.generation,
            },
        )
        .await
        {
            if let Some(state) = reply.snapshot {
                if matches_revision(row, &state)? {
                    return Ok(Some(state));
                }
            }
        }
    }
    Ok(None)
}

fn matches_revision(row: &Observed, state: &BindingSnapshot) -> Result<bool> {
    Ok(state.generation == row.generation
        && state.provider_id == row.provider
        && state.model == row.model
        && row.matches(state)?)
}

/// Keep a launch-only snapshot reachable across exec through the user's worker
/// socket. A stale frontend transfers only its identity and observed generation.
pub async fn retain_for_transfer(session: &crate::session::Session) -> Result<()> {
    let state = session.binding_state();
    let row = session.store.db.observed_binding(&session.id)?;
    if row.pending.is_some() || state.observation_only || !matches_revision(&row, &state)? {
        return Ok(());
    }
    if resident(&session.paths, &session.id, &row).await?.is_some() {
        return Ok(());
    }
    // A legacy worker may still own an admitted turn. Leave it alone.
    if let Some(worker) = session_worker::current(&session.paths.data_dir, &session.id)? {
        if worker.status != "stopped"
            && session_worker::call(&worker, &Request::Ping)
                .await
                .is_ok_and(|reply| reply.generation_supported != Some(true))
        {
            return Ok(());
        }
    }
    let worker = session_worker::ensure_worker(
        &session.paths.data_dir,
        &std::env::current_exe()?,
        &session.id,
        &session.workspace,
    )
    .await?;
    if let Err(error) = session_worker::call(
        &worker,
        &Request::SetBinding {
            binding: (*state).clone(),
        },
    )
    .await
    {
        if session.store.db.observed_binding(&session.id)?.generation == row.generation {
            return Err(error.context("Cannot transfer the private session binding"));
        }
    }
    Ok(())
}

/// Commit the selector and its version together. A failed selector update rolls both back.
pub async fn commit(
    root: &Path,
    db: &std::sync::Arc<Stores>,
    id: &str,
    expected: u64,
    state: &mut BindingSnapshot,
    requester: &str,
    picker_effort: Option<Option<&str>>,
) -> Result<bool> {
    let shard=db.open_session(id)?;
    let worker = if uuid::Uuid::parse_str(id).is_ok() {
        session_worker::current(root, id)?
    } else {
        None
    };
    let live = worker.filter(|w| {
        w.status != "stopped"
            && w.child_pid
                .is_some_and(|pid| !session_worker::process_exited(pid))
    });
    let legacy = if let Some(worker) = &live {
        match session_worker::call(worker, &Request::Ping).await {
            Ok(reply) => reply.generation_supported != Some(true),
            Err(_) if worker.child_pid.is_some_and(session_worker::process_exited) => false,
            Err(e) => return Err(e),
        }
    } else {
        false
    };
    let pending = live.as_ref().filter(|_| legacy).map(|w| w.id.as_str());
    let reserved = pending.map(|id| (id, live.as_ref().and_then(|w| w.child_pid)));
    let change = shard.publish_provider(expected,state,reserved,requester,picker_effort)?;
    match change {
        Write::Published(generation) => state.generation = generation,
        Write::Unchanged => {
            state.generation = expected;
            return Ok(true);
        }
        Write::Conflict => {
            let paths = RuntimePaths::new(root.to_path_buf())?;
            *state = observe(&paths, db, id, state.clone()).await?;
            return Ok(false);
        }
    }
    if let Some(worker) = live {
        if legacy {
            if session_worker::shutdown_worker(&worker).await.is_ok()
                || worker.child_pid.is_some_and(session_worker::process_exited)
            {
                finish_retirement(db, &worker, state.generation)?;
            } else {
                eprintln!("Provider switch pending: the legacy worker continues its current job on its original provider. Use /continue to observe. New requests wait for the worker to retire.");
                // The observer may disconnect again before the old job completes.
                // The retirement helper owns no transport and inherits no credentials.
                if let Err(error) = start_retirement(&worker, state.generation) {
                    eprintln!("Detached worker retirement unavailable ({error}); this frontend will finish the cutover.");
                    let db = db.clone();
                    let generation = state.generation;
                    tokio::spawn(async move {
                        if let Err(error) = retire_worker(&db, &worker, generation).await {
                            eprintln!("Provider activation failed: {error:#}");
                        }
                    });
                }
            }
        } else if let Err(error) = session_worker::call(
            &worker,
            &Request::SetBinding {
                binding: state.clone(),
            },
        )
        .await
        {
            // The durable generation fences its old transport. The next explicit
            // submission can install this same generation after a lost acknowledgement.
            eprintln!("Provider selection saved. Worker refresh pending: {error}");
        }
    }
    Ok(true)
}

const RETIRE_ENV: &str = "DEEPSEEK_CODE_RETIRE_SESSION_WORKER";

fn finish_retirement(
    db: &Stores,
    worker: &session_worker::Worker,
    generation: u64,
) -> Result<()> {
    db.open_session(&worker.session_id)?.activate_provider(&worker.id,generation)
}

fn start_retirement(worker: &session_worker::Worker, generation: u64) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe()?);
    command
        .env_clear()
        .env(RETIRE_ENV, session_worker::directory(worker))
        .env("DEEPSEEK_CODE_RETIRE_GENERATION", generation.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        command.creation_flags(0x08000000 | 0x00000200);
    }
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub async fn retire_from_env() -> Result<bool> {
    let Some(dir) = std::env::var_os(RETIRE_ENV) else {
        return Ok(false);
    };
    let dir = std::path::PathBuf::from(dir);
    let worker = session_worker::read_worker(&dir)?;
    ensure!(
        session_worker::directory(&worker).canonicalize()? == dir.canonicalize()?,
        "Invalid retiring worker directory"
    );
    let db = RuntimePaths::new(worker.root.clone())?.open_database()?;
    let generation = std::env::var("DEEPSEEK_CODE_RETIRE_GENERATION")?.parse()?;
    retire_worker(&db, &worker, generation).await?;
    Ok(true)
}

async fn retire_worker(
    db: &Stores,
    worker: &session_worker::Worker,
    generation: u64,
) -> Result<()> {
    loop {
        let state = db.observed_binding(&worker.session_id)?;
        let reserved = state.generation == generation && state.pending.as_deref() == Some(worker.id.as_str());
        if !reserved {
            return Ok(());
        }
        if worker.child_pid.is_none_or(session_worker::process_exited)
            || session_worker::shutdown_worker(&worker).await.is_ok()
        {
            finish_retirement(db, worker, generation)?;
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub fn validate_worker_snapshot(
    paths: &RuntimePaths,
    session: &str,
    state: &BindingSnapshot,
) -> Result<()> {
    let config = paths.read_config()?;
    let catalog_keys: Vec<_> = paths
        .providers()?
        .into_iter()
        .filter_map(|p| p.builtin_id)
        .map(|id| deepseek_code_core::native_engine::provider_runtime::credential_env(&id))
        .collect();
    let mut names: std::collections::HashSet<&str> = public_variables().collect();
    names.extend(catalog_keys.iter().map(String::as_str));
    names.extend(
        config
            .providers
            .iter()
            .filter_map(|p| p.settings.get("apiKeyEnv")?.as_str()),
    );
    validate_environment(&state.environment, &names)?;
    let db = paths.open_database()?;
    let observed = db.observed_binding(session)?;
    ensure!(observed.pending.is_none(), "Provider switch pending");
    ensure!(
        matches_revision(&observed, state)?,
        "Worker snapshot does not match the committed binding"
    );
    state.active()?;
    Ok(())
}

// Only fixed provider variables may change the host process. Arbitrary configured
// credential names belong to the external dcl child's launch environment.
fn validate_environment(
    values: &BTreeMap<String, Option<OsString>>,
    names: &std::collections::HashSet<&str>,
) -> Result<()> {
    ensure!(
        values
            .iter()
            .all(|(key, value)| names.contains(key.as_str())
                && !key.contains(['=', '\0'])
                && !key.is_empty()
                && value
                    .as_ref()
                    .is_none_or(|s| !s.as_encoded_bytes().contains(&0))),
        "Unsupported worker provider environment"
    );
    Ok(())
}

pub fn install_public(state: &BindingSnapshot) {
    install_public_environment(&state.environment);
}

fn install_public_environment(values: &BTreeMap<String, Option<OsString>>) {
    for name in public_variables() {
        match values.get(name).and_then(Option::as_ref) {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
}

#[cfg(test)]
fn install_environment(value: &str) -> Result<()> {
    let values: BTreeMap<String, String> = serde_json::from_str(value)?;
    let values = values
        .into_iter()
        .map(|(k, v)| (k, Some(OsString::from(v))))
        .collect();
    validate_environment(&values, &public_variables().collect())?;
    install_public_environment(&values);
    Ok(())
}

/// An opaque, memory-only comparison token. Include the selector because
/// native adapters may select a different transport for individual models.
pub fn identity(selection: &ResolvedProvider) -> String {
    let p = &selection.provider;
    let input = serde_json::to_vec(&(
        &p.id,
        &p.base_url,
        &p.api_format,
        &p.api_key,
        &p.builtin_id,
        &selection.model.id,
        &selection.model.api,
        capture(p.builtin_id.as_deref()),
    ))
    .expect("serializable provider transport");
    ring::digest::digest(&ring::digest::SHA256, &input)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_cloud_binding_replaces_and_removes_only_adapter_inputs() {
        const NAME: &str =
            "worker_binding::tests::worker_cloud_binding_replaces_and_removes_only_adapter_inputs";
        if std::env::var_os("I4_CLOUD_FIXTURE").is_none() {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME])
                .env_clear()
                .env("I4_CLOUD_FIXTURE", "1")
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("AWS_REGION", "old-region")
                .env("AWS_ACCESS_KEY_ID", "old-dummy")
                .env("AWS_SESSION_TOKEN", "old-dummy-token")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let builtin = Some("amazon-bedrock");
        install_environment(r#"{"AWS_REGION":"new-region","AWS_ACCESS_KEY_ID":"new-dummy"}"#)
            .unwrap();
        assert_eq!(std::env::var("AWS_REGION").unwrap(), "new-region");
        assert_eq!(std::env::var("AWS_ACCESS_KEY_ID").unwrap(), "new-dummy");
        assert!(std::env::var_os("AWS_SESSION_TOKEN").is_none());
        assert!(install_environment(r#"{"AWS_REGION":"partial","PATH":"/bad"}"#).is_err());
        assert_eq!(std::env::var("AWS_REGION").unwrap(), "new-region");
        assert_eq!(capture(builtin)["AWS_ACCESS_KEY_ID"], "new-dummy");
        std::env::set_var("API_BASE_URL", "http://127.0.0.1:1");
        install_environment("{}").unwrap();
        assert!(std::env::var_os("API_BASE_URL").is_none());
        assert!(std::env::var_os("AWS_ACCESS_KEY_ID").is_none());
    }
}
