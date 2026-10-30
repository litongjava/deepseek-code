//! Development Remote Control server with simulated data.
//! Usage: cargo run -p deepseek-code-core --example remote_control_mock -- [PORT] [TOKEN] [PORT_FILE]
//! Then `/remote-control --server http://127.0.0.1:PORT` in the CLI. Play the
//! remote client with POST /mock/send (frame JSON), /mock/drop, /mock/fail
//! ({"status":503,"count":2}) and inspect GET /mock/state.
use deepseek_code_core::services::remote_control::mock::MockServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let port = args.next().map(|v| v.parse()).transpose()?.unwrap_or(0);
    let token = args.next().filter(|v| !v.is_empty() && v != "-");
    let port_file = args.next();
    let server = MockServer::start(port, token, vec![]).await?;
    if let Some(file) = port_file {
        std::fs::write(file, server.url.rsplit(':').next().unwrap_or_default())?;
    }
    println!("Remote Control mock listening on {}", server.url);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
