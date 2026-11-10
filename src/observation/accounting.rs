//! One process inventory is classified and summed once per published sample.
use super::metric;
use serde_json::{Value, json};
#[derive(Default)]
struct Totals {
    count: usize,
    missing: usize,
    bytes: u64,
}
impl Totals {
    fn record(&mut self, p: &mut Value) {
        self.count += 1;
        let bytes = super::process_bytes(p);
        if let Some(n) = bytes {
            self.bytes += n;
        } else {
            self.missing += 1;
        }
        p["ram_bytes"] = json!(bytes);
    }
    fn known(&self) -> Option<u64> {
        (self.count == 0 || self.count > self.missing).then_some(self.bytes)
    }
    fn json(&self) -> Value {
        json!({"count": self.count, "unmeasured_count": self.missing, "known_bytes": self.known()})
    }
    fn add(&mut self, other: &Self) {
        self.count += other.count;
        self.missing += other.missing;
        self.bytes += other.bytes;
    }
}
// Engine::status supplies both inventory lists, including genuinely empty lists.
pub fn prepare(s: &mut Value) {
    let mut assigned = Totals::default();
    let mut unknown = Totals::default();
    for service in s["services"].as_array_mut().expect("service inventory") {
        let mut totals = Totals::default();
        let ps = service["processes"]
            .as_array_mut()
            .expect("service processes");
        let mut cpu = Some(0.0);
        for p in ps {
            totals.record(p);
            cpu = cpu.zip(p["cpu_percent"].as_f64()).map(|(a, b)| a + b);
        }
        let identified = totals.count > 0 || service["state"] == "stopped";
        service["ram_bytes"] = json!((identified && totals.missing == 0).then_some(totals.bytes));
        service["cpu_percent"] = json!(if identified { cpu } else { None });
        service["process_accounting"] = totals.json();
        assigned.add(&totals);
    }
    for p in s["unknown"].as_array_mut().expect("unattributed inventory") {
        unknown.record(p);
    }
    let mut all = Totals::default();
    all.add(&assigned);
    all.add(&unknown);
    s["process_accounting"] =
        json!({"attributed": assigned.json(), "unattributed": unknown.json(), "all": all.json()});
    let m = &mut s["memory"];
    m["process_total"] = json!(all.known());
    m["unknown_process_ram"] = json!(unknown.known());
    m["system_unaccounted"] = json!(
        m["used"]
            .as_u64()
            .filter(|_| all.missing == 0)
            .map(|used| used.saturating_sub(all.bytes))
    );
}
pub fn resources(s: &Value) -> Value {
    let snap = &s["snapshot"];
    let t = &snap["sampled_at"];
    let stale = super::is_stale(snap);
    let bytes = |key: &str, mac_source: &str| {
        let source = if cfg!(target_os = "macos") {
            mac_source
        } else {
            "sysinfo"
        };
        metric(
            s["memory"][key].clone(),
            source,
            "bytes",
            t,
            stale,
            "os_reported",
        )
    };
    let cpu_source = if cfg!(target_os = "macos") {
        "mach:HOST_CPU_LOAD_INFO.delta"
    } else {
        "sysinfo"
    };
    let system = json!({
        "cpu_percent":metric(s["cpu_percent"].clone(),cpu_source,"percent",t,stale,"os_reported"),
        "total":bytes("total","sysctl:hw.memsize"), "used":bytes("used","mach:HOST_VM_INFO64"),
        "available":bytes("host_available","mach:HOST_VM_INFO64"), "swap_used":bytes("swap_used","sysctl:vm.swapusage"),
        "swap_total":bytes("swap_total","sysctl:vm.swapusage"),
        "pressure":metric(s["memory"]["pressure"].clone(),"sysctl:kern.memorystatus_vm_pressure_level","level",t,stale,"os_reported")
    });
    let mut groups = s["process_accounting"].clone();
    if let Some(rows) = groups.as_object_mut() {
        for group in rows.values_mut() {
            group["known_bytes"] = metric(
                group["known_bytes"].clone(),
                "sum_of_measured_processes",
                "bytes",
                t,
                stale,
                "aggregate",
            );
        }
    }
    json!({"system":system,"process_accounting":groups})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_zero_unreadable_and_partial_inventory() {
        let mut s = json!({"services":[
            {"state":"running","processes":[{"memory_basis":"phys_footprint","ram_bytes":0,"cpu_percent":0}]},
            {"state":"running","processes":[]},{"state":"stopped","processes":[]}],
            "unknown":[{"memory_basis":"rss_fallback","ram_bytes":0},{"memory_basis":"rss","ram_bytes":10}],
            "memory":{"used":50},"snapshot":{"sampled_at":123,"stale":false}});
        prepare(&mut s);
        let r = resources(&s);
        assert_eq!(s["services"][0]["ram_bytes"], 0);
        assert!(s["services"][1]["ram_bytes"].is_null());
        assert_eq!(s["services"][2]["ram_bytes"], 0);
        assert!(s["unknown"][0]["ram_bytes"].is_null());
        assert_eq!(
            r["process_accounting"]["unattributed"]["known_bytes"]["value"],
            10
        );
        assert_eq!(
            r["process_accounting"]["unattributed"]["unmeasured_count"],
            1
        );
        assert!(s["memory"]["system_unaccounted"].is_null());
    }
}
