//! Claude Design uses its own OAuth credential and explicit, durable upload plans.
#[path = "design_assets.rs"]
mod assets;
use super::anthropic_auth::{self as auth, AuthStatus, LoginFlow};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};
const CLIENT: &str = "59637612-477b-4836-a601-b0589eda7704";
const SCOPES: &str = "user:design:read user:design:write";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const API: &str = "https://api.anthropic.com";
const RPC: &str = "/anthropic.omelette.api.v1alpha.OmeletteService";
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
fn slot(root: &Path) -> PathBuf {
    root.join("design-auth")
}
pub fn status(root: &Path) -> Result<AuthStatus> {
    auth::status(&slot(root))
}
pub async fn begin_login() -> Result<LoginFlow> {
    auth::begin_scoped_login(CLIENT, SCOPES).await
}
fn validate_tokens(tokens: &auth::Tokens) -> Result<()> {
    ensure!(
        SCOPES
            .split_whitespace()
            .all(|s| tokens.scopes.iter().any(|v| v == s)),
        "Design authorization did not grant both read and write scopes"
    );
    ensure!(
        tokens.refresh_token.is_some(),
        "Design authorization has no refresh token"
    );
    Ok(())
}
pub async fn finish_login(root: &Path, flow: LoginFlow) -> Result<AuthStatus> {
    let tokens = flow.exchange(TOKEN_URL).await?;
    validate_tokens(&tokens)?;
    let _guard = LOCK.lock().await;
    auth::save(&slot(root), &tokens)?;
    status(root)
}
pub async fn logout(root: &Path) -> Result<()> {
    auth::logout(&slot(root)).await
}
async fn token(root: &Path) -> Result<String> {
    let _guard = LOCK.lock().await;
    let path = slot(root);
    let mut tokens = auth::load(&path)?.context("Design is not authorized; run /design-login")?;
    validate_tokens(&tokens)?;
    if tokens.expires_at <= chrono::Utc::now().timestamp() + 60 {
        let value=auth::post_token(TOKEN_URL,&json!({"grant_type":"refresh_token","refresh_token":tokens.refresh_token,"client_id":CLIENT,"scope":SCOPES})).await?;
        tokens = auth::tokens_from_response(value, Some(&tokens))?;
        validate_tokens(&tokens)?;
        auth::save(&path, &tokens)?;
    }
    Ok(tokens.access_token)
}
struct Client {
    http: reqwest::Client,
    origin: String,
    bearer: String,
}
impl Client {
    async fn open(root: &Path) -> Result<Self> {
        Self::new(API, token(root).await?, false)
    }
    fn new(origin: &str, bearer: String, fixture: bool) -> Result<Self> {
        let url = reqwest::Url::parse(origin)?;
        let allowed = url.scheme() == "https"
            && url.host_str() == Some("api.anthropic.com")
            && url.port_or_known_default() == Some(443);
        let local =
            fixture && cfg!(test) && url.scheme() == "http" && url.host_str() == Some("127.0.0.1");
        ensure!(
            (allowed || local)
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "Design credentials can only be sent to the first-party API"
        );
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(60))
                .build()?,
            origin: origin.trim_end_matches('/').into(),
            bearer,
        })
    }
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        ensure!(
            path.starts_with("/v1/design/") || path.starts_with(&format!("{RPC}/")),
            "Invalid Design API path"
        );
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.origin))
            .bearer_auth(&self.bearer)
            .header("anthropic-version", "2023-06-01")
            .header("X-Anthropic-Client", "claude-cli-design-sync");
        if let Some(body) = body {
            ensure!(
                serde_json::to_vec(body)?.len() <= 32 * 1024 * 1024,
                "Design request exceeds 32 MiB"
            );
            request = request.json(body);
        }
        let mut response = request.send().await.context("Design request failed")?;
        ensure!(
            response.status().is_success(),
            "Design request failed with HTTP {}",
            response.status().as_u16()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 32 * 1024 * 1024,
                "Design response exceeds 32 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            Ok(json!({}))
        } else {
            serde_json::from_slice(&bytes).context("Invalid Design JSON response")
        }
    }
    async fn rpc(&self, method: &str, body: &Value) -> Result<Value> {
        self.request(
            reqwest::Method::POST,
            &format!("{RPC}/{method}"),
            Some(body),
        )
        .await
    }
}
pub async fn consent(root: &Path, grant: bool) -> Result<Value> {
    Client::open(root)
        .await?
        .request(
            if grant {
                reqwest::Method::POST
            } else {
                reqwest::Method::DELETE
            },
            "/v1/design/consent",
            Some(&json!({"consent":"agent_design_projects"})),
        )
        .await
}
pub async fn grants(root: &Path) -> Result<Value> {
    Client::open(root)
        .await?
        .request(reqwest::Method::GET, "/v1/design/grants", None)
        .await
}
pub async fn grant(root: &Path, project: &str) -> Result<Value> {
    project_id(project)?;
    Client::open(root)
        .await?
        .request(
            reqwest::Method::POST,
            "/v1/design/grants",
            Some(&json!({"project_id":project})),
        )
        .await
}
pub async fn list(root: &Path) -> Result<Value> {
    Client::open(root)
        .await?
        .rpc(
            "ListOrgProjects",
            &json!({"type":"PROJECT_TYPE_DESIGN_SYSTEM"}),
        )
        .await
}
pub async fn project(root: &Path, id: &str) -> Result<Value> {
    project_id(id)?;
    Client::open(root)
        .await?
        .rpc("GetProject", &json!({"projectId":id}))
        .await
}
fn project_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).context("Expected a Design project UUID")?;
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadPlan {
    #[serde(default)]
    pub remote: Option<RemoteSnapshot>,
    #[serde(default)]
    pub deletes: Vec<String>,
    pub id: String,
    pub project_id: String,
    pub directory: PathBuf,
    pub files: Vec<File>,
    pub status: String,
    pub uploaded: usize,
    pub error: Option<String>,
}
fn safe(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !path.split('/').any(|s| {
            s.eq_ignore_ascii_case(".claude")
                || s.eq_ignore_ascii_case("CLAUDE.md")
                || s.eq_ignore_ascii_case(".git")
        })
}
fn hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn scan(root: &Path, dir: &Path, files: &mut Vec<File>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "Design bundles cannot contain symbolic links"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "Design bundles cannot contain reparse points"
            );
        }
        if meta.is_dir() {
            scan(root, &path, files)?;
        } else {
            ensure!(meta.is_file(), "Design bundle contains a non-regular file");
            let relative = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            ensure!(
                safe(&relative),
                "Reserved or unsafe Design bundle path: {relative}"
            );
            ensure!(
                meta.len() <= 12 * 1024 * 1024,
                "Design file exceeds 12 MiB: {relative}"
            );
            let bytes = std::fs::read(&path)?;
            files.push(File {
                path: relative,
                sha256: hash(&bytes),
                bytes: bytes.len() as u64,
            });
            ensure!(files.len() <= 10000, "Design bundle exceeds 10000 files");
        }
    }
    Ok(())
}
fn plan_path(root: &Path, id: &str) -> Result<PathBuf> {
    project_id(id)?;
    Ok(root.join("tasks/design-plans").join(format!("{id}.json")))
}
fn save_plan(root: &Path, plan: &UploadPlan) -> Result<()> {
    let path = plan_path(root, &plan.id)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer_pretty(&mut file, plan)?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn load_plan(root: &Path, id: &str) -> Result<UploadPlan> {
    Ok(serde_json::from_slice(&std::fs::read(plan_path(
        root, id,
    )?)?)?)
}
/// Plans prebuilt bundles only. This does not claim to compile React or validate renders.
pub fn plan(root: &Path, project: &str, directory: &Path) -> Result<UploadPlan> {
    project_id(project)?;
    let directory = directory.canonicalize()?;
    let mut files = Vec::new();
    scan(&directory, &directory, &mut files)?;
    for required in [
        "_ds_bundle.js",
        "styles.css",
        "_ds_sync.json",
        "_ds_needs_recompile",
    ] {
        ensure!(files.iter().any(|f|f.path==required),"Prebuilt Design bundle missing {required}; run the official converter and render validator first");
    }
    let header = std::fs::read_to_string(directory.join("_ds_bundle.js"))?;
    ensure!(
        header
            .lines()
            .next()
            .is_some_and(|l| l.contains("@ds-bundle")),
        "Missing @ds-bundle header"
    );
    let _: Value = serde_json::from_slice(&std::fs::read(directory.join("_ds_sync.json"))?)?;
    for file in files
        .iter()
        .filter(|f| f.path.starts_with("components/") && f.path.ends_with(".html"))
    {
        let html = std::fs::read_to_string(directory.join(&file.path))?;
        ensure!(
            html.lines()
                .next()
                .is_some_and(|line| line.starts_with("<!--")
                    && line.contains("@dsCard")
                    && line.contains("group=\"")),
            "Component preview has no @dsCard group header: {}",
            file.path
        );
        for extension in ["jsx", "d.ts", "prompt.md"] {
            let sibling = format!("{}.{}", file.path.trim_end_matches(".html"), extension);
            ensure!(
                files.iter().any(|f| f.path == sibling),
                "Component preview missing contract: {sibling}"
            );
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let plan = UploadPlan {
        remote: None,
        deletes: Vec::new(),
        id: uuid::Uuid::new_v4().to_string(),
        project_id: project.into(),
        directory,
        files,
        status: "planned".into(),
        uploaded: 0,
        error: None,
    };
    save_plan(root, &plan)?;
    Ok(plan)
}
pub async fn upload(root: &Path, id: &str) -> Result<UploadPlan> {
    let client = Client::open(root).await?;
    upload_with(root, id, &client).await
}
async fn upload_with(root: &Path, id: &str, client: &Client) -> Result<UploadPlan> {
    let mut plan = load_plan(root, id)?;
    ensure!(
        plan.status == "planned",
        "Upload plan was already started; create a fresh plan after checking remote state"
    );
    let mut current = Vec::new();
    scan(&plan.directory, &plan.directory, &mut current)?;
    current.sort_by(|a, b| a.path.cmp(&b.path));
    ensure!(
        serde_json::to_value(&current)? == serde_json::to_value(&plan.files)?,
        "Bundle changed since approval plan; generate a fresh plan"
    );
    let project = client
        .rpc("GetProject", &json!({"projectId":plan.project_id}))
        .await?;
    ensure!(
        project["type"] == "PROJECT_TYPE_DESIGN_SYSTEM",
        "Target is not a Design-system project"
    );
    let observed = remote_snapshot(client, &plan.project_id).await?;
    if let Some(expected) = &plan.remote {
        ensure!(
            expected == &observed,
            "Remote Design project changed since planning; create a fresh plan"
        );
    } else {
        ensure!(
            observed.paths.is_empty(),
            "Target contains files; use /design-sync remote-plan ID before upload"
        );
    }
    let claim = plan_path(root, id)?.with_extension("started");
    let _claim = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(claim)
        .context("Upload plan already claimed")?;
    plan.status = "uploading".into();
    save_plan(root, &plan)?;
    let result:Result<()>=async {
        let mut order:Vec<_>=plan.files.iter().collect();
        order.sort_by_key(|f|if f.path=="_ds_needs_recompile"{0}else if f.path=="_ds_sync.json"{2}else{1});
        for file in order{
            if file.path == "_ds_sync.json" && !plan.deletes.is_empty() {
                let deleted=client.rpc("DeleteFiles",&json!({"projectId":plan.project_id,"paths":plan.deletes})).await?;
                ensure!(deleted["deleted"].as_u64()==Some(plan.deletes.len() as u64), "Design API did not confirm all planned deletions");
                let sentinel=plan.files.iter().find(|f|f.path=="_ds_needs_recompile").context("Missing sentinel")?;
                let data=std::fs::read(plan.directory.join(&sentinel.path))?;
                use base64::Engine;
                client.rpc("WriteFiles",&json!({"projectId":plan.project_id,"files":[{"path":sentinel.path,"data":base64::engine::general_purpose::STANDARD.encode(data),"encoding":"base64"}],"deduplicate":false})).await?;
            }
            let path=plan.directory.join(&file.path).canonicalize()?;ensure!(path.starts_with(&plan.directory),"Bundle file escaped approved directory");
            let data=std::fs::read(path)?;ensure!(hash(&data)==file.sha256,"Bundle changed during upload");
            use base64::Engine;
            let written=client.rpc("WriteFiles",&json!({"projectId":plan.project_id,"files":[{"path":file.path,"data":base64::engine::general_purpose::STANDARD.encode(data),"encoding":"base64"}],"deduplicate":false})).await?;
            ensure!(written["files"].as_array().is_some_and(|f|f.len()==1),"Design API did not confirm the uploaded file");
            plan.uploaded+=1;save_plan(root,&plan)?;
        }
        let final_snapshot=remote_snapshot(client,&plan.project_id).await?;
        ensure!(plan.files.iter().filter(|f|f.path!="_ds_needs_recompile").all(|f|final_snapshot.paths.contains(&f.path)), "Remote file verification is incomplete");
        ensure!(plan.deletes.iter().all(|p|!final_snapshot.paths.contains(p)), "Remote deletion verification is incomplete");
        Ok(())
    }.await;
    match result {
        Ok(()) => plan.status = "files_uploaded".into(),
        Err(error) => {
            plan.status = "failed".into();
            plan.error = Some(format!("{error:#}"));
        }
    }
    save_plan(root, &plan)?;
    Ok(plan)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteSnapshot {
    pub paths: Vec<String>,
    pub sidecar_hash: Option<String>,
}
async fn remote_snapshot(client: &Client, project: &str) -> Result<RemoteSnapshot> {
    let mut paths = Vec::new();
    for _ in 0..50 {
        let page = client
            .rpc(
                "ListFiles",
                &json!({"projectId":project,"depth":-1,"offset":paths.len()}),
            )
            .await?;
        let entries = page["entries"]
            .as_array()
            .context("Design ListFiles response missing entries")?;
        for entry in entries {
            paths.push(
                entry["path"]
                    .as_str()
                    .context("Invalid remote file path")?
                    .to_owned(),
            );
        }
        if page["truncated"] != true {
            paths.sort();
            paths.dedup();
            let sidecar_hash = if paths.iter().any(|p| p == "_ds_sync.json") {
                let file = client
                    .rpc(
                        "GetFile",
                        &json!({"projectId":project,"path":"_ds_sync.json","raw":true}),
                    )
                    .await?;
                {
                    use base64::Engine;
                    let data = base64::engine::general_purpose::STANDARD.decode(
                        file["content"]
                            .as_str()
                            .context("Remote sidecar missing content")?,
                    )?;
                    Some(hash(&data))
                }
            } else {
                None
            };
            return Ok(RemoteSnapshot {
                paths,
                sidecar_hash,
            });
        }
        ensure!(
            !entries.is_empty(),
            "Truncated Design file listing made no progress"
        );
    }
    anyhow::bail!("Design file listing exceeded 50 pages")
}
pub async fn download_sidecar(root: &Path, project: &str, destination: &Path) -> Result<()> {
    project_id(project)?;
    let client = Client::open(root).await?;
    let response = client
        .rpc(
            "GetFile",
            &json!({"projectId":project,"path":"_ds_sync.json","raw":true}),
        )
        .await?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(
        response["content"]
            .as_str()
            .context("Missing sidecar content")?,
    )?;
    let _: Value = serde_json::from_slice(&bytes).context("Invalid remote sync sidecar")?;
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("Choose a new local snapshot path")?;
    file.write_all(&bytes)?;
    Ok(())
}

pub async fn remote_plan(root: &Path, id: &str) -> Result<UploadPlan> {
    let client = Client::open(root).await?;
    remote_plan_with(root, id, &client).await
}
async fn remote_plan_with(root: &Path, id: &str, client: &Client) -> Result<UploadPlan> {
    let mut plan = load_plan(root, id)?;
    ensure!(
        plan.status == "planned",
        "Only an unstarted plan can be updated"
    );
    let remote = remote_snapshot(&client, &plan.project_id).await?;
    plan.deletes.clear();
    if !remote.paths.is_empty() {
        ensure!(
            remote.sidecar_hash.is_some(),
            "Existing project has no sync sidecar; cannot reconcile it automatically"
        );
        let receipt: Value = serde_json::from_slice(&std::fs::read(
            plan.directory.join(".design-conversion.json"),
        )?)?;
        ensure!(receipt["remote_hash"].as_str()==remote.sidecar_hash.as_deref(), "Remote sidecar differs from the converter input; download it and rebuild before planning");
        let verdict: Value =
            serde_json::from_slice(&std::fs::read(plan.directory.join(".resync-verdict.json"))?)
                .context("Re-sync needs the converter's current verdict")?;
        ensure!(verdict["ok"] == true, "Re-sync converter validation failed");
        let deletes = verdict["upload"]["deletePaths"]
            .as_array()
            .context("Converter verdict has no reconciliation deletePaths")?;
        for value in deletes {
            let path = value.as_str().context("Invalid deletion path")?;
            ensure!(
                safe(path)
                    && remote.paths.iter().any(|p| p == path)
                    && !plan.files.iter().any(|f| f.path == path),
                "Unsafe, missing or conflicting remote deletion path"
            );
            plan.deletes.push(path.into());
        }
        plan.deletes.sort();
        plan.deletes.dedup();
    }
    plan.remote = Some(remote);
    save_plan(root, &plan)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Path as RoutePath, State},
        http::StatusCode,
        routing::post,
        Json, Router,
    };
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Fixture {
        seen: Arc<Mutex<Vec<(String, Value)>>>,
        fail: bool,
    }
    async fn rpc(
        State(state): State<Fixture>,
        RoutePath(method): RoutePath<String>,
        Json(value): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        state
            .seen
            .lock()
            .unwrap()
            .push((method.clone(), value.clone()));
        match method.as_str() {
            "GetProject" => (
                StatusCode::OK,
                Json(json!({"type":"PROJECT_TYPE_DESIGN_SYSTEM"})),
            ),
            "ListFiles" => {
                let seen = state.seen.lock().unwrap();
                let mut paths = std::collections::BTreeSet::new();
                for (method, body) in seen.iter() {
                    if method == "WriteFiles" && !state.fail {
                        for file in body["files"].as_array().unwrap() {
                            paths.insert(file["path"].as_str().unwrap().to_owned());
                        }
                    }
                    if method == "DeleteFiles" {
                        for path in body["paths"].as_array().unwrap() {
                            paths.remove(path.as_str().unwrap());
                        }
                    }
                }
                (
                    StatusCode::OK,
                    Json(
                        json!({"entries":paths.into_iter().map(|path|json!({"path":path})).collect::<Vec<_>>(),"truncated":false}),
                    ),
                )
            }
            "GetFile" => (
                StatusCode::OK,
                Json(json!({"content":"e30=","isBase64":false})),
            ),
            "WriteFiles" if state.fail => (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"fixture denial"})),
            ),
            "WriteFiles" => (StatusCode::OK, Json(json!({"files":value["files"]}))),
            "DeleteFiles" => (
                StatusCode::OK,
                Json(json!({"deleted":value["paths"].as_array().unwrap().len()})),
            ),
            _ => (StatusCode::NOT_FOUND, Json(json!({}))),
        }
    }
    fn bundle(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for (path, body) in [
            ("_ds_bundle.js", "/* @ds-bundle: {} */\nwindow.Library={};"),
            ("styles.css", "body{}"),
            ("_ds_sync.json", "{}"),
            ("_ds_needs_recompile", "{\"by\":\"design-sync-cli\"}"),
        ] {
            std::fs::write(dir.join(path), body).unwrap();
        }
    }
    #[test]
    fn rejects_wrong_host_reserved_paths_and_inference_only_tokens() {
        for host in [
            "https://evil.example",
            "https://api.anthropic.com.evil.example",
            "http://api.anthropic.com",
            "https://api.anthropic.com/x",
            "https://u@api.anthropic.com",
        ] {
            assert!(Client::new(host, "fixture".into(), false).is_err());
        }
        for path in [
            "../outside",
            ".claude/config",
            "x/CLAUDE.md",
            "C:/absolute",
            "a\\b",
        ] {
            assert!(!safe(path));
        }
        let tokens = auth::Tokens {
            access_token: "not-real".into(),
            refresh_token: Some("not-real".into()),
            expires_at: 0,
            scopes: vec!["user:inference".into()],
        };
        assert!(validate_tokens(&tokens).is_err());
    }
    #[test]
    fn dedicated_credentials_do_not_replace_inference_login() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = auth::Tokens {
            access_token: "design-fixture".into(),
            refresh_token: Some("design-refresh-fixture".into()),
            expires_at: chrono::Utc::now().timestamp() + 3600,
            scopes: SCOPES.split_whitespace().map(str::to_owned).collect(),
        };
        validate_tokens(&tokens).unwrap();
        auth::save(&slot(dir.path()), &tokens).unwrap();
        assert!(status(dir.path()).unwrap().authenticated);
        assert!(!auth::status(dir.path()).unwrap().authenticated);
    }
    #[tokio::test]
    async fn real_rpc_upload_orders_sentinel_and_sidecar_and_records_failures() {
        for fail in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("bundle");
            bundle(&source);
            let data = dir.path().join("data");
            let plan = plan(&data, &uuid::Uuid::new_v4().to_string(), &source).unwrap();
            let fixture = Fixture {
                seen: Arc::new(Mutex::new(Vec::new())),
                fail,
            };
            let seen = fixture.seen.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .route(&format!("{RPC}/{{method}}"), post(rpc))
                .with_state(fixture);
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = Client::new(&origin, "design-fixture".into(), true).unwrap();
            let result = upload_with(&data, &plan.id, &client).await.unwrap();
            assert_eq!(
                result.status,
                if fail { "failed" } else { "files_uploaded" }
            );
            assert_eq!(
                load_plan(&data, &plan.id).unwrap().uploaded,
                if fail { 0 } else { 4 }
            );
            let requests = seen.lock().unwrap();
            let writes: Vec<_> = requests.iter().filter(|(m, _)| m == "WriteFiles").collect();
            assert_eq!(writes[0].1["files"][0]["path"], "_ds_needs_recompile");
            if !fail {
                assert_eq!(
                    writes.last().unwrap().1["files"][0]["path"],
                    "_ds_sync.json"
                );
                assert_eq!(writes[0].1["files"][0]["encoding"], "base64");
            }
            assert!(upload_with(&data, &plan.id, &client).await.is_err());
            server.abort();
        }
    }
    #[tokio::test]
    async fn resync_plan_binds_converter_snapshot_and_rejects_remote_races() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bundle");
        bundle(&source);
        let data = dir.path().join("data");
        let plan = plan(&data, &uuid::Uuid::new_v4().to_string(), &source).unwrap();
        std::fs::write(
            source.join(".resync-verdict.json"),
            r#"{"ok":true,"upload":{"deletePaths":["obsolete.txt"]}}"#,
        )
        .unwrap();
        std::fs::write(
            source.join(".design-conversion.json"),
            json!({"remote_hash":hash(b"{}")}).to_string(),
        )
        .unwrap();
        let seen = Arc::new(Mutex::new(vec![(
            "WriteFiles".into(),
            json!({"files":[{"path":"_ds_sync.json"},{"path":"obsolete.txt"}]}),
        )]));
        let fixture = Fixture {
            seen: seen.clone(),
            fail: false,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(&format!("{RPC}/{{method}}"), post(rpc))
            .with_state(fixture);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::new(&origin, "design-fixture".into(), true).unwrap();
        let updated = remote_plan_with(&data, &plan.id, &client).await.unwrap();
        assert_eq!(updated.deletes, vec!["obsolete.txt"]);
        seen.lock().unwrap().push((
            "WriteFiles".into(),
            json!({"files":[{"path":"concurrent.txt"}]}),
        ));
        assert!(upload_with(&data, &plan.id, &client)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed since planning"));
        assert_eq!(load_plan(&data, &plan.id).unwrap().status, "planned");
        remote_plan_with(&data, &plan.id, &client).await.unwrap();
        let done = upload_with(&data, &plan.id, &client).await.unwrap();
        assert_eq!(done.status, "files_uploaded", "{:?}", done.error);
        let calls = seen.lock().unwrap();
        assert!(calls.iter().any(|(method, _)| method == "DeleteFiles"));
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "WriteFiles")
                .last()
                .unwrap()
                .1["files"][0]["path"],
            "_ds_sync.json"
        );
        server.abort();
    }
    #[tokio::test]
    async fn node_pipeline_records_real_child_verdict_and_rejects_failure() {
        let dir = tempfile::tempdir().unwrap();
        let converter = dir.path().join("converter");
        std::fs::create_dir(&converter).unwrap();
        let output = dir.path().join("bundle");
        let config = dir.path().join("config.json");
        std::fs::write(&config, "{}").unwrap();
        std::fs::write(converter.join("resync.mjs"),r#"import fs from 'node:fs';
const args=process.argv.slice(2);const out=args[args.indexOf('--out')+1];
fs.mkdirSync(out,{recursive:true});fs.writeFileSync(out+'/actual-child.txt','NODE_PIPELINE_SENTINEL');
process.stdout.write(JSON.stringify({ok:true,verification:{pendingGrade:['Button']},upload:{deletePaths:[]}}));"#).unwrap();
        let recipe = ConvertRecipe {
            workspace: dir.path().into(),
            converter: Some(converter.clone()),
            config,
            node_modules: dir.path().into(),
            output: output.clone(),
            remote: None,
            entry: None,
        };
        let result = convert(&dir.path().join("data"), &recipe)
            .await
            .unwrap_or_else(|error| {
                for entry in std::fs::read_dir(dir.path().join("data/design-conversions")).unwrap()
                {
                    eprintln!(
                        "CONVERTER LOG: {}",
                        std::fs::read_to_string(entry.unwrap().path().join("converter.log"))
                            .unwrap()
                    );
                }
                panic!("{error:#}")
            });
        assert_eq!(
            std::fs::read_to_string(output.join("actual-child.txt")).unwrap(),
            "NODE_PIPELINE_SENTINEL"
        );
        assert_eq!(result.verdict["verification"]["pendingGrade"][0], "Button");
        assert!(output.join(".design-conversion.json").exists());
        std::fs::write(
            converter.join("resync.mjs"),
            "process.stdout.write(JSON.stringify({ok:false}));process.exit(1)",
        )
        .unwrap();
        assert!(convert(&dir.path().join("data"), &recipe)
            .await
            .unwrap_err()
            .to_string()
            .contains("validation failed"));
    }
    #[tokio::test]
    async fn modified_bundle_is_rejected_before_any_network() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bundle");
        bundle(&source);
        let plan = plan(dir.path(), &uuid::Uuid::new_v4().to_string(), &source).unwrap();
        std::fs::write(source.join("styles.css"), "changed").unwrap();
        let client = Client::new("http://127.0.0.1:9", "fixture".into(), true).unwrap();
        assert!(upload_with(dir.path(), &plan.id, &client)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed"));
    }

    #[test]
    fn bundled_converter_materialization_is_repeatable_and_rejects_tampering() {
        let root = tempfile::tempdir().unwrap();
        let installed = install_converter_assets(root.path()).unwrap();
        assert!(installed.join("package-lock.json").is_file());
        assert!(installed.join("storybook/probe.mjs").is_file());
        assert_eq!(install_converter_assets(root.path()).unwrap(), installed);
        std::fs::write(installed.join("resync.mjs"), "modified").unwrap();
        assert!(install_converter_assets(root.path())
            .unwrap_err()
            .to_string()
            .contains("modified"));
    }

    #[tokio::test]
    #[ignore = "requires Node/npm, browser and CLAUDE_DESIGN_TEST_RECIPE fixture; installs pinned dependencies"]
    async fn bundled_converter_real_pipeline() {
        let recipe_file = std::env::var("CLAUDE_DESIGN_TEST_RECIPE").expect("fixture recipe path");
        let recipe: ConvertRecipe =
            serde_json::from_slice(&std::fs::read(recipe_file).unwrap()).unwrap();
        assert!(
            recipe.converter.is_none(),
            "test must exercise the embedded default"
        );
        let root = tempfile::tempdir().unwrap();
        setup_converter(root.path(), false).await.unwrap();
        let conversion = convert(root.path(), &recipe).await.unwrap();
        assert_eq!(conversion.verdict["ok"], true);
        assert!(conversion.output.join("_ds_bundle.js").is_file());
        assert!(conversion.output.join(".design-conversion.json").is_file());
        println!("{}", serde_json::to_string_pretty(&conversion).unwrap());
    }
}

/// A bundled or user-selected converter; Rust owns cancellation, receipts and
/// upload plans, while Node runs the TypeScript bundler and browser validators.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertRecipe {
    pub workspace: PathBuf,
    #[serde(default)]
    pub converter: Option<PathBuf>,
    pub config: PathBuf,
    pub node_modules: PathBuf,
    pub output: PathBuf,
    pub remote: Option<PathBuf>,
    #[serde(default)]
    pub entry: Option<PathBuf>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Conversion {
    pub id: String,
    pub output: PathBuf,
    pub verdict: Value,
    pub log: PathBuf,
}
fn node_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(disk) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(disk);
        }
    }
    path
}
pub async fn convert(root: &Path, recipe: &ConvertRecipe) -> Result<Conversion> {
    let workspace = node_path(recipe.workspace.canonicalize()?);
    let converter = node_path(match &recipe.converter {
        Some(path) => path.canonicalize()?,
        None => {
            let path = install_converter_assets(root)?;
            ensure!(
                path.join("node_modules/esbuild").is_dir(),
                "Run /design-sync setup to install the bundled converter dependencies first"
            );
            path.canonicalize()?
        }
    });
    let remote_hash = recipe
        .remote
        .as_ref()
        .map(|p| std::fs::read(p).map(|b| hash(&b)))
        .transpose()?;
    let config = node_path(recipe.config.canonicalize()?);
    let node_modules = node_path(recipe.node_modules.canonicalize()?);
    ensure!(
        converter.join("resync.mjs").is_file(),
        "Converter directory needs resync.mjs and its companion scripts"
    );
    ensure!(
        config.is_file() && node_modules.is_dir(),
        "Config and node_modules must exist"
    );
    let output = if recipe.output.is_absolute() {
        recipe.output.clone()
    } else {
        workspace.join(&recipe.output)
    };
    let output = if output.exists() {
        node_path(output.canonicalize()?)
    } else {
        node_path(
            output
                .parent()
                .context("Output needs a parent directory")?
                .canonicalize()?,
        )
        .join(
            output
                .file_name()
                .context("Output needs a directory name")?,
        )
    };
    ensure!(
        !output.exists() || output.is_dir(),
        "Output must be a bundle directory"
    );
    ensure!(
        !workspace.starts_with(&output)
            && !converter.starts_with(&output)
            && !node_modules.starts_with(&output),
        "Bundle output cannot contain the workspace, converter or dependency directory"
    );
    let id = uuid::Uuid::new_v4().to_string();
    let directory = root.join("diagnostics/design-conversions").join(&id);
    std::fs::create_dir_all(&directory)?;
    let log = directory.join("converter.log");
    let stdout = directory.join("verdict.json");
    let mut process = tokio::process::Command::new("node");
    process
        .current_dir(&workspace)
        .arg(converter.join("resync.mjs"))
        .args(["--config"])
        .arg(config)
        .arg("--node-modules")
        .arg(node_modules)
        .arg("--out")
        .arg(&output);
    if let Some(entry) = &recipe.entry {
        process.arg("--entry").arg(node_path(entry.canonicalize()?));
    }
    if let Some(remote) = &recipe.remote {
        process
            .arg("--remote")
            .arg(node_path(remote.canonicalize()?));
    }
    process
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&stdout)?)
        .stderr(std::fs::File::create(&log)?)
        .kill_on_drop(true);
    #[cfg(windows)]
    process.creation_flags(0x08000000);
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process
        .spawn()
        .context("Cannot start Node converter; install Node and the converter dependencies")?;
    let _tree =
        super::tool_hooks::ProcessTree::new(child.id().context("Missing converter process")?)?;
    let status = tokio::time::timeout(Duration::from_secs(1800), child.wait())
        .await
        .context("Design conversion timed out; see converter log")??;
    ensure!(
        std::fs::metadata(&stdout)?.len() <= 4 * 1024 * 1024,
        "Converter verdict exceeds 4 MiB"
    );
    let verdict: Value = serde_json::from_slice(&std::fs::read(&stdout)?)
        .context("Converter did not return a JSON verdict; see converter.log")?;
    ensure!(
        status.success() && verdict["ok"] == true,
        "Converter or validation failed; see {}",
        log.display()
    );
    let result = Conversion {
        id,
        output: output.canonicalize()?,
        verdict,
        log,
    };
    let current_remote_hash = recipe
        .remote
        .as_ref()
        .map(|p| std::fs::read(p).map(|b| hash(&b)))
        .transpose()?;
    ensure!(
        remote_hash == current_remote_hash,
        "Remote sidecar input changed during conversion; rebuild using a stable snapshot"
    );
    std::fs::write(
        result.output.join(".design-conversion.json"),
        serde_json::to_vec_pretty(&json!({"conversion_id":result.id,"remote_hash":remote_hash}))?,
    )?;
    std::fs::write(
        directory.join("receipt.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    Ok(result)
}

pub fn install_converter_assets(root: &Path) -> Result<PathBuf> {
    let directory = root.join("design-converter").join(assets::VERSION);
    for (relative, content) in assets::FILES {
        let path = directory.join(relative);
        if path.is_file() {
            ensure!(std::fs::read(&path)?==content.as_bytes(), "Bundled converter resource was modified: {}; choose an explicit converter directory to use a custom version",path.display());
            continue;
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?;
        file.write_all(content.as_bytes())?;
    }
    Ok(directory)
}
pub async fn setup_converter(root: &Path, browser: bool) -> Result<PathBuf> {
    let directory = install_converter_assets(root)?;
    let log = directory.join("install.log");
    let mut command = tokio::process::Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" });
    command
        .current_dir(&directory)
        .args(["ci", "--no-audit", "--no-fund"])
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::OpenOptions::new().append(true).open(&log)?)
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("Node/npm are required for Design converter setup")?;
    let _tree =
        super::tool_hooks::ProcessTree::new(child.id().context("Missing installer process")?)?;
    ensure!(
        tokio::time::timeout(Duration::from_secs(1800), child.wait())
            .await
            .context("Converter install timed out")??
            .success(),
        "Converter install failed; see {}",
        log.display()
    );
    if browser {
        let mut command = tokio::process::Command::new("node");
        command
            .current_dir(&directory)
            .arg(node_path(directory.canonicalize()?).join("node_modules/playwright/cli.js"))
            .args(["install", "chromium"])
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::OpenOptions::new().append(true).open(&log)?)
            .stderr(std::fs::OpenOptions::new().append(true).open(&log)?)
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let _tree =
            super::tool_hooks::ProcessTree::new(child.id().context("Missing browser installer")?)?;
        ensure!(
            tokio::time::timeout(Duration::from_secs(1800), child.wait())
                .await
                .context("Browser install timed out")??
                .success(),
            "Browser installation failed; see {}",
            log.display()
        );
    }
    Ok(directory)
}
