mod engine;
mod model;
mod protocol;
use anyhow::{Context, Result, bail, ensure};
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
    if !["serve", "mcp", "check"].contains(&mode) {
        println!(
            "Runtime Gatekeeper\n  runtime-gatekeeper serve [runtime.yaml]\n  runtime-gatekeeper mcp [runtime.yaml]\n  runtime-gatekeeper check [runtime.yaml]"
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
    if mode == "mcp" {
        return bridge(config);
    }
    let port = config.port;
    // Bind first: failure must not mutate live state or credentials.
    let server = Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let mut engine = engine::Engine::new(config)?;
    let token = uuid::Uuid::new_v4().to_string();
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(engine.config.state_dir.join("token"))?;
    f.write_all(token.as_bytes())?;
    f.sync_all()?;
    eprintln!("Runtime Gatekeeper: http://127.0.0.1:{port}/#{token}");
    eprintln!(
        "Foreground daemon; no services auto-start. Stop daemon only after releasing jobs and stopping managed services."
    );
    for mut req in server.incoming_requests() {
        let header = |name: &'static str| {
            req.headers()
                .iter()
                .find(|h| h.field.equiv(name))
                .map(|h| h.value.as_str().to_owned())
        };
        let host_ok = header("Host")
            .is_some_and(|v| v == format!("127.0.0.1:{port}") || v == format!("localhost:{port}"));
        let origin_ok = header("Origin").is_none_or(|v| {
            v == format!("http://127.0.0.1:{port}") || v == format!("http://localhost:{port}")
        });
        if !host_ok || !origin_ok {
            respond(req, 403, "text/plain", "Forbidden".into());
            continue;
        }
        let method = req.method().as_str().to_owned();
        let url = req.url().to_owned();
        if method == "GET" && url == "/" {
            respond(
                req,
                200,
                "text/html; charset=utf-8",
                include_str!("dashboard.html").into(),
            );
            continue;
        }
        let auth_ok = header("Authorization").is_some_and(|v| v == format!("Bearer {token}"));
        if !auth_ok {
            respond(req, 401, "text/plain", "Unauthorized".into());
            continue;
        }
        if method == "GET" && url == "/api/status" {
            respond(req, 200, "application/json", engine.status().to_string());
            continue;
        }
        if method == "POST" && url == "/rpc" {
            if !header("Content-Type").is_some_and(|v| v.starts_with("application/json")) {
                respond(req, 415, "text/plain", "JSON required".into());
                continue;
            }
            let mut body = String::new();
            let read = req
                .as_reader()
                .take(1024 * 1024 + 1)
                .read_to_string(&mut body);
            if read.is_err() || body.len() > 1024 * 1024 {
                respond(
                    req,
                    413,
                    "text/plain",
                    "Body too large or invalid UTF-8".into(),
                );
                continue;
            }
            let output = match serde_json::from_str::<Value>(&body) {
                Ok(v) => protocol::rpc(&mut engine, v),
                Err(_) => Some(protocol::error(Value::Null, -32700, "Parse error")),
            };
            if let Some(v) = output {
                respond(req, 200, "application/json", v.to_string());
            } else {
                respond(req, 204, "application/json", String::new());
            }
            continue;
        }
        respond(req, 404, "text/plain", "Not found".into());
    }
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
fn bridge(config: model::Config) -> Result<()> {
    let token = fs::read_to_string(config.state_dir.join("token"))
        .context("start daemon before MCP client")?;
    let client = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build();
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
        let response = client
            .post(&format!("http://127.0.0.1:{}/rpc", config.port))
            .set("Authorization", &format!("Bearer {}", token.trim()))
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
                    bail!("daemon transport failed: {e}");
                }
            }
        }
    }
    Ok(())
}
