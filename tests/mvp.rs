use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
struct Harness {
    child: Child,
    dir: tempfile::TempDir,
    port: u16,
    token: String,
    service_port: u16,
}
impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let service_port = free_port();
        let external_port = free_port();
        let slow_port = free_port();
        fs::write(dir.path().join("runtime.yaml"),format!("port: {port}\nstate_dir: state\nsafety_margin_bytes: 0\nservices:\n  demo:\n    mode: managed\n    command: ['@self', fixture, '{service_port}']\n    port: {service_port}\n    memory_bytes: 1048576\n    disposable: true\n    allowed_actions: [start, stop, restart]\n  external:\n    mode: discovered\n    port: {external_port}\n  slow:\n    mode: managed\n    command: [/bin/sleep, '30']\n    port: {slow_port}\n    startup_timeout_ms: 100\n    allowed_actions: [start, stop]\n  fail:\n    mode: managed\n    command: [/usr/bin/false]\n    allowed_actions: [start, stop]\n")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_runtime-gatekeeper"))
            .arg("serve")
            .arg(dir.path().join("runtime.yaml"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let token = loop {
            if let Ok(t) = fs::read_to_string(dir.path().join("state/token"))
                && !t.is_empty()
            {
                break t;
            }
            assert!(Instant::now() < deadline, "daemon start timed out");
            thread::sleep(Duration::from_millis(50));
        };
        Self {
            child,
            dir,
            port,
            token,
            service_port,
        }
    }
    fn rpc(&self, name: &str, args: Value) -> Value {
        ureq::post(&format!("http://127.0.0.1:{}/rpc",self.port)).set("Authorization",&format!("Bearer {}",self.token)).send_json(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap().into_json().unwrap()
    }
    fn call(&self, name: &str, args: Value) -> Value {
        let v = self.rpc(name, args);
        assert_eq!(v["result"]["isError"], false, "{v}");
        v["result"]["structuredContent"].clone()
    }
    fn request(&self, id: &str) -> Value {
        self.call(
            "runtime.request",
            json!({"job_id":id,"agent":"test","requires":["demo"]}),
        )
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn lifecycle_shared_leases_and_blocked_resources() {
    let h = Harness::new();
    let blocked = h.call(
        "runtime.request",
        json!({"job_id":"huge","agent":"test","requires":["demo"],"memory_bytes":u64::MAX-1048576}),
    );
    assert_eq!(blocked["status"], "BLOCKED_RESOURCE");
    assert!(blocked["shortfall"].as_u64().unwrap() > 0);
    assert!(TcpListener::bind(("127.0.0.1", h.service_port)).is_ok());
    let one = h.request("one");
    assert_eq!(one["status"], "READY");
    assert_eq!(h.request("one")["runtime_id"], one["runtime_id"]);
    let two = h.request("two");
    assert_eq!(two["status"], "READY");
    let service = h.call("services.get", json!({"service_id":"demo"}));
    assert_eq!(service["class"], "managed");
    assert_eq!(service["leases"].as_array().unwrap().len(), 2);
    assert!(!service["processes"].as_array().unwrap().is_empty());
    for action in ["services.stop", "services.restart"] {
        assert_eq!(
            h.rpc(action, json!({"service_id":"demo"}))["result"]["isError"],
            true
        );
    }
    h.call("runtime.release", json!({"runtime_id":one["runtime_id"]}));
    assert!(
        h.call("runtime.cleanup", json!({}))["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    h.call("runtime.release", json!({"runtime_id":two["runtime_id"]}));
    h.call("services.restart", json!({"service_id":"demo"}));
    let result = h.call("runtime.cleanup", json!({}));
    assert_eq!(result["results"][0]["result"], "stopped");
    assert!(TcpListener::bind(("127.0.0.1", h.service_port)).is_ok());
    assert_eq!(
        h.rpc("services.restart", json!({"service_id":"external"}))["result"]["isError"],
        true
    );
    assert!(
        fs::read_to_string(h.dir.path().join("state/events.jsonl"))
            .unwrap()
            .lines()
            .all(|l| serde_json::from_str::<Value>(l).is_ok())
    );
}
#[test]
fn external_listener_is_never_adopted_or_stopped() {
    let h = Harness::new();
    let listener = TcpListener::bind(("127.0.0.1", h.service_port)).unwrap();
    let v = h.call("services.get", json!({"service_id":"demo"}));
    assert_eq!(v["class"], "discovered");
    assert_eq!(
        h.rpc("services.start", json!({"service_id":"demo"}))["result"]["isError"],
        true
    );
    assert_eq!(
        h.rpc("services.stop", json!({"service_id":"demo"}))["result"]["isError"],
        true
    );
    h.call("runtime.cleanup", json!({}));
    assert!(listener.local_addr().is_ok());
}
#[test]
fn failure_rolls_back_new_services_and_does_not_lease() {
    let h = Harness::new();
    let v = h.call(
        "runtime.request",
        json!({"job_id":"failure","agent":"test","requires":["demo","fail"]}),
    );
    assert_eq!(v["status"], "BLOCKED_SERVICE");
    assert_eq!(v["rollback"][0]["result"], "stopped");
    assert!(TcpListener::bind(("127.0.0.1", h.service_port)).is_ok());
    assert_eq!(
        h.call("services.get", json!({"service_id":"demo"}))["leases"],
        json!([])
    );
}
#[test]
fn stdio_protocol_and_http_security() {
    let h = Harness::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_runtime-gatekeeper"))
        .arg("mcp")
        .arg(h.dir.path().join("runtime.yaml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let mut input = child.stdin.take().unwrap();
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
    )
    .unwrap();
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(lines[1]["result"]["tools"].as_array().unwrap().len(), 11);
    let url = format!("http://127.0.0.1:{}/api/status", h.port);
    assert!(matches!(
        ureq::get(&url).call(),
        Err(ureq::Error::Status(401, _))
    ));
    assert!(matches!(
        ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", h.token))
            .set("Origin", "https://evil.example")
            .call(),
        Err(ureq::Error::Status(403, _))
    ));
    assert!(matches!(
        ureq::get(&url).set("Host", "evil.example").call(),
        Err(ureq::Error::Status(403, _))
    ));
    assert_eq!(
        h.rpc(
            "runtime.request",
            json!({"job_id":"bad","agent":"t","requires":["missing"]})
        )["result"]["isError"],
        true
    );
    assert_eq!(
        h.rpc("runtime.cleanup", json!({"force":true}))["error"]["code"],
        -32602
    );
}

#[test]
fn restart_preserves_leases_without_adopting_processes() {
    let mut h = Harness::new();
    let _listener = TcpListener::bind(("127.0.0.1", h.service_port)).unwrap();
    let lease = h.request("existing");
    assert_eq!(lease["status"], "READY");
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    h.child = Command::new(env!("CARGO_BIN_EXE_runtime-gatekeeper"))
        .arg("serve")
        .arg(h.dir.path().join("runtime.yaml"))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let token = fs::read_to_string(h.dir.path().join("state/token")).unwrap();
        if !token.is_empty() && token != h.token {
            h.token = token;
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        h.call("jobs.get", json!({"job_id":"existing"}))["status"],
        "INTERRUPTED"
    );
    assert_eq!(h.request("existing")["status"], "BLOCKED_SERVICE");
    assert_eq!(
        h.call("services.get", json!({"service_id":"demo"}))["class"],
        "discovered"
    );
    assert_eq!(
        h.rpc("services.stop", json!({"service_id":"demo"}))["result"]["isError"],
        true
    );
    h.call("runtime.release", json!({"runtime_id":lease["runtime_id"]}));
    assert!(
        h.call("runtime.cleanup", json!({}))["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unhealthy_start_rolls_back_and_job_id_cannot_change() {
    let h = Harness::new();
    assert_eq!(h.request("same")["status"], "READY");
    assert_eq!(
        h.rpc(
            "runtime.request",
            json!({"job_id":"same","agent":"other","requires":["demo"]})
        )["result"]["isError"],
        true
    );
    let lease = h.call("jobs.get", json!({"job_id":"same"}));
    h.call("runtime.release", json!({"runtime_id":lease["runtime_id"]}));
    h.call("runtime.cleanup", json!({}));
    let failed = h.call(
        "runtime.request",
        json!({"job_id":"timeout","agent":"test","requires":["slow"]}),
    );
    assert_eq!(failed["status"], "BLOCKED_SERVICE");
    assert!(
        failed["reason"]
            .as_str()
            .unwrap()
            .contains("HEALTH_TIMEOUT")
    );
    assert_eq!(
        h.call("services.get", json!({"service_id":"slow"}))["state"],
        "stopped"
    );
}

#[test]
fn storage_failure_disables_mutations() {
    let h = Harness::new();
    let _external = TcpListener::bind(("127.0.0.1", h.service_port)).unwrap();
    let events = h.dir.path().join("state/events.jsonl");
    fs::rename(&events, h.dir.path().join("state/events.backup")).unwrap();
    fs::create_dir(&events).unwrap();
    assert_eq!(
        h.rpc(
            "runtime.request",
            json!({"job_id":"storage","agent":"test","requires":["demo"]})
        )["result"]["isError"],
        true
    );
    assert_eq!(h.call("runtime.status", json!({}))["storage_failed"], true);
    let denied = h.rpc("runtime.cleanup", json!({}));
    assert_eq!(denied["result"]["isError"], true);
    assert!(
        denied["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("STORAGE_FAILED")
    );
}

#[test]
fn concurrent_requests_start_once_and_failed_batch_preserves_shared_service() {
    let h = Harness::new();
    let results = thread::scope(|scope| {
        let a = scope.spawn(|| h.request("parallel-a"));
        let b = scope.spawn(|| h.request("parallel-b"));
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    assert!(results.iter().all(|v| v["status"] == "READY"));
    let events = fs::read_to_string(h.dir.path().join("state/events.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|l| serde_json::from_str::<Value>(l).unwrap()["event"] == "service_started")
            .count(),
        1
    );
    let failed = h.call(
        "runtime.request",
        json!({"job_id":"batch","agent":"test","requires":["demo","fail"]}),
    );
    assert_eq!(failed["status"], "BLOCKED_SERVICE");
    assert_eq!(
        h.call("services.get", json!({"service_id":"demo"}))["state"],
        "running"
    );
    for r in results {
        h.call("runtime.release", json!({"runtime_id":r["runtime_id"]}));
        h.call("runtime.release", json!({"runtime_id":r["runtime_id"]}));
    }
    h.call("runtime.cleanup", json!({}));
}

#[test]
fn process_exit_is_not_reported_ready_on_retry() {
    let h = Harness::new();
    let lease = h.request("exit");
    let service = h.call("services.get", json!({"service_id":"demo"}));
    let pid = service["processes"][0]["pid"].as_u64().unwrap() as i32;
    // This isolated daemon created this fixture; simulate its external crash.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    thread::sleep(Duration::from_millis(150));
    assert_eq!(h.request("exit")["status"], "BLOCKED_SERVICE");
    assert_eq!(
        h.call("services.get", json!({"service_id":"demo"}))["leases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.call("runtime.release", json!({"runtime_id":lease["runtime_id"]}));
}

#[test]
fn duplicate_daemon_and_corrupt_state_fail_closed() {
    let mut h = Harness::new();
    let token = h.token.clone();
    let duplicate = Command::new(env!("CARGO_BIN_EXE_runtime-gatekeeper"))
        .arg("serve")
        .arg(h.dir.path().join("runtime.yaml"))
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert_eq!(
        fs::read_to_string(h.dir.path().join("state/token")).unwrap(),
        token
    );
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    fs::write(h.dir.path().join("state/state.json"), "not JSON").unwrap();
    let corrupt = Command::new(env!("CARGO_BIN_EXE_runtime-gatekeeper"))
        .arg("serve")
        .arg(h.dir.path().join("runtime.yaml"))
        .output()
        .unwrap();
    assert!(!corrupt.status.success());
    assert!(
        String::from_utf8(corrupt.stderr)
            .unwrap()
            .contains("refusing to discard leases")
    );
    assert_eq!(
        fs::read_to_string(h.dir.path().join("state/state.json")).unwrap(),
        "not JSON"
    );
}

#[test]
fn rollback_write_failure_keeps_survivor_visible_and_disables_control() {
    let h = Harness::new();
    let reply = thread::scope(|scope| {
        let request = scope.spawn(|| {
            h.rpc(
                "runtime.request",
                json!({"job_id":"rollback-fault","agent":"test","requires":["demo","slow"]}),
            )
        });
        let events = h.dir.path().join("state/events.jsonl");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = fs::read_to_string(&events).unwrap_or_default();
            if text.contains("service_started") {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        fs::rename(&events, h.dir.path().join("state/events.saved")).unwrap();
        fs::create_dir(&events).unwrap();
        request.join().unwrap()
    });
    assert_eq!(reply["result"]["isError"], true);
    let status = h.call("runtime.status", json!({}));
    assert_eq!(status["storage_failed"], true);
    let job = h.call("jobs.get", json!({"job_id":"rollback-fault"}));
    assert_eq!(job["status"], "BLOCKED_SERVICE");
    assert!(
        job["result"]["rollback"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["result"] != "stopped")
    );
    assert_eq!(
        h.rpc("runtime.cleanup", json!({}))["result"]["isError"],
        true
    );
    // Fault injection intentionally prevents supervisor shutdown; stop only fixtures
    // whose ownership was returned by this isolated daemon, after observing evidence.
    for service in status["services"].as_array().unwrap() {
        if service["class"] == "managed" {
            for p in service["processes"].as_array().unwrap() {
                unsafe {
                    libc::kill(p["pid"].as_u64().unwrap() as i32, libc::SIGKILL);
                }
            }
        }
    }
    thread::sleep(Duration::from_millis(100));
}
