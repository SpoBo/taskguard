//! The Queue view as a tree: the jobs of one task-runner run under one
//! header, and the taskguard calls that run inside a job under that job.
//!
//! Jobs of one `turbo run` used to show as loose rows, and since a wrapper
//! such as `bun run ci:local` runs straight through, nothing showed that one
//! command started them. Calls nested inside a job (a dev stack's start-up
//! builds) had no row at all: they run under the job's claim and leave only
//! a note in `nested/`.

use crate::queue::{Entry, Nested, Queue};
use crate::sys;
use std::collections::{HashMap, HashSet};

/// One row of the Queue view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Row {
    /// The header of a task-runner run: an index into `Tree::groups`.
    Group(usize),
    /// A running (`waiting` false) or waiting job: an index into the
    /// snapshot's running or waiting list. `depth` 1 is inside a group.
    Job { waiting: bool, i: usize, depth: u8 },
    /// A call nested inside the running job `outer` (an index into the
    /// running list): an index into that job's subtasks.
    Sub { outer: usize, i: usize, depth: u8 },
}

/// A task-runner run with more than one job in the queue.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The run's id, as "turbo:4242".
    pub id: String,
    /// The command that started it, or the id when it cannot be read.
    pub label: String,
    pub running: usize,
    pub waiting: usize,
    /// What its running jobs use now, and the memory they need.
    pub cpu: f64,
    pub mem_kb: u64,
    pub need_mem_kb: u64,
    /// When the run queued its first job.
    pub since: f64,
    pub folded: bool,
}

/// A taskguard call running inside a job, measured from its process tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Sub {
    pub key: String,
    pub pid: i32,
    pub since: f64,
    pub cpu: f64,
    pub mem_kb: u64,
}

#[derive(Debug, Default)]
pub struct Tree {
    pub rows: Vec<Row>,
    pub groups: Vec<Group>,
    /// The tree lines in front of each row's name: "├ ", "└ ", and "│ " for
    /// a row above that still has rows below it.
    pub lead: Vec<String>,
}

impl Row {
    pub fn depth(&self) -> u8 {
        match self {
            Row::Group(_) => 0,
            Row::Job { depth, .. } | Row::Sub { depth, .. } => *depth,
        }
    }
}

/// The tree lines for rows at these depths. A row is the last of its
/// parent's when no later row at its depth comes before one above it.
fn leads(depths: &[u8]) -> Vec<String> {
    let last = |r: usize| depths[r + 1..].iter().find(|&&d| d <= depths[r]).is_none_or(|&d| d < depths[r]);
    // For each depth: whether the row that opened it was the last one.
    let mut open: Vec<bool> = Vec::new();
    (0..depths.len())
        .map(|r| {
            let d = depths[r] as usize;
            open.truncate(d);
            let mut lead = String::new();
            if d > 0 {
                for &was_last in &open[1..] {
                    lead.push_str(if was_last { "  " } else { "│ " });
                }
                lead.push_str(if last(r) { "└ " } else { "├ " });
            }
            open.resize(d, false);
            open.push(last(r));
            lead
        })
        .collect()
}

/// What folds a row: a group by its run id (`Group::id`), a job by its pid.
pub fn fold_id_of_job(e: &Entry) -> String {
    format!("pid:{}", e.pid)
}

/// Lay out the rows. Jobs keep their order: running first, then waiting in
/// queue order. A run with two jobs or more becomes a group where its first
/// job would be, with its running and then its waiting jobs below it. Calls
/// nested in a running job follow that job.
pub fn build(
    running: &[Entry],
    waiting: &[&Entry],
    subs: &HashMap<i32, Vec<Sub>>,
    labels: &HashMap<String, String>,
    folded: &HashSet<String>,
) -> Tree {
    let mut count: HashMap<&str, usize> = HashMap::new();
    for e in running.iter().chain(waiting.iter().copied()) {
        if let Some(p) = &e.pipeline {
            *count.entry(p).or_default() += 1;
        }
    }

    let mut tree = Tree::default();
    let job = |tree: &mut Tree, waiting: bool, i: usize, depth: u8| {
        tree.rows.push(Row::Job { waiting, i, depth });
        if waiting {
            return;
        }
        let e = &running[i];
        if folded.contains(&fold_id_of_job(e)) {
            return;
        }
        for k in 0..subs.get(&e.pid).map(Vec::len).unwrap_or(0) {
            tree.rows.push(Row::Sub { outer: i, i: k, depth: depth + 1 });
        }
    };
    let mut done: HashSet<&str> = HashSet::new();
    // Running jobs come in directory order, which may change between two
    // loads; the oldest first keeps the rows in place.
    let mut by_start: Vec<usize> = (0..running.len()).collect();
    by_start.sort_by(|&a, &b| {
        let start = |i: usize| running[i].started_at.unwrap_or(f64::MAX);
        start(a).total_cmp(&start(b)).then(running[a].pid.cmp(&running[b].pid))
    });
    let order = by_start.iter().map(|&i| (false, i)).chain((0..waiting.len()).map(|i| (true, i)));
    for (is_waiting, i) in order {
        let e = if is_waiting { waiting[i] } else { &running[i] };
        let Some(p) = e.pipeline.as_deref().filter(|p| count.get(p).copied().unwrap_or(0) >= 2) else {
            job(&mut tree, is_waiting, i, 0);
            continue;
        };
        if !done.insert(p) {
            continue;
        }
        let mine_running: Vec<usize> = by_start.iter().copied().filter(|&j| running[j].pipeline.as_deref() == Some(p)).collect();
        let mine_waiting: Vec<usize> = (0..waiting.len()).filter(|&j| waiting[j].pipeline.as_deref() == Some(p)).collect();
        let all = mine_running.iter().map(|&j| &running[j]).chain(mine_waiting.iter().map(|&j| waiting[j]));
        let since = all.filter_map(|e| e.pipeline_since).fold(f64::MAX, f64::min);
        let g = Group {
            id: p.to_string(),
            label: labels.get(p).cloned().unwrap_or_else(|| p.to_string()),
            running: mine_running.len(),
            waiting: mine_waiting.len(),
            cpu: mine_running.iter().map(|&j| running[j].live_cpu).sum(),
            mem_kb: mine_running.iter().map(|&j| running[j].live_mem_kb).sum(),
            need_mem_kb: mine_running.iter().map(|&j| running[j].need_mem_kb).sum(),
            since: if since == f64::MAX { e.queued_at } else { since },
            folded: folded.contains(p),
        };
        let folded = g.folded;
        tree.rows.push(Row::Group(tree.groups.len()));
        tree.groups.push(g);
        if folded {
            continue;
        }
        for j in mine_running {
            job(&mut tree, false, j, 1);
        }
        for j in mine_waiting {
            job(&mut tree, true, j, 1);
        }
    }
    tree.lead = leads(&tree.rows.iter().map(Row::depth).collect::<Vec<_>>());
    tree
}

/// The command a task-runner run stands for, as people typed it: the first
/// package script above the runner (`bun run ci:local` around `turbo run
/// ...`), else the runner's own command line. A `node .../turbo` shim
/// between them is skipped.
pub fn label(id: &str, procs: &[sys::ProcInfo]) -> Option<String> {
    let pid: i32 = id.split_once(':')?.1.parse().ok()?;
    let ups = std::iter::once(pid).chain(sys::ancestors(pid, procs).into_iter().skip(1)).take(6);
    let script = ups.filter(|&p| p > 1).filter_map(sys::proc_args).find(|a| is_script_run(a));
    let args = script.or_else(|| sys::proc_args(pid))?;
    Some(command_text(&args))
}

/// `bun run ci:local`, `npm run test`, `pnpm run lint`, `yarn run x`.
fn is_script_run(args: &[String]) -> bool {
    let tool = |a: &String| a.rsplit('/').next().unwrap_or(a).to_string();
    args.len() >= 3 && matches!(tool(&args[0]).as_str(), "bun" | "npm" | "pnpm" | "yarn") && args[1] == "run"
}

/// A command line with the program's directory left out.
fn command_text(args: &[String]) -> String {
    let mut words = args.iter();
    let first = words.next().map(|a| a.rsplit('/').next().unwrap_or(a)).unwrap_or_default();
    std::iter::once(first).chain(words.map(String::as_str)).collect::<Vec<_>>().join(" ")
}

/// The calls nested in each running job, by the job's pid, measured now.
/// `prev` holds each call's CPU time at the last reading, for its cores.
pub fn subtasks(
    q: &Queue,
    running: &[Entry],
    procs: &[sys::ProcInfo],
    now: f64,
    prev: &mut HashMap<i32, (f64, u64)>,
) -> HashMap<i32, Vec<Sub>> {
    let outers: HashSet<i32> = running.iter().map(|e| e.pid).collect();
    let children = sys::children_map(procs);
    let mut out: HashMap<i32, Vec<Sub>> = HashMap::new();
    let mut seen = HashMap::new();
    let mut notes: Vec<Nested> = q.nested().into_iter().filter(|n| outers.contains(&n.outer)).collect();
    notes.sort_by(|a, b| a.since.total_cmp(&b.since));
    for n in notes {
        // A call that has ended leaves its note until its job ends.
        let tree = sys::descendants(n.pid, &children);
        let samples: Vec<sys::ProcSample> = tree.iter().filter_map(|&p| sys::proc_sample(p)).collect();
        if samples.is_empty() {
            continue;
        }
        let cpu_ns: u64 = samples.iter().map(|s| s.cpu_ns).sum();
        let cpu = match prev.get(&n.pid) {
            Some((t, ns)) if now > *t => cpu_ns.saturating_sub(*ns) as f64 / 1e9 / (now - t),
            _ => 0.0,
        };
        seen.insert(n.pid, (now, cpu_ns));
        let key = if n.key.is_empty() { sys::proc_name(n.pid) } else { n.key };
        out.entry(n.outer).or_default().push(Sub {
            key,
            pid: n.pid,
            since: n.since,
            cpu,
            mem_kb: samples.iter().map(|s| s.footprint_kb).sum(),
        });
    }
    *prev = seen;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(pid: i32, pipeline: Option<&str>) -> Entry {
        Entry {
            pid,
            key: format!("j{pid}"),
            pipeline: pipeline.map(str::to_string),
            pipeline_since: pipeline.map(|_| 100.0),
            ..Default::default()
        }
    }

    fn sub(key: &str) -> Sub {
        Sub { key: key.into(), pid: 0, since: 0.0, cpu: 0.0, mem_kb: 0 }
    }

    #[test]
    fn a_run_with_two_jobs_or_more_becomes_one_group() {
        let running =
            [job(1, Some("turbo:9")), job(2, None), job(3, Some("turbo:9"))].map(|e| Entry { started_at: Some(e.pid as f64), ..e });
        let w = [job(4, Some("turbo:9")), job(5, Some("turbo:7"))];
        let waiting: Vec<&Entry> = w.iter().collect();
        let labels = HashMap::from([("turbo:9".to_string(), "bun run ci:local".to_string())]);
        let t = build(&running, &waiting, &HashMap::new(), &labels, &HashSet::new());
        assert_eq!(
            t.rows,
            vec![
                Row::Group(0),
                Row::Job { waiting: false, i: 0, depth: 1 },
                Row::Job { waiting: false, i: 2, depth: 1 },
                Row::Job { waiting: true, i: 0, depth: 1 },
                Row::Job { waiting: false, i: 1, depth: 0 },
                // A run with one job in the queue stays a plain row.
                Row::Job { waiting: true, i: 1, depth: 0 },
            ]
        );
        let g = &t.groups[0];
        assert_eq!((g.label.as_str(), g.running, g.waiting, g.since), ("bun run ci:local", 2, 1, 100.0));
    }

    #[test]
    fn rows_get_tree_lines() {
        let l = leads(&[0, 1, 2, 2, 1, 2, 0, 1]);
        assert_eq!(l, ["", "├ ", "│ ├ ", "│ └ ", "└ ", "  └ ", "", "└ "]);
    }

    #[test]
    fn a_run_is_named_by_the_script_above_it() {
        let a = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert!(is_script_run(&a("/opt/bin/bun run ci:local")));
        assert!(!is_script_run(&a("node /w/node_modules/turbo/bin/turbo run lint")), "the shim is skipped");
        assert_eq!(command_text(&a("/opt/bin/bun run ci:local")), "bun run ci:local");
    }

    #[test]
    fn nested_calls_follow_their_job_and_rows_fold() {
        let running = [job(1, None), job(2, Some("turbo:9")), job(3, Some("turbo:9"))];
        let subs = HashMap::from([(1, vec![sub("dalp:prepare"), sub("dalp:sdk")])]);
        let t = build(&running, &[], &subs, &HashMap::new(), &HashSet::new());
        assert_eq!(
            t.rows[..3],
            [Row::Job { waiting: false, i: 0, depth: 0 }, Row::Sub { outer: 0, i: 0, depth: 1 }, Row::Sub { outer: 0, i: 1, depth: 1 }]
        );
        assert_eq!(t.groups[0].label, "turbo:9", "without a label, the run's id");

        let folded = HashSet::from(["pid:1".to_string(), "turbo:9".to_string()]);
        let t = build(&running, &[], &subs, &HashMap::new(), &folded);
        assert_eq!(t.rows, vec![Row::Job { waiting: false, i: 0, depth: 0 }, Row::Group(0)]);
        assert!(t.groups[0].folded);
    }
}
