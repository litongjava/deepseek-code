//! Network serving options for the Bridge.
//!
//! Desktop binds the Bridge to loopback and authenticates with a per-process key.
//! Server mode (`dcd-server`) can additionally listen on other addresses, serve the
//! built frontend, and accept a persistent access token from a browser login.

use axum::http::{header, HeaderMap, Method, StatusCode};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Cookie carrying the access token for requests that cannot set headers
/// (EventSource streams, `<img>` sources, downloads).
pub const SESSION_COOKIE: &str = "dcd_token";

#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub host: IpAddr,
    pub port: u16,
    /// Desktop falls back to the next ports when 30085 is taken; a server must not.
    pub try_next_ports: bool,
    /// Persistent token accepted from browsers in addition to the process key.
    pub access_token: Option<String>,
    /// Built frontend served for every non-API path, with SPA fallback to index.html.
    pub static_dir: Option<PathBuf>,
    /// Extra origins allowed to call the API cross-origin (for example a mobile app).
    pub allowed_origins: Vec<String>,
    pub tls: Option<TlsFiles>,
    /// TLS is terminated by a reverse proxy in front of this server.
    pub behind_https_proxy: bool,
    /// The operator accepts plain HTTP on a network address. The access token and
    /// session cookie then cross the network unencrypted.
    pub allow_http: bool,
}

impl ServeOptions {
    /// The Desktop default: loopback only, process key only, port fallback enabled.
    pub fn local(port: u16) -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            try_next_ports: true,
            access_token: None,
            static_dir: None,
            allowed_origins: Vec::new(),
            tls: None,
            behind_https_proxy: false,
            allow_http: false,
        }
    }

    pub fn is_server_mode(&self) -> bool {
        self.access_token.is_some()
    }

    /// Whether browsers reach this server over HTTPS.
    pub fn is_https(&self) -> bool {
        self.tls.is_some() || self.behind_https_proxy
    }

    /// Whether a network address is served over plain HTTP by explicit choice.
    pub fn is_plain_http_on_network(&self) -> bool {
        !self.host.is_loopback() && !self.is_https()
    }

    /// Exposing command execution to a network requires a token and HTTPS,
    /// unless plain HTTP was explicitly allowed.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.host.is_loopback() {
            return Ok(());
        }
        if self.access_token.is_none() {
            anyhow::bail!("Listening on {} requires an access token", self.host);
        }
        if !self.is_https() && !self.allow_http {
            anyhow::bail!(
                "Listening on {} requires HTTPS: pass --tls-cert and --tls-key, \
                 --behind-https-proxy when a reverse proxy terminates TLS, \
                 or --allow-http to accept plain HTTP",
                self.host
            );
        }
        Ok(())
    }
}

/// Request authorization rules shared by Desktop and server mode.
#[derive(Clone, Debug)]
pub struct AccessPolicy {
    pub keys: Vec<String>,
    pub server_mode: bool,
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Access {
    Allow,
    Deny(StatusCode, &'static str),
}

/// Paths that never need a credential.
fn is_public(path: &str, method: &Method, server_mode: bool) -> bool {
    // GitHub redirects the browser here without a credential; the single-use
    // OAuth state issued by this process authenticates the request.
    if path == "/health" || path.starts_with("/api/im/webhook/") || path == "/api/github/callback" {
        return true;
    }
    if path == "/api/auth/login" || path == "/api/auth/status" || path == "/api/auth/logout" {
        return true;
    }
    // The frontend shell and its assets; API calls are still protected.
    if server_mode && !path.starts_with("/api/") && path != "/metrics" && !path.starts_with("/data/")
        && !path.starts_with("/custom/")
    {
        return true;
    }
    if server_mode {
        return false;
    }
    // Loopback Desktop keeps its historical public reads.
    path == "/metrics"
        || ((path == "/api/system-status" || path == "/api/workspace-config") && method == Method::GET)
}

impl AccessPolicy {
    pub fn check(&self, method: &Method, path: &str, headers: &HeaderMap) -> Access {
        if method == Method::OPTIONS {
            return Access::Allow;
        }
        if self.server_mode && !self.origin_allowed(headers) {
            return Access::Deny(StatusCode::FORBIDDEN, "Cross-origin request rejected");
        }
        if is_public(path, method, self.server_mode) {
            return Access::Allow;
        }
        if self.authenticated(headers) {
            Access::Allow
        } else {
            Access::Deny(StatusCode::UNAUTHORIZED, "Invalid or missing API key")
        }
    }

    pub fn authenticated(&self, headers: &HeaderMap) -> bool {
        request_credentials(headers).iter().any(|token| self.accepts(token))
    }

    pub fn accepts(&self, token: &str) -> bool {
        self.keys.iter().any(|key| constant_time_eq(key.as_bytes(), token.as_bytes()))
    }

    /// Same-origin requests and configured origins pass; other browsers are refused
    /// so that a page on another site cannot drive this server with a stored cookie.
    fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
            // Non-browser clients and same-origin GETs omit Origin; the cookie is
            // SameSite=Strict, so a cross-site navigation does not carry it.
            return true;
        };
        if self.allowed_origins.iter().any(|allowed| allowed == origin) {
            return true;
        }
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        let origin_host = origin.split_once("://").map(|(_, rest)| rest).unwrap_or("");
        !host.is_empty() && origin_host.eq_ignore_ascii_case(host)
    }
}

/// HTTP/2 carries the host in the `:authority` pseudo-header, which hyper exposes as
/// the URI authority rather than a `Host` header. Copy it into `Host` so the origin
/// check and handlers that build URLs see the same value over HTTP/1.1 and HTTP/2.
pub fn fill_host_from_authority(headers: &mut HeaderMap, uri: &axum::http::Uri) {
    if headers.contains_key(header::HOST) {
        return;
    }
    if let Some(value) = uri
        .authority()
        .and_then(|authority| header::HeaderValue::from_str(authority.as_str()).ok())
    {
        headers.insert(header::HOST, value);
    }
}

/// Cache policy for the served web UI. Vite names files under `/assets/` by their
/// content hash, so browsers may keep them forever and skip revalidation; every
/// other path (index.html and the SPA fallback) is revalidated so a new build loads.
pub fn static_cache_control(path: &str) -> &'static str {
    if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// Adds `static_cache_control` to successful static responses.
pub async fn static_cache_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let policy = static_cache_control(request.uri().path());
    let mut response = next.run(request).await;
    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, header::HeaderValue::from_static(policy));
    }
    response
}

/// Credentials from `x-api-key`, `Authorization: Bearer`, and the session cookie.
/// A request passes when any of them is valid, so a stale header left by another
/// mode does not hide a valid cookie.
pub fn request_credentials(headers: &HeaderMap) -> Vec<String> {
    let mut credentials = Vec::new();
    if let Some(value) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        credentials.push(value.to_string());
    }
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = value.strip_prefix("Bearer ") {
            credentials.push(token.to_string());
        }
    }
    credentials.extend(
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .filter(|(name, _)| *name == SESSION_COOKIE)
            .map(|(_, value)| value.to_string()),
    );
    credentials
}

pub fn session_cookie(token: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000{secure}")
}

pub fn clear_session_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Reads the access token, creating a random one readable only by this user.
/// Returns the token and whether it was newly created.
pub fn load_or_create_token(path: &Path) -> anyhow::Result<(String, bool)> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let token = existing.trim().to_string();
        if token.len() >= 16 {
            return Ok((token, false));
        }
        anyhow::bail!("Access token in {} is shorter than 16 characters", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options.open(path)?.write_all(token.as_bytes())?;
    Ok((token, true))
}

/// Serves `app` over TLS with a PEM certificate chain and private key.
pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    files: &TlsFiles,
) -> anyhow::Result<()> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};

    let certs = CertificateDer::pem_file_iter(&files.cert)
        .map_err(|e| anyhow::anyhow!("Cannot read certificate {}: {e}", files.cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Invalid certificate {}: {e}", files.cert.display()))?;
    let key = PrivateKeyDer::from_pem_file(&files.key)
        .map_err(|e| anyhow::anyhow!("Cannot read private key {}: {e}", files.key.display()))?;
    let mut config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(module = "Bridge", "Accept failed: {error}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(stream).await else { return };
            let service = hyper_util::service::TowerToHyperService::new(app);
            let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), service)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn policy(server_mode: bool) -> AccessPolicy {
        AccessPolicy {
            keys: vec!["process-key".into(), "access-token-0123456789".into()],
            server_mode,
            allowed_origins: vec!["tauri://localhost".into()],
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn credentials_come_from_header_bearer_or_cookie() {
        assert_eq!(request_credentials(&headers(&[("x-api-key", "a")])), ["a"]);
        assert_eq!(request_credentials(&headers(&[("authorization", "Bearer b")])), ["b"]);
        assert_eq!(request_credentials(&headers(&[("cookie", "x=1; dcd_token=c; y=2")])), ["c"]);
        assert!(request_credentials(&headers(&[("cookie", "dcd_tokenx=c")])).is_empty());
        // A stale Bearer key from another mode does not hide a valid cookie.
        let p = policy(true);
        let mixed = headers(&[("authorization", "Bearer stale"), ("cookie", "dcd_token=access-token-0123456789")]);
        assert_eq!(p.check(&Method::GET, "/api/conversations", &mixed), Access::Allow);
    }

    #[test]
    fn server_mode_protects_api_but_serves_the_shell() {
        let p = policy(true);
        let none = HeaderMap::new();
        assert_eq!(p.check(&Method::GET, "/", &none), Access::Allow);
        assert_eq!(p.check(&Method::GET, "/assets/index.js", &none), Access::Allow);
        assert_eq!(p.check(&Method::GET, "/health", &none), Access::Allow);
        assert_eq!(p.check(&Method::POST, "/api/auth/login", &none), Access::Allow);
        for path in ["/api/conversations", "/api/workspace-config", "/api/system-status", "/metrics", "/data/sync/pull"] {
            assert!(matches!(p.check(&Method::GET, path, &none), Access::Deny(StatusCode::UNAUTHORIZED, _)), "{path}");
        }
        let cookie = headers(&[("cookie", "dcd_token=access-token-0123456789")]);
        assert_eq!(p.check(&Method::GET, "/api/conversations", &cookie), Access::Allow);
        let wrong = headers(&[("cookie", "dcd_token=access-token-0123456788")]);
        assert!(matches!(p.check(&Method::GET, "/api/conversations", &wrong), Access::Deny(..)));
    }

    #[test]
    fn server_mode_rejects_other_origins_even_with_a_credential() {
        let p = policy(true);
        let same = headers(&[("host", "dev.example:8443"), ("origin", "https://dev.example:8443"), ("x-api-key", "process-key")]);
        assert_eq!(p.check(&Method::POST, "/api/chat", &same), Access::Allow);
        let app = headers(&[("host", "dev.example:8443"), ("origin", "tauri://localhost"), ("x-api-key", "process-key")]);
        assert_eq!(p.check(&Method::POST, "/api/chat", &app), Access::Allow);
        let evil = headers(&[("host", "dev.example:8443"), ("origin", "https://evil.example"), ("cookie", "dcd_token=process-key")]);
        assert!(matches!(p.check(&Method::POST, "/api/chat", &evil), Access::Deny(StatusCode::FORBIDDEN, _)));
        // Login itself is origin-checked so another site cannot probe tokens.
        assert!(matches!(p.check(&Method::POST, "/api/auth/login", &evil), Access::Deny(StatusCode::FORBIDDEN, _)));
    }

    #[test]
    fn http2_authority_counts_as_host_for_same_origin() {
        let p = policy(true);
        let uri: axum::http::Uri = "https://dev.example:8443/assets/index.js".parse().unwrap();
        let mut h2 = headers(&[("origin", "https://dev.example:8443")]);
        assert!(matches!(p.check(&Method::GET, "/assets/index.js", &h2), Access::Deny(StatusCode::FORBIDDEN, _)));
        fill_host_from_authority(&mut h2, &uri);
        assert_eq!(p.check(&Method::GET, "/assets/index.js", &h2), Access::Allow);
        // An explicit Host header is kept as sent.
        let mut h1 = headers(&[("host", "other.example"), ("origin", "https://dev.example:8443")]);
        fill_host_from_authority(&mut h1, &uri);
        assert!(matches!(p.check(&Method::GET, "/assets/index.js", &h1), Access::Deny(StatusCode::FORBIDDEN, _)));
        // Origin-form URIs (HTTP/1.1 without Host) add nothing.
        let mut bare = HeaderMap::new();
        fill_host_from_authority(&mut bare, &"/".parse().unwrap());
        assert!(bare.get(header::HOST).is_none());
    }

    #[test]
    fn hashed_assets_are_immutable_and_the_shell_is_revalidated() {
        assert_eq!(static_cache_control("/assets/index-DeEXW22c.js"), "public, max-age=31536000, immutable");
        assert_eq!(static_cache_control("/"), "no-cache");
        assert_eq!(static_cache_control("/index.html"), "no-cache");
        assert_eq!(static_cache_control("/chat/123"), "no-cache");
        assert_eq!(static_cache_control("/manifest.webmanifest"), "no-cache");
    }

    #[test]
    fn desktop_mode_keeps_loopback_rules() {
        let p = policy(false);
        let none = HeaderMap::new();
        assert_eq!(p.check(&Method::GET, "/api/system-status", &none), Access::Allow);
        assert!(matches!(p.check(&Method::GET, "/", &none), Access::Deny(..)));
        assert_eq!(p.check(&Method::OPTIONS, "/api/chat", &none), Access::Allow);
    }

    #[test]
    fn network_binding_requires_token_and_https() {
        let mut options = ServeOptions::local(30085);
        assert!(options.validate().is_ok());
        options.host = "0.0.0.0".parse().unwrap();
        assert!(options.validate().is_err());
        options.access_token = Some("access-token-0123456789".into());
        assert!(options.validate().is_err());
        options.behind_https_proxy = true;
        assert!(options.validate().is_ok());
        assert!(!options.is_plain_http_on_network());
        options.behind_https_proxy = false;
        options.allow_http = true;
        assert!(options.validate().is_ok());
        assert!(options.is_plain_http_on_network());
        // Plain HTTP never waives the access token.
        options.access_token = None;
        assert!(options.validate().is_err());
    }

    #[test]
    fn token_file_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("dcd-token-{}", uuid::Uuid::new_v4()));
        let path = dir.join("server-token");
        let (first, created) = load_or_create_token(&path).unwrap();
        assert!(created && first.len() == 64);
        let (second, created) = load_or_create_token(&path).unwrap();
        assert!(!created);
        assert_eq!(first, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cookies_are_strict_and_secure_over_https() {
        assert!(session_cookie("t", true).contains("HttpOnly; SameSite=Strict"));
        assert!(session_cookie("t", true).ends_with("; Secure"));
        assert!(!session_cookie("t", false).contains("Secure"));
    }
}
