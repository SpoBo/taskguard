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

/// The longest a footprint is reused before it is read again in full.
pub const FOOTPRINT_REUSE: f64 = 120.0;

/// Footprints of processes outside the queue, kept between two sweeps over
/// every process on the machine.
///
/// On Linux the footprint is PSS, and the kernel adds it up by walking every
/// page of the process: one sweep over a busy machine costs seconds of CPU,
/// while the resident size in /proc/<pid>/stat costs next to nothing. So a
/// sweep reads a process's footprint in full every two minutes, and in
/// between moves the last full reading by however much the resident size
/// grew or shrank since. macOS reads the footprint cheaply and keeps nothing.
#[derive(Default)]
pub struct Footprints {
    known: HashMap<i32, Footprint>,
    /// Full readings taken so far.
    pub reads: usize,
}

struct Footprint {
    /// When the process started, so a reused pid is not taken for the old one.
    start: u64,
    kb: u64,
    rss_kb: u64,
    due: f64,
}

impl Footprints {
    /// The footprint of `pid`, which started at `start` and has `rss_kb`
    /// resident now. `read` takes the full reading; it runs for a process not
    /// seen before, once the last full reading is due again, or when the
    /// resident size moved by more than a quarter since it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn get(&mut self, pid: i32, start: u64, rss_kb: u64, now: f64, read: impl FnOnce() -> u64) -> u64 {
        let seen = match self.known.get(&pid) {
            Some(f) if f.start == start => {
                if now < f.due && rss_kb.abs_diff(f.rss_kb) <= f.rss_kb / 4 {
                    return (f.kb + rss_kb).saturating_sub(f.rss_kb).min(rss_kb);
                }
                true
            }
            _ => false,
        };
        let kb = read();
        self.reads += 1;
        // The first reading of a process is due again after a share of the
        // usual time that depends on its pid, so the processes found together
        // when the recorder starts are not all read again in the same sweep.
        let share = if seen { 1.0 } else { ((pid as u64).wrapping_mul(2_654_435_761) % 1000) as f64 / 1000.0 };
        self.known.insert(pid, Footprint { start, kb, rss_kb, due: now + FOOTPRINT_REUSE * share });
        kb
    }

    /// Forget the processes that are gone.
    pub fn retain(&mut self, alive: impl Fn(i32) -> bool) {
        self.known.retain(|pid, _| alive(*pid));
    }
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
    fn a_footprint_is_moved_with_the_resident_size_until_it_is_due() {
        let mut f = Footprints::default();
        assert_eq!(f.get(10, 5, 1000, 0.0, || 600), 600);
        assert_eq!(f.get(10, 5, 1100, 1.0, || unreachable!()), 700);
        assert_eq!(f.get(10, 5, 900, 2.0, || unreachable!()), 500);
        assert_eq!(f.get(10, 5, 1000, FOOTPRINT_REUSE, || 650), 650, "due again");
        assert_eq!(f.get(10, 5, 1000, FOOTPRINT_REUSE * 2.0 - 1.0, || unreachable!()), 650);
        assert_eq!(f.reads, 2);
    }

    #[test]
    fn a_big_move_or_a_reused_pid_is_read_again() {
        let mut f = Footprints::default();
        f.get(10, 5, 1000, 0.0, || 600);
        assert_eq!(f.get(10, 5, 1300, 1.0, || 900), 900, "grew by more than a quarter");
        assert_eq!(f.get(10, 6, 1300, 2.0, || 50), 50, "another process with the same pid");
        f.retain(|_| false);
        assert_eq!(f.get(10, 6, 1300, 3.0, || 70), 70, "forgotten");
        assert_eq!(f.reads, 4);
    }

    #[test]
    fn the_first_readings_are_due_again_spread_out() {
        let mut f = Footprints::default();
        for pid in 1000..1600 {
            f.get(pid, 0, 1000, 0.0, || 1);
        }
        let mut most = 0;
        for tick in 1..=60 {
            let before = f.reads;
            for pid in 1000..1600 {
                f.get(pid, 0, 1000, tick as f64 * 2.0, || 1);
            }
            most = most.max(f.reads - before);
        }
        assert_eq!(f.reads, 1200, "each read once more within FOOTPRINT_REUSE");
        assert!(most <= 30, "{most} full readings in one sweep");
    }

    /// What keeps the recorder cheap: it samples every process on the machine
    /// every two seconds, and PSS costs the kernel a walk over every page.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sweep_two_seconds_later_reads_few_footprints_in_full() {
        let mut f = Footprints::default();
        let procs = list_procs();
        let now = crate::db::now();
        for p in &procs {
            proc_sample_with(p.pid, now, &mut f);
        }
        let first = f.reads;
        for p in &procs {
            proc_sample_with(p.pid, now + 2.0, &mut f);
        }
        let again = f.reads - first;
        assert!(again <= first / 10 + 3, "{again} of {first} processes read in full again");
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

    #[test]
    fn a_stopped_process_is_seen_as_stopped() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        assert_eq!(proc_stopped(pid), Some(false));
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let t = std::time::Instant::now();
        while proc_stopped(pid) != Some(true) && t.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(proc_stopped(pid), Some(true));
        unsafe { libc::kill(pid, libc::SIGCONT) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(proc_stopped(pid), None, "gone");
    }
}
