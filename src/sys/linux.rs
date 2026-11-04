//! Linux readings from /proc.

use super::{CpuTicks, ProcInfo, ProcSample, Psi};
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

pub fn swap_free_kb() -> Option<u64> {
    meminfo_kb("SwapFree")
}

/// Make the process that calls it, and every process it starts from then
/// on, the OOM killer's first pick. Raising the score needs no privilege.
/// Only async-signal-safe calls: it runs between fork and exec.
pub fn raise_oom_score() {
    let path = c"/proc/self/oom_score_adj";
    let value = b"800";
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
        if fd >= 0 {
            libc::write(fd, value.as_ptr() as *const libc::c_void, value.len());
            libc::close(fd);
        }
    }
}

pub fn cpu_ticks() -> CpuTicks {
    fs::read_to_string("/proc/stat").ok().and_then(|t| super::parse_stat(&t)).map(|(t, _)| t).unwrap_or_default()
}

/// One line ("some" or "full") of a pressure-stall file: "avg10", the share
/// of the last 10 seconds in which tasks stalled, and "total", the
/// microseconds they stalled since boot. None on a kernel without PSI.
fn psi(resource: &str, line: &str) -> Option<Psi> {
    super::parse_psi(&fs::read_to_string(format!("/proc/pressure/{resource}")).ok()?, line)
}

/// Memory "some avg10": the share of the last 10 seconds in which at least
/// one task waited on memory.
pub fn mem_pressure_pct() -> Option<f64> {
    psi("memory", "some").map(|p| p.avg10)
}

/// CPU "some": at least one runnable task waited for a core. It counts every
/// program on the machine, also the load taskguard did not start.
pub fn cpu_pressure() -> Option<Psi> {
    psi("cpu", "some")
}

/// IO "full": every task that could run was stalled on IO at once.
pub fn io_pressure() -> Option<Psi> {
    psi("io", "full")
}

/// The fields after the command name in /proc/<pid>/stat. The name is wrapped
/// in parentheses and may itself hold spaces or parentheses, so the split
/// happens at the LAST closing parenthesis.
fn stat_fields(pid: i32) -> Option<Vec<String>> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// Some(true) when the process is stopped (SIGSTOP, SIGTSTP), Some(false)
/// when it runs or sleeps, None when it is gone or a zombie.
pub fn proc_stopped(pid: i32) -> Option<bool> {
    match stat_fields(pid)?.first()?.as_str() {
        "Z" | "X" => None,
        s => Some(s == "T"),
    }
}

pub fn list_procs() -> Vec<ProcInfo> {
    list_procs_stat().into_iter().map(|(p, _)| p).collect()
}

/// The fields of /proc/<pid>/stat after the name, as a sweep read them.
pub struct Stat(Vec<String>);

/// `list_procs` with the stat of each process, so a sweep that samples them
/// after reads it once and not twice.
pub fn list_procs_stat() -> Vec<(ProcInfo, Stat)> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| {
            // after the name: state ppid ...
            let f = stat_fields(pid)?;
            Some((ProcInfo { pid, ppid: f.get(1)?.parse().ok()? }, Stat(f)))
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

fn page_kb() -> u64 {
    static K: OnceLock<u64> = OnceLock::new();
    *K.get_or_init(|| {
        let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if v > 0 { v as u64 / 1024 } else { 4 }
    })
}

/// Field `i` of /proc/<pid>/stat as numbered in proc(5), from `stat_fields`,
/// which drops the two fields up to the name's end.
fn stat_num(f: &[String], i: usize) -> u64 {
    f.get(i - 3).and_then(|v| v.parse().ok()).unwrap_or(0)
}

pub fn proc_sample(pid: i32) -> Option<ProcSample> {
    sample_from(pid, &stat_fields(pid)?, footprint_kb(pid))
}

/// `proc_sample` for a sweep over every process: the footprint comes from
/// `footprints` while the last full reading still holds.
pub fn proc_sample_with(pid: i32, now: f64, footprints: &mut super::Footprints) -> Option<ProcSample> {
    proc_sample_stat(pid, &Stat(stat_fields(pid)?), now, footprints)
}

/// `proc_sample_with` from the stat that `list_procs_stat` read.
pub fn proc_sample_stat(pid: i32, stat: &Stat, now: f64, footprints: &mut super::Footprints) -> Option<ProcSample> {
    let f = &stat.0;
    // starttime is field 22, rss (in pages) 24.
    let kb = footprints.get(pid, stat_num(f, 22), stat_num(f, 24) * page_kb(), now, || footprint_kb(pid));
    sample_from(pid, f, kb)
}

fn sample_from(pid: i32, f: &[String], footprint_kb: u64) -> Option<ProcSample> {
    // majflt is field 12, utime 14, stime 15.
    let num = |i: usize| stat_num(f, i);
    let ticks_ns = (num(14) + num(15)) * 1_000_000_000 / clk_tck();
    // schedstat: time on a CPU, time waiting on a run queue, time slices.
    let (cpu_ns, runnable_ns) = fs::read_to_string(format!("/proc/{pid}/schedstat"))
        .ok()
        .and_then(|t| {
            let mut it = t.split_whitespace().map(|v| v.parse::<u64>().ok());
            Some((it.next()??, it.next()??))
        })
        .unwrap_or((ticks_ns, 0));
    Some(ProcSample { footprint_kb, cpu_ns, runnable_ns, pageins: num(12) })
}

pub fn proc_name(pid: i32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_else(|_| format!("pid {pid}"))
}

/// The command line of a process. None when it is gone or a kernel thread.
pub fn proc_args(pid: i32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    (!args.is_empty()).then_some(args)
}

/// The program's path, for grouping processes that share a name.
pub fn proc_path(pid: i32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.display().to_string())
}

/// Bytes the process and its threads read from and wrote to storage since it
/// started. None when it is gone or belongs to another user.
pub fn proc_io_bytes(pid: i32) -> Option<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)).and_then(|v| v.trim().parse::<u64>().ok());
    Some(field("read_bytes:")? + field("write_bytes:")?)
}

/// The directory the process runs in.
pub fn proc_cwd(pid: i32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.display().to_string())
}
