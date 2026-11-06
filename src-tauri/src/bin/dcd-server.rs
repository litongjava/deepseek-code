//! Standalone Bridge server: the Desktop backend and its web UI without a window.
//!
//! Browsers, the mobile apps and other clients connect over HTTP(S). It listens on
//! loopback by default; listening on a network address requires HTTPS, and every
//! API request needs the access token stored in `<data dir>/server-token`.

use deepseek_desktop_tauri_lib::bridge::server_mode::{self, ServeOptions, TlsFiles};
use deepseek_desktop_tauri_lib::bridge::BridgeServer;
use deepseek_desktop_tauri_lib::launch_permissions::LaunchPermissions;
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "Usage: dcd-server [options]

Serves the DeepSeek Code web UI and API.

  --host <addr>              Listen address (default 127.0.0.1)
  --port <port>              Listen port (default 30085)
  --web-dir <dir>            Built web UI to serve (default: web/ or dist/ next to
                             the executable, then ./dist)
  --no-web                   Serve the API only
  --token-file <file>        Access token file (default <data dir>/server-token)
  --print-token              Print the access token and exit
  --tls-cert <file>          PEM certificate chain for HTTPS
  --tls-key <file>           PEM private key for HTTPS
  --behind-https-proxy       A reverse proxy terminates HTTPS in front of this server
  --allow-origin <origin>    Also accept API calls from this origin (repeatable)
  --dangerously-skip-permissions
                             Start in bypass mode

Data is stored in DEEPSEEK_CODE_DATA_DIR or ~/.deepseek-code, shared with Desktop.
Run only one of Desktop and dcd-server on the same data directory at a time.";

struct Args {
    options: ServeOptions,
    web_dir: Option<PathBuf>,
    no_web: bool,
    token_file: Option<PathBuf>,
    print_token: bool,
    permissions: Vec<String>,
}

fn parse_args(args: Vec<String>) -> anyhow::Result<Args> {
    let mut parsed = Args {
        options: ServeOptions::local(30085),
        web_dir: None,
        no_web: false,
        token_file: None,
        print_token: false,
        permissions: Vec::new(),
    };
    parsed.options.try_next_ports = false;
    // App origins cannot be produced by web pages, so mobile clients work by default.
    parsed.options.allowed_origins =
        vec!["tauri://localhost".into(), "http://tauri.localhost".into(), "https://tauri.localhost".into()];
    let (mut cert, mut key) = (None, None);
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| iter.next().ok_or_else(|| anyhow::anyhow!("{name} needs a value"));
        match arg.as_str() {
            "--host" => parsed.options.host = value("--host")?.parse()?,
            "--port" => parsed.options.port = value("--port")?.parse()?,
            "--web-dir" => parsed.web_dir = Some(value("--web-dir")?.into()),
            "--no-web" => parsed.no_web = true,
            "--token-file" => parsed.token_file = Some(value("--token-file")?.into()),
            "--print-token" => parsed.print_token = true,
            "--tls-cert" => cert = Some(PathBuf::from(value("--tls-cert")?)),
            "--tls-key" => key = Some(PathBuf::from(value("--tls-key")?)),
            "--behind-https-proxy" => parsed.options.behind_https_proxy = true,
            "--allow-origin" => parsed.options.allowed_origins.push(value("--allow-origin")?),
            "--dangerously-skip-permissions" | "--allow-dangerously-skip-permissions" => {
                parsed.permissions.push(arg)
            }
            other => anyhow::bail!("Unknown option: {other}"),
        }
    }
    parsed.options.tls = match (cert, key) {
        (Some(cert), Some(key)) => Some(TlsFiles { cert, key }),
        (None, None) => None,
        _ => anyhow::bail!("--tls-cert and --tls-key must be given together"),
    };
    Ok(parsed)
}

fn default_web_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    [exe_dir.join("web"), exe_dir.join("dist"), PathBuf::from("dist")]
        .into_iter()
        .find(|dir| dir.join("index.html").is_file())
}

fn main() {
    // Sandboxed commands re-enter this executable to carry the network proxy.
    if let Some(code) = deepseek_code_core::tools::sandbox::run_bridge_from_args() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| matches!(arg.as_str(), "--help" | "-h")) {
        println!("{USAGE}");
        return;
    }
    let args = parse_args(args).unwrap_or_else(|error| {
        eprintln!("{error}. Use dcd-server --help for supported options.");
        std::process::exit(2)
    });
    if let Err(error) = run(args) {
        eprintln!("dcd-server: {error}");
        std::process::exit(1);
    }
}

fn run(mut args: Args) -> anyhow::Result<()> {
    let data_dir = deepseek_code_core::runtime::RuntimePaths::default_data_dir()?;
    let token_file = args.token_file.clone().unwrap_or_else(|| data_dir.join("server-token"));
    let (token, created) = server_mode::load_or_create_token(&token_file)?;
    if args.print_token {
        println!("{token}");
        return Ok(());
    }
    args.options.access_token = Some(token.clone());
    if !args.no_web {
        args.options.static_dir = args.web_dir.clone().or_else(default_web_dir);
        if args.options.static_dir.is_none() {
            eprintln!("No web UI found (build it with `npm run build`, or pass --web-dir); serving the API only.");
        }
    }
    args.options.validate()?;
    let permissions = LaunchPermissions::parse(args.permissions.clone())?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,Bridge=info")),
        )
        .init();

    let db_path = data_dir.join(deepseek_code_core::runtime::DATABASE_FILE);
    let db = Arc::new(deepseek_desktop_tauri_lib::db::DbManager::new(db_path)?);
    db.init()?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let scheduler_db = db.clone();
        let scheduler_dir = data_dir.clone();
        tokio::spawn(async move {
            deepseek_desktop_tauri_lib::scheduler::ScheduledTaskScheduler::new(scheduler_db)
                .with_data_dir(scheduler_dir)
                .start()
                .await;
        });
        let scheme = if args.options.is_https() { "https" } else { "http" };
        let host = match args.options.host {
            std::net::IpAddr::V4(ip) if ip.is_unspecified() => "<this host>".to_string(),
            std::net::IpAddr::V6(ip) if ip.is_unspecified() => "<this host>".to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        };
        println!("DeepSeek Code server: {scheme}://{host}:{}", args.options.port);
        if created {
            println!("Access token (saved in {}): {token}", token_file.display());
        } else {
            println!("Access token: {} (dcd-server --print-token)", token_file.display());
        }
        let bridge = BridgeServer::new(data_dir, db).with_launch_permissions(permissions);
        bridge.serve(args.options).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<Args> {
        parse_args(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn defaults_listen_on_loopback_without_port_fallback() {
        let args = parse(&[]).unwrap();
        assert!(args.options.host.is_loopback());
        assert_eq!(args.options.port, 30085);
        assert!(!args.options.try_next_ports);
        assert!(args.options.allowed_origins.contains(&"tauri://localhost".to_string()));
    }

    #[test]
    fn parses_network_and_tls_options() {
        let args = parse(&["--host", "0.0.0.0", "--port", "8443", "--tls-cert", "c.pem", "--tls-key", "k.pem",
            "--allow-origin", "https://app.example"]).unwrap();
        assert_eq!(args.options.port, 8443);
        assert!(args.options.is_https());
        assert!(args.options.allowed_origins.contains(&"https://app.example".to_string()));
    }

    #[test]
    fn rejects_incomplete_or_unknown_options() {
        assert!(parse(&["--tls-cert", "c.pem"]).is_err());
        assert!(parse(&["--port"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}
