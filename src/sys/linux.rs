//! Linux readings from /proc.

use super::{CpuTicks, ProcInfo, ProcSample};
use std::fs;
use std::sync::OnceLock;

fn meminfo_kb(field: &str) -> Option<u64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    text.lines()
        .find(|l| l.starts_with(field) && l[field.len()..].starts_with(':'))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

pub fn mem_total_kb() -> u64 {
    meminfo_kb("MemTotal").unwrap_or(0)
}

/// Total minus available: memory the kernel cannot hand out without swapping.
pub fn mem_used_kb() -> u64 {
    let total = mem_total_kb();
    total.saturating_sub(meminfo_kb("MemAvailable").unwrap_or(total))
}

pub fn cpu_ticks() -> CpuTicks {
    let Ok(text) = fs::read_to_string("/proc/stat") else {
        return CpuTicks::default();
    };
    let Some(line) = text.lines().find(|l| l.starts_with("cpu ")) else {
        return CpuTicks::default();
    };
    // user nice system idle iowait irq softirq steal
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    let get = |i: usize| v.get(i).copied().unwrap_or(0);
    let busy = get(0) + get(1) + get(2) + get(5) + get(6) + get(7);
    CpuTicks { busy, total: busy + get(3) + get(4) }
}

/// The "some avg10" figure from pressure-stall information: the share of the
/// last 10 seconds in which at least one task waited on memory.
pub fn mem_pressure_pct() -> Option<f64> {
    let text = fs::read_to_string("/proc/pressure/memory").ok()?;
    let line = text.lines().find(|l| l.starts_with("some"))?;
    line.split_whitespace().find_map(|f| f.strip_prefix("avg10=")).and_then(|v| v.parse().ok())
}

/// The fields after the command name in /proc/<pid>/stat. The name is wrapped
/// in parentheses and may itself hold spaces or parentheses, so the split
/// happens at the LAST closing parenthesis.
fn stat_fields(pid: i32) -> Option<Vec<String>> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

pub fn list_procs() -> Vec<ProcInfo> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| {
            // after the name: state ppid ...
            let f = stat_fields(pid)?;
            Some(ProcInfo { pid, ppid: f.get(1)?.parse().ok()? })
        })
        .collect()
}

fn clk_tck() -> u64 {
    static T: OnceLock<u64> = OnceLock::new();
    *T.get_or_init(|| {
        let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if v > 0 { v as u64 } else { 100 }
    })
}

fn footprint_kb(pid: i32) -> u64 {
    // PSS splits shared pages between the processes that map them, so a tree of
    // forked workers is not counted several times over.
    let field = |path: String, name: &str| -> Option<u64> {
        let text = fs::read_to_string(path).ok()?;
        text.lines().find(|l| l.starts_with(name)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok())
    };
    field(format!("/proc/{pid}/smaps_rollup"), "Pss:").or_else(|| field(format!("/proc/{pid}/status"), "VmRSS:")).unwrap_or(0)
}

pub fn proc_sample(pid: i32) -> Option<ProcSample> {
    let f = stat_fields(pid)?;
    // Field numbers from proc(5), minus the two before the name's end:
    // majflt is field 12, utime 14, stime 15.
    let num = |i: usize| f.get(i - 3).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let ticks_ns = (num(14) + num(15)) * 1_000_000_000 / clk_tck();
    // schedstat: time on a CPU, time waiting on a run queue, time slices.
    let (cpu_ns, runnable_ns) = fs::read_to_string(format!("/proc/{pid}/schedstat"))
        .ok()
        .and_then(|t| {
            let mut it = t.split_whitespace().map(|v| v.parse::<u64>().ok());
            Some((it.next()??, it.next()??))
        })
        .unwrap_or((ticks_ns, 0));
    Some(ProcSample { footprint_kb: footprint_kb(pid), cpu_ns, runnable_ns, pageins: num(12) })
}

pub fn proc_name(pid: i32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_else(|_| format!("pid {pid}"))
}

/// The program's path, for grouping processes that share a name.
pub fn proc_path(pid: i32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.display().to_string())
}
