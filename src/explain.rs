//! Where the CPU, memory and IO of this machine go now, for the Overview:
//! taskguard's jobs, each running dev stack, what else runs here, and the
//! load of the host outside this machine.
//!
//! "This machine" is the top of the cgroup v2 tree a process here can see. In
//! an LXC container, or a Docker container with its own cgroup namespace,
//! that is the container's own cgroup, while /proc/stat and /proc/pressure
//! still read the whole host: what the host shows beyond the cgroup is load
//! outside this machine. On bare metal the top of the tree is the host's root
//! cgroup, so nothing is outside. macOS has no cgroups: the view shows the
//! machine totals it showed before, and nothing outside.

use crate::queue::Entry;
use crate::sys::{self, CpuTicks, ProcInfo};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// CPU, memory and IO of one part of the machine. None where it is not
/// known: no reading, or the first reading of a rate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub cores: Option<f64>,
    pub mem_kb: Option<u64>,
    /// Bytes per second read from and written to storage.
    pub io_bps: Option<f64>,
}

impl Usage {
    fn plus(self, o: Usage) -> Usage {
        Usage {
            cores: self.cores.zip(o.cores).map(|(a, b)| a + b),
            mem_kb: self.mem_kb.zip(o.mem_kb).map(|(a, b)| a + b),
            io_bps: self.io_bps.zip(o.io_bps).map(|(a, b)| a + b),
        }
    }

    /// What `self` holds beyond `part`, never below 0.
    pub fn minus(self, part: Usage) -> Usage {
        Usage {
            cores: self.cores.zip(part.cores).map(|(a, b)| (a - b).max(0.0)),
            mem_kb: self.mem_kb.zip(part.mem_kb).map(|(a, b)| a.saturating_sub(b)),
            io_bps: self.io_bps.zip(part.io_bps).map(|(a, b)| (a - b).max(0.0)),
        }
    }
}

/// One running dev stack: a `devenv up` or `process-compose` process tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Stack {
    pub pid: i32,
    /// The directory it runs in, shortened.
    pub label: String,
    /// The taskguard job it runs as, (key, namespace), when it was admitted
    /// as one. Then the whole job is this one line.
    pub job: Option<(String, String)>,
    pub usage: Usage,
}

/// What the host shows beyond this machine's cgroup.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Outside {
    /// Busy cores of the host (/proc/stat) beyond the cgroup's own.
    pub cores: Option<f64>,
    /// The share of time tasks waited for a core, or for IO ("some" avg10),
    /// on the host beyond the share in which tasks of the cgroup did.
    pub cpu_psi: Option<f64>,
    pub io_psi: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Explain {
    /// This machine, from its own cgroup. None without a readable cgroup v2;
    /// then the view falls back to the machine totals.
    pub machine: Option<Usage>,
    /// taskguard's jobs that are not dev stacks.
    pub jobs: Usage,
    pub stacks: Vec<Stack>,
    /// None on bare metal and without a cgroup.
    pub outside: Option<Outside>,
}

impl Explain {
    /// taskguard's jobs and the dev stacks: each process counted once.
    pub fn tracked(&self) -> Usage {
        self.stacks.iter().fold(self.jobs, |sum, s| sum.plus(s.usage))
    }

    /// What runs on this machine that neither a job nor a stack explains,
    /// such as idle agents and editors.
    pub fn untracked(&self, machine: Usage) -> Usage {
        machine.minus(self.tracked())
    }
}

// ---------------------------------------------------------------- parsers ----

/// The cgroup v2 path of this process: the "0::" line of /proc/self/cgroup.
/// A machine with only cgroup v1 has none.
pub fn parse_self_cgroup(text: &str) -> Option<&str> {
    text.lines().find_map(|l| l.strip_prefix("0::")).map(str::trim).filter(|p| p.starts_with('/'))
}

/// One "key value" line of a flat file such as cpu.stat or memory.stat.
pub fn parse_flat(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')).and_then(|v| v.trim().parse().ok())
}

/// Bytes read and written on every device in a cgroup's io.stat, whose
/// lines read "8:0 rbytes=1 wbytes=2 rios=3 wios=4 dbytes=0 dios=0".
pub fn parse_io_stat(text: &str) -> u64 {
    text.split_whitespace()
        .filter_map(|f| f.strip_prefix("rbytes=").or_else(|| f.strip_prefix("wbytes=")))
        .filter_map(|v| v.parse::<u64>().ok())
        .sum()
}

/// One reading of this machine's cgroup.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cgroup {
    /// CPU time of every process in it, microseconds.
    pub usage_usec: Option<u64>,
    /// memory.current minus the file cache the kernel can drop at once
    /// (inactive_file), so a big page cache does not read as untracked load.
    pub mem_kb: Option<u64>,
    pub io_bytes: Option<u64>,
    /// "some" avg10 of cpu.pressure and io.pressure.
    pub cpu_psi: Option<f64>,
    pub io_psi: Option<f64>,
    /// The host's root cgroup, which has no cgroup.type and no
    /// memory.current: this machine is the whole host.
    pub root: bool,
}

/// Read the cgroup v2 tree mounted at `mount`, for a process whose
/// /proc/self/cgroup reads `self_cgroup`. The process itself sits in a leaf
/// such as a login session; the machine is the top of the tree it sees. The
/// mount shows that tree only when the process's own cgroup is in it.
pub fn read_cgroup(mount: &Path, self_cgroup: &str) -> Option<Cgroup> {
    let own = parse_self_cgroup(self_cgroup)?;
    if !mount.join(own.trim_start_matches('/')).is_dir() {
        return None;
    }
    let read = |f: &str| fs::read_to_string(mount.join(f)).ok();
    let psi = |f: &str| read(f).and_then(|t| sys::parse_psi(&t, "some")).map(|p| p.avg10);
    let mem_kb = read("memory.current").and_then(|t| t.trim().parse::<u64>().ok()).map(|bytes| {
        let cache = read("memory.stat").and_then(|t| parse_flat(&t, "inactive_file")).unwrap_or(0);
        bytes.saturating_sub(cache) / 1024
    });
    let c = Cgroup {
        usage_usec: read("cpu.stat").and_then(|t| parse_flat(&t, "usage_usec")),
        mem_kb,
        io_bytes: read("io.stat").map(|t| parse_io_stat(&t)),
        cpu_psi: psi("cpu.pressure"),
        io_psi: psi("io.pressure"),
        root: !mount.join("cgroup.type").exists(),
    };
    (c.usage_usec.is_some() || c.mem_kb.is_some() || c.io_bytes.is_some()).then_some(c)
}

// ----------------------------------------------------------------- the split ----

/// Busy cores between two /proc/stat readings that count `cores` cores.
pub fn busy_cores(prev: CpuTicks, now: CpuTicks, cores: usize) -> Option<f64> {
    let total = now.total.checked_sub(prev.total).filter(|t| *t > 0)?;
    Some(now.busy.saturating_sub(prev.busy) as f64 / total as f64 * cores as f64)
}

/// The host's load that this machine's cgroup does not explain. Whenever a
/// task of the cgroup stalls, a task of the host stalls, so the host's "some"
/// share beyond the cgroup's is time in which only tasks outside stalled.
pub fn outside(
    host_cores: Option<f64>,
    here_cores: Option<f64>,
    host_psi: (Option<f64>, Option<f64>),
    here_psi: (Option<f64>, Option<f64>),
) -> Outside {
    let beyond = |host: Option<f64>, here: Option<f64>| Some((host? - here?).max(0.0));
    Outside { cores: beyond(host_cores, here_cores), cpu_psi: beyond(host_psi.0, here_psi.0), io_psi: beyond(host_psi.1, here_psi.1) }
}

// ----------------------------------------------------------------- stacks ----

/// Subcommands of a process-compose client, which talks to a running stack.
const CLIENT: [&str; 10] = ["down", "attach", "process", "project", "list", "version", "completion", "docs", "help", "recipe"];

/// Does the command line run a dev stack: `devenv up`, or a process-compose
/// server, not one of its client commands such as `process-compose down`?
pub fn is_stack(args: &[String]) -> bool {
    let Some(prog) = args.first() else { return false };
    // Nix wraps programs: .process-compose-wrapped.
    let base = prog.rsplit('/').next().unwrap_or(prog).trim_start_matches('.').trim_end_matches("-wrapped");
    let mut words = args[1..].iter().map(String::as_str).take_while(|a| *a != "--");
    match base {
        "devenv" => words.any(|w| w == "up"),
        "process-compose" => !words.any(|w| CLIENT.contains(&w)),
        _ => false,
    }
}

/// A process whose name could be a dev stack's, so its command line is worth
/// reading. Linux cuts names to 15 characters.
fn may_be_stack(name: &str) -> bool {
    name.contains("devenv") || name.contains("process-compos")
}

/// The roots of the running dev stacks: each process that `stack` says runs
/// one, and that no other such process started. A `devenv up` starts
/// process-compose; together they are one stack.
pub fn find_stacks(procs: &[ProcInfo], mut stack: impl FnMut(i32) -> bool) -> Vec<i32> {
    let hits: HashSet<i32> = procs.iter().map(|p| p.pid).filter(|p| stack(*p)).collect();
    let mut roots: Vec<i32> = hits.iter().copied().filter(|p| !sys::ancestors(*p, procs)[1..].iter().any(|a| hits.contains(a))).collect();
    roots.sort_unstable();
    roots
}

/// Short names for stacks, from the directory each runs in: its last part,
/// or the last two where the last part alone is the same for two stacks
/// (worktrees of one repo).
pub fn labels(stacks: &[(i32, Option<String>)]) -> Vec<String> {
    let tail = |dir: &str, n: usize| {
        let parts: Vec<&str> = dir.trim_end_matches('/').rsplit('/').filter(|p| !p.is_empty()).take(n).collect();
        parts.into_iter().rev().collect::<Vec<_>>().join("/")
    };
    stacks
        .iter()
        .map(|(pid, dir)| match dir {
            Some(d) => {
                let short = tail(d, 1);
                let shared = stacks.iter().filter(|(_, o)| o.as_deref().is_some_and(|o| tail(o, 1) == short)).count() > 1;
                if shared { tail(d, 2) } else { short }
            }
            None => format!("pid {pid}"),
        })
        .collect()
}

// ---------------------------------------------------------------- sampling ----

/// Counters kept between two readings, for rates.
#[derive(Default)]
pub struct Sampler {
    at: f64,
    /// CPU time and IO bytes of each process measured last time.
    procs: HashMap<i32, (u64, u64)>,
    cgroup: Option<Cgroup>,
    host: Option<CpuTicks>,
    names: HashMap<i32, String>,
    footprints: sys::Footprints,
}

impl Sampler {
    /// Read the machine now. `running` are taskguard's running jobs.
    pub fn sample(&mut self, running: &[Entry]) -> Explain {
        // The time of this reading itself, not of the load around it: a rate
        // over the wrong interval is wrong by the whole difference.
        let now = crate::db::now();
        let procs = sys::list_procs();
        let children = sys::children_map(&procs);
        let alive: HashSet<i32> = procs.iter().map(|p| p.pid).collect();
        self.names.retain(|pid, _| alive.contains(pid));
        self.footprints.retain(|pid| alive.contains(&pid));
        let dt = now - self.at;
        let rates = (0.2..30.0).contains(&dt);

        let in_jobs: HashSet<i32> = running.iter().flat_map(|e| sys::descendants(e.pid, &children)).collect();
        let job_pids: HashMap<i32, &Entry> = running.iter().map(|e| (e.pid, e)).collect();
        let names = &mut self.names;
        let roots = find_stacks(&procs, |pid| {
            may_be_stack(names.entry(pid).or_insert_with(|| sys::proc_name(pid))) && sys::proc_args(pid).is_some_and(|a| is_stack(&a))
        });
        // Each process goes to one line: a stack admitted as a job is its
        // whole job, a stack outside the queue is what of it no job runs.
        let mut claimed: HashSet<i32> = HashSet::new();
        let (mut stacks, mut stack_pids): (Vec<Stack>, Vec<Vec<i32>>) = (Vec::new(), Vec::new());
        for root in roots {
            if claimed.contains(&root) {
                continue;
            }
            let outer = sys::ancestors(root, &procs).into_iter().rev().find_map(|a| job_pids.get(&a).copied());
            let (job, pids): (_, Vec<i32>) = match outer {
                Some(e) => (Some((e.key.clone(), e.ns.clone())), sys::descendants(e.pid, &children)),
                None => (None, sys::descendants(root, &children).into_iter().filter(|p| !in_jobs.contains(p)).collect()),
            };
            let pids: Vec<i32> = pids.into_iter().filter(|p| !claimed.contains(p)).collect();
            claimed.extend(&pids);
            stacks.push(Stack { pid: root, label: String::new(), job, usage: Usage::default() });
            stack_pids.push(pids);
        }
        let job_rest: Vec<i32> = in_jobs.iter().copied().filter(|p| !claimed.contains(p)).collect();

        let prev = std::mem::take(&mut self.procs);
        let jobs = self.measure(&job_rest, &prev, now, dt, rates);
        let dirs: Vec<(i32, Option<String>)> = stacks.iter().map(|s| (s.pid, sys::proc_cwd(s.pid))).collect();
        for ((stack, pids), label) in stacks.iter_mut().zip(&stack_pids).zip(labels(&dirs)) {
            stack.label = label;
            stack.usage = self.measure(pids, &prev, now, dt, rates);
        }

        let cgroup = read_cgroup(Path::new("/sys/fs/cgroup"), &fs::read_to_string("/proc/self/cgroup").unwrap_or_default());
        let stat = fs::read_to_string("/proc/stat").ok().and_then(|t| sys::parse_stat(&t));
        let host_cores = match (self.host, stat) {
            (Some(p), Some((t, cores))) if rates => busy_cores(p, t, cores),
            _ => None,
        };
        let rate = |now: Option<u64>, prev: Option<u64>, per_s: f64| match (now, prev) {
            (Some(n), Some(p)) if rates && n >= p => Some((n - p) as f64 / per_s / dt),
            _ => None,
        };
        let prev_cgroup = self.cgroup;
        let machine = cgroup.map(|c| Usage {
            cores: rate(c.usage_usec, prev_cgroup.and_then(|p| p.usage_usec), 1e6).or(host_cores.filter(|_| c.root)),
            mem_kb: c.mem_kb.or_else(|| c.root.then(sys::mem_used_kb)),
            io_bps: rate(c.io_bytes, prev_cgroup.and_then(|p| p.io_bytes), 1.0),
        });
        let host_psi =
            |r: &str| fs::read_to_string(format!("/proc/pressure/{r}")).ok().and_then(|t| sys::parse_psi(&t, "some")).map(|p| p.avg10);
        let outside = match (cgroup, machine) {
            (Some(c), Some(m)) if !c.root => Some(outside(host_cores, m.cores, (host_psi("cpu"), host_psi("io")), (c.cpu_psi, c.io_psi))),
            _ => None,
        };

        self.at = now;
        self.cgroup = cgroup;
        self.host = stat.map(|(t, _)| t);
        Explain { machine, jobs, stacks, outside }
    }

    /// CPU, memory and IO of a set of processes. A rate needs the reading
    /// before; a process not seen then adds nothing to it yet.
    fn measure(&mut self, pids: &[i32], prev: &HashMap<i32, (u64, u64)>, now: f64, dt: f64, rates: bool) -> Usage {
        let (mut cores, mut mem_kb, mut io) = (0.0, 0u64, 0.0);
        for &pid in pids {
            let Some(s) = sys::proc_sample_with(pid, now, &mut self.footprints) else { continue };
            let bytes = sys::proc_io_bytes(pid).unwrap_or(0);
            mem_kb += s.footprint_kb;
            if let Some(&(cpu_ns, io_bytes)) = prev.get(&pid) {
                cores += s.cpu_ns.saturating_sub(cpu_ns) as f64 / 1e9 / dt;
                io += bytes.saturating_sub(io_bytes) as f64 / dt;
            }
            self.procs.insert(pid, (s.cpu_ns, bytes));
        }
        Usage { cores: rates.then_some(cores), mem_kb: Some(mem_kb), io_bps: rates.then_some(io) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn the_own_cgroup_is_the_v2_line() {
        assert_eq!(parse_self_cgroup("0::/\n"), Some("/"));
        assert_eq!(
            parse_self_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n"),
            Some("/user.slice/user-1000.slice/session-3.scope")
        );
        // A hybrid machine lists the v1 controllers first.
        assert_eq!(parse_self_cgroup("12:memory:/lxc/111\n1:name=systemd:/init.scope\n0::/init.scope\n"), Some("/init.scope"));
        assert_eq!(parse_self_cgroup("12:memory:/lxc/111\n1:cpu,cpuacct:/\n"), None, "cgroup v1 only");
        assert_eq!(parse_self_cgroup(""), None);
    }

    #[test]
    fn cgroup_files_are_parsed() {
        let cpu = "usage_usec 81234567\nuser_usec 60000000\nsystem_usec 21234567\nnr_periods 0\n";
        assert_eq!(parse_flat(cpu, "usage_usec"), Some(81_234_567));
        assert_eq!(parse_flat(cpu, "user"), None, "a whole key, not a prefix");
        let mem = "anon 1000\nfile 9000\ninactive_file 4096\nactive_file 2048\n";
        assert_eq!(parse_flat(mem, "inactive_file"), Some(4096));
        assert_eq!(parse_flat(mem, "file"), Some(9000));
        let io =
            "8:0 rbytes=1048576 wbytes=2097152 rios=10 wios=20 dbytes=0 dios=0\n259:0 rbytes=100 wbytes=50 rios=1 wios=1 dbytes=7 dios=1\n";
        assert_eq!(parse_io_stat(io), 1048576 + 2097152 + 150);
        assert_eq!(parse_io_stat(""), 0, "no IO yet");
    }

    /// A fake cgroup mount: `files` at the top, and the process's own leaf.
    fn mount(files: &[(&str, &str)], leaf: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(leaf.trim_start_matches('/'))).unwrap();
        for (name, text) in files {
            fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    #[test]
    fn a_container_reads_its_own_cgroup() {
        let m = mount(
            &[
                ("cgroup.type", "domain\n"),
                ("cpu.stat", "usage_usec 5000000\nuser_usec 4000000\n"),
                ("memory.current", "8589934592\n"),
                ("memory.stat", "anon 4294967296\ninactive_file 1073741824\n"),
                ("io.stat", "8:0 rbytes=4096 wbytes=8192 rios=1 wios=2 dbytes=0 dios=0\n"),
                ("cpu.pressure", "some avg10=12.50 avg60=10.00 avg300=5.00 total=123\nfull avg10=1.00 avg60=0 avg300=0 total=4\n"),
                ("io.pressure", "some avg10=3.00 avg60=0 avg300=0 total=9\nfull avg10=2.00 avg60=0 avg300=0 total=5\n"),
            ],
            "/init.scope",
        );
        let c = read_cgroup(m.path(), "0::/init.scope\n").unwrap();
        assert_eq!(
            c,
            Cgroup {
                usage_usec: Some(5_000_000),
                mem_kb: Some(7 << 20),
                io_bytes: Some(12288),
                cpu_psi: Some(12.5),
                io_psi: Some(3.0),
                root: false,
            }
        );
    }

    #[test]
    fn the_hosts_root_cgroup_is_the_whole_host() {
        let m = mount(&[("cpu.stat", "usage_usec 99\n"), ("io.stat", "")], "/user.slice/user-1000.slice");
        let c = read_cgroup(m.path(), "0::/user.slice/user-1000.slice\n").unwrap();
        assert!(c.root);
        assert_eq!((c.usage_usec, c.mem_kb, c.io_bytes), (Some(99), None, Some(0)));
    }

    #[test]
    fn no_cgroup_v2_reads_nothing() {
        let m = mount(&[("cpu.stat", "usage_usec 99\n")], "/");
        assert_eq!(read_cgroup(m.path(), "1:cpu:/\n"), None, "cgroup v1");
        assert_eq!(read_cgroup(m.path(), ""), None, "no /proc/self/cgroup, as on macOS");
        assert_eq!(read_cgroup(m.path(), "0::/docker/abc\n"), None, "the mount is not this process's tree");
        let empty = mount(&[], "/");
        assert_eq!(read_cgroup(empty.path(), "0::/\n"), None, "no file to read");
    }

    #[test]
    fn untracked_is_the_machine_minus_jobs_and_stacks() {
        let u = |c: f64, m: u64, io: f64| Usage { cores: Some(c), mem_kb: Some(m), io_bps: Some(io) };
        let stack = |job: bool, usage| Stack { pid: 1, label: "s".into(), job: job.then(|| ("k".into(), "ns".into())), usage };
        let e = Explain {
            machine: Some(u(8.0, 20_000, 900.0)),
            jobs: u(3.0, 6_000, 100.0),
            stacks: vec![stack(false, u(1.5, 4_000, 50.0)), stack(true, u(1.0, 3_000, 0.0))],
            ..Default::default()
        };
        assert_eq!(e.tracked(), u(5.5, 13_000, 150.0));
        assert_eq!(e.untracked(e.machine.unwrap()), u(2.5, 7_000, 750.0));
        // Jobs that read more than the cgroup at this instant leave nothing, not less.
        assert_eq!(u(1.0, 100, 0.0).minus(u(2.0, 200, 5.0)), u(0.0, 0, 0.0));
        // The first reading has no rates: those stay unknown.
        let first = Usage { cores: None, mem_kb: Some(50), io_bps: None };
        assert_eq!(u(8.0, 100, 1.0).minus(first), Usage { cores: None, mem_kb: Some(50), io_bps: None });
        assert_eq!(Explain::default().tracked(), Usage::default(), "no reading at all");
        assert_eq!(Explain { jobs: u(0.0, 0, 0.0), ..Default::default() }.tracked(), u(0.0, 0, 0.0), "no jobs, no stacks");
    }

    #[test]
    fn outside_is_what_the_host_shows_beyond_the_cgroup() {
        assert_eq!(busy_cores(CpuTicks { busy: 100, total: 1000 }, CpuTicks { busy: 400, total: 2000 }, 32), Some(9.6));
        assert_eq!(busy_cores(CpuTicks { busy: 1, total: 10 }, CpuTicks { busy: 1, total: 10 }, 4), None, "no time passed");
        let o = outside(Some(9.6), Some(2.1), (Some(40.0), Some(6.0)), (Some(5.0), Some(1.5)));
        assert!((o.cores.unwrap() - 7.5).abs() < 1e-9);
        assert_eq!((o.cpu_psi, o.io_psi), (Some(35.0), Some(4.5)));
        // The cgroup's own reading can run a little ahead of the host's.
        assert_eq!(outside(Some(2.0), Some(2.2), (Some(1.0), None), (Some(3.0), Some(1.0))).cores, Some(0.0));
        // A cgroup without pressure files says nothing about pressure outside.
        let none = outside(Some(4.0), None, (Some(40.0), Some(6.0)), (None, None));
        assert_eq!(none, Outside { cores: None, cpu_psi: None, io_psi: None });
    }

    #[test]
    fn stacks_are_found_by_their_command_line() {
        assert!(is_stack(&args("devenv up")));
        assert!(is_stack(&args("/nix/store/abc-devenv-1.11/bin/devenv --profile api-e2e up -d")));
        assert!(is_stack(&args("/nix/store/x-process-compose/bin/.process-compose-wrapped --unix-socket /tmp/pc.sock --tui=false")));
        assert!(is_stack(&args("process-compose up -f process-compose.yml")));
        assert!(!is_stack(&args("process-compose --unix-socket /tmp/pc.sock down")), "a client");
        assert!(!is_stack(&args("process-compose process list")));
        assert!(!is_stack(&args("devenv shell")));
        assert!(!is_stack(&args("devenv shell -- devenv up")), "after -- is another command");
        assert!(!is_stack(&args("bun run up")));
        assert!(!is_stack(&[]));
    }

    #[test]
    fn a_stack_is_its_outermost_process() {
        // 10 zsh -> 11 devenv up -> 12 process-compose -> 13 postgres, 14 bun;
        // 20 process-compose (detached, by itself) -> 21 redis;
        // 30 taskguard -> 31 devenv up -> 32 process-compose; 40 process-compose down.
        let tree = [(10, 1), (11, 10), (12, 11), (13, 12), (14, 12), (20, 1), (21, 20), (30, 1), (31, 30), (32, 31), (40, 10)];
        let procs: Vec<ProcInfo> = tree.iter().map(|&(pid, ppid)| ProcInfo { pid, ppid }).collect();
        let cmd: HashMap<i32, &str> = [
            (10, "zsh"),
            (11, "devenv up"),
            (12, "process-compose --tui=false"),
            (13, "postgres -D db"),
            (14, "bun run dev"),
            (20, "process-compose -D"),
            (21, "redis-server"),
            (30, "taskguard --id stack -- devenv up"),
            (31, "devenv up"),
            (32, "process-compose"),
            (40, "process-compose down"),
        ]
        .into();
        let roots = find_stacks(&procs, |pid| is_stack(&args(cmd[&pid])));
        assert_eq!(roots, vec![11, 20, 31]);
    }

    #[test]
    fn stack_labels_are_short_and_tell_worktrees_apart() {
        let got = labels(&[
            (1, Some("/Users/v/.treehouse/dalp-a91e/3/dalp".into())),
            (2, Some("/Users/v/.treehouse/dalp-a91e/5/dalp/".into())),
            (3, Some("/home/v/src/site".into())),
            (4, None),
        ]);
        assert_eq!(got, vec!["3/dalp", "5/dalp", "site", "pid 4"]);
    }

    /// The real machine: this test's own process is no stack, and every
    /// rate is known from the second reading on.
    #[test]
    fn a_second_reading_has_rates() {
        let me = std::process::id() as i32;
        let entry = Entry { pid: me, key: "test".into(), ns: "ns".into(), ..Default::default() };
        let mut s = Sampler::default();
        let first = s.sample(std::slice::from_ref(&entry));
        assert_eq!(first.jobs.cores, None, "no rate from one reading");
        assert!(first.jobs.mem_kb.unwrap() > 0);
        std::thread::sleep(std::time::Duration::from_millis(250));
        let second = s.sample(std::slice::from_ref(&entry));
        assert!(second.jobs.cores.is_some() && second.jobs.io_bps.is_some());
        assert!(second.stacks.iter().all(|st| st.pid != me));
        if cfg!(target_os = "macos") {
            assert_eq!((second.machine, second.outside), (None, None), "no cgroups on macOS");
        }
    }
}
