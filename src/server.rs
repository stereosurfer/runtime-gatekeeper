//! One engine writer; HTTP reads use a timestamped snapshot during slow controls.
use crate::{engine::Engine, protocol, respond};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Request, Server};
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
struct Shared {
    snapshot: Value,
    sampled: Instant,
    sampled_at: u64,
    sample_id: String,
    busy: Option<String>,
    clients: BTreeMap<String, Value>,
}
impl Shared {
    fn status(&self) -> Value {
        let mut s = self.snapshot.clone();
        s["snapshot"] = json!({"id": self.sample_id, "sampled_at":self.sampled_at,"age_ms":self.sampled.elapsed().as_millis() as u64,"control_operation":self.busy,"stale":self.sampled.elapsed()>Duration::from_secs(10)});
        s["resources"] = crate::observation::resources(&s);
        s["mcp"] = json!({"transport":"stdio bridge → loopback daemon","tool_exposure":"unknown_requires_client_verification","clients":self.clients.values().collect::<Vec<_>>(),"note":"Last observed traffic is not proof a client is still connected. Client tool exposure cannot be inferred by the daemon."});
        s
    }
    fn observe(&mut self, bridge: &Option<String>, v: &Value, output: &Option<Value>) {
        let Some(id) = bridge else { return };
        if !self.clients.contains_key(id) && self.clients.len() >= 64 {
            let oldest = self
                .clients
                .iter()
                .min_by_key(|(_, v)| v["last_seen"].as_u64().unwrap_or(0))
                .map(|(k, _)| k.clone())
                .unwrap();
            self.clients.remove(&oldest);
        }
        let c = self.clients.entry(id.clone()).or_insert_with(
            || json!({"bridge_id":id,"first_seen":now(),"tool_exposure":"unknown"}),
        );
        c["last_seen"] = json!(now());
        c["last_method"] = v["method"].clone();
        if v["method"] == "initialize"
            && output
                .as_ref()
                .is_some_and(|o| o["result"]["serverInfo"].is_object())
        {
            // Store only bounded display fields, never arbitrary client metadata.
            for field in ["name", "version"] {
                c[field] = json!(
                    v["params"]["clientInfo"][field]
                        .as_str()
                        .unwrap_or("unknown")
                        .chars()
                        .take(128)
                        .collect::<String>()
                );
            }
            c["initialized_at"] = json!(now());
        }
        if let Some(o) = output {
            let ok = o.get("error").is_none() && o["result"]["isError"] != true;
            c["last_result"] = json!(if ok { "success" } else { "error" });
            if ok && v["method"] == "tools/list" {
                c["tools_list_at"] = json!(now());
            }
            if v["method"] == "tools/call" {
                c["last_tool"] = json!(
                    v["params"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(128)
                        .collect::<String>()
                );
                if ok {
                    c["last_successful_call_at"] = json!(now());
                }
            }
        }
    }
}
fn read_call(
    s: Value,
    name: &str,
    args: Value,
    reader: Option<&mut crate::observation::QueueReader>,
) -> Result<Value> {
    if name == "runtime.observe" {
        return crate::observation::report(&s, args["service_id"].as_str(), reader);
    }
    let mut data = match name {
        "runtime.status" => s,
        "services.list" => json!({"services":s["services"],"snapshot":s["snapshot"]}),
        "jobs.list" => json!({"jobs":s["jobs"],"snapshot":s["snapshot"]}),
        "services.get" | "jobs.get" => {
            let (list, key, wanted) = if name == "services.get" {
                ("services", "id", &args["service_id"])
            } else {
                ("jobs", "runtime_id", &args["job_id"])
            };
            let mut row = s[list]
                .as_array()
                .context("snapshot unavailable")?
                .iter()
                .find(|v| {
                    if list == "jobs" {
                        v["request"]["job_id"] == *wanted
                    } else {
                        v[key] == *wanted
                    }
                })
                .cloned()
                .context("unknown service or job")?;
            row["snapshot"] = s["snapshot"].clone();
            row
        }
        _ => bail!("unknown read tool"),
    };
    data["evidence_scope"] = json!({
        "source": "gatekeeper_registry",
        "native_queue": "unavailable_in_this_view",
        "graphics_memory": "unavailable_in_this_view"
    });
    Ok(data)
}
fn reply(req: Request, output: Option<Value>) {
    match output {
        Some(v) => respond(req, 200, "application/json", v.to_string()),
        None => respond(req, 204, "application/json", String::new()),
    }
}
struct Work {
    req: Request,
    message: Value,
    bridge: Option<String>,
}
fn capture(engine: &mut Engine, host: &mut crate::host_sample::HostSampler) -> Value {
    let mut s = engine.status();
    host.apply(&mut s);
    crate::observation::prepare(&mut s);
    s
}
pub fn run(server: Server, mut engine: Engine, token: String, observer_token: String, port: u16) {
    let mut host = crate::host_sample::HostSampler::new();
    let shared = Arc::new(Mutex::new(Shared {
        snapshot: capture(&mut engine, &mut host),
        sample_id: uuid::Uuid::new_v4().to_string(),
        sampled: Instant::now(),
        sampled_at: now(),
        busy: None,
        clients: BTreeMap::new(),
    }));
    let (tx, rx) = mpsc::sync_channel::<Work>(16);
    let state = shared.clone();
    std::thread::spawn(move || {
        loop {
            let work = match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(w) => Some(w),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            };
            let result = work.as_ref().map(|w| {
                state.lock().unwrap().busy = Some(
                    w.message["params"]["name"]
                        .as_str()
                        .unwrap_or("control")
                        .into(),
                );
                protocol::rpc(
                    false,
                    |name, args| engine.call(name, args),
                    w.message.clone(),
                )
            });
            let snapshot = capture(&mut engine, &mut host);
            {
                let mut s = state.lock().unwrap();
                s.snapshot = snapshot;
                s.sample_id = uuid::Uuid::new_v4().to_string();
                s.sampled = Instant::now();
                s.sampled_at = now();
                s.busy = None;
                if let (
                    Some(Work {
                        message, bridge, ..
                    }),
                    Some(o),
                ) = (&work, &result)
                {
                    s.observe(bridge, message, o);
                }
            }
            if let (Some(w), Some(o)) = (work, result) {
                reply(w.req, o);
            }
        }
    });
    serve_http(&server, shared, tx, &token, &observer_token, port);
}
struct ReadRequest {
    req: Request,
    message: Value,
    bridge: Option<String>,
    observer: bool,
}
impl ReadRequest {
    fn reply(
        self,
        shared: &Mutex<Shared>,
        mut reader: Option<&mut crate::observation::QueueReader>,
    ) {
        let snapshot = shared.lock().unwrap().status();
        let output = protocol::rpc(
            self.observer,
            |name, args| read_call(snapshot.clone(), name, args, reader.as_deref_mut()),
            self.message.clone(),
        );
        shared
            .lock()
            .unwrap()
            .observe(&self.bridge, &self.message, &output);
        reply(self.req, output);
    }
}
fn serve_http(
    server: &Server,
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::SyncSender<Work>,
    token: &str,
    observer_token: &str,
    port: u16,
) {
    // Never queue a future probe against a potentially replaced service.
    let (probe_tx, probe_rx) = mpsc::sync_channel::<ReadRequest>(0);
    let state = shared.clone();
    std::thread::spawn(move || {
        let mut reader = crate::observation::QueueReader::default();
        while let Ok(work) = probe_rx.recv() {
            work.reply(&state, Some(&mut reader));
        }
    });
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
        if req.method().as_str() == "GET" && req.url() == "/" {
            respond(
                req,
                200,
                "text/html; charset=utf-8",
                include_str!("dashboard.html").into(),
            );
            continue;
        }
        let authorization = header("Authorization");
        let observer = authorization.as_deref() == Some(&format!("Bearer {observer_token}"));
        if !observer && authorization.as_deref() != Some(&format!("Bearer {token}")) {
            respond(req, 401, "text/plain", "Unauthorized".into());
            continue;
        }
        if req.method().as_str() == "GET" && req.url() == "/api/status" {
            let mut s = shared.lock().unwrap().status();
            s["read_only"] = json!(observer);
            respond(req, 200, "application/json", s.to_string());
            continue;
        }
        if req.method().as_str() != "POST" || req.url() != "/rpc" {
            respond(req, 404, "text/plain", "Not found".into());
            continue;
        }
        if !header("Content-Type").is_some_and(|v| v.starts_with("application/json")) {
            respond(req, 415, "text/plain", "JSON required".into());
            continue;
        }
        let bridge = header("X-Gatekeeper-Bridge")
            .filter(|v| v.len() <= 64 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        let mut body = String::new();
        if req
            .as_reader()
            .take(1024 * 1024 + 1)
            .read_to_string(&mut body)
            .is_err()
            || body.len() > 1024 * 1024
        {
            respond(
                req,
                413,
                "text/plain",
                "Body too large or invalid UTF-8".into(),
            );
            continue;
        }
        let message: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => {
                reply(
                    req,
                    Some(protocol::error(Value::Null, -32700, "Parse error")),
                );
                continue;
            }
        };
        if !observer
            && message["method"] == "tools/call"
            && !protocol::read_only(message["params"]["name"].as_str().unwrap_or(""))
        {
            if let Err(e) = tx.try_send(Work {
                req,
                message,
                bridge,
            }) {
                respond(
                    match e {
                        mpsc::TrySendError::Full(w) | mpsc::TrySendError::Disconnected(w) => w.req,
                    },
                    503,
                    "text/plain",
                    "Control queue unavailable; inspect state before retry".into(),
                );
            }
        } else {
            let targeted = message["method"] == "tools/call"
                && message["params"]["name"] == "runtime.observe"
                && message["params"]["arguments"]["service_id"].is_string();
            let work = ReadRequest {
                req,
                message,
                bridge,
                observer,
            };
            if targeted {
                if let Err(
                    mpsc::TrySendError::Full(work) | mpsc::TrySendError::Disconnected(work),
                ) = probe_tx.try_send(work)
                {
                    work.reply(&shared, None);
                }
            } else {
                work.reply(&shared, None);
            }
        }
    }
}

// Test binary only. Freeze the sampler while exercising the production HTTP router and stdio bridge.
#[cfg(test)]
#[test]
#[ignore = "driven by tests/snapshot_dashboard.py with an isolated temporary directory"]
fn serve_fixed_snapshot() {
    let root = std::path::PathBuf::from(std::env::var_os("GK_FIXTURE_DIR").unwrap());
    let fixture: Value =
        serde_json::from_slice(&std::fs::read(root.join("fixture.json")).unwrap()).unwrap();
    let mut raw = fixture["raw"].clone();
    crate::observation::prepare(&mut raw);
    let server = Server::http(("127.0.0.1", 0)).unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let shared = Arc::new(Mutex::new(Shared {
        snapshot: raw,
        sampled: Instant::now() - Duration::from_millis(fixture["age_ms"].as_u64().unwrap()),
        sampled_at: fixture["sampled_at"].as_u64().unwrap(),
        sample_id: fixture["sample_id"].as_str().unwrap().to_owned(),
        busy: fixture["busy"].as_str().map(str::to_owned),
        clients: BTreeMap::new(),
    }));
    let (tx, _rx) = mpsc::sync_channel(1);
    std::fs::write(root.join("port"), port.to_string()).unwrap();
    serve_http(
        &server,
        shared,
        tx,
        "fixture-admin",
        "fixture-observer",
        port,
    );
}
