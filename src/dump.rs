//! `taskguard dump`: everything taskguard knows about a job, as one JSON
//! document, to find out later why it reserved what it did and to tune the
//! rules from real histories.

use crate::config::{self, Config};
use crate::db::{self, Db};
use crate::key;
use crate::machine;
use crate::matcher;
use crate::need;
use crate::queue::Queue;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The options of `dump`.
#[derive(Debug, Default, PartialEq)]
struct Args {
    pattern: Option<String>,
    cmd: Option<String>,
    run: Option<i64>,
    runs: usize,
}

fn parse(args: &[String]) -> Result<Args> {
    let mut a = Args { runs: 20, ..Default::default() };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if arg.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg, None),
        };
        let mut value = || -> Result<String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match name {
            "--cmd" => a.cmd = Some(value()?),
            "--run" => a.run = Some(value()?.parse().map_err(|_| anyhow::anyhow!("--run needs a run id, a number"))?),
            "--runs" => a.runs = value()?.parse().map_err(|_| anyhow::anyhow!("--runs needs a number"))?,
            _ if arg.starts_with('-') => bail!("unknown option {arg}"),
            _ if a.pattern.is_none() => a.pattern = Some(arg.to_string()),
            _ => bail!("one key pattern only; quote it so the shell leaves the * alone"),
        }
        i += 1;
    }
    if [a.pattern.is_some(), a.cmd.is_some(), a.run.is_some()].iter().filter(|x| **x).count() != 1 {
        bail!("name the job one way: a key PATTERN, --cmd \"COMMAND\", or --run ID");
    }
    Ok(a)
}

/// `taskguard dump PATTERN | --cmd "COMMAND" | --run ID [--runs N]`
pub fn dump(args: &[String]) -> Result<i32> {
    let a = parse(args)?;
    let dir = config::state_dir();
    let db = Db::open_dir(&dir)?;
    let here = std::env::current_dir()?;
    let mut jobs = Vec::new();
    if let Some(cmdline) = &a.cmd {
        let argv = matcher::split_shell(cmdline);
        let k = key::run_key(&here, &argv);
        jobs.push(job(&db, &dir, &k, Some((&argv, &here)), &a)?);
    } else {
        let keys = match (a.run, &a.pattern) {
            (Some(id), _) => match db.key_of_run(id)? {
                Some(k) => vec![k],
                None => bail!("no run {id} in the history"),
            },
            (None, Some(p)) => {
                let all = db.keys(Some(p))?;
                if all.is_empty() {
                    bail!("no key matches {p:?}; taskguard history lists them");
                }
                all
            }
            _ => unreachable!(),
        };
        for k in keys {
            jobs.push(job(&db, &dir, &k, None, &a)?);
        }
    }
    let m = machine::measure(None, 0);
    let out = json!({
        "taskguard": env!("CARGO_PKG_VERSION"),
        "generated_at": db::now(),
        "state_dir": dir,
        "machine": m,
        "focus_run": a.run,
        "jobs": jobs,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(0)
}

/// One job: how it is matched, what it learned, what its next run reserves
/// and why, its recent runs with what each did to the need, and its live
/// queue entries. `given` is the command and directory when the user named
/// one; otherwise both come from the newest run.
fn job(db: &Db, dir: &Path, k: &str, given: Option<(&Vec<String>, &PathBuf)>, a: &Args) -> Result<Value> {
    let mut rows = db.runs_json(k, a.runs)?;
    let newest = rows.first().cloned();
    let newest = newest.as_ref();
    let text = |r: Option<&Value>, f: &str| r.and_then(|r| r[f].as_str()).map(str::to_string);
    // The settings of the directory the job runs in, when it still exists.
    let (argv, cwd) = match given {
        Some((argv, cwd)) => (argv.clone(), cwd.clone()),
        None => (
            text(newest, "cmd").map(|c| matcher::split_shell(&c)).unwrap_or_default(),
            text(newest, "cwd").map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or(std::env::current_dir()?),
        ),
    };
    let cfg = Config::load(&cwd, &key::checkout_root(&cwd))?;
    let cls = cfg.classify(&argv, k)?;
    let rules = cfg.learn();
    // As the runner does before it learns: see `Db::fill_startup_peaks`.
    if let Some(s) = cfg.long_lived(&cls, cls.pool.as_ref().map(|p| p.0.as_str()))
        && db.fill_startup_peaks(k, s)? > 0
    {
        rows = db.runs_json(k, a.runs)?;
    }
    let (learned, history_key) =
        if given.is_some() { need::history(db, &cfg, &cwd, &argv, k) } else { (db.learned(k, &rules)?, k.to_string()) };
    let pool = cls.pool.as_ref().map(|p| p.0.clone());
    let guess = (learned.runs == 0).then(|| need::guess(Some(db), &cfg, None, &argv, cls.label.as_deref(), pool.as_deref()));
    // A --min-cpu or --min-mem flag is not in the config: the newest run
    // recorded what it was given.
    let num = |f: &str| newest.and_then(|r| r[f].as_f64());
    let min_cpu = cls.min_cpu.or(num("min_cpu"));
    let min_mem = cls.min_mem_kb.or(num("min_mem_kb").map(|v| v as u64));
    let boost_on = cfg.boost(&cls, pool.as_deref());
    let next = need::derive(&need::inputs(&cfg, &learned, Some(&history_key), guess.as_ref(), min_cpu, min_mem, boost_on));

    // What each run did to the need, in the order `Db::learned` reads them.
    let outliers: Vec<i64> = learned.outliers.iter().flat_map(|o| o.runs.iter().map(|r| r.run_id)).collect();
    let ignored: Vec<i64> = learned.failed.iter().map(|r| r.run_id).collect();
    let mut seen = 0;
    let mut runs = Vec::new();
    for mut r in rows {
        let id = r["id"].as_i64().unwrap_or(0);
        // What `Db::learned` counts: a long-lived job's start-up peak.
        let peak = r["startup_mem_kb"].as_i64().or(r["peak_mem_kb"].as_i64()).unwrap_or(0);
        let exit = r["exit"].as_i64();
        let failed = exit.is_some_and(|e| e != 0);
        let finished = !r["ended_at"].is_null();
        let counted = finished && peak > 0;
        let place = counted.then(|| {
            seen += 1;
            seen - 1
        });
        let in_window = place.is_some_and(|p| p < rules.keep);
        let boost = boost_on
            && place.is_some_and(|p| p < rules.boost_runs)
            && db::boosts(r["starved"].as_str(), r["need_mem_kb"].as_i64(), peak, failed);
        let mut notes = Vec::new();
        if !finished {
            notes.push("still waiting or running".to_string());
        } else if !counted {
            notes.push("no memory reading: not learned from".into());
        } else if !in_window {
            notes.push(format!("older than the last {} runs: no longer counts", rules.keep));
        }
        if outliers.contains(&id) {
            notes.push("an outlier: counts by outlier_weights".into());
        }
        if ignored.contains(&id) {
            notes.push("failed, and its peak is ignored".into());
        } else if failed && in_window {
            notes.push("failed: its peak counts only up to what passing runs took".into());
        }
        if boost {
            notes.push("starved of memory at its need: adds the 25% boost".into());
        }
        r["role"] = json!({
            "in_window": in_window,
            "failed": failed,
            "outlier": outliers.contains(&id),
            "failed_ignored": ignored.contains(&id),
            "boost": boost,
            "notes": notes,
        });
        r["wait_spans"] = db.wait_spans_json(id)?.into();
        r["adjustments"] = db.adjustments_json(id)?.into();
        runs.push(r);
    }

    let q = Queue::open(dir)?;
    let live: Vec<Value> =
        q.running().into_iter().chain(q.waiting()).filter(|e| e.key == k).map(|e| serde_json::to_value(e).unwrap_or_default()).collect();
    let origin: serde_json::Map<String, Value> =
        cfg.origin.iter().filter(|(_, v)| *v != "built-in").map(|(k, v)| (k.clone(), Value::from(v.as_str()))).collect();

    Ok(json!({
        "key": k,
        "history_key": history_key,
        "match": {
            "cmd": argv.join(" "),
            "cwd": cwd,
            "namespace": key::namespace(&cwd),
            "passthrough": cls.passthrough,
            "now": cls.now,
            "pool": cls.pool.as_ref().map(|(n, s, pc)| json!({"name": n, "max_slots": s, "per_checkout": pc})),
            "label": cls.label,
            "min_cpu": min_cpu,
            "min_mem_kb": min_mem,
            "priority": cls.priority,
            "long_lived_startup_s": cfg.long_lived(&cls, pool.as_deref()),
            "why": cls.why,
        },
        "config": {
            "layers": cfg.layers,
            "set_by": origin,
            "rules": {
                "keep": rules.keep,
                "boost_runs": rules.boost_runs,
                "boost": boost_on,
                "outlier_ratio": rules.outlier_ratio,
                "outlier_min_kb": rules.outlier_min_kb,
                "outlier_weights": rules.outlier_weights,
                "new_job_mem_kb": rules.new_job_mem_kb,
                "cpu_max": cfg.cpu_max,
                "mem_max": cfg.mem_max,
                "noise_mem": cfg.noise_mem,
                "noise_cpu": cfg.noise_cpu,
                "partial_fit": cfg.partial_fit,
                "max_backfill": cfg.max_backfill,
            },
        },
        "learned": learned,
        "next_need": next,
        "runs": runs,
        "pruned": db.pruned_json(k)?,
        "live": live,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn the_job_is_named_one_way() {
        assert_eq!(parse(&args("'k*'")).unwrap().pattern.as_deref(), Some("'k*'"));
        let a = parse(&args("--run 42 --runs=5")).unwrap();
        assert_eq!((a.run, a.runs), (Some(42), 5));
        assert!(parse(&args("")).is_err());
        assert!(parse(&args("k --run 1")).is_err());
        assert!(parse(&args("--run x")).is_err());
    }
}
