//! Platform layer: machine-wide readings and per-process readings.
//!
//! Everything above this module works in the same units on every platform:
//! kilobytes for memory, nanoseconds for CPU time, and a raw count for
//! page-ins.

use std::collections::HashMap;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("taskguard supports macOS and Linux only");

/// One reading of one process.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcSample {
    /// Memory the process really holds. macOS: physical footprint, which
    /// charges compressed pages at their original size. Linux: PSS.
    pub footprint_kb: u64,
    /// User + system CPU time since the process started.
    pub cpu_ns: u64,
    /// Time the process was ready to run but had to wait for a free core.
    pub runnable_ns: u64,
    /// Page-ins (macOS) or major faults (Linux) since the process started.
    pub pageins: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
}

/// Cumulative CPU tick counters for the whole machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTicks {
    pub busy: u64,
    pub total: u64,
}

pub fn ncpu() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// pid -> its direct children.
pub fn children_map(procs: &[ProcInfo]) -> HashMap<i32, Vec<i32>> {
    let mut map: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs {
        map.entry(p.ppid).or_default().push(p.pid);
    }
    map
}

/// The root and every process below it. The memory of a command often lives in
/// a grandchild: a JavaScript entry point is a node process that spawns the
/// native compiler, so measuring the direct child alone reports almost nothing.
pub fn descendants(root: i32, children: &HashMap<i32, Vec<i32>>) -> Vec<i32> {
    let mut out = vec![root];
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some(kids) = children.get(&p) {
            for &k in kids {
                if k != p && !out.contains(&k) {
                    out.push(k);
                    stack.push(k);
                }
            }
        }
    }
    out
}

/// Every pid that is the given pid or one of its ancestors.
pub fn ancestors(pid: i32, procs: &[ProcInfo]) -> Vec<i32> {
    let parent: HashMap<i32, i32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut out = vec![pid];
    let mut cur = pid;
    while let Some(&pp) = parent.get(&cur) {
        if pp <= 1 || out.contains(&pp) {
            break;
        }
        out.push(pp);
        cur = pp;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendants_walks_the_whole_tree() {
        let procs = [
            ProcInfo { pid: 10, ppid: 1 },
            ProcInfo { pid: 11, ppid: 10 },
            ProcInfo { pid: 12, ppid: 11 },
            ProcInfo { pid: 13, ppid: 10 },
            ProcInfo { pid: 20, ppid: 1 },
        ];
        let mut d = descendants(10, &children_map(&procs));
        d.sort();
        assert_eq!(d, vec![10, 11, 12, 13]);
    }

    #[test]
    fn ancestors_stop_at_init() {
        let procs = [ProcInfo { pid: 10, ppid: 1 }, ProcInfo { pid: 11, ppid: 10 }, ProcInfo { pid: 12, ppid: 11 }];
        assert_eq!(ancestors(12, &procs), vec![12, 11, 10]);
    }

    #[test]
    fn this_process_can_be_measured() {
        let s = proc_sample(std::process::id() as i32).expect("own process");
        assert!(s.footprint_kb > 0);
        assert!(mem_total_kb() > 0);
        assert!(mem_used_kb() > 0);
        let t = cpu_ticks();
        assert!(t.total >= t.busy);
        assert!(list_procs().iter().any(|p| p.pid == std::process::id() as i32));
    }
}
