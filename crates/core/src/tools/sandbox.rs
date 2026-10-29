//! Bash sandboxing, as in the reference CLI: bubblewrap on Linux and
//! sandbox-exec on macOS keep commands from writing outside the project
//! (plus temp and configured paths) and can cut network access. Enabled by
//! `"sandbox": {"enabled": true}` in settings.json.
//!
//! `sandbox.network.allowedDomains` / `deniedDomains` limit network access to
//! named domains, as in the reference CLI: the command runs without a network
//! of its own and reaches the outside only through a filtering HTTP proxy on
//! the host. On Linux a small bridge inside the sandbox (this executable,
//! started as `__sandbox-bridge`) carries the proxy's Unix socket to a local
//! port; on macOS the profile allows only the proxy's local port.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The effective `sandbox` settings: user, then project, then local. Domains
/// from `WebFetch(domain:...)` allow rules are collected as well, since the
/// reference CLI merges them into the network allowlist.
pub fn settings(data_dir: &Path, workspace: &Path) -> Value {
    let mut merged = json!({});
    let mut web_fetch_domains = Vec::new();
    for source in [data_dir.join("settings.json"), workspace.join(".claude/settings.json"), workspace.join(".claude/settings.local.json")] {
        let Some(file) = std::fs::read(&source).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) else { continue };
        for rule in file["permissions"]["allow"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if let Some(domain) = rule.strip_prefix("WebFetch(domain:").and_then(|rest| rest.strip_suffix(')')) {
                web_fetch_domains.push(json!(domain));
            }
        }
        if let (Some(target), Some(layer)) = (merged.as_object_mut(), file["sandbox"].as_object()) {
            for (key, value) in layer {
                // Network lists add up across sources; other keys override.
                if key == "network" && value.is_object() && target.get("network").is_some_and(Value::is_object) {
                    let network = target.get_mut("network").unwrap().as_object_mut().unwrap();
                    for (name, item) in value.as_object().unwrap() {
                        match (network.get_mut(name).and_then(Value::as_array_mut), item.as_array()) {
                            (Some(existing), Some(more)) => existing.extend(more.iter().cloned()),
                            _ => { network.insert(name.clone(), item.clone()); }
                        }
                    }
                } else {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
    }
    merged["webFetchDomains"] = Value::Array(web_fetch_domains);
    merged
}

/// What the tool receives when the sandbox is on: writable paths and network.
pub fn policy(settings: &Value, workspace: &Path, data_dir: &Path, extra_writable: &[PathBuf]) -> Option<Value> {
    if settings["enabled"] != true { return None; }
    let mut writable: Vec<String> = vec![workspace.to_string_lossy().into_owned(), std::env::temp_dir().to_string_lossy().into_owned()];
    writable.extend(extra_writable.iter().map(|path| path.to_string_lossy().into_owned()));
    writable.extend(settings["filesystem"]["allowWrite"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
    let network = &settings["network"];
    let strings = |value: &Value| value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
    // Domain filtering applies once either list is configured.
    let rules = (network.get("allowedDomains").is_some() || network.get("deniedDomains").is_some()).then(|| {
        let mut allowed = strings(&network["allowedDomains"]);
        if network.get("allowedDomains").is_some() { allowed.extend(strings(&settings["webFetchDomains"])); }
        json!({"allowed": allowed, "denied": strings(&network["deniedDomains"]), "restricted": network.get("allowedDomains").is_some(), "dir": data_dir.join("sp")})
    });
    Some(json!({
        "writable": writable,
        "network": network != "deny",
        "network_rules": rules,
        "allow_unsandboxed": settings["allowUnsandboxedCommands"] != false,
    }))
}

/// Domain rules for the filtering proxy.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NetworkRules {
    allowed: Vec<String>,
    denied: Vec<String>,
    /// Only listed domains may be reached (allowedDomains is set).
    restricted: bool,
}

fn domain_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() && host.ends_with(&format!(".{suffix}")),
        None => pattern == host,
    }
}

impl NetworkRules {
    fn from_policy(rules: &Value) -> Self {
        let list = |key: &str| rules[key].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        Self { allowed: list("allowed"), denied: list("denied"), restricted: rules["restricted"] == true }
    }

    /// A denied match always wins; otherwise the host must be listed when
    /// allowedDomains is set.
    fn allows(&self, host: &str) -> bool {
        !self.denied.iter().any(|pattern| domain_matches(pattern, host))
            && (!self.restricted || self.allowed.iter().any(|pattern| domain_matches(pattern, host)))
    }
}

const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Serve one proxy client: CONNECT tunnels and absolute-URI HTTP requests to
/// allowed hosts; everything else gets a 403 naming the setting.
async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut client: S, rules: Arc<NetworkRules>, blocked: Arc<Mutex<Vec<String>>>) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buffer = Vec::new();
    let end = loop {
        let mut chunk = [0u8; 4096];
        let read = client.read(&mut chunk).await?;
        anyhow::ensure!(read > 0, "client closed before sending a request");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") { break index + 4; }
        anyhow::ensure!(buffer.len() <= MAX_HEAD_BYTES, "request head too large");
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let rest = buffer[end..].to_vec();
    let first = head.lines().next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let (method, target, version) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or("HTTP/1.1"));
    let (host, port, forwarded) = if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = target.rsplit_once(':').context("CONNECT target needs host:port")?;
        (host.trim_matches(['[', ']']).to_owned(), port.parse::<u16>()?, None)
    } else {
        let url = reqwest::Url::parse(target).context("proxy requests need an absolute URL")?;
        anyhow::ensure!(url.scheme() == "http", "only http:// URLs can be proxied without CONNECT");
        let host = url.host_str().context("URL has no host")?.trim_matches(['[', ']']).to_owned();
        let path = match url.query() { Some(query) => format!("{}?{query}", url.path()), None => url.path().to_owned() };
        let head = head.replacen(first, &format!("{method} {path} {version}"), 1);
        (host, url.port_or_known_default().unwrap_or(80), Some(head))
    };
    if !rules.allows(&host) {
        blocked.lock().unwrap_or_else(|e| e.into_inner()).push(host.clone());
        let body = format!("Connection to {host} blocked by the sandbox network allowlist (sandbox.network.allowedDomains / deniedDomains in settings.json).\n");
        client.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
        return Ok(());
    }
    let mut upstream = tokio::time::timeout(std::time::Duration::from_secs(30), tokio::net::TcpStream::connect((host.as_str(), port))).await.context("connect timed out")??;
    match forwarded {
        None => client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?,
        Some(head) => upstream.write_all(head.as_bytes()).await?,
    }
    upstream.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

struct Proxy {
    endpoint: String,
    /// Hosts refused so far, in order, so a command's result can name them.
    blocked: Arc<Mutex<Vec<String>>>,
}

/// Running proxies, one per distinct rule set.
static PROXIES: std::sync::LazyLock<Mutex<HashMap<NetworkRules, Proxy>>> = std::sync::LazyLock::new(Default::default);

/// How many hosts the proxy for this policy has refused so far.
pub fn blocked_mark(policy: &Value) -> usize {
    let rules = NetworkRules::from_policy(&policy["network_rules"]);
    PROXIES.lock().unwrap_or_else(|e| e.into_inner()).get(&rules).map_or(0, |proxy| proxy.blocked.lock().unwrap_or_else(|e| e.into_inner()).len())
}

/// Hosts refused since `mark`, without repeats.
pub fn blocked_since(policy: &Value, mark: usize) -> Vec<String> {
    let rules = NetworkRules::from_policy(&policy["network_rules"]);
    let proxies = PROXIES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(proxy) = proxies.get(&rules) else { return Vec::new() };
    let blocked = proxy.blocked.lock().unwrap_or_else(|e| e.into_inner());
    let mut hosts: Vec<String> = Vec::new();
    for host in blocked.iter().skip(mark) { if !hosts.contains(host) { hosts.push(host.clone()); } }
    hosts
}

/// The proxy for `rules`, started on the current runtime when first needed:
/// a Unix socket path on Linux, a local TCP port on other platforms.
fn ensure_proxy(rules: &Value) -> Result<String> {
    let parsed = NetworkRules::from_policy(rules);
    let mut proxies = PROXIES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(proxy) = proxies.get(&parsed) { return Ok(proxy.endpoint.clone()); }
    let runtime = tokio::runtime::Handle::try_current().context("The sandbox network proxy needs an async runtime")?;
    let shared = Arc::new(parsed.clone());
    let blocked = Arc::new(Mutex::new(Vec::new()));
    let log = blocked.clone();
    let endpoint;
    #[cfg(unix)]
    if cfg!(target_os = "linux") {
        // Socket paths are limited to about 100 bytes; prefer the data
        // directory, falling back to the temp directory for long paths.
        let name = format!("{}-{}.sock", std::process::id(), proxies.len());
        let dir = PathBuf::from(rules["dir"].as_str().unwrap_or_default());
        let path = if dir.join(&name).as_os_str().len() < 100 && std::fs::create_dir_all(&dir).is_ok() { dir.join(&name) } else { std::env::temp_dir().join(format!("dcl-{name}")) };
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).with_context(|| format!("Starting the sandbox proxy at {}", path.display()))?;
        listener.set_nonblocking(true)?;
        let listener = { let _guard = runtime.enter(); tokio::net::UnixListener::from_std(listener)? };
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (rules, log) = (shared.clone(), log.clone());
                tokio::spawn(async move { if let Err(error) = serve(stream, rules, log).await { tracing::debug!("sandbox proxy: {error:#}"); } });
            }
        });
        endpoint = path.to_string_lossy().into_owned();
        proxies.insert(parsed, Proxy { endpoint: endpoint.clone(), blocked });
        return Ok(endpoint);
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("Starting the sandbox proxy")?;
    listener.set_nonblocking(true)?;
    endpoint = listener.local_addr()?.port().to_string();
    let listener = { let _guard = runtime.enter(); tokio::net::TcpListener::from_std(listener)? };
    runtime.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (rules, log) = (shared.clone(), log.clone());
            tokio::spawn(async move { if let Err(error) = serve(stream, rules, log).await { tracing::debug!("sandbox proxy: {error:#}"); } });
        }
    });
    proxies.insert(parsed, Proxy { endpoint: endpoint.clone(), blocked });
    Ok(endpoint)
}

fn proxy_env(port: &str) -> Vec<(String, String)> {
    let url = format!("http://127.0.0.1:{port}");
    let mut env: Vec<(String, String)> = ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"].iter().map(|name| (name.to_string(), url.clone())).collect();
    env.extend(["NO_PROXY", "no_proxy"].iter().map(|name| (name.to_string(), "localhost,127.0.0.1,::1".to_string())));
    env
}

/// Run as `__sandbox-bridge SOCKET -- COMMAND...` inside the sandbox: listen
/// on a local port, carry each connection to the proxy's socket, and run the
/// command with the proxy variables set. Returns None for any other argv.
pub fn run_bridge_from_args() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("__sandbox-bridge") { return None; }
    Some(bridge(&args[2..]).unwrap_or_else(|error| { eprintln!("sandbox bridge: {error:#}"); 126 }))
}

#[cfg(unix)]
fn bridge(args: &[String]) -> Result<i32> {
    use std::os::unix::net::UnixStream;
    let socket = PathBuf::from(args.first().context("socket path missing")?);
    anyhow::ensure!(args.get(1).map(String::as_str) == Some("--") && args.len() > 2, "usage: __sandbox-bridge SOCKET -- COMMAND...");
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port().to_string();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(upstream) = UnixStream::connect(&socket) else { continue };
            let (Ok(mut client_read), Ok(mut upstream_write)) = (client.try_clone(), upstream.try_clone()) else { continue };
            let (mut client_write, mut upstream_read) = (client, upstream);
            std::thread::spawn(move || { let _ = std::io::copy(&mut client_read, &mut upstream_write); let _ = upstream_write.shutdown(std::net::Shutdown::Write); });
            std::thread::spawn(move || { let _ = std::io::copy(&mut upstream_read, &mut client_write); let _ = client_write.shutdown(std::net::Shutdown::Write); });
        }
    });
    let status = std::process::Command::new(&args[2]).args(&args[3..]).envs(proxy_env(&port)).status().with_context(|| format!("starting {}", args[2]))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(not(unix))]
fn bridge(_args: &[String]) -> Result<i32> {
    bail!("the sandbox bridge runs only on Unix")
}

/// The executable that runs the bridge: this one, unless a test names another.
fn bridge_executable() -> Result<PathBuf> {
    match std::env::var_os("DCL_SANDBOX_BRIDGE") {
        Some(path) => Ok(PathBuf::from(path)),
        None => std::env::current_exe().context("Locating the executable for the sandbox bridge"),
    }
}

/// The program and leading arguments that run `shell` inside the sandbox.
pub fn wrap(policy: &Value, shell: &str) -> Result<(String, Vec<String>)> {
    let writable: Vec<&str> = policy["writable"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|path| Path::new(path).exists()).collect();
    let network = policy["network"] != false;
    let filtered = network && policy["network_rules"].is_object();
    if cfg!(target_os = "linux") {
        let Some(bwrap) = super::lsp_servers::find_on_path("bwrap") else {
            bail!("The sandbox is enabled but bubblewrap (bwrap) is not installed; install it or set sandbox.enabled to false");
        };
        let mut args: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--die-with-parent"].map(String::from).to_vec();
        for path in writable { args.extend(["--bind".into(), path.into(), path.into()]); }
        if !network || filtered { args.push("--unshare-net".into()); }
        args.push("--".into());
        if filtered {
            let socket = ensure_proxy(&policy["network_rules"])?;
            args.extend([bridge_executable()?.to_string_lossy().into_owned(), "__sandbox-bridge".into(), socket, "--".into()]);
        }
        args.push(shell.into());
        return Ok((bwrap.to_string_lossy().into_owned(), args));
    }
    if cfg!(target_os = "macos") {
        let quote = |path: &str| format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""));
        let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write* (literal \"/dev/null\") (subpath \"/private/tmp\") (subpath \"/private/var/folders\")");
        for path in writable { profile.push_str(&format!(" (subpath {})", quote(path))); }
        profile.push_str(")\n");
        if !network { profile.push_str("(deny network*)\n"); }
        if filtered {
            // Untested here: only the proxy's local port is reachable.
            let port = ensure_proxy(&policy["network_rules"])?;
            profile.push_str(&format!("(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:{port}\"))\n(allow network-outbound (remote unix-socket))\n"));
            let mut args = vec!["-p".into(), profile, "/usr/bin/env".into()];
            args.extend(proxy_env(&port).into_iter().map(|(name, value)| format!("{name}={value}")));
            args.push(shell.into());
            return Ok(("/usr/bin/sandbox-exec".into(), args));
        }
        return Ok(("/usr/bin/sandbox-exec".into(), vec!["-p".into(), profile, shell.into()]));
    }
    bail!("The sandbox is enabled but is not supported on this platform; set sandbox.enabled to false")
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn domain_rules_match_wildcards_and_deny_wins() {
        assert!(domain_matches("*.example.com", "api.example.com") && !domain_matches("*.example.com", "example.com"));
        assert!(domain_matches("GitHub.com.", "github.com") && !domain_matches("github.com", "evilgithub.com"));
        let rules = NetworkRules { allowed: vec!["*.npmjs.org".into(), "github.com".into()], denied: vec!["bad.npmjs.org".into()], restricted: true };
        assert!(rules.allows("registry.npmjs.org") && rules.allows("github.com"));
        assert!(!rules.allows("bad.npmjs.org") && !rules.allows("example.com"));
        let deny_only = NetworkRules { allowed: vec![], denied: vec!["example.com".into()], restricted: false };
        assert!(deny_only.allows("github.com") && !deny_only.allows("example.com"));
    }

    #[test]
    fn network_lists_add_up_across_sources_with_web_fetch_rules() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".claude")).unwrap();
        std::fs::write(data.path().join("settings.json"), r#"{"sandbox":{"enabled":true,"network":{"allowedDomains":["github.com"]}},"permissions":{"allow":["WebFetch(domain:docs.rs)","Bash(ls)"]}}"#).unwrap();
        std::fs::write(workspace.path().join(".claude/settings.json"), r#"{"sandbox":{"network":{"allowedDomains":["*.npmjs.org"],"deniedDomains":["x.npmjs.org"]}}}"#).unwrap();
        let policy = policy(&settings(data.path(), workspace.path()), workspace.path(), data.path(), &[]).unwrap();
        let rules = &policy["network_rules"];
        assert_eq!(rules["allowed"], json!(["github.com", "*.npmjs.org", "docs.rs"]));
        assert_eq!(rules["denied"], json!(["x.npmjs.org"]));
        assert_eq!(policy["network"], true);
        // Without either list, the network stays open as before.
        assert!(super::policy(&json!({"enabled":true}), workspace.path(), data.path(), &[]).unwrap()["network_rules"].is_null());
    }

    #[tokio::test]
    async fn proxy_tunnels_allowed_hosts_and_refuses_others() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = server.accept().await {
                tokio::spawn(async move { let mut buf = [0u8; 1024]; let read = stream.read(&mut buf).await.unwrap_or(0); let _ = stream.write_all(format!("echo:{}", String::from_utf8_lossy(&buf[..read])).as_bytes()).await; });
            }
        });
        let rules = Arc::new(NetworkRules { allowed: vec!["localhost".into()], denied: vec![], restricted: true });
        let (mut client, proxy_side) = tokio::io::duplex(8192);
        tokio::spawn(serve(proxy_side, rules.clone(), Default::default()));
        client.write_all(format!("CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost\r\n\r\nping").as_bytes()).await.unwrap();
        let mut reply = vec![0u8; 256];
        let mut got = String::new();
        while !got.contains("echo:ping") { let read = client.read(&mut reply).await.unwrap(); assert!(read > 0, "{got}"); got.push_str(&String::from_utf8_lossy(&reply[..read])); }
        assert!(got.starts_with("HTTP/1.1 200 Connection Established"));
        let (mut client, proxy_side) = tokio::io::duplex(8192);
        let blocked: Arc<Mutex<Vec<String>>> = Default::default();
        tokio::spawn(serve(proxy_side, rules, blocked.clone()));
        client.write_all(format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut refused = String::new();
        client.read_to_string(&mut refused).await.unwrap();
        assert!(refused.starts_with("HTTP/1.1 403") && refused.contains("allowedDomains"), "{refused}");
        assert_eq!(*blocked.lock().unwrap(), ["127.0.0.1"]);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn settings_merge_and_policy_lists_writable_paths() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".claude")).unwrap();
        std::fs::write(data.path().join("settings.json"), r#"{"sandbox":{"enabled":true,"network":"deny"}}"#).unwrap();
        std::fs::write(workspace.path().join(".claude/settings.local.json"), r#"{"sandbox":{"filesystem":{"allowWrite":["/var/cache/x"]}}}"#).unwrap();
        let merged = settings(data.path(), workspace.path());
        let policy = policy(&merged, workspace.path(), data.path(), &[]).unwrap();
        assert_eq!(policy["network"], false);
        assert_eq!(policy["allow_unsandboxed"], true);
        assert!(policy["writable"].as_array().unwrap().iter().any(|p| p == "/var/cache/x"));
        assert!(super::policy(&json!({"enabled":false}), workspace.path(), data.path(), &[]).is_none());
    }

    #[tokio::test]
    async fn sandboxed_commands_write_only_to_allowed_paths_and_can_lose_network() {
        if super::super::lsp_servers::find_on_path("bwrap").is_none() { return; }
        crate::config::OrchestrationConfig::init_global().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let cwd = workspace.path().to_str().unwrap();
        let policy = policy(&json!({"enabled":true,"network":"deny"}), workspace.path(), workspace.path(), &[]).unwrap();
        let run = |command: String| super::super::tool_bash_async(json!({"command":command,"__sandbox":policy.clone()}), cwd);
        let inside = run("echo ok > inside.txt && cat inside.txt".into()).await.unwrap();
        assert_eq!(inside["stdout"].as_str().unwrap().trim(), "ok");
        let blocked = run(format!("echo no > {}/x.txt", outside.path().display())).await;
        assert!(blocked.is_err() || blocked.unwrap()["exit_code"] != 0);
        assert!(!outside.path().join("x.txt").exists());
        let escaped = super::super::tool_bash_async(json!({"command":format!("echo yes > {}/y.txt", outside.path().display()),"__sandbox":policy.clone(),"dangerouslyDisableSandbox":true}), cwd).await.unwrap();
        assert_eq!(escaped["exit_code"], 0);
        assert!(outside.path().join("y.txt").exists());
    }
}
