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
        Self::with_timeout(100)
    }
    fn with_timeout(timeout_ms: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let service_port = free_port();
        let external_port = free_port();
        let slow_port = free_port();
        // The short-lived test commands have explicit startup reservations so
        // their rollback paths are not masked by the unknown-memory gate.
        fs::write(dir.path().join("runtime.yaml"),format!("port: {port}\nstate_dir: state\nsafety_margin_bytes: 0\nservices:\n  demo:\n    mode: managed\n    command: ['@self', fixture, '{service_port}']\n    port: {service_port}\n    memory_bytes: 1048576\n    disposable: true\n    allowed_actions: [start, stop, restart]\n  external:\n    mode: discovered\n    port: {external_port}\n  slow:\n    mode: managed\n    command: [/bin/sleep, '30']\n    port: {slow_port}\n    memory_bytes: 16777216\n    startup_timeout_ms: {timeout_ms}\n    allowed_actions: [start, stop]\n  fail:\n    mode: managed\n    command: [/usr/bin/false]\n    memory_bytes: 16777216\n    allowed_actions: [start, stop]\n")).unwrap();
        let config_path = dir.path().join("runtime.yaml");
        let config = fs::read_to_string(&config_path).unwrap().replace(
            "    disposable: true",
            &format!(
                "    match_executable: '{}'\n    match_args: [fixture, '{}']\n    disposable: true",
                env!("CARGO_BIN_EXE_runtime-gatekeeper"),
                service_port
            ),
        );
        fs::write(&config_path, config).unwrap();
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
    fn try_call(&self, name: &str, args: Value) -> Option<Value> {
        let response: Value = ureq::post(&format!("http://127.0.0.1:{}/rpc", self.port))
            .timeout(Duration::from_secs(3))
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
            .ok()?
            .into_json()
            .ok()?;
        (response["result"]["isError"] == false)
            .then(|| response["result"]["structuredContent"].clone())
    }
    fn stop_test_owned_services(&self) {
        // Do not infer ownership from a reachable port or the display class.
        // All mutations go through the isolated daemon's lease and owned-child
        // checks while that daemon still holds its process handles.
        let Some(status) = self.try_call("runtime.status", json!({})) else {
            return;
        };
        if let Some(jobs) = status["jobs"].as_array() {
            for job in jobs {
                if ["READY", "INTERRUPTED"].contains(&job["status"].as_str().unwrap_or("")) {
                    let _ =
                        self.try_call("runtime.release", json!({"runtime_id":job["runtime_id"]}));
                }
            }
        }
        let _ = self.try_call("runtime.cleanup", json!({}));
        if let Some(status) = self.try_call("runtime.status", json!({}))
            && let Some(services) = status["services"].as_array()
        {
            for service in services {
                if service["actions"]
                    .as_array()
                    .is_some_and(|actions| actions.contains(&json!("stop")))
                {
                    let _ = self.try_call("services.stop", json!({"service_id":service["id"]}));
                }
            }
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop_test_owned_services();
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

#[cfg(target_os = "macos")]
#[test]
fn memory_status_identifies_the_admission_source() {
    let h = Harness::new();
    let status = h.call("runtime.status", json!({}));
    let memory = &status["memory"];
    let sysinfo = memory["sysinfo_available"].as_u64().unwrap();
    let free = memory["free_memory"].as_u64().unwrap();
    println!(
        "admission memory: sysinfo_available={sysinfo}, free_memory={free}, available={}, basis={}",
        memory["available"], memory["available_basis"]
    );
    if sysinfo == 0 && free > 0 {
        assert_eq!(memory["available"], free);
        assert_eq!(memory["available_basis"], "macos_free_pages_fallback");
    } else {
        assert_eq!(memory["available"], sysinfo);
        assert_eq!(memory["available_basis"], "sysinfo_available");
    }
}
#[test]
fn external_listener_is_never_adopted_or_stopped() {
    let h = Harness::new();
    let listener = TcpListener::bind(("127.0.0.1", h.service_port)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let v = loop {
        let v = h.call("services.get", json!({"service_id":"demo"}));
        if v["state"] == "unhealthy" {
            break v;
        }
        assert!(
            Instant::now() < deadline,
            "external listener missing from snapshot"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(v["identity_verified"], false);
    assert_eq!(v["leases"], json!([]));
    assert_eq!(v["actions"], json!([]));
    let blocked = h.request("external-port");
    assert_eq!(blocked["status"], "BLOCKED_SERVICE");
    assert_eq!(
        h.call("jobs.get", json!({"job_id":"external-port"}))["status"],
        "BLOCKED_SERVICE"
    );
    assert_eq!(
        h.call("services.get", json!({"service_id":"demo"}))["leases"],
        json!([])
    );
    assert_eq!(
        h.rpc("services.start", json!({"service_id":"demo"}))["result"]["isError"],
        true
    );
    assert_eq!(
        h.rpc("services.stop", json!({"service_id":"demo"}))["result"]["isError"],
        true
    );
    h.call("runtime.cleanup", json!({}));
    drop(h);
    assert!(listener.local_addr().is_ok());
}

#[test]
fn harness_drop_releases_owned_fixture_before_stopping_daemon() {
    let service_port;
    {
        let h = Harness::new();
        service_port = h.service_port;
        assert_eq!(h.request("drop-cleanup")["status"], "READY");
        // Deliberately leave the READY lease for the test harness to release.
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", service_port)) {
            drop(listener);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "test fixture survived Harness::drop"
        );
        thread::sleep(Duration::from_millis(25));
    }
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
    assert_eq!(lines[1]["result"]["tools"].as_array().unwrap().len(), 12);
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
    let lease = h.request("existing");
    assert_eq!(lease["status"], "READY");
    let service = h.call("services.get", json!({"service_id":"demo"}));
    let pid = service["processes"][0]["pid"].as_u64().unwrap() as i32;
    // End this test-owned fixture before replacing it with an unrelated
    // listener. The restarted daemon must not adopt the replacement.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    let listener = loop {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", h.service_port)) {
            break listener;
        }
        assert!(Instant::now() < deadline, "test fixture did not exit");
        thread::sleep(Duration::from_millis(50));
    };
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
        h.call("services.get", json!({"service_id":"demo"}))["state"],
        "unhealthy"
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
    assert!(listener.local_addr().is_ok());
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

struct McpClient {
    child: Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}
impl McpClient {
    fn new(h: &Harness, name: &str) -> Self {
        Self::with_mode(h, name, "mcp")
    }
    fn with_mode(h: &Harness, name: &str, mode: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_runtime-gatekeeper"))
            .arg(mode)
            .arg(h.dir.path().join("runtime.yaml"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut c = Self {
            child,
            input,
            output,
        };
        assert!(c.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":name,"version":"test"}}}))["result"].is_object());
        c
    }
    fn send(&mut self, v: Value) -> Value {
        use std::io::{BufRead, Write};
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    fn call(&mut self, name: &str, args: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}}))
    }
}
impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn snapshot_and_mcp_remain_responsive_during_slow_start() {
    let h = Harness::with_timeout(2500);
    let mut mcp = McpClient::new(&h, "slow-start-observer");
    thread::scope(|scope| {
        let control = scope.spawn(|| h.rpc("services.start", json!({"service_id":"slow"})));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = h.call("runtime.status", json!({}));
            if status["snapshot"]["control_operation"] == "services.start" {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        let start = Instant::now();
        let status: Value = ureq::get(&format!("http://127.0.0.1:{}/api/status", h.port))
            .set("Authorization", &format!("Bearer {}", h.token))
            .call()
            .unwrap()
            .into_json()
            .unwrap();
        assert_eq!(status["snapshot"]["control_operation"], "services.start");
        assert_eq!(
            mcp.call("runtime.status", json!({}))["result"]["isError"],
            false
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "read paths blocked behind startup"
        );
        assert_eq!(control.join().unwrap()["result"]["isError"], true);
    });
    assert!(h.call("runtime.status", json!({}))["snapshot"]["control_operation"].is_null());
}
#[test]
fn two_clients_share_leases_and_one_disconnect_does_not_stop_other() {
    let h = Harness::new();
    let mut a = McpClient::new(&h, "client-a");
    let mut b = McpClient::new(&h, "client-b");
    let list = a.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}));
    for t in list["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["outputSchema"]["type"], "object");
        assert!(t["outputSchema"]["required"].is_array());
    }
    let one = a.call(
        "runtime.request",
        json!({"job_id":"a","agent":"a","requires":["demo"]}),
    )["result"]["structuredContent"]
        .clone();
    let two = b.call(
        "runtime.request",
        json!({"job_id":"b","agent":"b","requires":["demo"]}),
    )["result"]["structuredContent"]
        .clone();
    assert_eq!(one["status"], "READY");
    assert_eq!(two["status"], "READY");
    drop(a);
    assert_eq!(b.call("services.get",json!({"service_id":"demo"}))["result"]["structuredContent"]["leases"].as_array().unwrap().len(),2);
    let s = b.call("runtime.status", json!({}))["result"]["structuredContent"].clone();
    assert_eq!(s["mcp"]["clients"].as_array().unwrap().len(), 2);
    assert_eq!(
        s["mcp"]["tool_exposure"],
        "unknown_requires_client_verification"
    );
    for c in s["mcp"]["clients"].as_array().unwrap() {
        assert!(c["initialized_at"].is_number());
        assert!(c["last_successful_call_at"].is_number());
    }
    for service in s["services"].as_array().unwrap() {
        for p in service["processes"].as_array().unwrap() {
            assert!(
                ["phys_footprint", "rss_fallback", "rss"]
                    .contains(&p["memory_basis"].as_str().unwrap())
            );
        }
    }
    for lease in [one, two] {
        assert_eq!(
            b.call("runtime.release", json!({"runtime_id":lease["runtime_id"]}))["result"]["structuredContent"]
                ["status"],
            "RELEASED"
        );
    }
    b.call("runtime.cleanup", json!({}));
    assert_eq!(
        b.call("services.get", json!({"service_id":3}))["error"]["code"],
        -32602
    );
}

#[test]
fn bridge_survives_transport_error_and_token_rotation() {
    let mut h = Harness::new();
    let mut client = McpClient::new(&h, "restart-observer");
    h.child.kill().unwrap();
    h.child.wait().unwrap();
    assert_eq!(
        client.call("runtime.status", json!({}))["error"]["code"],
        -32000
    );
    h.child = Command::new(env!("CARGO_BIN_EXE_runtime-gatekeeper"))
        .arg("serve")
        .arg(h.dir.path().join("runtime.yaml"))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let new_token = fs::read_to_string(h.dir.path().join("state/token")).unwrap_or_default();
        if !new_token.is_empty() && new_token != h.token {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        client.call("runtime.status", json!({}))["result"]["isError"],
        false
    );
}

#[test]
fn observation_read_does_not_grant_a_lease_or_mutate_runtime_state() {
    let h = Harness::new();
    let state = h.dir.path().join("state");
    let before = fs::read(state.join("state.json")).unwrap();
    let events = fs::read(state.join("events.jsonl")).unwrap();
    let v = h.call("runtime.observe", json!({"service_id":"demo"}));
    assert_eq!(v["observation_only"], true);
    assert!(v["snapshot"]["sampled_at"].is_u64());
    assert_eq!(
        v["services"][0]["native_queue"]["reason"],
        "no_configured_source"
    );
    assert_eq!(
        v["system"]["pressure"]["source"],
        "sysctl:kern.memorystatus_vm_pressure_level"
    );
    assert_eq!(
        h.rpc("runtime.observe", json!({"service_id":"nope"}))["result"]["isError"],
        true
    );
    assert!(
        h.rpc("runtime.observe", json!({"action":"cleanup"}))
            .get("error")
            .is_some()
    );
    assert_eq!(before, fs::read(state.join("state.json")).unwrap());
    assert_eq!(events, fs::read(state.join("events.jsonl")).unwrap());
    assert!(TcpListener::bind(("127.0.0.1", h.service_port)).is_ok());
}

#[test]
fn observer_filters_catalog_and_refuses_direct_control_including_cleanup() {
    let h = Harness::new();
    let mut observer = McpClient::with_mode(&h, "local-admin", "mcp-observe");
    let list = observer.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
    let names: Vec<_> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "runtime.status",
            "runtime.observe",
            "services.list",
            "services.get",
            "jobs.list",
            "jobs.get"
        ]
    );
    let registry = observer.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"jobs.list","arguments":{}}}));
    let data = &registry["result"]["structuredContent"];
    assert_eq!(data["jobs"], json!([]));
    assert_eq!(
        data["evidence_scope"]["native_queue"],
        "unavailable_in_this_view"
    );
    let state_file = h.dir.path().join("state/state.json");
    let events = h.dir.path().join("state/events.jsonl");
    let before = (fs::read(&state_file).unwrap(), fs::read(&events).unwrap());
    for (name, args) in [
        ("runtime.cleanup", json!({})),
        (
            "runtime.request",
            json!({"job_id":"x","agent":"observer","requires":["demo"]}),
        ),
        ("runtime.release", json!({"runtime_id":"x"})),
        ("services.start", json!({"service_id":"demo"})),
        ("services.stop", json!({"service_id":"demo"})),
        ("services.restart", json!({"service_id":"demo"})),
    ] {
        let out = observer.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}}));
        assert_eq!(out["error"]["code"], -32602, "{out}");
    }
    assert_eq!(
        before,
        (fs::read(state_file).unwrap(), fs::read(events).unwrap())
    );
}
