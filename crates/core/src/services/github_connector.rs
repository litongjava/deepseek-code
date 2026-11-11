//! GitHub connector: account connection, repository browsing, and copying
//! selected files into a conversation's workspace.
//!
//! Connect with OAuth (when the app has a GitHub OAuth client secret) or with
//! a personal access token. The token is stored in the data directory,
//! readable only by the current user.

use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_CLIENT_ID: &str = "Ov23liWiTL6v74GsI2U7";
const TOKEN_FILE: &str = "github-token.json";
const STATE_LIFETIME: Duration = Duration::from_secs(600);
const USER_AGENT: &str = "DeepSeek-Code";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredToken {
    pub access_token: String,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

/// A selected file or folder; folders include every file below them.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub path: String,
    #[serde(default)]
    pub is_folder: bool,
}

pub struct GithubConnector {
    data_dir: PathBuf,
    api_base: String,
    oauth_base: String,
    client_id: String,
    client_secret: Option<String>,
    http: reqwest::Client,
    pending_states: Mutex<HashMap<String, (Instant, String)>>,
}

impl GithubConnector {
    /// OAuth credentials come from `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET`.
    pub fn new(data_dir: PathBuf) -> Self {
        let client_id = std::env::var("GITHUB_CLIENT_ID")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let client_secret = std::env::var("GITHUB_CLIENT_SECRET")
            .ok()
            .filter(|v| !v.trim().is_empty());
        Self::with_endpoints(
            data_dir,
            "https://api.github.com",
            "https://github.com",
            client_id,
            client_secret,
        )
    }

    pub fn with_endpoints(
        data_dir: PathBuf,
        api_base: &str,
        oauth_base: &str,
        client_id: Option<String>,
        client_secret: Option<String>,
    ) -> Self {
        Self {
            data_dir,
            api_base: api_base.trim_end_matches('/').to_string(),
            oauth_base: oauth_base.trim_end_matches('/').to_string(),
            client_id: client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string()),
            client_secret,
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            pending_states: Mutex::new(HashMap::new()),
        }
    }

    fn token_path(&self) -> PathBuf {
        self.data_dir.join(TOKEN_FILE)
    }

    pub fn token(&self) -> Option<StoredToken> {
        let text = std::fs::read_to_string(self.token_path()).ok()?;
        serde_json::from_str::<StoredToken>(&text)
            .ok()
            .filter(|t| !t.access_token.is_empty())
    }

    fn save_token(&self, token: &StoredToken) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        let path = self.token_path();
        let text = serde_json::to_string_pretty(token)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options.open(&path)?.write_all(text.as_bytes())?;
        Ok(())
    }

    pub fn disconnect(&self) -> Result<()> {
        match std::fs::remove_file(self.token_path()) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    pub fn status(&self) -> Value {
        match self.token() {
            Some(token) => json!({
                "connected": true,
                "user": {"login": token.login, "avatar_url": token.avatar_url, "name": token.name},
                "oauthAvailable": self.client_secret.is_some(),
            }),
            None => json!({"connected": false, "oauthAvailable": self.client_secret.is_some()}),
        }
    }

    /// The authorize URL; `redirect_uri` is this Bridge's callback route.
    pub fn auth_url(&self, redirect_uri: &str) -> Result<Value> {
        ensure!(
            self.client_secret.is_some(),
            "GitHub OAuth is not configured (set GITHUB_CLIENT_ID and GITHUB_CLIENT_SECRET); connect with a personal access token instead"
        );
        let state = uuid::Uuid::new_v4().simple().to_string();
        let mut states = self.pending_states.lock().unwrap();
        states.retain(|_, (created, _)| created.elapsed() < STATE_LIFETIME);
        states.insert(state.clone(), (Instant::now(), redirect_uri.to_string()));
        let url = format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}&scope=repo,read:user&state={}",
            self.oauth_base,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            state
        );
        Ok(json!({"url": url, "state": state}))
    }

    /// Completes OAuth. The state must come from `auth_url` of this process,
    /// so another site cannot attach its own GitHub account.
    pub async fn complete_oauth(&self, code: &str, state: &str) -> Result<StoredToken> {
        let redirect_uri = {
            let mut states = self.pending_states.lock().unwrap();
            let (created, redirect) = states
                .remove(state)
                .context("Unknown or expired OAuth state")?;
            ensure!(
                created.elapsed() < STATE_LIFETIME,
                "OAuth state expired; connect again"
            );
            redirect
        };
        let secret = self
            .client_secret
            .clone()
            .context("GitHub OAuth is not configured")?;
        let response: Value = self
            .http
            .post(format!("{}/login/oauth/access_token", self.oauth_base))
            .header("Accept", "application/json")
            .json(&json!({"client_id": self.client_id, "client_secret": secret, "code": code, "redirect_uri": redirect_uri}))
            .send()
            .await?
            .json()
            .await?;
        let access_token = response["access_token"]
            .as_str()
            .map(str::to_string)
            .with_context(|| {
                format!(
                    "GitHub refused the authorization: {}",
                    response["error_description"]
                        .as_str()
                        .or(response["error"].as_str())
                        .unwrap_or("unknown error")
                )
            })?;
        self.connect_with_token(&access_token).await
    }

    /// Validates a token (OAuth or personal access token) and saves it.
    pub async fn connect_with_token(&self, access_token: &str) -> Result<StoredToken> {
        let access_token = access_token.trim();
        ensure!(!access_token.is_empty(), "Token is empty");
        let user = self
            .api(access_token, "/user")
            .await
            .context("GitHub rejected the token")?;
        let token = StoredToken {
            access_token: access_token.to_string(),
            login: user["login"].as_str().map(str::to_string),
            avatar_url: user["avatar_url"].as_str().map(str::to_string),
            name: user["name"].as_str().map(str::to_string),
        };
        self.save_token(&token)?;
        Ok(token)
    }

    fn require_token(&self) -> Result<String> {
        self.token()
            .map(|t| t.access_token)
            .context("GitHub is not connected")
    }

    async fn api(&self, token: &str, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}{}", self.api_base, path))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!(
                "GitHub API {} for {}: {}",
                status.as_u16(),
                path,
                body["message"].as_str().unwrap_or("")
            );
        }
        Ok(body)
    }

    pub async fn repos(&self, page: u32) -> Result<Value> {
        let token = self.require_token()?;
        let repos = self
            .api(
                &token,
                &format!("/user/repos?sort=updated&per_page=30&page={}", page.max(1)),
            )
            .await?;
        Ok(Value::Array(
            repos.as_array().into_iter().flatten().map(|r| json!({
                "id": r["id"], "name": r["name"], "full_name": r["full_name"], "description": r["description"],
                "private": r["private"], "html_url": r["html_url"], "language": r["language"], "updated_at": r["updated_at"],
            })).collect(),
        ))
    }

    pub async fn contents(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Value> {
        let token = self.require_token()?;
        let mut api_path = format!(
            "/repos/{}/{}/contents/{}",
            segment(owner)?,
            segment(repo)?,
            encode_path(path)
        );
        if !git_ref.is_empty() {
            api_path.push_str(&format!("?ref={}", urlencoding::encode(git_ref)));
        }
        self.api(&token, &api_path).await
    }

    async fn resolved_tree(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<(String, Value)> {
        let (owner, repo) = (segment(owner)?, segment(repo)?);
        let git_ref = if git_ref.is_empty() {
            let info = self.api(token, &format!("/repos/{owner}/{repo}")).await?;
            info["default_branch"]
                .as_str()
                .unwrap_or("main")
                .to_string()
        } else {
            git_ref.to_string()
        };
        let branch = self
            .api(
                token,
                &format!(
                    "/repos/{owner}/{repo}/branches/{}",
                    urlencoding::encode(&git_ref)
                ),
            )
            .await?;
        let sha = branch["commit"]["commit"]["tree"]["sha"]
            .as_str()
            .context("Tree not found for this branch")?;
        let tree = self
            .api(
                token,
                &format!("/repos/{owner}/{repo}/git/trees/{sha}?recursive=1"),
            )
            .await?;
        Ok((git_ref, tree))
    }

    pub async fn tree(&self, owner: &str, repo: &str, git_ref: &str) -> Result<Value> {
        let token = self.require_token()?;
        Ok(self.resolved_tree(&token, owner, repo, git_ref).await?.1)
    }

    /// Copies the selected files to `<workspace>/github/<owner>/<repo>/`.
    pub async fn materialize(
        &self,
        workspace: &Path,
        repo_full_name: &str,
        git_ref: &str,
        selections: &[Selection],
    ) -> Result<Value> {
        let token = self.require_token()?;
        let (owner, repo) = repo_full_name
            .split_once('/')
            .context("Repository must be owner/name")?;
        let (owner, repo) = (segment(owner)?, segment(repo)?);
        ensure!(!selections.is_empty(), "Select at least one file or folder");
        let (git_ref, tree) = self.resolved_tree(&token, owner, repo, git_ref).await?;
        let blobs: Vec<(&str, &str, u64)> = tree["tree"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry["type"] == "blob")
            .filter_map(|entry| {
                Some((
                    entry["path"].as_str()?,
                    entry["sha"].as_str()?,
                    entry["size"].as_u64().unwrap_or(0),
                ))
            })
            .collect();
        let mut seen = HashSet::new();
        let mut wanted = Vec::new();
        for selection in selections {
            let prefix = if selection.path.is_empty() {
                String::new()
            } else {
                format!("{}/", selection.path)
            };
            for &(path, sha, size) in &blobs {
                let matches = if selection.is_folder {
                    prefix.is_empty() || path.starts_with(&prefix)
                } else {
                    path == selection.path
                };
                if matches && seen.insert(path) {
                    wanted.push((path, sha, size));
                }
            }
        }
        ensure!(!wanted.is_empty(), "No files matched the selection");
        let root = workspace.join("github").join(owner).join(repo);
        std::fs::create_dir_all(&root)?;
        let mut copied = 0usize;
        let mut skipped = Vec::new();
        for chunk in wanted.chunks(8) {
            let results = futures::future::join_all(chunk.iter().map(|&(path, sha, _)| {
                let token = token.clone();
                let root = root.clone();
                async move {
                    let target = root.join(safe_relative(path)?);
                    let blob = self
                        .api(&token, &format!("/repos/{owner}/{repo}/git/blobs/{sha}"))
                        .await?;
                    let encoded = blob["content"]
                        .as_str()
                        .context("Blob has no content")?
                        .replace('\n', "");
                    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&target, bytes)?;
                    anyhow::Ok(())
                }
            }))
            .await;
            for (result, &(path, _, _)) in results.into_iter().zip(chunk) {
                match result {
                    Ok(()) => copied += 1,
                    Err(error) => skipped.push(json!({"path": path, "error": error.to_string()})),
                }
            }
        }
        Ok(json!({
            "ok": true, "repoFullName": format!("{owner}/{repo}"), "ref": git_ref,
            "rootDir": root.to_string_lossy(), "fileCount": copied, "skipped": skipped.len(), "errors": skipped,
        }))
    }
}

/// Owner and repository names: one URL path segment, no traversal.
fn segment(value: &str) -> Result<&str> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
        "Invalid GitHub owner or repository name: {value}"
    );
    Ok(value)
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(|part| urlencoding::encode(part).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Repository paths are written under the target folder only.
fn safe_relative(path: &str) -> Result<PathBuf> {
    let mut clean = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => bail!("Unsafe path in repository: {path}"),
        }
    }
    ensure!(!clean.as_os_str().is_empty(), "Empty path in repository");
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::Path as UrlPath,
        routing::{get, post},
        Json, Router,
    };

    async fn mock_github() -> String {
        let app = Router::new()
            .route("/user", get(|headers: axum::http::HeaderMap| async move {
                if headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer good") {
                    Ok(Json(json!({"login": "octo", "avatar_url": "a", "name": "Octo"})))
                } else {
                    Err((axum::http::StatusCode::UNAUTHORIZED, Json(json!({"message": "Bad credentials"}))))
                }
            }))
            .route("/user/repos", get(|| async { Json(json!([{"id": 1, "name": "demo", "full_name": "octo/demo", "private": false, "extra": "x"}])) }))
            .route("/repos/octo/demo", get(|| async { Json(json!({"default_branch": "main"})) }))
            .route("/repos/octo/demo/branches/main", get(|| async { Json(json!({"commit": {"commit": {"tree": {"sha": "t1"}}}})) }))
            .route("/repos/octo/demo/git/trees/t1", get(|| async { Json(json!({"tree": [
                {"type": "blob", "path": "README.md", "sha": "b1", "size": 5},
                {"type": "blob", "path": "src/main.rs", "sha": "b2", "size": 4},
                {"type": "tree", "path": "src", "sha": "t2"},
                {"type": "blob", "path": "../escape.txt", "sha": "b3", "size": 1},
            ]})) }))
            .route("/repos/octo/demo/git/blobs/{sha}", get(|UrlPath(sha): UrlPath<String>| async move {
                let text = match sha.as_str() { "b1" => "hello", "b2" => "main", _ => "x" };
                Json(json!({"content": base64::engine::general_purpose::STANDARD.encode(text)}))
            }))
            .route("/login/oauth/access_token", post(|Json(body): Json<Value>| async move {
                Json(if body["code"] == "ok" { json!({"access_token": "good"}) } else { json!({"error": "bad_verification_code"}) })
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn token_connection_browsing_and_copying_files() {
        let base = mock_github().await;
        let dir = tempfile::tempdir().unwrap();
        let github = GithubConnector::with_endpoints(dir.path().into(), &base, &base, None, None);
        assert_eq!(github.status()["connected"], false);
        assert!(github.repos(1).await.is_err());
        assert!(github.connect_with_token("bad").await.is_err());
        let token = github.connect_with_token(" good ").await.unwrap();
        assert_eq!(token.login.as_deref(), Some("octo"));
        assert_eq!(github.status()["user"]["login"], "octo");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(TOKEN_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let repos = github.repos(1).await.unwrap();
        assert_eq!(repos[0]["full_name"], "octo/demo");
        assert!(repos[0].get("extra").is_none());

        let workspace = dir.path().join("workspace");
        let result = github
            .materialize(
                &workspace,
                "octo/demo",
                "",
                &[
                    Selection {
                        path: "src".into(),
                        is_folder: true,
                    },
                    Selection {
                        path: "README.md".into(),
                        is_folder: false,
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(result["fileCount"], 2);
        let root = workspace.join("github/octo/demo");
        assert_eq!(
            std::fs::read_to_string(root.join("README.md")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "main"
        );

        // A whole-repository selection reports the unsafe path instead of writing outside.
        let all = github
            .materialize(
                &workspace,
                "octo/demo",
                "main",
                &[Selection {
                    path: String::new(),
                    is_folder: true,
                }],
            )
            .await
            .unwrap();
        assert_eq!(all["skipped"], 1);
        assert!(!workspace.join("github/octo/escape.txt").exists());
        assert!(github
            .materialize(
                &workspace,
                "octo/../x",
                "",
                &[Selection {
                    path: "a".into(),
                    is_folder: false
                }]
            )
            .await
            .is_err());

        github.disconnect().unwrap();
        assert_eq!(github.status()["connected"], false);
    }

    #[tokio::test]
    async fn oauth_requires_a_secret_and_a_state_issued_by_this_process() {
        let base = mock_github().await;
        let dir = tempfile::tempdir().unwrap();
        let without = GithubConnector::with_endpoints(dir.path().into(), &base, &base, None, None);
        assert!(without
            .auth_url("http://127.0.0.1:30085/api/github/callback")
            .is_err());

        let github = GithubConnector::with_endpoints(
            dir.path().into(),
            &base,
            &base,
            Some("id".into()),
            Some("secret".into()),
        );
        let auth = github
            .auth_url("http://127.0.0.1:30085/api/github/callback")
            .unwrap();
        let state = auth["state"].as_str().unwrap().to_string();
        assert!(auth["url"]
            .as_str()
            .unwrap()
            .contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A30085%2Fapi%2Fgithub%2Fcallback"));
        assert!(github.complete_oauth("ok", "forged").await.is_err());
        assert!(github.complete_oauth("bad", &state).await.is_err());
        // A state is single-use.
        let again = github
            .auth_url("http://127.0.0.1:30085/api/github/callback")
            .unwrap();
        let token = github
            .complete_oauth("ok", again["state"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(token.login.as_deref(), Some("octo"));
        assert!(github
            .complete_oauth("ok", again["state"].as_str().unwrap())
            .await
            .is_err());
    }
}
