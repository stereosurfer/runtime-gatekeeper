mod engine;
mod host_sample;
mod model;
mod observation;
mod platform;
mod protocol;
mod server;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::Duration,
};
use tiny_http::{Header, Response, Server};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("help");
    if mode == "fixture" {
        let port: u16 = args.get(2).context("fixture port required")?.parse()?;
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
        for mut stream in listener.incoming().flatten() {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
        }
        return Ok(());
    }
    if !["serve", "mcp", "mcp-observe", "check"].contains(&mode) {
        println!(
            "Runtime Gatekeeper\n  runtime-gatekeeper serve [runtime.yaml]\n  runtime-gatekeeper mcp [runtime.yaml]\n  runtime-gatekeeper mcp-observe [runtime.yaml]\n  runtime-gatekeeper check [runtime.yaml]"
        );
        return Ok(());
    }
    let path =
        PathBuf::from(args.get(2).map(String::as_str).unwrap_or("runtime.yaml")).canonicalize()?;
    let mut config: model::Config = serde_saphyr::from_slice(&fs::read(&path)?)?;
    if config.state_dir.is_relative() {
        config.state_dir = path.parent().unwrap().join(&config.state_dir);
    }
    config.validate()?;
    if mode == "check" {
        println!("Configuration valid: {} services", config.services.len());
        return Ok(());
    }
    if mode == "mcp" || mode == "mcp-observe" {
        return bridge(config, mode == "mcp-observe");
    }
    let port = config.port;
    // Bind first: failure must not mutate live state or credentials.
    let server = Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let engine = engine::Engine::new(config)?;
    let observer_token = new_token(&engine.config.state_dir.join("observer-token"))?;
    let token = new_token(&engine.config.state_dir.join("token"))?;
    eprintln!("Runtime Gatekeeper: http://127.0.0.1:{port}/#{token}");
    eprintln!(
        "Foreground daemon; no services auto-start. Stop daemon only after releasing jobs and stopping managed services."
    );
    server::run(server, engine, token, observer_token, port);
    Ok(())
}
fn respond(req: tiny_http::Request, code: u16, kind: &str, body: String) {
    let response=Response::from_string(body).with_status_code(code)
        .with_header(Header::from_bytes("Content-Type",kind).unwrap())
        .with_header(Header::from_bytes("Cache-Control","no-store").unwrap())
        .with_header(Header::from_bytes("X-Content-Type-Options","nosniff").unwrap())
        .with_header(Header::from_bytes("Referrer-Policy","no-referrer").unwrap())
        .with_header(Header::from_bytes("Content-Security-Policy","default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'").unwrap());
    let _ = req.respond(response);
}
fn new_token(path: &std::path::Path) -> Result<String> {
    let token = uuid::Uuid::new_v4().to_string();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    Ok(token)
}
fn local_client() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .redirects(0)
        .timeout(Duration::from_secs(300))
        .build()
}
fn bridge(config: model::Config, observer: bool) -> Result<()> {
    let bridge_id = uuid::Uuid::new_v4().to_string();
    let client = local_client();
    let credential = if observer { "observer-token" } else { "token" };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut stdout = std::io::stdout().lock();
    loop {
        let mut bytes = Vec::new();
        let n = std::io::Read::by_ref(&mut input)
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        ensure!(bytes.len() <= 1024 * 1024, "MCP message exceeds 1 MiB");
        let v: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                writeln!(
                    stdout,
                    "{}",
                    protocol::error(Value::Null, -32700, "Parse error")
                )?;
                stdout.flush()?;
                continue;
            }
        };
        let has_id = v.get("id").is_some();
        let v_id = v.get("id").cloned().unwrap_or(Value::Null);
        let token = fs::read_to_string(config.state_dir.join(credential))
            .context("start daemon before MCP client")?;
        let response = client
            .post(&format!("http://127.0.0.1:{}/rpc", config.port))
            .set("Authorization", &format!("Bearer {}", token.trim()))
            .set("X-Gatekeeper-Bridge", &bridge_id)
            .send_json(v);
        match response {
            Ok(r) => {
                if r.status() != 204 {
                    let body = r.into_string()?;
                    writeln!(stdout, "{body}")?;
                    stdout.flush()?;
                }
            }
            Err(e) => {
                if has_id {
                    writeln!(
                        stdout,
                        "{}",
                        protocol::error(
                            v_id.clone(),
                            -32000,
                            "daemon transport failed; verify status before retrying a mutation"
                        )
                    )?;
                    stdout.flush()?;
                    eprintln!("daemon transport failed: {e}");
                }
            }
        }
    }
    Ok(())
}
