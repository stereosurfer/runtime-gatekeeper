//! Small operating-system adapters kept outside the runtime engine.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    process::Command,
};

/// Return the owner PIDs of TCP listeners. None means the ownership probe
/// failed, which callers must treat as unverified rather than as an open port.
pub fn listening_pids() -> Option<BTreeMap<u16, BTreeSet<u32>>> {
    let output = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut result: BTreeMap<u16, BTreeSet<u32>> = BTreeMap::new();
    let mut pid = None;
    for line in String::from_utf8(output.stdout).ok()?.lines() {
        match line.chars().next() {
            Some('p') => pid = line[1..].parse::<u32>().ok(),
            Some('n') => {
                let Some(owner) = pid else { continue };
                let Some((_, port)) = line[1..].rsplit_once(':') else {
                    continue;
                };
                if let Ok(port) = port.parse::<u16>() {
                    result.entry(port).or_default().insert(owner);
                }
            }
            _ => {}
        }
    }
    Some(result)
}

/// Match the listener to the registered process inventory and distinguish PID reuse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInstance {
    pub pid: u32,
    pub start_time: u64,
    pub birth_marker: u64,
}

#[cfg(target_os = "macos")]
pub fn process_instance(pid: u32) -> Option<ProcessInstance> {
    use std::mem::{MaybeUninit, size_of};
    let mut out = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid.try_into().ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            out.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let info = unsafe { out.assume_init() };
    if info.pbi_pid != pid || info.pbi_start_tvsec == 0 {
        return None;
    }
    Some(ProcessInstance {
        pid,
        start_time: info.pbi_start_tvsec,
        birth_marker: info.pbi_start_tvusec,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn process_instance(_: u32) -> Option<ProcessInstance> {
    None
}

/// Bound lookup time as well as network I/O. Require a unique visible TCP listener.
pub fn current_listener(port: u16) -> Option<ProcessInstance> {
    use std::{
        io::Read,
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut child = Command::new("lsof")
        .args(["-nP", "-a", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut text = String::new();
    child
        .stdout
        .take()?
        .take(4097)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() > 4096 {
        return None;
    }
    let mut owners = text.lines().filter_map(|line| line.strip_prefix('p'));
    let pid = owners.next()?.parse().ok()?;
    if owners.next().is_some() {
        return None;
    }
    process_instance(pid)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn proc_pid_rusage(
        pid: libc::c_int,
        flavor: libc::c_int,
        buffer: *mut libc::c_void,
    ) -> libc::c_int;
}

/// Return the process memory value used by admission and dashboard accounting.
///
/// macOS's physical footprint includes graphics allocations that RSS omits,
/// which is important for Metal-backed applications such as Draw Things and
/// ComfyUI. The v4 layout is retained for compatibility with older supported
/// macOS releases; failures intentionally fall back to sysinfo's RSS value.
pub fn measured_memory(pid: u32, resident_bytes: u64) -> (u64, &'static str) {
    #[cfg(target_os = "macos")]
    {
        macos_phys_footprint(pid)
            .filter(|&n| n > 0 || resident_bytes == 0)
            .map(|n| (n, "phys_footprint"))
            .unwrap_or((resident_bytes, "rss_fallback"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        (resident_bytes, "rss")
    }
}

pub fn process_memory_basis() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macOS phys_footprint (含 Metal/IOAccelerator；讀取失敗時退回 RSS)"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "RSS"
    }
}

/// Conservative admission input when sysinfo's macOS available-memory formula
/// saturates at zero. Its free-memory value excludes speculative pages and
/// therefore counts only currently unused physical pages; it is not a claim
/// about reclaimable inactive pages or future allocations.
pub fn admission_available_memory(sysinfo_available: u64, free_memory: u64) -> (u64, &'static str) {
    #[cfg(target_os = "macos")]
    if sysinfo_available == 0 && free_memory > 0 {
        return (free_memory, "macos_free_pages_fallback");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = free_memory;
    (sysinfo_available, "sysinfo_available")
}

#[cfg(target_os = "macos")]
fn macos_phys_footprint(pid: u32) -> Option<u64> {
    // rusage_info_v4 is 296 bytes on macOS. The phys_footprint field follows
    // the 16-byte UUID and seven u64 counters (offset 72).
    const RUSAGE_INFO_V4: libc::c_int = 4;
    const BUFFER_BYTES: usize = 296;
    const PHYS_FOOTPRINT_OFFSET: usize = 72;

    let mut usage = [0u8; BUFFER_BYTES];
    let rc = unsafe {
        proc_pid_rusage(
            pid as libc::c_int,
            RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    if rc != 0 {
        return None;
    }
    let bytes: [u8; 8] = usage[PHYS_FOOTPRINT_OFFSET..PHYS_FOOTPRINT_OFFSET + 8]
        .try_into()
        .ok()?;
    let footprint = u64::from_ne_bytes(bytes);
    Some(footprint)
}

#[cfg(test)]
mod tests {
    use super::process_memory_basis;

    #[test]
    fn reports_a_non_empty_memory_basis() {
        assert!(!process_memory_basis().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn admission_uses_only_free_pages_when_sysinfo_available_saturates() {
        use super::admission_available_memory;
        use crate::model::memory_gate;

        assert_eq!(
            admission_available_memory(0, 1_048_576),
            (1_048_576, "macos_free_pages_fallback")
        );
        assert_eq!(admission_available_memory(0, 0), (0, "sysinfo_available"));
        assert_eq!(
            admission_available_memory(2_097_152, 1_048_576),
            (2_097_152, "sysinfo_available")
        );
        let (zero, _) = admission_available_memory(0, 0);
        assert_eq!(memory_gate(1, zero, 0, 0).status, "BLOCKED_RESOURCE");
        let (free_pages, _) = admission_available_memory(0, 1_048_576);
        assert_eq!(
            memory_gate(1, free_pages, 1_048_576, 0).status,
            "BLOCKED_RESOURCE"
        );
        assert_eq!(
            memory_gate(1_048_577, free_pages, 0, 0).status,
            "BLOCKED_RESOURCE"
        );
    }
}
