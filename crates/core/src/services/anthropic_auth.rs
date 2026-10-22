//! First-party OAuth with an application-owned credential store. Never reads
//! Claude Code's credentials, logs tokens, or sends OAuth credentials to proxies.
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const MARKER: &str = "anthropic-oauth:";
const OAUTH_BETA: &str = "oauth-2025-04-20";
pub const FAST_BETA: &str = "fast-mode-2026-02-01";
static TOKEN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_at: i64,
    pub(crate) scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub expires_at: Option<i64>,
    pub scopes: Vec<String>,
}

pub struct LoginFlow {
    pub authorization_url: String,
    listener: TcpListener,
    state: String,
    verifier: String,
    redirect_uri: String,
    client_id: String,
}

pub async fn begin_login() -> Result<LoginFlow> {
    begin_scoped_login(CLIENT_ID, SCOPES).await
}

pub(crate) async fn begin_scoped_login(client_id: &str, scopes: &str) -> Result<LoginFlow> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let redirect_uri = format!(
        "http://localhost:{}/callback",
        listener.local_addr()?.port()
    );
    let verifier = random_secret()?;
    let state = random_secret()?;
    let authorization_url = scoped_auth_url(
        AUTHORIZE_URL,
        &redirect_uri,
        &state,
        &verifier,
        client_id,
        scopes,
    )?;
    Ok(LoginFlow {
        authorization_url,
        listener,
        state,
        verifier,
        redirect_uri,
        client_id: client_id.into(),
    })
}

pub fn open_browser(url: &str) -> Result<()> {
    open::that_detached(url).context("Could not open login browser")
}

impl LoginFlow {
    pub async fn finish(self, data_dir: &Path) -> Result<AuthStatus> {
        self.finish_with_token_url(data_dir, TOKEN_URL).await
    }

    async fn finish_with_token_url(self, data_dir: &Path, token_url: &str) -> Result<AuthStatus> {
        let tokens = self.exchange(token_url).await?;
        ensure!(
            tokens.scopes.iter().any(|scope| scope == "user:inference"),
            "Authorization did not grant inference scope; credentials were not saved"
        );
        let _lock = TOKEN_LOCK.lock().await;
        save(data_dir, &tokens)?;
        status(data_dir)
    }

    pub(crate) async fn exchange(self, token_url: &str) -> Result<Tokens> {
        let code = tokio::time::timeout(Duration::from_secs(300), self.authorization_code())
            .await
            .context("Login timed out after five minutes")??;
        let response = post_token(token_url, &json!({"grant_type":"authorization_code","code":code,"client_id":self.client_id,"redirect_uri":self.redirect_uri,"code_verifier":self.verifier,"state":self.state})).await?;
        let tokens = tokens_from_response(response, None)?;
        Ok(tokens)
    }

    async fn authorization_code(&self) -> Result<String> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let mut request = Vec::new();
            let read = tokio::time::timeout(Duration::from_secs(5), async {
                while request.len() < 8192 {
                    let mut byte = [0u8];
                    if stream.read(&mut byte).await? == 0 {
                        break;
                    }
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                Ok::<_, std::io::Error>(())
            })
            .await;
            if !matches!(read, Ok(Ok(()))) {
                continue;
            }
            let line = String::from_utf8_lossy(&request);
            let mut parts = line.lines().next().unwrap_or("").split_whitespace();
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("");
            let parsed = parse_callback(method, target, &self.state);
            let (status, message) = if parsed.is_ok() {
                (
                    "200 OK",
                    "Authorization received. Return to the Rust CLI to complete login.",
                )
            } else {
                (
                    "400 Bad Request",
                    "Invalid authorization callback. Return to the login page and try again.",
                )
            };
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}", message.len());
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
            if let Ok(code) = parsed {
                return Ok(code);
            }
            if method == "GET" && target.starts_with("/callback?") {
                if let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) {
                    let values = url
                        .query_pairs()
                        .collect::<std::collections::HashMap<_, _>>();
                    if values
                        .get("state")
                        .is_some_and(|value| value == self.state.as_str())
                        && values.contains_key("error")
                    {
                        bail!("Authorization was declined; no credentials were saved");
                    }
                }
            }
        }
    }
}

fn parse_callback(method: &str, target: &str, state: &str) -> Result<String> {
    ensure!(
        method == "GET" && target.starts_with("/callback?"),
        "Invalid callback request"
    );
    let url = reqwest::Url::parse(&format!("http://localhost{target}"))?;
    let values = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    ensure!(
        values.get("state").is_some_and(|value| value == state),
        "OAuth state mismatch"
    );
    ensure!(!values.contains_key("error"), "Authorization was declined");
    let code = values
        .get("code")
        .filter(|code| !code.is_empty())
        .context("Authorization code missing")?;
    ensure!(code.len() <= 4096, "Invalid authorization code");
    Ok(code.to_string())
}

fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Secure randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
fn auth_url(base: &str, redirect: &str, state: &str, verifier: &str) -> Result<String> {
    scoped_auth_url(base, redirect, state, verifier, CLIENT_ID, SCOPES)
}

fn scoped_auth_url(
    base: &str,
    redirect: &str,
    state: &str,
    verifier: &str,
    client_id: &str,
    scopes: &str,
) -> Result<String> {
    let challenge =
        URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref());
    let mut url = reqwest::Url::parse(base)?;
    url.query_pairs_mut().extend_pairs([
        ("code", "true"),
        ("client_id", client_id),
        ("response_type", "code"),
        ("redirect_uri", redirect),
        ("scope", scopes),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state),
    ]);
    Ok(url.into())
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

pub(crate) async fn post_token(url: &str, body: &Value) -> Result<Value> {
    let response = client()?
        .post(url)
        .json(body)
        .send()
        .await
        .context("OAuth token request failed")?;
    ensure!(
        response.status().is_success(),
        "OAuth token request failed with HTTP {}",
        response.status().as_u16()
    );
    response
        .json()
        .await
        .context("Invalid OAuth token response")
}

pub(crate) fn tokens_from_response(response: Value, previous: Option<&Tokens>) -> Result<Tokens> {
    let access_token = response["access_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("OAuth response has no access token")?
        .to_owned();
    let expires_in = response["expires_in"]
        .as_i64()
        .filter(|v| *v > 0 && *v <= 366 * 86400)
        .context("OAuth response has invalid expiration")?;
    let refresh_token = response["refresh_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .or_else(|| previous.and_then(|p| p.refresh_token.clone()));
    let scopes = response["scope"]
        .as_str()
        .map(|v| v.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| previous.map(|p| p.scopes.clone()).unwrap_or_default());
    Ok(Tokens {
        access_token,
        refresh_token,
        expires_at: chrono::Utc::now().timestamp() + expires_in,
        scopes,
    })
}

fn credential_path(data_dir: &Path) -> PathBuf {
    data_dir.join("anthropic-oauth.dat")
}

pub(crate) fn save(data_dir: &Path, tokens: &Tokens) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let data = protect(&serde_json::to_vec(tokens)?)?;
    let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&data)?;
    file.as_file().sync_all()?;
    file.persist(credential_path(data_dir))
        .map_err(|_| anyhow::anyhow!("Could not persist OAuth credentials"))?;
    Ok(())
}

pub(crate) fn load(data_dir: &Path) -> Result<Option<Tokens>> {
    let path = credential_path(data_dir);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Reading OAuth credential store"),
    };
    let data = unprotect(&bytes)?;
    Ok(Some(
        serde_json::from_slice(&data).context("Invalid OAuth credential store")?,
    ))
}

pub fn status(data_dir: &Path) -> Result<AuthStatus> {
    Ok(match load(data_dir)? {
        Some(tokens) => AuthStatus {
            authenticated: true,
            expires_at: Some(tokens.expires_at),
            scopes: tokens.scopes,
        },
        None => AuthStatus {
            authenticated: false,
            expires_at: None,
            scopes: vec![],
        },
    })
}

pub async fn logout(data_dir: &Path) -> Result<()> {
    let _lock = TOKEN_LOCK.lock().await;
    match std::fs::remove_file(credential_path(data_dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn credential_marker(data_dir: &Path) -> Result<String> {
    ensure!(
        data_dir.is_absolute(),
        "OAuth data directory must be absolute"
    );
    Ok(format!("{MARKER}{}", data_dir.display()))
}

pub fn is_oauth_marker(key: &str) -> bool {
    key.starts_with(MARKER)
}

async fn access_token(data_dir: &Path, token_url: &str) -> Result<String> {
    let _lock = TOKEN_LOCK.lock().await;
    let mut tokens = load(data_dir)?.context("Not logged in to Anthropic; use /login")?;
    if tokens.expires_at <= chrono::Utc::now().timestamp() + 60 {
        let refresh = tokens
            .refresh_token
            .as_deref()
            .context("OAuth token expired; use /login")?;
        let response = post_token(token_url, &json!({"grant_type":"refresh_token","refresh_token":refresh,"client_id":CLIENT_ID,"scope":tokens.scopes.join(" ")})).await?;
        tokens = tokens_from_response(response, Some(&tokens))?;
        save(data_dir, &tokens)?;
    }
    Ok(tokens.access_token)
}

fn ensure_first_party(base_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(base_url)?;
    ensure!(url.scheme() == "https" && url.host_str() == Some("api.anthropic.com") && url.port_or_known_default() == Some(443) && url.username().is_empty() && url.password().is_none(), "Anthropic OAuth and fast mode require https://api.anthropic.com; credentials will not be sent to another host");
    Ok(())
}

pub async fn authorize(
    mut request: reqwest::RequestBuilder,
    key: &str,
    base_url: &str,
    fast: bool,
) -> Result<reqwest::RequestBuilder> {
    let mut betas = Vec::new();
    if let Some(directory) = key.strip_prefix(MARKER) {
        ensure_first_party(base_url)?;
        let target = request
            .try_clone()
            .context("OAuth requests must have a replayable body")?
            .build()?;
        ensure_first_party(target.url().as_str())?;
        let directory = Path::new(directory);
        ensure!(
            directory.is_absolute(),
            "OAuth credential directory is invalid"
        );
        request = request.bearer_auth(access_token(directory, TOKEN_URL).await?);
        betas.push(OAUTH_BETA);
    } else {
        request = request.header("x-api-key", key);
    }
    if fast {
        ensure_first_party(base_url)?;
        betas.push(FAST_BETA);
    }
    if !betas.is_empty() {
        request = request.header("anthropic-beta", betas.join(","));
    }
    Ok(request)
}

pub fn apply_fast(body: &mut Value, enabled: bool, model: &str, base_url: &str) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    ensure_first_party(base_url)?;
    ensure!(
        model.starts_with("claude-opus-4-6"),
        "Fast mode is supported only for Claude Opus 4.6"
    );
    body["speed"] = json!("fast");
    Ok(())
}

pub async fn check_fast_eligibility(key: &str, base_url: &str, model: &str) -> Result<()> {
    apply_fast(&mut json!({}), true, model, base_url)?;
    let request = client()?.get("https://api.anthropic.com/api/claude_code_penguin_mode");
    let response = authorize(request, key, base_url, false)
        .await?
        .send()
        .await
        .context("Fast-mode eligibility request failed")?;
    ensure!(
        response.status().is_success(),
        "Fast-mode eligibility check failed with HTTP {}",
        response.status().as_u16()
    );
    let response: Value = response.json().await?;
    ensure!(
        response["enabled"] == true,
        "Fast mode is unavailable for this account: {}",
        response["disabled_reason"].as_str().unwrap_or("unknown")
    );
    Ok(())
}

#[cfg(windows)]
pub(crate) fn protect(data: &[u8]) -> Result<Vec<u8>> {
    crypt(data, true)
}
#[cfg(windows)]
pub(crate) fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    crypt(data, false)
}
#[cfg(windows)]
fn crypt(data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{LocalFree, HLOCAL},
            Security::Cryptography::*,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len().try_into()?,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )?;
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )?;
        }
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as *mut _));
        Ok(bytes)
    }
}
#[cfg(not(windows))]
pub(crate) fn protect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}
#[cfg(not(windows))]
pub(crate) fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_state_and_first_party_boundaries() {
        let url = reqwest::Url::parse(
            &auth_url(
                AUTHORIZE_URL,
                "http://localhost:123/callback",
                "state",
                "verifier",
            )
            .unwrap(),
        )
        .unwrap();
        let values = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(values["code_challenge_method"], "S256");
        assert_ne!(values["code_challenge"], "verifier");
        assert!(parse_callback("GET", "/callback?code=c&state=wrong", "state").is_err());
        assert_eq!(
            parse_callback("GET", "/callback?code=c&state=state", "state").unwrap(),
            "c"
        );
        for base in [
            "http://api.anthropic.com",
            "https://api.anthropic.com.example.com",
            "https://proxy.example.com",
        ] {
            assert!(ensure_first_party(base).is_err());
        }
        let mut body = json!({});
        apply_fast(
            &mut body,
            true,
            "claude-opus-4-6",
            "https://api.anthropic.com",
        )
        .unwrap();
        assert_eq!(body["speed"], "fast");
        assert!(apply_fast(
            &mut body,
            true,
            "claude-sonnet-4-6",
            "https://api.anthropic.com"
        )
        .is_err());
    }

    #[tokio::test]
    async fn credentials_persist_and_logout_without_touching_other_files() {
        let root = tempfile::tempdir().unwrap();
        let tokens = Tokens {
            access_token: "fixture-secret-access".into(),
            refresh_token: Some("fixture-secret-refresh".into()),
            expires_at: chrono::Utc::now().timestamp() + 3600,
            scopes: vec!["user:inference".into()],
        };
        save(root.path(), &tokens).unwrap();
        assert!(status(root.path()).unwrap().authenticated);
        #[cfg(windows)]
        assert!(
            !String::from_utf8_lossy(&std::fs::read(credential_path(root.path())).unwrap())
                .contains("fixture-secret")
        );
        assert_eq!(
            load(root.path()).unwrap().unwrap().access_token,
            "fixture-secret-access"
        );
        std::fs::write(root.path().join("unrelated"), "keep").unwrap();
        logout(root.path()).await.unwrap();
        assert!(!status(root.path()).unwrap().authenticated);
        assert!(root.path().join("unrelated").exists());
    }

    #[tokio::test]
    async fn oauth_and_fast_headers_never_include_the_credential_marker() {
        let root = tempfile::tempdir().unwrap();
        save(
            root.path(),
            &Tokens {
                access_token: "fixture-token".into(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                scopes: vec!["user:inference".into()],
            },
        )
        .unwrap();
        let marker = credential_marker(root.path()).unwrap();
        let builder = client()
            .unwrap()
            .post("https://api.anthropic.com/v1/messages");
        let request = authorize(builder, &marker, "https://api.anthropic.com", true)
            .await
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer fixture-token");
        assert!(request.headers().get("x-api-key").is_none());
        assert_eq!(
            request.headers()["anthropic-beta"],
            "oauth-2025-04-20,fast-mode-2026-02-01"
        );
        let malicious = client().unwrap().post("https://example.com/v1/messages");
        assert!(authorize(malicious, &marker, "https://example.com", false)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn expired_nonrefreshable_tokens_fail_without_a_network_request() {
        let root = tempfile::tempdir().unwrap();
        save(
            root.path(),
            &Tokens {
                access_token: "expired-fixture".into(),
                refresh_token: None,
                expires_at: 0,
                scopes: vec!["user:inference".into()],
            },
        )
        .unwrap();
        let error = access_token(root.path(), "http://127.0.0.1:1/token")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("expired"));
        assert!(!error.to_string().contains("expired-fixture"));
    }

    #[tokio::test]
    async fn local_callback_exchanges_code_and_refreshes_tokens() {
        use axum::{routing::post, Json, Router};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let app = Router::new().route("/token", post(|Json(body): Json<Value>| async move {
            assert_eq!(body["client_id"], CLIENT_ID);
            if body["grant_type"] == "authorization_code" { assert_eq!(body["code"], "fixture-code"); assert!(body["code_verifier"].as_str().unwrap().len() >= 43); }
            else { assert_eq!(body["refresh_token"], "fixture-refresh"); }
            Json(json!({"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_in":3600,"scope":"user:inference user:profile"}))
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let flow = begin_login().await.unwrap();
        let callback = format!(
            "{}?code=fixture-code&state={}",
            flow.redirect_uri, flow.state
        );
        let finish = flow.finish_with_token_url(root.path(), &endpoint);
        let request = async {
            reqwest::get(callback).await.unwrap();
        };
        let (status, _) = tokio::join!(finish, request);
        assert!(status.unwrap().authenticated);
        let mut tokens = load(root.path()).unwrap().unwrap();
        tokens.expires_at = 0;
        save(root.path(), &tokens).unwrap();
        assert_eq!(
            access_token(root.path(), &endpoint).await.unwrap(),
            "fixture-access"
        );
        assert!(load(root.path()).unwrap().unwrap().expires_at > chrono::Utc::now().timestamp());
        server.abort();
    }
}
