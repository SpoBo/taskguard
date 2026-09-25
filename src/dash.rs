//! Queries behind the dashboard and the warnings. Views stay thin: they only
//! draw what these functions return.

use crate::db::Db;
use crate::insight;
use crate::report::{dur, gb};
use anyhow::Result;
use rusqlite::params;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bucket {
    pub t: f64,
    pub cpu: f64,
    pub mem_kb: f64,
    pub has_data: bool,
}

/// Raw samples cover the last 48 hours; longer ranges read the one-minute
/// rollups.
fn use_raw(from: f64) -> bool {
    crate::db::now() - from <= 48.0 * 3600.0
}

/// Spread one sample over the buckets its interval overlaps. A sample taken at
/// `t` stands for the `span` seconds before it; without this, buckets narrower
/// than the sample interval would alternate between empty and full.
fn spread(t: f64, span: f64, from: f64, w: f64, n: usize, mut add: impl FnMut(usize, f64)) {
    let (a, b) = (t - span, t);
    let first = (((a - from) / w).floor().max(0.0)) as usize;
    let last = (((b - from) / w).floor().max(0.0) as usize).min(n - 1);
    for i in first..=last {
        let (s, e) = (from + w * i as f64, from + w * (i + 1) as f64);
        let overlap = (b.min(e) - a.max(s)).max(0.0);
        if overlap > 0.0 {
            add(i, overlap);
        }
    }
}

/// Machine load per bucket, `n` buckets from `from` to `to`, as the time
/// weighted average of the samples that cover each bucket.
pub fn machine_series(db: &Db, from: f64, to: f64, n: usize) -> Result<(Vec<Bucket>, usize, f64)> {
    let w = (to - from) / n as f64;
    let mut out: Vec<Bucket> = (0..n).map(|i| Bucket { t: from + w * i as f64, ..Default::default() }).collect();
    let mut cover = vec![0.0f64; n];
    let (rows, span): (Vec<(f64, f64, f64)>, f64) = if use_raw(from) {
        let mut s = db.conn.prepare_cached("SELECT ts, cpu_busy, mem_used_kb FROM machine_samples WHERE ts >= ?1 AND ts < ?2 + 60")?;
        let v = s
            .query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as f64)))?
            .collect::<std::result::Result<_, _>>()?;
        (v, 2.0)
    } else {
        let mut s = db
            .conn
            .prepare_cached("SELECT (minute + 1) * 60, cpu_avg, mem_avg_kb FROM machine_1m WHERE minute * 60 >= ?1 AND minute * 60 < ?2")?;
        let v = s
            .query_map(params![from, to], |r| Ok((r.get::<_, i64>(0)? as f64, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        (v, 60.0)
    };
    for (t, c, m) in rows {
        spread(t, span, from, w, n, |i, o| {
            out[i].cpu += c * o;
            out[i].mem_kb += m * o;
            cover[i] += o;
        });
    }
    for (b, c) in out.iter_mut().zip(&cover) {
        if *c > 0.0 {
            b.cpu /= c;
            b.mem_kb /= c;
            b.has_data = true;
        }
    }
    let (ncpu, total): (i64, i64) = db
        .conn
        .query_row("SELECT ncpu, mem_total_kb FROM machine_samples ORDER BY ts DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap_or((crate::sys::ncpu() as i64, crate::sys::mem_total_kb() as i64));
    Ok((out, ncpu as usize, total as f64))
}

/// taskguard's own load per namespace per bucket: ns -> (cores, mem_kb) per
/// bucket, time weighted the same way as the machine series.
pub fn ns_series(db: &Db, from: f64, to: f64, n: usize, sample_every: f64) -> Result<BTreeMap<String, Vec<(f64, f64)>>> {
    let w = (to - from) / n as f64;
    let mut out: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    // (end of the covered interval, namespace, cores, memory, seconds covered)
    type Row = (f64, String, f64, f64, f64);
    let rows: Vec<Row> = if use_raw(from) {
        let mut s = db.conn.prepare_cached(
            "SELECT js.ts, r.ns, js.cores_used, js.mem_kb, coalesce(js.span_s, ?3) FROM job_samples js JOIN runs r ON r.id = js.run_id
             WHERE js.ts >= ?1 AND js.ts < ?2 + 60",
        )?;
        s.query_map(params![from, to, sample_every], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as f64, r.get(4)?)))?
            .collect::<std::result::Result<_, _>>()?
    } else {
        let mut s = db.conn.prepare_cached(
            "SELECT (minute + 1) * 60, ns, cores_avg, mem_avg_kb FROM ns_1m WHERE minute * 60 >= ?1 AND minute * 60 < ?2",
        )?;
        s.query_map(params![from, to], |r| Ok((r.get::<_, i64>(0)? as f64, r.get(1)?, r.get(2)?, r.get(3)?, 60.0)))?
            .collect::<std::result::Result<_, _>>()?
    };
    for (t, ns, c, m, span) in rows {
        let v = out.entry(ns).or_insert_with(|| vec![(0.0, 0.0); n]);
        spread(t, span, from, w, n, |i, o| {
            v[i].0 += c * o / w;
            v[i].1 += m * o / w;
        });
    }
    Ok(out)
}

/// Per group of programs outside taskguard (agents, browsers, ...): cores and
/// memory per bucket. Each reading stands for the recorder interval before it.
pub fn group_series(db: &Db, from: f64, to: f64, n: usize, every: f64) -> Result<BTreeMap<String, Vec<(f64, f64)>>> {
    let w = (to - from) / n as f64;
    let mut out: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    type Row = (f64, String, f64, f64, f64);
    let rows: Vec<Row> = if use_raw(from) {
        let mut s = db.conn.prepare_cached("SELECT ts, grp, cores, mem_kb FROM group_samples WHERE ts >= ?1 AND ts < ?2 + 60")?;
        s.query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as f64, every)))?
            .collect::<std::result::Result<_, _>>()?
    } else {
        let mut s = db.conn.prepare_cached(
            "SELECT (minute + 1) * 60, grp, cores_avg, mem_avg_kb FROM group_1m WHERE minute * 60 >= ?1 AND minute * 60 < ?2",
        )?;
        s.query_map(params![from, to], |r| Ok((r.get::<_, i64>(0)? as f64, r.get(1)?, r.get(2)?, r.get(3)?, 60.0)))?
            .collect::<std::result::Result<_, _>>()?
    };
    for (t, g, c, m, span) in rows {
        let v = out.entry(g).or_insert_with(|| vec![(0.0, 0.0); n]);
        spread(t, span, from, w, n, |i, o| {
            v[i].0 += c * o / w;
            v[i].1 += m * o / w;
        });
    }
    Ok(out)
}

/// How many jobs waited in each bucket, and the most common main blocker.
pub fn waiting_series(db: &Db, from: f64, to: f64, n: usize) -> Result<Vec<(usize, String)>> {
    let w = (to - from) / n as f64;
    let mut s = db.conn.prepare_cached("SELECT from_ts, to_ts, blocker FROM wait_spans WHERE to_ts >= ?1 AND from_ts < ?2")?;
    let spans: Vec<(f64, f64, String)> =
        s.query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
    Ok((0..n)
        .map(|i| {
            let (a, b) = (from + w * i as f64, from + w * (i + 1) as f64);
            let mut by: BTreeMap<&str, usize> = BTreeMap::new();
            for (f, t, bl) in &spans {
                if *f < b && *t >= a {
                    *by.entry(bl.as_str()).or_default() += 1;
                }
            }
            let total = by.values().sum();
            let main = by.iter().max_by_key(|(_, c)| **c).map(|(k, _)| k.to_string()).unwrap_or_default();
            (total, main)
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Marker {
    Starved,
    Now,
}

pub fn markers(db: &Db, from: f64, to: f64) -> Result<Vec<(f64, Marker)>> {
    let mut out = Vec::new();
    let mut s = db.conn.prepare_cached("SELECT ended_at FROM runs WHERE starved IS NOT NULL AND ended_at >= ?1 AND ended_at < ?2")?;
    for t in s.query_map(params![from, to], |r| r.get::<_, f64>(0))? {
        out.push((t?, Marker::Starved));
    }
    let mut s = db.conn.prepare_cached("SELECT started_at FROM runs WHERE now = 1 AND started_at >= ?1 AND started_at < ?2")?;
    for t in s.query_map(params![from, to], |r| r.get::<_, f64>(0))? {
        out.push((t?, Marker::Now));
    }
    Ok(out)
}

/// Jobs that ran at a moment, for the time cursor.
pub fn runs_at(db: &Db, t: f64) -> Result<Vec<(String, String)>> {
    let mut s = db.conn.prepare_cached(
        "SELECT ns, key FROM runs WHERE started_at <= ?1 AND coalesce(ended_at, ?1 + 1) >= ?1 ORDER BY started_at LIMIT 20",
    )?;
    let v = s.query_map([t], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

#[derive(Debug, Clone, Default)]
pub struct RunRow {
    pub id: i64,
    pub ended_at: f64,
    pub ns: String,
    pub key: String,
    pub label: Option<String>,
    pub waited_s: f64,
    pub dur_s: f64,
    pub cores_used: Option<f64>,
    pub cores_wanted: Option<f64>,
    pub peak_mem_kb: Option<u64>,
    pub exit: Option<i64>,
    pub main_blocker: Option<String>,
    pub starved: Option<String>,
    pub starved_detail: Vec<String>,
    pub now: bool,
}

fn run_row(r: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    let detail: Option<String> = r.get(13)?;
    Ok(RunRow {
        id: r.get(0)?,
        ended_at: r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
        ns: r.get(2)?,
        key: r.get(3)?,
        label: r.get(4)?,
        waited_s: r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
        dur_s: r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
        cores_used: r.get(7)?,
        cores_wanted: r.get(8)?,
        peak_mem_kb: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        exit: r.get(10)?,
        main_blocker: r.get(11)?,
        starved: r.get(12)?,
        starved_detail: detail.and_then(|d| serde_json::from_str(&d).ok()).unwrap_or_default(),
        now: r.get::<_, i64>(14)? == 1,
    })
}

const RUN_COLS: &str = "id, ended_at, ns, key, label, waited_s, ended_at - started_at, cores_used, cores_wanted, peak_mem_kb, exit,
                        main_blocker, starved, starved_detail, now";

pub fn runs(db: &Db, ns: Option<&str>, text: &str, limit: usize) -> Result<Vec<RunRow>> {
    let sql = format!(
        "SELECT {RUN_COLS} FROM runs WHERE ended_at IS NOT NULL AND imported = 0 AND (?1 IS NULL OR ns = ?1) AND key LIKE ?2
         ORDER BY ended_at DESC LIMIT ?3"
    );
    let mut s = db.conn.prepare_cached(&sql)?;
    let v = s.query_map(params![ns, format!("%{text}%"), limit as i64], run_row)?.collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

pub fn key_runs(db: &Db, key: &str, limit: usize) -> Result<Vec<RunRow>> {
    let sql = format!("SELECT {RUN_COLS} FROM runs WHERE key = ?1 AND ended_at IS NOT NULL ORDER BY ended_at DESC LIMIT ?2");
    let mut s = db.conn.prepare_cached(&sql)?;
    let v = s.query_map(params![key, limit as i64], run_row)?.collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

pub fn wait_spans(db: &Db, run_id: i64) -> Result<Vec<(f64, String, String)>> {
    let mut s = db
        .conn
        .prepare_cached("SELECT to_ts - from_ts, blocker, coalesce(detail, '') FROM wait_spans WHERE run_id = ?1 ORDER BY from_ts")?;
    let v = s.query_map([run_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

/// "12s order → 30s memory → 5s cpu"
pub fn blocker_history(spans: &[(f64, String, String)]) -> String {
    spans.iter().map(|(d, b, _)| format!("{} {b}", dur(*d))).collect::<Vec<_>>().join(" → ")
}

/// One change taskguard made to a job's needs on its own, and why.
#[derive(Debug, Clone)]
pub struct Adjustment {
    pub ts: f64,
    pub key: String,
    pub kind: String,
    pub old: Option<f64>,
    pub new: Option<f64>,
    pub reason: String,
}

pub fn adjustments(db: &Db, key: Option<&str>, since: f64) -> Result<Vec<Adjustment>> {
    let mut s = db.conn.prepare_cached(
        "SELECT ts, key, kind, old, new, reason FROM adjustments WHERE (?1 IS NULL OR key = ?1) AND ts >= ?2 ORDER BY ts DESC LIMIT 200",
    )?;
    let v = s
        .query_map(params![key, since], |r| {
            Ok(Adjustment { ts: r.get(0)?, key: r.get(1)?, kind: r.get(2)?, old: r.get(3)?, new: r.get(4)?, reason: r.get(5)? })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(v)
}

pub fn adjustment_text(kind: &str, old: Option<f64>, new: Option<f64>) -> String {
    match kind {
        "cpu_need" => match (old, new) {
            (Some(o), Some(n)) if n <= o + 0.05 => format!("next run keeps reserving {o:.1} cores"),
            (Some(o), Some(n)) => format!("next run reserves {n:.1} cores (was {o:.1})"),
            (None, Some(n)) => format!("next run reserves {n:.1} cores"),
            _ => "CPU need raised".into(),
        },
        "mem_boost" => match (old, new) {
            (Some(o), Some(n)) => format!("next runs reserve {} of memory (was {})", gb(n as u64), gb(o as u64)),
            (_, Some(n)) => format!("next runs reserve {} of memory", gb(n as u64)),
            _ => "memory need raised by 25%".into(),
        },
        "suggest_min" => "a minimum is suggested".into(),
        _ => kind.to_string(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct TrendRow {
    pub key: String,
    pub ns: String,
    pub runs: usize,
    pub cpu: Option<(f64, f64, f64)>,
    pub mem: Option<(f64, f64, f64)>,
    pub dur: Option<(f64, f64, f64)>,
    pub spark_cpu: Vec<f64>,
    pub spark_mem: Vec<f64>,
    pub spark_dur: Vec<f64>,
}

impl TrendRow {
    pub fn biggest(&self) -> f64 {
        [self.cpu, self.mem, self.dur].iter().flatten().map(|t| t.0.abs()).fold(0.0, f64::max)
    }
}

pub fn trends(db: &Db, n: usize) -> Result<Vec<TrendRow>> {
    let mut s = db.conn.prepare_cached(
        "SELECT key, ns, cores_wanted, peak_mem_kb, ended_at - started_at FROM runs
         WHERE ended_at IS NOT NULL AND peak_mem_kb > 0 AND imported = 0 AND exit = 0 ORDER BY ended_at",
    )?;
    let mut by: BTreeMap<String, TrendRow> = BTreeMap::new();
    for r in s.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<f64>>(4)?))
    })? {
        let (k, ns, c, m, d) = r?;
        let e = by.entry(k.clone()).or_insert_with(|| TrendRow { key: k, ns, ..Default::default() });
        e.runs += 1;
        if let Some(c) = c {
            e.spark_cpu.push(c);
        }
        e.spark_mem.push(m as f64);
        if let Some(d) = d {
            e.spark_dur.push(d);
        }
    }
    let mut v: Vec<TrendRow> = by
        .into_values()
        .map(|mut t| {
            t.cpu = insight::trend(&t.spark_cpu, n);
            t.mem = insight::trend(&t.spark_mem, n);
            t.dur = insight::trend(&t.spark_dur, n);
            t
        })
        .filter(|t| t.cpu.is_some() || t.mem.is_some() || t.dur.is_some())
        .collect();
    v.sort_by(|a, b| b.biggest().total_cmp(&a.biggest()));
    Ok(v)
}

pub fn trend_text(t: &TrendRow) -> String {
    let mut parts = Vec::new();
    if let Some((p, a, b)) = t.mem {
        parts.push(format!("memory {p:+.0}% ({} → {})", gb(a as u64), gb(b as u64)));
    }
    if let Some((p, a, b)) = t.cpu {
        parts.push(format!("cores {p:+.0}% ({a:.1} → {b:.1})"));
    }
    if let Some((p, a, b)) = t.dur {
        parts.push(format!("duration {p:+.0}% ({} → {})", dur(a), dur(b)));
    }
    parts.join("   ")
}

#[derive(Debug, Clone)]
pub struct Warning {
    pub ts: f64,
    pub kind: &'static str,
    pub key: String,
    pub title: String,
    pub lines: Vec<String>,
}

/// Every warning since `since`, newest first.
pub fn warnings(db: &Db, since: f64, cpu_max: f64, mem_max: f64) -> Result<Vec<Warning>> {
    let mut out = Vec::new();
    let adj = adjustments(db, None, since)?;
    let mut s = db
        .conn
        .prepare_cached(&format!("SELECT {RUN_COLS} FROM runs WHERE starved IS NOT NULL AND ended_at >= ?1 ORDER BY ended_at DESC"))?;
    let starved: Vec<RunRow> = s.query_map([since], run_row)?.collect::<std::result::Result<_, _>>()?;
    for r in starved {
        let mut lines = r.starved_detail.clone();
        if let Some(a) = adj.iter().find(|a| a.key == r.key && a.kind != "suggest_min") {
            lines.push(format!("taskguard changed: {}", adjustment_text(&a.kind, a.old, a.new)));
        }
        out.push(Warning {
            ts: r.ended_at,
            kind: "starved",
            key: r.key.clone(),
            title: format!("possibly starved ({}): {}", r.starved.clone().unwrap_or_default(), r.key),
            lines,
        });
    }
    for Adjustment { ts, key, kind, new, reason, .. } in &adj {
        if kind != "suggest_min" {
            continue;
        }
        let (flag, toml) = match new {
            Some(v) if *v > 256.0 => (
                format!("--min-mem {}", gb(*v as u64).replace(' ', "")),
                format!("min_mem = \"{}\"", gb(*v as u64).replace(' ', "").replace("GB", "G").replace("MB", "M")),
            ),
            Some(v) => (format!("--min-cpu {v:.0}"), format!("min_cpu = {v:.0}")),
            None => continue,
        };
        out.push(Warning {
            ts: *ts,
            kind: "repeated",
            key: key.clone(),
            title: format!("starved again and again: {key}"),
            lines: vec![
                reason.clone(),
                format!("pin it with the prefix: taskguard {flag} ..."),
                format!("or in .taskguard.toml:  [[job]]  key = \"{key}\"  {toml}"),
            ],
        });
    }
    for t in trends(db, 10)? {
        if t.biggest() < 50.0 {
            continue;
        }
        out.push(Warning {
            ts: crate::db::now(),
            kind: "trend",
            key: t.key.clone(),
            title: format!("growing: {}", t.key),
            lines: vec![trend_text(&t)],
        });
    }
    // --now runs that pushed the machine over a limit while they ran.
    let mut s = db.conn.prepare_cached(
        "SELECT r.id, r.key, r.started_at, max(m.cpu_busy * 100.0 / m.ncpu), max(m.mem_used_kb * 100.0 / m.mem_total_kb)
         FROM runs r JOIN machine_samples m ON m.ts BETWEEN r.started_at AND coalesce(r.ended_at, r.started_at)
         WHERE r.now = 1 AND r.started_at >= ?1 GROUP BY r.id",
    )?;
    let rows: Vec<(i64, String, f64, f64, f64)> =
        s.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<std::result::Result<_, _>>()?;
    for (_id, key, ts, cpu, mem) in rows {
        if cpu >= cpu_max || mem >= mem_max {
            out.push(Warning {
                ts,
                kind: "now",
                key: key.clone(),
                title: format!("skipped the queue and went over a limit: {key}"),
                lines: vec![format!(
                    "machine peaked at {cpu:.0}% CPU and {mem:.0}% memory while it ran (limits {cpu_max:.0}% / {mem_max:.0}%)"
                )],
            });
        }
    }
    out.sort_by(|a, b| b.ts.total_cmp(&a.ts));
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct NsRow {
    pub ns: String,
    pub cpu_hours: f64,
    pub gb_hours: f64,
    pub runs: usize,
    pub wait_s: f64,
    pub failed: usize,
    pub starved: usize,
    pub top_keys: Vec<(String, f64)>,
}

pub fn namespaces(db: &Db, from: f64, sample_every: f64) -> Result<Vec<NsRow>> {
    let mut s = db.conn.prepare_cached(
        "SELECT ns, sum(coalesce(cpu_seconds, 0)), count(*), sum(coalesce(waited_s, 0)), sum(exit != 0), sum(starved IS NOT NULL)
         FROM runs WHERE ended_at >= ?1 AND imported = 0 GROUP BY ns",
    )?;
    let mut rows: Vec<NsRow> = s
        .query_map([from], |r| {
            Ok(NsRow {
                ns: r.get(0)?,
                cpu_hours: r.get::<_, f64>(1)? / 3600.0,
                runs: r.get::<_, i64>(2)? as usize,
                wait_s: r.get(3)?,
                failed: r.get::<_, i64>(4)? as usize,
                starved: r.get::<_, i64>(5)? as usize,
                ..Default::default()
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    for row in &mut rows {
        let kb_s: f64 = db.conn.query_row(
            "SELECT coalesce(sum(js.mem_kb), 0) FROM job_samples js JOIN runs r ON r.id = js.run_id WHERE r.ns = ?1 AND js.ts >= ?2",
            params![row.ns, from],
            |r| r.get(0),
        )?;
        row.gb_hours = kb_s * sample_every / 1024.0 / 1024.0 / 3600.0;
        let mut s = db.conn.prepare_cached(
            "SELECT key, sum(coalesce(cpu_seconds, 0)) AS c FROM runs WHERE ns = ?1 AND ended_at >= ?2 GROUP BY key ORDER BY c DESC LIMIT 3",
        )?;
        row.top_keys = s
            .query_map(params![row.ns, from], |r| Ok((r.get(0)?, r.get::<_, f64>(1)? / 3600.0)))?
            .collect::<std::result::Result<_, _>>()?;
    }
    rows.sort_by(|a, b| b.cpu_hours.total_cmp(&a.cpu_hours));
    Ok(rows)
}

/// One row per key for `taskguard history`.
pub struct HistoryRow {
    pub key: String,
    pub ns: String,
    pub runs: usize,
    pub cpu: Option<f64>,
    pub mem_kb: Option<u64>,
    pub last_dur_s: Option<f64>,
    pub last_exit: Option<i64>,
}

pub fn history(db: &Db, keep: usize) -> Result<Vec<HistoryRow>> {
    let mut s = db.conn.prepare_cached("SELECT DISTINCT key, ns FROM runs WHERE ended_at IS NOT NULL AND peak_mem_kb > 0")?;
    let keys: Vec<(String, String)> = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
    let mut out = Vec::new();
    for (key, ns) in keys {
        let l = db.learned(&key, keep, 5)?;
        let total: i64 = db.conn.query_row("SELECT count(*) FROM runs WHERE key = ?1 AND ended_at IS NOT NULL", [&key], |r| r.get(0))?;
        let (last_dur_s, last_exit) = db
            .conn
            .query_row(
                "SELECT ended_at - started_at, exit FROM runs WHERE key = ?1 AND ended_at IS NOT NULL ORDER BY ended_at DESC LIMIT 1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((None, None));
        out.push(HistoryRow { key, ns, runs: total as usize, cpu: l.cpu, mem_kb: l.mem_kb, last_dur_s, last_exit });
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.mem_kb.unwrap_or(0)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NewRun, RunResult, now};

    #[test]
    fn series_are_bucketed() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        let t0 = (now() / 60.0).floor() * 60.0 - 600.0;
        for i in 0..300 {
            let s = crate::machine::MachineSample {
                ts: t0 + i as f64 * 2.0,
                cpu_inst: if i <= 150 { 2.0 } else { 6.0 },
                ncpu: 12,
                mem_used_kb: 1000,
                mem_total_kb: 4000,
                ..Default::default()
            };
            db.machine_sample(&s, 0, 0).unwrap();
        }
        let id = db.insert_run(&NewRun { ns: "dalp", key: "k", ..Default::default() }).unwrap();
        // Each sample stands for the two seconds before it, so these cover
        // exactly the second half.
        for i in 0..150 {
            db.job_sample(id, t0 + 302.0 + i as f64 * 2.0, 2.0, 4.0, 4.0, 800, 0.0).unwrap();
        }
        let (m, ncpu, total) = machine_series(&db, t0, t0 + 600.0, 2).unwrap();
        assert_eq!((m[0].cpu, m[1].cpu, ncpu, total), (2.0, 6.0, 12, 4000.0));
        let ns = ns_series(&db, t0, t0 + 600.0, 2, 2.0).unwrap();
        let d = &ns["dalp"];
        assert!(d[0].0.abs() < 1e-9 && (d[1].0 - 4.0).abs() < 1e-9, "{d:?}");
        assert!((d[1].1 - 800.0).abs() < 1e-6);

        // The same through the one-minute rollups.
        db.rollup(t0 + 600.0, 2.0).unwrap();
        let r: f64 =
            db.conn.query_row("SELECT cores_avg FROM ns_1m WHERE minute = ?1", [((t0 + 360.0) / 60.0) as i64], |r| r.get(0)).unwrap();
        assert!((r - 4.0).abs() < 1e-9, "{r}");
        let _ = db.finish_run(id, &RunResult { ended_at: now(), measured: false, ..Default::default() });
    }
}
