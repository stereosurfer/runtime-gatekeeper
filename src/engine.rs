use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    net::{SocketAddr, TcpStream},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use sysinfo::{Pid, System};

struct Owned {
    child: Child,
    start_time: u64,
}
pub struct Engine {
    pub config: Config,
    pub state: State,
    system: System,
    owned: BTreeMap<String, Owned>,
    _lock: File,
    storage_failed: Cell<bool>,
}
pub fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(100),
    )
    .is_ok()
}
impl Engine {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        fs::create_dir_all(&config.state_dir)?;
        fs::set_permissions(&config.state_dir, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(config.state_dir.join("daemon.lock"))?;
        lock.try_lock_exclusive()
            .context("another daemon owns this state directory")?;
        let path = config.state_dir.join("state.json");
        let state: State = if path.exists() {
            serde_json::from_slice(&fs::read(path)?)
                .context("invalid state; refusing to discard leases")?
        } else {
            State::default()
        };
        let mut e = Self {
            config,
            state,
            system: System::new_all(),
            owned: BTreeMap::new(),
            _lock: lock,
            storage_failed: Cell::new(false),
        };
        // No persisted PID is adopted as controllable. Retain leases for explicit release.
        for job in e.state.jobs.values_mut() {
            if job.status == "READY" {
                job.status = "INTERRUPTED".into();
                job.result = json!({"status":"INTERRUPTED","reason":"daemon restarted; verify external services and release old lease"});
            }
        }
        e.persist("daemon_started", json!({}))?;
        Ok(e)
    }
    fn persist(&self, event: &str, detail: Value) -> Result<()> {
        let result = self.persist_inner(event, detail);
        if result.is_err() {
            self.storage_failed.set(true);
        }
        result
    }
    fn persist_inner(&self, event: &str, detail: Value) -> Result<()> {
        let tmp = self.config.state_dir.join("state.json.tmp");
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&self.state)?)?;
        f.sync_all()?;
        fs::rename(tmp, self.config.state_dir.join("state.json"))?;
        let mut events = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.config.state_dir.join("events.jsonl"))?;
        writeln!(
            events,
            "{}",
            json!({"at":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),"event":event,"detail":detail})
        )?;
        events.sync_all()?;
        Ok(())
    }
    fn refresh(&mut self) {
        self.system.refresh_all();
        self.owned.retain(|_, o| {
            o.child.try_wait().ok().flatten().is_none()
                && self
                    .system
                    .process(Pid::from_u32(o.child.id()))
                    .is_some_and(|p| p.start_time() == o.start_time)
        });
    }
    fn leases(&self, id: &str) -> Vec<Value> {
        self.state.jobs.values().filter(|j| ["READY","INTERRUPTED"].contains(&j.status.as_str()) && j.request.requires.iter().any(|s| s==id))
            .map(|j| json!({"runtime_id":j.runtime_id,"job_id":j.request.job_id,"agent":j.request.agent,"status":j.status})).collect()
    }
    fn processes(&self) -> Vec<Process> {
        let mut rows: Vec<Process> = self
            .system
            .processes()
            .iter()
            .map(|(pid, p)| Process {
                pid: pid.as_u32(),
                parent: p.parent().map(|p| p.as_u32()),
                start_time: p.start_time(),
                name: p.name().to_string_lossy().into(),
                executable: p.exe().map(|p| p.to_path_buf()),
                args: p.cmd().iter().map(|s| s.to_string_lossy().into()).collect(),
                ram_bytes: p.memory(),
                cpu_percent: p.cpu_usage(),
                service: None,
                class: "unknown".into(),
            })
            .collect();
        for row in &mut rows {
            let managed = self
                .owned
                .iter()
                .find(|(_, o)| {
                    // Dedicated process group includes foreground workers and helpers.
                    unsafe { libc::getpgid(row.pid as i32) == o.child.id() as i32 }
                })
                .map(|(id, _)| id.clone());
            if let Some(id) = managed {
                row.service = Some(id);
                row.class = "managed".into();
                continue;
            }
            let matches: Vec<_> = self
                .config
                .services
                .iter()
                .filter(|(_, s)| {
                    s.match_executable
                        .as_ref()
                        .is_some_and(|p| Some(p) == row.executable.as_ref())
                        && s.match_args.iter().all(|arg| row.args.contains(arg))
                })
                .collect();
            if matches.len() == 1 {
                row.service = Some(matches[0].0.clone());
                row.class = "discovered".into();
            }
        }
        // Inherit only unambiguous parent associations; never double count a process.
        for _ in 0..rows.len() {
            let map: BTreeMap<_, _> = rows
                .iter()
                .filter_map(|r| {
                    r.service
                        .as_ref()
                        .map(|s| (r.pid, (s.clone(), r.class.clone())))
                })
                .collect();
            let mut changed = false;
            for r in &mut rows {
                if r.service.is_none()
                    && let Some((s, c)) = r.parent.and_then(|p| map.get(&p))
                {
                    r.service = Some(s.clone());
                    r.class = c.clone();
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.ram_bytes));
        rows
    }
    fn views(&self, processes: &[Process]) -> Vec<Value> {
        self.config.services.iter().map(|(id,s)| {
            let ps:Vec<_>=processes.iter().filter(|p|p.service.as_ref()==Some(id)).collect();
            let healthy=s.port.map(port_open);
            let class=if self.owned.contains_key(id) {"managed"} else if !ps.is_empty() || healthy==Some(true) {"discovered"} else if s.mode==Mode::Managed {"managed"} else {"discovered"};
            let leases=self.leases(id);
            let running=!ps.is_empty() || healthy==Some(true);
            let mut actions=Vec::new();
            if s.mode==Mode::Managed && class=="managed" && leases.is_empty() {
                for a in &s.allowed_actions { if (a=="start" && !running) || (a!="start" && self.owned.contains_key(id)) { actions.push(a); } }
            }
            json!({"id":id,"class":class,"configured_mode":s.mode,"state":if running {if healthy==Some(false){"unhealthy"}else{"running"}} else {"stopped"},"healthy":healthy,"port":s.port,"ram_bytes":ps.iter().map(|p|p.ram_bytes).sum::<u64>(),"cpu_percent":ps.iter().map(|p|p.cpu_percent).sum::<f32>(),"memory_estimate_bytes":s.memory_bytes,"disposable":s.disposable,"leases":leases,"processes":ps,"actions":actions})
        }).collect()
    }
    pub fn status(&mut self) -> Value {
        self.refresh();
        let ps = self.processes();
        let process_total = ps.iter().map(|p| p.ram_bytes).sum::<u64>();
        let unknown_processes: Vec<_> = ps.iter().filter(|p| p.service.is_none()).collect();
        let unknown_process_ram = unknown_processes.iter().map(|p| p.ram_bytes).sum::<u64>();
        let system_unaccounted = self.system.used_memory().saturating_sub(process_total);
        json!({"name":"Runtime Gatekeeper","storage_failed":self.storage_failed.get(),"platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"memory":{"total":self.system.total_memory(),"used":self.system.used_memory(),"available":self.system.available_memory(),"swap_used":self.system.used_swap(),"swap_total":self.system.total_swap(),"safety_margin":self.config.safety_margin_bytes,"process_total":process_total,"unknown_process_ram":unknown_process_ram,"system_unaccounted":system_unaccounted},"cpu_percent":self.system.global_cpu_usage(),"services":self.views(&ps),"jobs":self.state.jobs.values().collect::<Vec<_>>(),"unknown":unknown_processes})
    }
    fn action_allowed(&self, id: &str, action: &str) -> Result<Service> {
        let s = self.config.services.get(id).context("unknown service")?;
        ensure!(
            s.mode == Mode::Managed && s.allowed_actions.contains(action),
            "CONTROL_DENIED: action not allowed for managed service"
        );
        ensure!(
            self.leases(id).is_empty(),
            "PROTECTED: service has active or interrupted leases"
        );
        Ok(s.clone())
    }
    fn start(&mut self, id: &str) -> Result<()> {
        let s = self.action_allowed(id, "start")?;
        self.refresh();
        if self.owned.contains_key(id) {
            return Ok(());
        }
        let ps = self.processes();
        ensure!(
            !ps.iter().any(|p| p.service.as_deref() == Some(id)),
            "EXTERNAL_SERVICE: refusing to adopt or duplicate discovered process"
        );
        ensure!(
            !s.port.is_some_and(port_open),
            "PORT_CONFLICT: listener already exists"
        );
        let exe = if s.command[0] == "@self" {
            std::env::current_exe()?
        } else {
            s.command[0].clone().into()
        };
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.config.state_dir.join(format!("{id}.log")))?;
        let mut command = Command::new(exe);
        command
            .args(&s.command[1..])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0);
        if let Some(cwd) = s.cwd {
            command.current_dir(cwd);
        }
        self.persist("service_start_intent", json!({"service":id}))?;
        let mut child = command.spawn().context("service spawn failed")?;
        self.system.refresh_all();
        let Some(start_time) = self
            .system
            .process(Pid::from_u32(child.id()))
            .map(|p| p.start_time())
        else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("service exited before ownership registration");
        };
        self.owned.insert(id.into(), Owned { child, start_time });
        let deadline = Instant::now() + Duration::from_millis(s.startup_timeout_ms);
        loop {
            std::thread::sleep(Duration::from_millis(100));
            self.refresh();
            if !self.owned.contains_key(id) {
                bail!("START_FAILED: service exited (see service log)");
            }
            if s.port.map(port_open).unwrap_or(true) {
                self.persist("service_started", json!({"service":id}))?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                // Ownership remains registered if rollback is incomplete.
                let cleanup = self.stop_owned(id);
                bail!("HEALTH_TIMEOUT: rollback result {cleanup:?}");
            }
        }
    }
    fn stop_owned(&mut self, id: &str) -> Result<()> {
        self.refresh();
        let o = self
            .owned
            .get(id)
            .context("NOT_OWNED: no live child handle; discovered processes are read-only")?;
        let pid = o.child.id();
        ensure!(
            unsafe { libc::getpgid(pid as i32) } == pid as i32,
            "ownership group mismatch"
        );
        let members: Vec<_> = self
            .system
            .processes()
            .iter()
            .filter(|(p, _)| unsafe { libc::getpgid(p.as_u32() as i32) } == pid as i32)
            .map(|(p, s)| (p.as_u32(), s.start_time()))
            .collect();
        self.persist(
            "service_stop_intent",
            json!({"service":id,"members":members}),
        )?;
        // Root is live and unreaped here. Never signal a persisted or discovered PID.
        ensure!(
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) } == 0,
            "SIGTERM failed"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            self.system.refresh_all();
            if let Some(o) = self.owned.get_mut(id) {
                let _ = o.child.try_wait();
            }
            let alive: Vec<_> = members
                .iter()
                .filter(|(p, t)| {
                    self.system
                        .process(Pid::from_u32(*p))
                        .is_some_and(|s| s.start_time() == *t)
                })
                .copied()
                .collect();
            if alive.is_empty() {
                self.owned.remove(id);
                self.persist("service_stopped", json!({"service":id}))?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                for (p, t) in &alive {
                    // Revalidate identity and process group immediately before each signal.
                    self.system.refresh_all();
                    if self
                        .system
                        .process(Pid::from_u32(*p))
                        .is_some_and(|s| s.start_time() == *t)
                        && unsafe { libc::getpgid(*p as i32) } == pid as i32
                    {
                        unsafe {
                            libc::kill(*p as i32, libc::SIGKILL);
                        };
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
                if let Some(o) = self.owned.get_mut(id) {
                    let _ = o.child.try_wait();
                }
                self.system.refresh_all();
                ensure!(
                    !alive.iter().any(|(p, t)| self
                        .system
                        .process(Pid::from_u32(*p))
                        .is_some_and(|s| s.start_time() == *t)),
                    "STOP_INCOMPLETE: inspect remaining processes"
                );
                self.owned.remove(id);
                self.persist("service_stopped", json!({"service":id,"escalated":true}))?;
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn gate(&self, required: u64, views: &[Value]) -> Value {
        // Outstanding estimates conservatively cover not-yet-materialized job memory.
        let outstanding = self
            .state
            .jobs
            .values()
            .filter(|j| ["READY", "INTERRUPTED"].contains(&j.status.as_str()))
            .fold(0u64, |n, j| n.saturating_add(j.request.memory_bytes));
        let service_pending =
            views
                .iter()
                .filter(|v| v["state"] != "stopped")
                .fold(0u64, |n, v| {
                    n.saturating_add(
                        v["memory_estimate_bytes"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_sub(v["ram_bytes"].as_u64().unwrap_or(0)),
                    )
                });
        let g = memory_gate(
            required,
            self.system.available_memory(),
            self.config.safety_margin_bytes,
            outstanding.saturating_add(service_pending),
        );
        let reclaimable: Vec<_> = views
            .iter()
            .filter(|v| {
                v["class"] == "managed"
                    && v["state"] != "stopped"
                    && v["leases"].as_array().is_some_and(|a| a.is_empty())
                    && v["actions"]
                        .as_array()
                        .is_some_and(|a| a.contains(&json!("stop")))
            })
            .collect();
        let protected: Vec<_> = views
            .iter()
            .filter(|v| v["state"] != "stopped" && !reclaimable.iter().any(|r| r["id"] == v["id"]))
            .collect();
        json!({"status":g.status,"resource":"memory","units":"bytes","required":g.required,"available":g.available,"shortfall":g.shortfall,"observed_available":self.system.available_memory(),"safety_margin":self.config.safety_margin_bytes,"outstanding_estimates":outstanding.saturating_add(service_pending),"reclaimable":reclaimable,"protected":protected})
    }
    pub fn request(&mut self, mut r: Request) -> Result<Value> {
        ensure!(
            !r.job_id.trim().is_empty()
                && r.job_id.len() <= 128
                && !r.agent.trim().is_empty()
                && r.agent.len() <= 128,
            "job_id and agent must be 1..128 characters"
        );
        ensure!(!r.requires.is_empty(), "requires cannot be empty");
        r.requires.sort();
        r.requires.dedup();
        for id in &r.requires {
            ensure!(
                self.config.services.contains_key(id),
                "unknown required service: {id}"
            );
        }
        if let Some(j) = self.state.jobs.get(&r.job_id) {
            ensure!(
                j.request == r,
                "JOB_CONFLICT: job_id already used with different request"
            );
            if ["READY", "INTERRUPTED"].contains(&j.status.as_str()) {
                let runtime_id = j.runtime_id.clone();
                let status = self.status();
                let ready = r.requires.iter().all(|id| {
                    status["services"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s["id"] == *id && s["state"] == "running")
                });
                return Ok(
                    json!({"status":if ready && self.state.jobs[&r.job_id].status=="READY" {"READY"} else {"BLOCKED_SERVICE"},"runtime_id":runtime_id,"job_id":r.job_id,"idempotent":true,"services":status["services"]}),
                );
            }
            ensure!(j.status != "RELEASED", "JOB_CLOSED: use a new job_id");
        }
        self.refresh();
        let views = self.views(&self.processes());
        let mut required = r.memory_bytes;
        let mut unavailable = Vec::new();
        for id in &r.requires {
            let s = &self.config.services[id];
            let v = views.iter().find(|v| v["id"] == *id).unwrap();
            if v["state"] != "running" {
                if v["state"] == "stopped"
                    && s.mode == Mode::Managed
                    && s.allowed_actions.contains("start")
                    && self.leases(id).is_empty()
                {
                    required = required
                        .checked_add(s.memory_bytes)
                        .context("memory requirement overflow")?;
                } else {
                    unavailable.push(id.clone());
                }
            }
        }
        let runtime_id = self
            .state
            .jobs
            .get(&r.job_id)
            .map(|j| j.runtime_id.clone())
            .unwrap_or_else(|| format!("rt-{}", uuid::Uuid::new_v4()));
        let mut result = self.gate(required, &views);
        if !unavailable.is_empty() {
            result = json!({"status":"BLOCKED_SERVICE","unavailable":unavailable});
        }
        if result["status"] == "READY" {
            let mut started: Vec<&str> = Vec::new();
            for id in &r.requires {
                let v = views.iter().find(|v| v["id"] == *id).unwrap();
                if v["state"] == "stopped" {
                    if let Err(err) = self.start(id) {
                        let mut rollback = Vec::new();
                        for old in started.iter().rev() {
                            rollback.push(json!({"service":old,"result":self.stop_owned(old).map(|_|"stopped".to_string()).unwrap_or_else(|e|e.to_string())}));
                        }
                        result = json!({"status":"BLOCKED_SERVICE","service":id,"reason":err.to_string(),"rollback":rollback});
                        break;
                    }
                    started.push(id.as_str());
                }
            }
        }
        result["runtime_id"] = json!(runtime_id);
        result["job_id"] = json!(r.job_id);
        self.state.jobs.insert(
            r.job_id.clone(),
            Job {
                runtime_id,
                request: r,
                status: result["status"].as_str().unwrap().into(),
                result: result.clone(),
            },
        );
        self.persist("runtime_request", result.clone())?;
        Ok(result)
    }
    pub fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        ensure!(
            !self.storage_failed.get()
                || matches!(
                    name,
                    "runtime.status" | "services.list" | "services.get" | "jobs.list" | "jobs.get"
                ),
            "STORAGE_FAILED: mutations disabled; repair storage and restart daemon"
        );
        let id = |key: &str| {
            args[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .context(format!("missing {key}"))
        };
        match name {
            "runtime.status" => Ok(self.status()),
            "services.list" => Ok(json!({"services":self.status()["services"]})),
            "services.get" => {
                let id = id("service_id")?;
                self.status()["services"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["id"] == id)
                    .cloned()
                    .context("unknown service")
            }
            "jobs.list" => Ok(json!({"jobs":self.state.jobs.values().collect::<Vec<_>>()})),
            "jobs.get" => Ok(serde_json::to_value(
                self.state.jobs.get(&id("job_id")?).context("unknown job")?,
            )?),
            "runtime.request" => self.request(serde_json::from_value(args)?),
            "runtime.release" => {
                let runtime_id = id("runtime_id")?;
                let j = self
                    .state
                    .jobs
                    .values_mut()
                    .find(|j| j.runtime_id == runtime_id)
                    .context("unknown runtime_id")?;
                j.status = "RELEASED".into();
                j.result = json!({"status":"RELEASED","runtime_id":runtime_id});
                self.persist("runtime_released", json!({"runtime_id":runtime_id}))?;
                Ok(
                    json!({"status":"RELEASED","runtime_id":runtime_id,"note":"services retained; runtime.cleanup stops idle disposable services"}),
                )
            }
            "runtime.cleanup" => {
                self.refresh();
                let ids: Vec<_> = self
                    .owned
                    .keys()
                    .filter(|id| {
                        self.config.services[*id].disposable
                            && self.config.services[*id].allowed_actions.contains("stop")
                            && self.leases(id).is_empty()
                    })
                    .cloned()
                    .collect();
                let results:Vec<_>=ids.iter().map(|id|json!({"service":id,"result":self.stop_owned(id).map(|_|"stopped".into()).unwrap_or_else(|e|e.to_string())})).collect();
                Ok(json!({"results":results}))
            }
            "services.start" | "services.stop" | "services.restart" => {
                let id = id("service_id")?;
                let action = name.split('.').nth(1).unwrap();
                self.action_allowed(&id, action)?;
                if action == "restart" {
                    self.action_allowed(&id, "start")?;
                    self.action_allowed(&id, "stop")?;
                }
                if action != "stop" {
                    self.refresh();
                    let views = self.views(&self.processes());
                    let required = if action == "restart" {
                        self.config.services[&id].memory_bytes.saturating_sub(
                            views
                                .iter()
                                .find(|v| v["id"] == id)
                                .and_then(|v| v["ram_bytes"].as_u64())
                                .unwrap_or(0),
                        )
                    } else {
                        self.config.services[&id].memory_bytes
                    };
                    let gate = self.gate(required, &views);
                    if gate["status"] != "READY" {
                        return Ok(gate);
                    }
                }
                if action != "start" {
                    self.stop_owned(&id)?;
                }
                if action != "stop" {
                    self.start(&id)?;
                }
                Ok(json!({"status":"OK","service_id":id,"action":action}))
            }
            _ => bail!("unknown tool: {name}"),
        }
    }
}
