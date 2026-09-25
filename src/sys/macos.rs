//! macOS readings through libproc and the Mach host interface. Every call here
//! is a syscall, not a subprocess: the bash version shelled out to
//! /usr/bin/footprint, which cost about 120 ms per process.

// libc marks the Mach interfaces deprecated in favour of the mach2 crate; the
// few calls used here are stable, and one fewer dependency is worth it.
#![allow(deprecated)]

use super::{CpuTicks, ProcInfo, ProcSample};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::sync::OnceLock;

const RUSAGE_INFO_V4: libc::c_int = 4;

fn host() -> libc::mach_port_t {
    // mach_host_self() adds a port reference on every call, so it is taken once.
    static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
    *HOST.get_or_init(|| unsafe { libc::mach_host_self() })
}

/// rusage times are in Mach absolute time units, not nanoseconds. On Apple
/// silicon one unit is 125/3 ns, so skipping this conversion under-reports CPU
/// time by a factor of about 40.
fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        unsafe { libc::mach_timebase_info(&mut info) };
        if info.denom == 0 { (1, 1) } else { (info.numer as u64, info.denom as u64) }
    })
}

fn abs_to_ns(v: u64) -> u64 {
    let (n, d) = timebase();
    ((v as u128) * (n as u128) / (d as u128)) as u64
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut v: u64 = 0;
    let mut len = size_of::<u64>();
    let r = unsafe { libc::sysctlbyname(cname.as_ptr(), &mut v as *mut u64 as *mut c_void, &mut len, std::ptr::null_mut(), 0) };
    (r == 0).then_some(v)
}

pub fn mem_total_kb() -> u64 {
    sysctl_u64("hw.memsize").unwrap_or(0) / 1024
}

fn page_size() -> u64 {
    sysctl_u64("hw.pagesize").unwrap_or(16384)
}

/// Memory in use: the higher of two readings, because each one fails in a
/// different way.
///
/// Active + wired + compressor is what Activity Monitor calls memory used. It
/// is honest on a calm machine, but under pressure it falls exactly when the
/// machine is in trouble: the kernel moves pages to the inactive list and writes
/// compressed pages to swap, and both leave the sum. A queue that trusted it
/// kept starting compiles at 97% memory, because the sum read 80%.
///
/// The kernel's own figure, `kern.memorystatus_level`, is the share of memory
/// it can still hand out. It counts file cache as free, so it reads low on a
/// calm machine, but it rises as pressure builds.
pub fn mem_used_kb() -> u64 {
    let total = mem_total_kb();
    let kernel = sysctl_u64("kern.memorystatus_level").map(|free_pct| total * 100u64.saturating_sub(free_pct) / 100);
    pages_used_kb().max(kernel.unwrap_or(0))
}

fn pages_used_kb() -> u64 {
    let mut stats: libc::vm_statistics64 = unsafe { zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let r = unsafe { libc::host_statistics64(host(), libc::HOST_VM_INFO64, &mut stats as *mut _ as libc::host_info64_t, &mut count) };
    if r != 0 {
        return 0;
    }
    let pages = stats.active_count as u64 + stats.wire_count as u64 + stats.compressor_page_count as u64;
    pages * page_size() / 1024
}

pub fn cpu_ticks() -> CpuTicks {
    let mut info: libc::host_cpu_load_info = unsafe { zeroed() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    let r = unsafe { libc::host_statistics(host(), libc::HOST_CPU_LOAD_INFO, &mut info as *mut _ as libc::host_info_t, &mut count) };
    if r != 0 {
        return CpuTicks::default();
    }
    // user, system, idle, nice
    let t = info.cpu_ticks;
    let busy = t[0] as u64 + t[1] as u64 + t[3] as u64;
    CpuTicks { busy, total: busy + t[2] as u64 }
}

/// macOS has no pressure-stall numbers. The kernel's own pressure level is
/// read instead: 1 normal, 2 warning, 4 critical, mapped to 0, 50 and 100.
pub fn mem_pressure_pct() -> Option<f64> {
    let level = sysctl_u64("kern.memorystatus_vm_pressure_level")? as u32;
    Some(match level {
        4 => 100.0,
        2 => 50.0,
        _ => 0.0,
    })
}

pub fn list_procs() -> Vec<ProcInfo> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    // Room for processes started between the two calls.
    let mut pids: Vec<libc::pid_t> = vec![0; n as usize + 64];
    let bytes = (pids.len() * size_of::<libc::pid_t>()) as libc::c_int;
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut c_void, bytes) };
    if got <= 0 {
        return Vec::new();
    }
    pids.truncate(got as usize);
    pids.into_iter()
        .filter(|&p| p > 0)
        .filter_map(|pid| {
            let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
            let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let r = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut c_void, size) };
            (r == size).then_some(ProcInfo { pid, ppid: info.pbi_ppid as i32 })
        })
        .collect()
}

pub fn proc_sample(pid: i32) -> Option<ProcSample> {
    let mut ri: libc::rusage_info_v4 = unsafe { zeroed() };
    let r = unsafe { libc::proc_pid_rusage(pid, RUSAGE_INFO_V4, &mut ri as *mut _ as *mut libc::rusage_info_t) };
    if r != 0 {
        return None;
    }
    let cpu_ns = abs_to_ns(ri.ri_user_time + ri.ri_system_time);
    Some(ProcSample {
        footprint_kb: ri.ri_phys_footprint / 1024,
        cpu_ns,
        // XNU counts a thread as runnable while it runs AND while it waits for
        // a core, so the wait alone is runnable minus CPU time.
        runnable_ns: abs_to_ns(ri.ri_runnable_time).saturating_sub(cpu_ns),
        pageins: ri.ri_pageins,
    })
}

pub fn proc_name(pid: i32) -> String {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n <= 0 {
        return format!("pid {pid}");
    }
    String::from_utf8_lossy(&buf[..n as usize]).into_owned()
}
