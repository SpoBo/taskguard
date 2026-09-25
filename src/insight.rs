//! What a run's measurements mean: sustained load, starvation, trends, and the
//! advice printed after a starved run.

use crate::report::gb;
use crate::sys::{self, ProcSample};
use std::collections::{HashMap, VecDeque};

/// Seconds of the rolling window that "sustained" load is averaged over, so a
/// short spike does not count.
const WINDOW_S: f64 = 10.0;

/// Measures one job's process tree across samples.
#[derive(Default)]
pub struct Tracker {
    last: HashMap<i32, ProcSample>,
    last_ts: Option<f64>,
    window: VecDeque<Step>,
    pub first_ts: Option<f64>,
    pub peak_mem_kb: u64,
    pub cpu_ns: u64,
    pub runnable_ns: u64,
    pub pageins: u64,
    pub max_used: f64,
    pub max_wanted: f64,
    pub max_pageins_per_s: f64,
    pub samples: usize,
    pub full_samples: usize,
    pub pressure_samples: usize,
}

#[derive(Clone, Copy)]
struct Step {
    dt: f64,
    cpu: f64,
    run: f64,
    pageins: f64,
}

/// One reading of the tree.
#[derive(Debug, Clone, Copy, Default)]
pub struct Reading {
    pub used: f64,
    pub wanted: f64,
    pub mem_kb: u64,
    pub pageins_per_s: f64,
}

impl Tracker {
    /// A tracker for a job that started at `t`: the first reading then covers
    /// the time since the start.
    pub fn starting_at(t: f64) -> Tracker {
        Tracker { last_ts: Some(t), first_ts: Some(t), ..Default::default() }
    }

    /// Read the whole tree under `root` now.
    pub fn sample(&mut self, root: i32, ts: f64) -> Option<Reading> {
        let procs = sys::list_procs();
        let tree = sys::descendants(root, &sys::children_map(&procs));
        let mut cur: HashMap<i32, ProcSample> = HashMap::new();
        for pid in tree {
            if let Some(s) = sys::proc_sample(pid) {
                cur.insert(pid, s);
            }
        }
        self.add(cur, ts)
    }

    /// Fold one set of per-process readings in. Each process contributes the
    /// growth of its own counters, so a child that exits between two samples
    /// does not make the tree's totals go backwards.
    pub fn add(&mut self, cur: HashMap<i32, ProcSample>, ts: f64) -> Option<Reading> {
        if cur.is_empty() {
            return None;
        }
        let mem: u64 = cur.values().map(|s| s.footprint_kb).sum();
        let (mut dcpu, mut drun, mut dpi) = (0u64, 0u64, 0u64);
        for (pid, s) in &cur {
            let prev = self.last.get(pid).copied().unwrap_or_default();
            dcpu += s.cpu_ns.saturating_sub(prev.cpu_ns);
            drun += s.runnable_ns.saturating_sub(prev.runnable_ns);
            dpi += s.pageins.saturating_sub(prev.pageins);
        }
        self.last = cur;
        self.peak_mem_kb = self.peak_mem_kb.max(mem);
        self.cpu_ns += dcpu;
        self.runnable_ns += drun;
        self.pageins += dpi;
        self.samples += 1;
        let first = *self.first_ts.get_or_insert(ts);
        let prev_ts = self.last_ts.replace(ts);
        // The first reading covers everything since the process started, which
        // is at most one sample interval, so it is counted over that span.
        let dt = match prev_ts {
            Some(p) => (ts - p).max(1e-3),
            None => (ts - first).max(1e-3).max(0.5),
        };
        let step = Step { dt, cpu: dcpu as f64 / 1e9, run: drun as f64 / 1e9, pageins: dpi as f64 };
        self.window.push_back(step);
        let mut span: f64 = self.window.iter().map(|s| s.dt).sum();
        while span - self.window.front().map(|s| s.dt).unwrap_or(0.0) >= WINDOW_S {
            span -= self.window.pop_front().map(|s| s.dt).unwrap_or(0.0);
        }
        let (c, r, p) = self.window.iter().fold((0.0, 0.0, 0.0), |a, s| (a.0 + s.cpu, a.1 + s.run, a.2 + s.pageins));
        if span >= WINDOW_S {
            self.max_used = self.max_used.max(c / span);
            self.max_wanted = self.max_wanted.max((c + r) / span);
            self.max_pageins_per_s = self.max_pageins_per_s.max(p / span);
        }
        Some(Reading {
            used: step.cpu / step.dt,
            wanted: (step.cpu + step.run) / step.dt,
            mem_kb: mem,
            pageins_per_s: step.pageins / step.dt,
        })
    }

    /// Sustained cores used and wanted. A run shorter than the window is
    /// averaged over its whole length.
    ///
    /// The whole-run average is also a floor: work done in processes that
    /// live shorter than one sample interval is missed by the samples, but
    /// the kernel's totals at exit include it.
    pub fn sustained(&self, wall: f64) -> (f64, f64) {
        let w = wall.max(1e-3);
        let avg_used = self.cpu_ns as f64 / 1e9 / w;
        let avg_wanted = (self.cpu_ns + self.runnable_ns) as f64 / 1e9 / w;
        (self.max_used.max(avg_used), self.max_wanted.max(avg_wanted).max(avg_used))
    }

    pub fn pageins_per_s(&self, wall: f64) -> f64 {
        if self.max_pageins_per_s > 0.0 { self.max_pageins_per_s } else { self.pageins as f64 / wall.max(1e-3) }
    }
}

// ------------------------------------------------------------ starvation ----

#[derive(Debug, Clone, PartialEq)]
pub struct Starvation {
    pub kind: &'static str,
    pub evidence: Vec<String>,
}

pub struct RunFacts {
    pub wall: f64,
    pub cpu_s: f64,
    pub runnable_s: f64,
    pub used: f64,
    pub wanted: f64,
    pub pageins_per_s: f64,
    pub pressure_frac: f64,
    pub full_frac: f64,
    pub median_dur: Option<f64>,
}

pub const CPU_WAIT_RATIO: f64 = 0.5;
pub const PAGEINS_PER_S: f64 = 200.0;
pub const PRESSURE_FRAC: f64 = 0.5;
pub const SLOWDOWN: f64 = 1.5;

/// Was this run held back? CPU: its threads waited on a core more than half
/// as long as they ran. Memory: it paged in heavily, or the machine was under
/// memory pressure for most of it. Slowdown: it took 1.5x its usual time while
/// the machine was full for most of it.
pub fn starvation(f: &RunFacts) -> Option<Starvation> {
    let mut found: Vec<(&'static str, String)> = Vec::new();
    if f.cpu_s >= 1.0 && f.runnable_s > CPU_WAIT_RATIO * f.cpu_s {
        found.push((
            "cpu",
            format!(
                "waited on CPU {:.0}% of its run time (wanted {:.1} cores, got {:.1})",
                f.runnable_s * 100.0 / (f.cpu_s + f.runnable_s),
                f.wanted,
                f.used
            ),
        ));
    }
    if f.pageins_per_s > PAGEINS_PER_S {
        found.push(("memory", format!("paged in {:.0} pages/s, so it ran short of memory", f.pageins_per_s)));
    } else if f.pressure_frac > PRESSURE_FRAC {
        found.push(("memory", format!("the machine was under memory pressure {:.0}% of the run", f.pressure_frac * 100.0)));
    }
    if let Some(m) = f.median_dur
        && f.wall > SLOWDOWN * m
        && f.full_frac > 0.5
    {
        found.push((
            "slowdown",
            format!("took {:.1}x its usual time while the machine was full {:.0}% of the run", f.wall / m, f.full_frac * 100.0),
        ));
    }
    let kind = found.first()?.0;
    Some(Starvation { kind, evidence: found.into_iter().map(|(_, e)| e).collect() })
}

// ---------------------------------------------------------------- advice ----

pub struct AdviceInput<'a> {
    pub argv: &'a [String],
    pub effective: &'a [String],
    pub starved: &'a Starvation,
    pub script: Option<&'a str>,
    pub package_json: Option<&'a str>,
    pub cwd: &'a str,
    pub cpu_before: Option<f64>,
    pub cpu_after: Option<f64>,
    pub mem_before: Option<u64>,
    pub mem_after: Option<u64>,
    pub got_cores: f64,
    pub wanted_cores: f64,
    pub peak_mem_kb: u64,
    pub others: Vec<String>,
    pub streak: usize,
}

fn tool_of(effective: &[String]) -> String {
    effective.first().map(|c| c.rsplit('/').next().unwrap_or(c).to_string()).unwrap_or_default()
}

fn node_based(tool: &str, argv: &[String]) -> bool {
    matches!(tool, "vitest" | "jest" | "hardhat" | "webpack" | "next" | "tsc" | "mocha" | "stryker" | "hardhat-runtime.ts")
        || argv.first().is_some_and(|a| a.ends_with("node"))
}

/// The advice lines after a starved run: what taskguard already changed, how
/// to pin a minimum, how to make the tool itself fit, and who took the room.
pub fn advice(a: &AdviceInput) -> Vec<String> {
    let mut out = Vec::new();
    match (a.starved.kind, a.cpu_before, a.cpu_after, a.mem_before, a.mem_after) {
        ("cpu" | "slowdown", b, Some(n), _, _) => out.push(match b {
            Some(b) if n <= b + 0.05 => format!("already done: the next run keeps reserving {b:.1} cores"),
            Some(b) => format!("already done: the next run reserves {n:.1} cores (was {b:.1})"),
            None => format!("already done: the next run reserves {n:.1} cores"),
        }),
        ("memory", _, _, b, Some(n)) => out.push(match b {
            Some(b) => format!("already done: the next run reserves {} of memory (was {})", gb(n), gb(b)),
            None => format!("already done: the next run reserves {} of memory", gb(n)),
        }),
        _ => {}
    }
    let min_flag = match a.starved.kind {
        "memory" => format!("--min-mem {}", size_flag(((a.peak_mem_kb as f64) * 1.25) as u64)),
        _ => format!("--min-cpu {}", a.wanted_cores.ceil().max(1.0) as u64),
    };
    let cmd = a.argv.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ");
    let place = match (a.script, a.package_json) {
        (Some(s), Some(p)) => format!("script \"{s}\" in {p}"),
        _ => format!("the command in {}", a.cwd),
    };
    let pin = format!("to pin it: {place} -> \"taskguard {min_flag} {cmd}\"");
    out.push(if a.streak >= 3 { format!("{pin} (starved {} runs in a row)", a.streak) } else { pin });

    let tool = tool_of(a.effective);
    let n = a.got_cores.floor().max(1.0) as u64;
    let tool_tip = match (a.starved.kind, tool.as_str()) {
        ("cpu" | "slowdown", "vitest") => Some(format!("or let vitest fit the room it gets: add --maxWorkers={n}")),
        ("cpu" | "slowdown", "jest") => Some(format!("or let jest fit the room it gets: add --maxWorkers={n}")),
        ("cpu" | "slowdown", "playwright") => Some(format!("or let playwright fit the room it gets: add --workers={n}")),
        ("cpu" | "slowdown", "turbo") => Some(format!("or let turbo fit the room it gets: add --concurrency={n}")),
        ("memory", t) if node_based(t, a.argv) => Some(format!(
            "or give node more heap: NODE_OPTIONS=--max-old-space-size={}",
            (a.peak_mem_kb as f64 * 1.25 / 1024.0).ceil() as u64
        )),
        _ => None,
    };
    out.extend(tool_tip);
    if !a.others.is_empty() {
        out.push(format!("the room went to: {}", a.others.join("; ")));
    }
    out
}

/// Quote a word for a shell command line when it needs it.
pub fn shell_quote(w: &str) -> String {
    let plain = !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%^".contains(c));
    if plain { w.to_string() } else { format!("'{}'", w.replace('\'', "'\\''")) }
}

fn size_flag(kb: u64) -> String {
    let g = kb as f64 / 1024.0 / 1024.0;
    if g >= 1.0 { format!("{}G", g.ceil() as u64) } else { format!("{}M", (kb / 1024).max(1)) }
}

// ---------------------------------------------------------------- trends ----

/// Percent change of the median of the last `n` values against the `n`
/// before them. `values` is oldest first. None when there is not enough history.
pub fn trend(values: &[f64], n: usize) -> Option<(f64, f64, f64)> {
    if values.len() < n + 3 {
        return None;
    }
    let recent = &values[values.len() - n.min(values.len())..];
    let before_end = values.len() - recent.len();
    let before = &values[before_end.saturating_sub(n)..before_end];
    let a = crate::db::median_of(before.to_vec())?;
    let b = crate::db::median_of(recent.to_vec())?;
    if a <= 0.0 {
        return None;
    }
    Some(((b - a) * 100.0 / a, a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(cpu_s: f64, run_s: f64, mem: u64) -> HashMap<i32, ProcSample> {
        HashMap::from([(1, ProcSample { cpu_ns: (cpu_s * 1e9) as u64, runnable_ns: (run_s * 1e9) as u64, footprint_kb: mem, pageins: 0 })])
    }

    #[test]
    fn sustained_ignores_a_spike() {
        let mut t = Tracker::default();
        // 2 cores used steadily for 20 s, then one 2-second spike at 8 cores.
        let mut cpu = 0.0;
        for i in 0..=10 {
            t.add(at(cpu, 0.0, 100), 1000.0 + i as f64 * 2.0);
            cpu += 4.0;
        }
        cpu += 12.0;
        t.add(at(cpu, 0.0, 300), 1022.0);
        let (used, wanted) = t.sustained(22.0);
        assert!(used < 3.5, "a spike is averaged out: {used}");
        assert!(used >= 2.0);
        assert_eq!(used, wanted);
        assert_eq!(t.peak_mem_kb, 300);
    }

    #[test]
    fn wanted_includes_waiting_for_a_core() {
        let mut t = Tracker::default();
        for i in 0..=6 {
            let s = i as f64 * 2.0;
            t.add(at(s * 1.0, s * 2.0, 100), 1000.0 + s);
        }
        let (used, wanted) = t.sustained(12.0);
        assert!((used - 1.0).abs() < 0.05, "{used}");
        assert!((wanted - 3.0).abs() < 0.05, "{wanted}");
    }

    #[test]
    fn exited_children_do_not_go_backwards() {
        let mut t = Tracker::default();
        let mut two = at(1.0, 0.0, 10);
        two.insert(2, ProcSample { cpu_ns: 5_000_000_000, ..Default::default() });
        t.add(two, 1000.0);
        t.add(at(2.0, 0.0, 10), 1002.0);
        assert_eq!(t.cpu_ns, 7_000_000_000);
    }

    fn facts() -> RunFacts {
        RunFacts {
            wall: 100.0,
            cpu_s: 200.0,
            runnable_s: 10.0,
            used: 2.0,
            wanted: 2.1,
            pageins_per_s: 0.0,
            pressure_frac: 0.0,
            full_frac: 0.0,
            median_dur: Some(100.0),
        }
    }

    #[test]
    fn starvation_thresholds() {
        assert_eq!(starvation(&facts()), None);
        let s = starvation(&RunFacts { runnable_s: 150.0, ..facts() }).unwrap();
        assert_eq!(s.kind, "cpu");
        assert!(s.evidence[0].contains("waited on CPU 43%"), "{:?}", s.evidence);
        assert_eq!(starvation(&RunFacts { pageins_per_s: 500.0, ..facts() }).unwrap().kind, "memory");
        assert_eq!(starvation(&RunFacts { wall: 200.0, full_frac: 0.8, ..facts() }).unwrap().kind, "slowdown");
        assert_eq!(starvation(&RunFacts { wall: 200.0, full_frac: 0.1, ..facts() }), None, "slow but the machine had room");
    }

    fn argv(s: &str) -> Vec<String> {
        crate::matcher::split_shell(s)
    }

    #[test]
    fn advice_names_the_script_and_the_tool_flag() {
        let a = argv("bun --bun vitest run tests/unit");
        let eff = crate::matcher::effective(&a);
        let st = Starvation { kind: "cpu", evidence: vec![] };
        let lines = advice(&AdviceInput {
            argv: &a,
            effective: &eff,
            starved: &st,
            script: Some("test"),
            package_json: Some("packages/api/package.json"),
            cwd: "/r/packages/api",
            cpu_before: Some(3.0),
            cpu_after: Some(6.1),
            mem_before: None,
            mem_after: None,
            got_cores: 2.2,
            wanted_cores: 6.1,
            peak_mem_kb: 0,
            others: vec![],
            streak: 1,
        });
        assert_eq!(
            lines,
            vec![
                "already done: the next run reserves 6.1 cores (was 3.0)".to_string(),
                "to pin it: script \"test\" in packages/api/package.json -> \"taskguard --min-cpu 7 bun --bun vitest run tests/unit\""
                    .to_string(),
                "or let vitest fit the room it gets: add --maxWorkers=2".to_string(),
            ]
        );
    }

    #[test]
    fn advice_per_tool() {
        let cases = [
            ("jest", "cpu", Some("--maxWorkers=2")),
            ("bunx playwright test", "cpu", Some("--workers=2")),
            ("turbo run build", "cpu", Some("--concurrency=2")),
            ("tsgo -p .", "cpu", None),
            ("hardhat compile", "memory", Some("--max-old-space-size=")),
            ("tsgo -p .", "memory", None),
        ];
        for (cmd, kind, want) in cases {
            let a = argv(cmd);
            let eff = crate::matcher::effective(&a);
            let st = Starvation { kind, evidence: vec![] };
            let lines = advice(&AdviceInput {
                argv: &a,
                effective: &eff,
                starved: &st,
                script: None,
                package_json: None,
                cwd: "/r",
                cpu_before: None,
                cpu_after: None,
                mem_before: None,
                mem_after: None,
                got_cores: 2.7,
                wanted_cores: 4.0,
                peak_mem_kb: 4 * 1024 * 1024,
                others: vec![],
                streak: 0,
            });
            let tip = lines.iter().find(|l| l.starts_with("or "));
            match want {
                Some(w) => assert!(tip.is_some_and(|t| t.contains(w)), "{cmd}: {lines:?}"),
                None => assert!(tip.is_none(), "{cmd}: {lines:?}"),
            }
        }
    }

    #[test]
    fn trends() {
        let v: Vec<f64> = (0..10).map(|_| 10.0).chain((0..10).map(|_| 15.0)).collect();
        let (pct, a, b) = trend(&v, 10).unwrap();
        assert_eq!((pct.round(), a, b), (50.0, 10.0, 15.0));
        assert_eq!(trend(&[1.0, 2.0], 10), None);
    }
}
