//! Read-only projection. The gate never consumes this report.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
mod accounting;
mod native_queue;
pub use accounting::{prepare, resources};
pub(crate) use native_queue::QueueReader;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn pressure_label(raw: u32) -> Option<&'static str> {
    match raw {
        1 => Some("normal"),
        2 => Some("warning"),
        4 => Some("critical"),
        _ => None,
    }
}
pub fn system_pressure() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        let mut raw = 0u32;
        let mut len = std::mem::size_of_val(&raw);
        // Fixed-size output; null newp requests a read only.
        let rc = unsafe {
            libc::sysctlbyname(
                c"kern.memorystatus_vm_pressure_level".as_ptr(),
                (&mut raw as *mut u32).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && len == std::mem::size_of_val(&raw) {
            return pressure_label(raw);
        }
    }
    None
}
fn metric(
    value: Value,
    source: &str,
    unit: &str,
    timestamp: &Value,
    stale: bool,
    confidence: &str,
) -> Value {
    let availability = if value.is_null() {
        "unavailable"
    } else if stale {
        "stale"
    } else {
        "available"
    };
    json!({"value":value,"unit":unit,"source":source,"timestamp":timestamp,"availability":availability,
        "confidence":if availability=="available"{confidence}else{"unknown"}})
}
fn is_stale(snap: &Value) -> bool {
    snap["stale"] != false || !snap["control_operation"].is_null() || !snap["sampled_at"].is_u64()
}
fn process_bytes(p: &Value) -> Option<u64> {
    match (p["memory_basis"].as_str(), p["ram_bytes"].as_u64()) {
        (Some("phys_footprint"), n) => n,
        (Some("rss" | "rss_fallback"), Some(n @ 1..)) => Some(n),
        _ => None,
    }
}
fn process_row(p: &Value, t: &Value, stale: bool) -> Value {
    let basis = p["memory_basis"].as_str().unwrap_or("unknown");
    let cpu_source = p["cpu_source"]
        .as_str()
        .unwrap_or("sysinfo:process.cpu_usage");
    let cpu_basis = if p["cpu_source"].is_string() {
        "os_reported"
    } else {
        "library_reported"
    };
    json!({"pid":p["pid"],"start_time":p["start_time"],"parent":p["parent"],"name":p["name"],
        "class":p["class"],"service":p["service"],
        "cpu":metric(p["cpu_percent"].clone(),cpu_source,"percent",t,stale,cpu_basis),
        "memory":metric(json!(process_bytes(p)),basis,"bytes",t,stale,if basis=="phys_footprint"{"os_reported"}else{"rss_proxy"})})
}
pub fn report(
    s: &Value,
    filter: Option<&str>,
    mut reader: Option<&mut QueueReader>,
) -> Result<Value> {
    let snap = &s["snapshot"];
    let t = &snap["sampled_at"];
    let stale = is_stale(snap);
    let m = &s["memory"];
    let jobs = s["jobs"].as_array().context("missing job ledger")?;
    let mut known = 0u64;
    let mut unknown = Vec::new();
    for j in jobs
        .iter()
        .filter(|j| matches!(j["status"].as_str(), Some("READY" | "INTERRUPTED")))
    {
        if let Some(n) = j["request"]["memory_bytes"].as_u64().filter(|n| *n > 0) {
            known = known.checked_add(n).context("reservation overflow")?;
        } else {
            unknown.push(j["runtime_id"].clone());
        }
    }
    // Report only the known pending portion, not a second headroom/READY calculation.
    let mut pending = 0u64;
    let mut unknown_services = Vec::new();
    for v in s["services"]
        .as_array()
        .context("missing registry")?
        .iter()
        .filter(|v| v["state"] != "stopped")
    {
        match (v["memory_estimate_bytes"].as_u64(), v["ram_bytes"].as_u64()) {
            (Some(n), Some(seen)) => {
                pending = pending
                    .checked_add(n.saturating_sub(seen))
                    .context("pending service overflow")?;
            }
            _ => unknown_services.push(v["id"].clone()),
        }
    }
    // The engine already sorts these rows by reported footprint.
    let unknown_rows = s["unknown"].as_array().map(|rows| {
        json!({"total_count":rows.len(),"top":rows.iter().take(5)
        .map(|p|process_row(p,t,stale)).collect::<Vec<_>>()})
    });
    let mut views = Vec::new();
    for v in s["services"]
        .as_array()
        .context("missing registry")?
        .iter()
        .filter(|v| filter.is_none_or(|id| v["id"] == id))
    {
        let ps = v["processes"].as_array().context("missing process list")?;
        let rows: Vec<_> = ps.iter().map(|p| process_row(p, t, stale)).collect();
        let complete = (!rows.is_empty() || v["state"] == "stopped")
            && rows.iter().all(|r| r["memory"]["value"].is_u64());
        views.push(json!({"id":v["id"],"class":v["class"],"state":v["state"],"identity_verified":v["identity_verified"],
          "service_instance":v["service_instance"],"exclusive_group":v["exclusive_group"],"leases":v["leases"],"processes":rows,
          "cpu":metric(v["cpu_percent"].clone(),"sum_of_process_cpu","percent",t,stale,"aggregate"),
          "process_accounting":v["process_accounting"],"attributed_memory":metric(if complete{v["ram_bytes"].clone()}else{Value::Null},"sum_of_attributed_processes","bytes",t,stale,"aggregate"),
          "native_queue":native_queue::observe(v,filter.is_some()&&!stale,reader.as_deref_mut()),
          "graphics_memory":metric(Value::Null,"GpuTaskMan:not_connected","bytes",&Value::Null,stale,"unknown")}));
    }
    ensure!(filter.is_none() || !views.is_empty(), "unknown service ID");
    Ok(json!({"version":1,"observation_only":true,"snapshot":snap,
        "system":s["resources"]["system"],"process_accounting":s["resources"]["process_accounting"],
        "reservations":{"known_job_bytes":metric(json!(known),"ready_interrupted_job_estimates","bytes",t,stale,"declared"),
        "known_pending_service_bytes":metric(json!(pending),"registered_estimate_minus_attributed_bytes","bytes",t,stale,"declared"),"unknown_service_estimate_ids":unknown_services,"unknown_estimate_runtime_ids":unknown,"safety_margin_bytes":m["safety_margin"],"storage_failed":s["storage_failed"]},
        "services":views,"unattributed_processes":unknown_rows}))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        let mut s = json!({"snapshot":{"sampled_at":100,"age_ms":1,"stale":false,"control_operation":null},"memory":{"available":999,"host_available":0,"pressure":null,"swap_used":0,"swap_total":0},"storage_failed":false,
      "jobs":[{"status":"INTERRUPTED","runtime_id":"rt1","request":{"memory_bytes":null}},{"status":"READY","runtime_id":"rt2","request":{"memory_bytes":8}},{"status":"RELEASED","request":{"memory_bytes":99}}],
      "services":[{"id":"comfyui","port":8003,"ram_bytes":77,"state":"unhealthy","identity_verified":false,"processes":[{"pid":5,"start_time":11,"ram_bytes":77,"memory_basis":"phys_footprint"}],"leases":[]}],"unknown":[]});
        prepare(&mut s);
        s["resources"] = resources(&s);
        s
    }
    #[test]
    fn unknown_is_not_zero_and_observations_are_not_permits() {
        let r = report(&fixture(), Some("comfyui"), None).unwrap();
        assert_eq!(r["system"]["available"]["value"], 0);
        assert_eq!(r["system"]["pressure"]["availability"], "unavailable");
        assert!(r["services"][0]["native_queue"]["value"].is_null());
        assert_eq!(
            r["reservations"]["unknown_estimate_runtime_ids"],
            json!(["rt1"])
        );
        assert_eq!(r["reservations"]["known_job_bytes"]["value"], 8);
        assert_eq!(r["observation_only"], true);
        assert!(r.get("status").is_none());
    }
    #[test]
    fn stale_and_busy_do_not_gain_new_sample_timestamps() {
        for (stale, busy) in [(true, Value::Null), (false, json!("runtime.request"))] {
            let mut s = fixture();
            s["snapshot"]["stale"] = stale.into();
            s["snapshot"]["control_operation"] = busy;
            s["resources"] = resources(&s);
            let r = report(&s, None, None).unwrap();
            assert_eq!(r["system"]["available"]["availability"], "stale");
            assert_eq!(r["system"]["available"]["timestamp"], 100);
            assert!(report(&s, Some("missing"), None).is_err());
        }
    }
    #[test]
    fn unreadable_process_is_not_zero_service_usage() {
        let mut s = fixture();
        s["services"][0]["processes"][0]["memory_basis"] = json!("rss_fallback");
        s["services"][0]["processes"][0]["ram_bytes"] = json!(0);
        let r = report(&s, None, None).unwrap();
        assert!(r["services"][0]["attributed_memory"]["value"].is_null());
    }
    #[test]
    fn pressure_unknown_codes_are_not_normal() {
        assert_eq!(
            [1, 2, 4, 0, 8].map(pressure_label),
            [
                Some("normal"),
                Some("warning"),
                Some("critical"),
                None,
                None
            ]
        );
    }
}
