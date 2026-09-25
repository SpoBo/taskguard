//! The SQLite store: runs, samples, rollups, and the queries on them.
//!
//! WAL mode lets the jobs and the recorder write while the dashboard reads.
//! The queue itself does NOT live here: it stays in plain files under a file
//! lock, the fast path that must keep working even when the database is busy.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::time::Duration;

pub struct Db {
    pub conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY,
  ns TEXT NOT NULL,
  pool TEXT,
  key TEXT NOT NULL,
  label TEXT,
  cwd TEXT,
  cmd TEXT,
  pid INTEGER,
  script TEXT,
  package_json TEXT,
  queued_at REAL NOT NULL,
  started_at REAL,
  ended_at REAL,
  exit INTEGER,
  waited_s REAL,
  main_blocker TEXT,
  now INTEGER NOT NULL DEFAULT 0,
  min_cpu REAL,
  min_mem_kb INTEGER,
  need_cpu REAL,
  need_mem_kb INTEGER,
  peak_mem_kb INTEGER,
  cores_used REAL,
  cores_wanted REAL,
  cpu_seconds REAL,
  runnable_seconds REAL,
  pageins INTEGER,
  starved TEXT,
  starved_detail TEXT,
  machine_full_frac REAL,
  imported INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS runs_key ON runs(key, ended_at);
CREATE INDEX IF NOT EXISTS runs_ended ON runs(ended_at);
CREATE TABLE IF NOT EXISTS adjustments (
  id INTEGER PRIMARY KEY,
  ts REAL NOT NULL,
  key TEXT NOT NULL,
  run_id INTEGER,
  kind TEXT NOT NULL,
  old REAL,
  new REAL,
  reason TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS machine_samples (
  ts REAL NOT NULL,
  cpu_busy REAL NOT NULL,
  mem_used_kb INTEGER NOT NULL,
  ncpu INTEGER NOT NULL,
  mem_total_kb INTEGER NOT NULL,
  mem_pressure REAL,
  waiting INTEGER NOT NULL,
  running INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS machine_ts ON machine_samples(ts);
CREATE TABLE IF NOT EXISTS job_samples (
  ts REAL NOT NULL,
  run_id INTEGER NOT NULL,
  cores_used REAL NOT NULL,
  cores_wanted REAL NOT NULL,
  mem_kb INTEGER NOT NULL,
  pageins_per_s REAL NOT NULL,
  span_s REAL
);
CREATE INDEX IF NOT EXISTS job_samples_ts ON job_samples(ts);
CREATE INDEX IF NOT EXISTS job_samples_run ON job_samples(run_id);
CREATE TABLE IF NOT EXISTS wait_spans (
  run_id INTEGER NOT NULL,
  from_ts REAL NOT NULL,
  to_ts REAL NOT NULL,
  blocker TEXT NOT NULL,
  detail TEXT
);
CREATE INDEX IF NOT EXISTS wait_spans_run ON wait_spans(run_id);
CREATE INDEX IF NOT EXISTS wait_spans_ts ON wait_spans(to_ts);
CREATE TABLE IF NOT EXISTS top_procs (
  ts REAL NOT NULL,
  name TEXT NOT NULL,
  pid INTEGER NOT NULL,
  mem_kb INTEGER NOT NULL,
  cores REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS top_procs_ts ON top_procs(ts);
CREATE TABLE IF NOT EXISTS machine_1m (
  minute INTEGER PRIMARY KEY,
  cpu_avg REAL, cpu_max REAL,
  mem_avg_kb REAL, mem_max_kb REAL,
  waiting_max INTEGER
);
CREATE TABLE IF NOT EXISTS ns_1m (
  minute INTEGER NOT NULL,
  ns TEXT NOT NULL,
  cores_avg REAL, mem_avg_kb REAL,
  PRIMARY KEY (minute, ns)
);
"#;

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// What history says about one key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Learned {
    pub runs: usize,
    /// Max of the recent peaks. None when no run was ever measured.
    pub mem_kb: Option<u64>,
    /// Median of the recent sustained "cores wanted". None when unknown.
    pub cpu: Option<f64>,
    /// Median duration, for estimated start times.
    pub dur_s: Option<f64>,
    /// A recent run was starved of memory, so the memory need gets +25%.
    pub mem_boosted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct NewRun<'a> {
    pub ns: &'a str,
    pub pool: Option<&'a str>,
    pub key: &'a str,
    pub label: Option<&'a str>,
    pub cwd: &'a str,
    pub cmd: &'a str,
    pub pid: i32,
    pub script: Option<&'a str>,
    pub package_json: Option<&'a str>,
    pub now: bool,
    pub min_cpu: Option<f64>,
    pub min_mem_kb: Option<u64>,
    pub need_cpu: f64,
    pub need_mem_kb: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RunResult {
    pub ended_at: f64,
    pub exit: i32,
    pub peak_mem_kb: u64,
    pub cores_used: f64,
    pub cores_wanted: f64,
    pub cpu_seconds: f64,
    pub runnable_seconds: f64,
    pub pageins: u64,
    pub starved: Option<String>,
    pub starved_detail: Option<String>,
    pub machine_full_frac: f64,
    /// False when the run never produced a sample: nothing is learned from it.
    pub measured: bool,
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

pub fn median_of(mut v: Vec<f64>) -> Option<f64> {
    median(&mut v)
}

impl Db {
    pub fn open_dir(dir: &Path) -> Result<Db> {
        std::fs::create_dir_all(dir).ok();
        Db::open(&dir.join("taskguard.db"))
    }

    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        // Databases made before a sample said how many seconds it covers.
        let has_span: bool = conn.prepare("SELECT 1 FROM pragma_table_info('job_samples') WHERE name = 'span_s'")?.exists([])?;
        if !has_span {
            conn.execute_batch("ALTER TABLE job_samples ADD COLUMN span_s REAL")?;
        }
        Ok(Db { conn })
    }

    pub fn learned(&self, key: &str, keep: usize, boost_runs: usize) -> Result<Learned> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT peak_mem_kb, cores_wanted, ended_at - started_at, starved FROM runs
             WHERE key = ?1 AND ended_at IS NOT NULL AND peak_mem_kb > 0
             ORDER BY ended_at DESC LIMIT ?2",
        )?;
        // peak memory, cores wanted, duration, starved
        type Row = (i64, Option<f64>, Option<f64>, Option<String>);
        let rows: Vec<Row> = stmt
            .query_map(params![key, keep.max(boost_runs) as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let recent = &rows[..rows.len().min(keep)];
        let mem = recent.iter().map(|r| r.0 as u64).max();
        let mut cpu = median_of(recent.iter().filter_map(|r| r.1).collect());
        // After a run that was starved of CPU, the need is at least what that
        // run wanted, so one starved run is never averaged away by the median.
        if let Some((_, Some(w), _, Some(st))) = rows.first()
            && (st == "cpu" || st == "slowdown")
        {
            cpu = cpu.map(|c| c.max(*w));
        }
        let dur = median_of(recent.iter().filter_map(|r| r.2).collect());
        let boosted = rows.iter().take(boost_runs).any(|r| r.3.as_deref() == Some("memory"));
        Ok(Learned { runs: recent.len(), mem_kb: mem, cpu, dur_s: dur, mem_boosted: boosted })
    }

    pub fn insert_run(&self, r: &NewRun) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (ns, pool, key, label, cwd, cmd, pid, script, package_json, queued_at, now,
                               min_cpu, min_mem_kb, need_cpu, need_mem_kb)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                r.ns,
                r.pool,
                r.key,
                r.label,
                r.cwd,
                r.cmd,
                r.pid,
                r.script,
                r.package_json,
                now(),
                r.now as i32,
                r.min_cpu,
                r.min_mem_kb.map(|v| v as i64),
                r.need_cpu,
                r.need_mem_kb as i64
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn mark_started(&self, id: i64, started_at: f64, waited_s: f64, main_blocker: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET started_at = ?2, waited_s = ?3, main_blocker = ?4 WHERE id = ?1",
            params![id, started_at, waited_s, main_blocker],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, id: i64, r: &RunResult) -> Result<()> {
        // A run that was never sampled records its end but no measurements:
        // a zero would only drag the learned needs down, which could then
        // admit a heavy run into a machine with no room for it.
        let m = |v: f64| if r.measured { Some(v) } else { None };
        self.conn.execute(
            "UPDATE runs SET ended_at = ?2, exit = ?3, peak_mem_kb = ?4, cores_used = ?5, cores_wanted = ?6,
                cpu_seconds = ?7, runnable_seconds = ?8, pageins = ?9, starved = ?10, starved_detail = ?11,
                machine_full_frac = ?12
             WHERE id = ?1",
            params![
                id,
                r.ended_at,
                r.exit,
                if r.measured { Some(r.peak_mem_kb as i64) } else { None },
                m(r.cores_used),
                m(r.cores_wanted),
                m(r.cpu_seconds),
                m(r.runnable_seconds),
                if r.measured { Some(r.pageins as i64) } else { None },
                r.starved,
                r.starved_detail,
                r.machine_full_frac
            ],
        )?;
        Ok(())
    }

    /// A run whose owner died without finishing it (killed with SIGKILL).
    pub fn abandon_run(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE runs SET ended_at = ?2, exit = -1 WHERE id = ?1 AND ended_at IS NULL", params![id, now()])?;
        Ok(())
    }

    /// One reading of a running job. It covers the `span_s` seconds before `ts`.
    #[allow(clippy::too_many_arguments)]
    pub fn job_sample(&self, run_id: i64, ts: f64, span_s: f64, used: f64, wanted: f64, mem_kb: u64, pageins_per_s: f64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO job_samples (ts, run_id, cores_used, cores_wanted, mem_kb, pageins_per_s, span_s) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ts, run_id, used, wanted, mem_kb as i64, pageins_per_s, span_s],
        )?;
        Ok(())
    }

    pub fn wait_span(&self, run_id: i64, from: f64, to: f64, blocker: &str, detail: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO wait_spans (run_id, from_ts, to_ts, blocker, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, from, to, blocker, detail],
        )?;
        Ok(())
    }

    pub fn adjustment(&self, key: &str, run_id: i64, kind: &str, old: Option<f64>, new: Option<f64>, reason: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO adjustments (ts, key, run_id, kind, old, new, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![now(), key, run_id, kind, old, new, reason],
        )?;
        Ok(())
    }

    /// A first guess for a job with no history: what similar jobs needed.
    /// Similar means the same label (typecheck, test, ...), or without a label,
    /// a command that starts the same way. Each similar key counts once, with
    /// its highest peak, so one key with many runs does not dominate. Memory
    /// is the 75th percentile, so most similar jobs fit in the guess; CPU the
    /// median. Returns (memory, cpu, similar keys).
    pub fn estimate(&self, label: Option<&str>, tool: &str) -> Result<(Option<u64>, Option<f64>, usize)> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT max(peak_mem_kb), avg(cores_wanted) FROM runs
             WHERE peak_mem_kb > 0 AND ended_at > ?3 AND (label = ?1 OR (?1 IS NULL AND cmd LIKE ?2))
             GROUP BY key",
        )?;
        let since = now() - 30.0 * 86400.0;
        let rows: Vec<(i64, Option<f64>)> = stmt
            .query_map(params![label, format!("{tool}%"), since], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        if rows.is_empty() {
            return Ok((None, None, 0));
        }
        let mut mems: Vec<u64> = rows.iter().map(|r| r.0 as u64).collect();
        mems.sort_unstable();
        let p75 = mems[(mems.len() * 3 / 4).min(mems.len() - 1)];
        Ok((Some(p75), median_of(rows.iter().filter_map(|r| r.1).collect()), rows.len()))
    }

    /// How many of the key's most recent runs in a row were starved.
    pub fn starved_streak(&self, key: &str) -> Result<usize> {
        let mut stmt =
            self.conn.prepare_cached("SELECT starved FROM runs WHERE key = ?1 AND ended_at IS NOT NULL ORDER BY ended_at DESC LIMIT 20")?;
        let rows: Vec<Option<String>> = stmt.query_map([key], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(rows.iter().take_while(|s| s.is_some()).count())
    }

    /// Median duration of past runs, for the slowdown check.
    pub fn median_duration(&self, key: &str, keep: usize, exclude: i64) -> Result<Option<f64>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ended_at - started_at FROM runs WHERE key = ?1 AND id != ?2 AND ended_at IS NOT NULL
             AND started_at IS NOT NULL AND exit = 0 ORDER BY ended_at DESC LIMIT ?3",
        )?;
        let v: Vec<f64> = stmt.query_map(params![key, exclude, keep as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(if v.len() >= 3 { median_of(v) } else { None })
    }

    pub fn machine_sample(&self, s: &crate::machine::MachineSample, waiting: usize, running: usize) -> Result<()> {
        self.conn.execute(
            "INSERT INTO machine_samples (ts, cpu_busy, mem_used_kb, ncpu, mem_total_kb, mem_pressure, waiting, running)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                s.ts,
                s.cpu_inst,
                s.mem_used_kb as i64,
                s.ncpu as i64,
                s.mem_total_kb as i64,
                s.mem_pressure,
                waiting as i64,
                running as i64
            ],
        )?;
        Ok(())
    }

    pub fn top_procs(&self, ts: f64, procs: &[(String, i32, u64, f64)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (name, pid, mem, cores) in procs {
            tx.execute(
                "INSERT INTO top_procs (ts, name, pid, mem_kb, cores) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, name, pid, *mem as i64, cores],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The biggest processes outside taskguard at the latest reading.
    pub fn latest_top_procs(&self) -> Result<Vec<(String, u64, f64)>> {
        let ts: Option<f64> = self.conn.query_row("SELECT max(ts) FROM top_procs", [], |r| r.get(0)).optional()?.flatten();
        let Some(ts) = ts else { return Ok(Vec::new()) };
        let mut stmt = self.conn.prepare_cached("SELECT name, mem_kb, cores FROM top_procs WHERE ts = ?1 ORDER BY mem_kb DESC")?;
        let v = stmt
            .query_map([ts], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(v)
    }

    /// Fold finished minutes into the one-minute tables, then drop old rows.
    pub fn rollup(&self, upto: f64, sample_every: f64) -> Result<()> {
        let last: i64 = self.conn.query_row("SELECT coalesce(max(minute), 0) FROM machine_1m", [], |r| r.get(0))?;
        let upto_min = (upto / 60.0).floor() as i64;
        self.conn.execute(
            "INSERT OR REPLACE INTO machine_1m (minute, cpu_avg, cpu_max, mem_avg_kb, mem_max_kb, waiting_max)
             SELECT CAST(ts / 60 AS INTEGER) AS m, avg(cpu_busy), max(cpu_busy), avg(mem_used_kb), max(mem_used_kb), max(waiting)
             FROM machine_samples WHERE ts >= ?1 * 60 AND ts < ?2 * 60 GROUP BY m",
            params![last, upto_min],
        )?;
        // Per namespace: each job sample stands for the seconds it covers, so
        // the average over the minute is the weighted sum over 60 s.
        self.conn.execute(
            "INSERT OR REPLACE INTO ns_1m (minute, ns, cores_avg, mem_avg_kb)
             SELECT CAST(js.ts / 60 AS INTEGER) AS m, r.ns, sum(js.cores_used * coalesce(js.span_s, ?3)) / 60.0,
                    sum(js.mem_kb * coalesce(js.span_s, ?3)) / 60.0
             FROM job_samples js JOIN runs r ON r.id = js.run_id
             WHERE js.ts >= ?1 * 60 AND js.ts < ?2 * 60
             GROUP BY m, r.ns",
            params![last, upto_min, sample_every],
        )?;
        Ok(())
    }

    pub fn prune(&self, raw_hours: u64, rollup_days: u64) -> Result<()> {
        let raw_cut = now() - raw_hours as f64 * 3600.0;
        let roll_cut = ((now() - rollup_days as f64 * 86400.0) / 60.0) as i64;
        self.conn.execute("DELETE FROM machine_samples WHERE ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM job_samples WHERE ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM top_procs WHERE ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM wait_spans WHERE to_ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM machine_1m WHERE minute < ?1", [roll_cut])?;
        self.conn.execute("DELETE FROM ns_1m WHERE minute < ?1", [roll_cut])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(db: &Db, key: &str, mem: u64, wanted: f64, secs: f64, starved: Option<&str>) {
        let id = db.insert_run(&NewRun { ns: "n", key, ..Default::default() }).unwrap();
        let t = now();
        db.mark_started(id, t - secs, 0.0, None).unwrap();
        db.finish_run(
            id,
            &RunResult {
                ended_at: t,
                peak_mem_kb: mem,
                cores_wanted: wanted,
                cores_used: wanted,
                measured: true,
                starved: starved.map(str::to_string),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn learning_uses_max_memory_and_median_cpu() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        assert_eq!(db.learned("k", 10, 5).unwrap(), Learned::default());
        run(&db, "k", 1000, 1.0, 10.0, None);
        run(&db, "k", 5000, 2.0, 12.0, None);
        run(&db, "k", 3000, 8.0, 11.0, None);
        let l = db.learned("k", 10, 5).unwrap();
        assert_eq!(l.runs, 3);
        assert_eq!(l.mem_kb, Some(5000));
        assert_eq!(l.cpu, Some(2.0), "median, so one spike does not move it");
        assert_eq!(l.dur_s.map(|d| d.round()), Some(11.0));
        assert!(!l.mem_boosted);
        run(&db, "k", 3000, 2.0, 11.0, Some("memory"));
        assert!(db.learned("k", 10, 5).unwrap().mem_boosted);
        assert_eq!(db.starved_streak("k").unwrap(), 1);
    }

    #[test]
    fn a_new_job_is_estimated_from_similar_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        assert_eq!(db.estimate(Some("typecheck"), "tsc").unwrap(), (None, None, 0));
        for (key, mem) in [("a", 300), ("b", 700), ("c", 900), ("d", 4200)] {
            let id = db.insert_run(&NewRun { ns: "n", key, label: Some("typecheck"), cmd: "tsc -p .", ..Default::default() }).unwrap();
            db.mark_started(id, now() - 5.0, 0.0, None).unwrap();
            db.finish_run(id, &RunResult { ended_at: now(), peak_mem_kb: mem, cores_wanted: 1.0, measured: true, ..Default::default() })
                .unwrap();
        }
        let (mem, cpu, n) = db.estimate(Some("typecheck"), "tsc").unwrap();
        assert_eq!((mem, cpu, n), (Some(4200), Some(1.0), 4), "with four keys, the 75th percentile is the highest peak");
        assert_eq!(db.estimate(None, "tsc").unwrap().2, 4, "without a label, the command decides");
        assert_eq!(db.estimate(Some("test"), "vitest").unwrap().2, 0);
    }

    #[test]
    fn unmeasured_runs_teach_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        let id = db.insert_run(&NewRun { ns: "n", key: "k", ..Default::default() }).unwrap();
        db.mark_started(id, now(), 0.0, None).unwrap();
        db.finish_run(id, &RunResult { ended_at: now(), measured: false, ..Default::default() }).unwrap();
        assert_eq!(db.learned("k", 10, 5).unwrap().runs, 0);
    }
}
