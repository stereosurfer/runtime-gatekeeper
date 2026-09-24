//! Small operating-system adapters kept outside the runtime engine.
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
pub fn measured_memory(pid: u32, resident_bytes: u64) -> u64 {
    #[cfg(target_os = "macos")]
    {
        macos_phys_footprint(pid).unwrap_or(resident_bytes)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        resident_bytes
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
    (footprint > 0).then_some(footprint)
}

#[cfg(test)]
mod tests {
    use super::process_memory_basis;

    #[test]
    fn reports_a_non_empty_memory_basis() {
        assert!(!process_memory_basis().is_empty());
    }
}
