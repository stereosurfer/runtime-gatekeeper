//! Host metrics for reporting. Admission continues to use its existing sysinfo view.
#[cfg(target_os = "macos")]
mod macos {
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::ffi::CStr;
    use std::mem::{MaybeUninit, size_of};
    unsafe extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
    pub struct HostSampler {
        host: libc::mach_port_t,
        previous: Option<[u32; 4]>,
        processes: HashMap<(u32, u64, u64), ProcessClock>,
    }
    #[derive(Clone, Copy)]
    struct ProcessClock {
        cpu_ticks: u64,
        wall_ticks: u64,
    }
    impl ProcessClock {
        fn usage_since(self, previous: Self) -> Option<f64> {
            let elapsed = self.wall_ticks.checked_sub(previous.wall_ticks)?;
            let active = self.cpu_ticks.checked_sub(previous.cpu_ticks)?;
            (elapsed > 0).then(|| active as f64 * 100.0 / elapsed as f64)
        }
    }
    #[allow(deprecated)]
    fn process_clock(p: &Value) -> Option<((u32, u64, u64), ProcessClock)> {
        let pid = i32::try_from(p["pid"].as_u64()?).ok()?;
        let mut out = MaybeUninit::<libc::proc_taskallinfo>::uninit();
        let size = size_of::<libc::proc_taskallinfo>() as i32;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKALLINFO,
                0,
                out.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let info = unsafe { out.assume_init() };
        let identity = (
            info.pbsd.pbi_pid,
            info.pbsd.pbi_start_tvsec,
            info.pbsd.pbi_start_tvusec,
        );
        if identity.0 != pid as u32
            || identity.1 == 0
            || Some(identity.1) != p["start_time"].as_u64()
        {
            return None;
        }
        let clock = ProcessClock {
            cpu_ticks: info
                .ptinfo
                .pti_total_user
                .checked_add(info.ptinfo.pti_total_system)?,
            wall_ticks: unsafe { libc::mach_absolute_time() },
        };
        Some((identity, clock))
    }
    // Call only with a fixed kernel name and its ABI output type.
    unsafe fn sysctl<T>(name: &CStr) -> Option<T> {
        let mut out = MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>();
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                out.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && size == size_of::<T>()).then(|| unsafe { out.assume_init() })
    }
    impl HostSampler {
        // libc deprecates its Mach bindings in favor of another crate; the system ABI is unchanged.
        #[allow(deprecated)]
        pub fn new() -> Self {
            Self {
                host: unsafe { libc::mach_host_self() },
                previous: None,
                processes: HashMap::new(),
            }
        }
        fn ticks(&self) -> Option<[u32; 4]> {
            let mut data = MaybeUninit::<libc::host_cpu_load_info>::uninit();
            let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
            let rc = unsafe {
                libc::host_statistics(
                    self.host,
                    libc::HOST_CPU_LOAD_INFO,
                    data.as_mut_ptr().cast(),
                    &mut count,
                )
            };
            (rc == 0 && count == libc::HOST_CPU_LOAD_INFO_COUNT)
                .then(|| unsafe { data.assume_init().cpu_ticks })
        }
        fn ram(&self) -> Option<(u64, u64)> {
            let mut data = MaybeUninit::<libc::vm_statistics64>::uninit();
            let mut count = libc::HOST_VM_INFO64_COUNT;
            let rc = unsafe {
                libc::host_statistics64(
                    self.host,
                    libc::HOST_VM_INFO64,
                    data.as_mut_ptr().cast(),
                    &mut count,
                )
            };
            let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
            if rc != 0 || count != libc::HOST_VM_INFO64_COUNT || page_size == 0 {
                return None;
            }
            let vm = unsafe { data.assume_init() };
            // Preserve the accounting formulas used by the pinned sysinfo macOS backend.
            let used = u64::from(vm.active_count)
                + u64::from(vm.wire_count)
                + u64::from(vm.compressor_page_count)
                + u64::from(vm.speculative_count);
            let reclaim = u64::from(vm.free_count)
                + u64::from(vm.inactive_count)
                + u64::from(vm.purgeable_count);
            let available = reclaim.saturating_sub(u64::from(vm.compressor_page_count));
            Some((
                used.checked_mul(page_size)?,
                available.checked_mul(page_size)?,
            ))
        }
        pub fn apply(&mut self, s: &mut Value) {
            // Replace the previous inventory each time, so a failed probe or a new process
            // cannot borrow a baseline. Process CPU is one-core percent and may exceed 100.
            let previous = std::mem::take(&mut self.processes);
            let mut measure_process = |p: &mut Value| {
                let usage = process_clock(p).and_then(|(id, clock)| {
                    self.processes.insert(id, clock);
                    clock.usage_since(*previous.get(&id)?)
                });
                p["cpu_percent"] = json!(usage);
                p["cpu_source"] = json!("proc_pidinfo:task_cpu_delta");
            };
            for svc in s["services"].as_array_mut().expect("service inventory") {
                for p in svc["processes"]
                    .as_array_mut()
                    .expect("service process list")
                {
                    measure_process(p);
                }
            }
            for p in s["unknown"].as_array_mut().expect("unattributed inventory") {
                measure_process(p);
            }
            let ticks = self.ticks();
            let cpu = self.previous.zip(ticks).and_then(|(prev, current)| {
                let delta =
                    std::array::from_fn::<_, 4, _>(|i| u64::from(current[i].wrapping_sub(prev[i])));
                let total = delta.iter().sum::<u64>();
                (total > 0).then(|| {
                    (total - delta[libc::CPU_STATE_IDLE as usize]) as f64 * 100.0 / total as f64
                })
            });
            self.previous = ticks;
            let ram = self.ram();
            let swap: Option<libc::xsw_usage> = unsafe { sysctl(c"vm.swapusage") };
            let total: Option<u64> = unsafe { sysctl(c"hw.memsize") };
            s["cpu_percent"] = json!(cpu);
            let m = &mut s["memory"];
            m["total"] = json!(total);
            m["used"] = json!(ram.map(|v| v.0));
            m["host_available"] = json!(ram.map(|v| v.1));
            m["swap_used"] = json!(swap.as_ref().map(|v| v.xsu_used));
            m["swap_total"] = json!(swap.as_ref().map(|v| v.xsu_total));
        }
    }
    impl Drop for HostSampler {
        #[allow(deprecated)]
        fn drop(&mut self) {
            unsafe {
                mach_port_deallocate(libc::mach_task_self(), self.host);
            }
        }
    }
    #[cfg(test)]
    #[test]
    fn cpu_delta_requires_two_comparable_samples() {
        let prev = ProcessClock {
            cpu_ticks: 100,
            wall_ticks: 10,
        };
        let cpu = |cpu_ticks, wall_ticks| {
            ProcessClock {
                cpu_ticks,
                wall_ticks,
            }
            .usage_since(prev)
        };
        assert_eq!(cpu(100, 20), Some(0.0));
        assert_eq!(cpu(125, 20), Some(250.0));
        assert_eq!(cpu(100, 10), None);
        assert_eq!(cpu(99, 20), None);
    }
}
#[cfg(target_os = "macos")]
pub use macos::HostSampler;

#[cfg(not(target_os = "macos"))]
pub struct HostSampler;
#[cfg(not(target_os = "macos"))]
impl HostSampler {
    pub fn new() -> Self {
        Self
    }
    pub fn apply(&mut self, _: &mut serde_json::Value) {}
}
