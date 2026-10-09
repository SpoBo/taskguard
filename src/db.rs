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

/// Every taskguard version on the machine shares this database, so old and new
/// versions write to it side by side. The rules:
/// - Add tables and columns; never drop, rename or retype one.
/// - A new column is nullable or has a default, so the INSERTs of older
///   versions, which name only the columns they know, keep working.
/// - A new column on an existing table is also added in `Db::open`, as
///   `span_s` is, because `CREATE TABLE IF NOT EXISTS` skips existing tables.
/// - Queries name their columns: no `SELECT *`.
///
/// `tests::schema_stays_compatible` checks the NOT NULL columns.
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
  imported INTEGER NOT NULL DEFAULT 0,
  paused_s REAL,
  package TEXT,
  steady_cores_wanted REAL,
  steady_mem_kb INTEGER
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
CREATE TABLE IF NOT EXISTS group_samples (
  ts REAL NOT NULL,
  grp TEXT NOT NULL,
  cores REAL NOT NULL,
  mem_kb INTEGER NOT NULL,
  procs INTEGER NOT NULL,
  top TEXT
);
CREATE INDEX IF NOT EXISTS group_samples_ts ON group_samples(ts);
CREATE TABLE IF NOT EXISTS group_1m (
  minute INTEGER NOT NULL,
  grp TEXT NOT NULL,
  cores_avg REAL, mem_avg_kb REAL,
  PRIMARY KEY (minute, grp)
);
CREATE TABLE IF NOT EXISTS ns_1m (
  minute INTEGER NOT NULL,
  ns TEXT NOT NULL,
  cores_avg REAL, mem_avg_kb REAL,
  PRIMARY KEY (minute, ns)
);
-- Proof receipts (`taskguard --receipt`): a command that ran on an exact tree of
-- files. Keyed by the tree hash, not the checkout, so a receipt made in one
-- worktree publishes from any other that holds the same files.
CREATE TABLE IF NOT EXISTS receipts (
  id INTEGER PRIMARY KEY,
  receipt TEXT NOT NULL,
  tree TEXT NOT NULL,
  head TEXT,
  repo TEXT,
  level TEXT NOT NULL,
  reason TEXT,
  ok INTEGER NOT NULL,
  exit INTEGER,
  cmd TEXT NOT NULL,
  host TEXT,
  os TEXT,
  arch TEXT,
  version TEXT,
  started_at REAL NOT NULL,
  duration_s REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS receipts_tree ON receipts(tree);
-- Each commit status a receipt was posted as.
CREATE TABLE IF NOT EXISTS receipt_posts (
  receipt_row INTEGER NOT NULL,
  sha TEXT NOT NULL,
  posted_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS pruned_runs (
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
  imported INTEGER NOT NULL DEFAULT 0,
  paused_s REAL,
  package TEXT,
  steady_cores_wanted REAL,
  steady_mem_kb INTEGER,
  pruned_at REAL NOT NULL,
  prune_reason TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS pruned_runs_key ON pruned_runs(key);
"#;

/// The columns `prune` moves from `runs` to `pruned_runs` and back. A column
/// added to `runs` is added to `pruned_runs` and here too, or a prune loses it.
const RUN_COLUMNS: &str = "id, ns, pool, key, label, cwd, cmd, pid, script, package_json, queued_at, started_at, ended_at, exit,
  waited_s, main_blocker, now, min_cpu, min_mem_kb, need_cpu, need_mem_kb, peak_mem_kb, cores_used, cores_wanted, cpu_seconds,
  runnable_seconds, pageins, starved, starved_detail, machine_full_frac, imported, paused_s, package, steady_cores_wanted,
  steady_mem_kb";

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// One group of programs outside taskguard at one moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupReading {
    pub group: String,
    pub cores: f64,
    pub mem_kb: u64,
    pub procs: usize,
    /// Its biggest programs, in words: "claude ×20 5.3 GB, bun ×8 1.0 GB".
    pub top: String,
}

/// A guess for a job with no history, and where it comes from in words.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    pub mem_kb: Option<u64>,
    pub cpu: Option<f64>,
    /// The 75th percentile of how long they took: a first run guessed to
    /// end within seconds does not hold other first runs back.
    pub dur_s: Option<f64>,
    pub from: String,
}

/// What history says about one key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Learned {
    pub runs: usize,
    /// Max of the recent peaks, with outliers held back (see `Outliers`).
    /// None when no run was ever measured.
    pub mem_kb: Option<u64>,
    /// The recent memory peaks that stand far above the others.
    pub outliers: Option<Outliers>,
    /// The cores recent runs used, weighted toward the newest runs, with
    /// outliers held back (see `cpu_need`). None when unknown.
    pub cpu: Option<f64>,
    /// Median duration, for estimated start times.
    pub dur_s: Option<f64>,
    /// A recent run was starved of memory, so the memory need gets +25%.
    pub mem_boosted: bool,
    /// For a long-lived job, what its recent runs took after their start-up
    /// (`mem_kb` and `cpu` hold the start-up peak): the max of the memory
    /// peaks and the median of the cores wanted on average. None for others.
    pub steady_mem_kb: Option<u64>,
    pub steady_cpu: Option<f64>,
}

/// How history turns into needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Learn {
    /// How many recent runs count.
    pub keep: usize,
    /// A memory-starved run among this many recent runs adds 25%.
    pub boost_runs: usize,
    /// A peak is an outlier when it is more than this many times the next
    /// peak below it. At or under 1, there are no outliers.
    pub outlier_ratio: f64,
    /// ... and also at least this much above it.
    pub outlier_min_kb: u64,
    /// How much the outliers count, by how many there are: the first entry
    /// for one, the second for two. Past the list they count in full.
    pub outlier_weights: Vec<f64>,
}

impl Default for Learn {
    fn default() -> Self {
        Learn { keep: 10, boost_runs: 5, outlier_ratio: 2.0, outlier_min_kb: 1024 * 1024, outlier_weights: vec![0.0, 0.5] }
    }
}

/// A recent run whose memory peak stands far above the others.
#[derive(Debug, Clone, PartialEq)]
pub struct Outlier {
    pub run_id: i64,
    pub peak_kb: u64,
    pub ended_at: f64,
    pub exit: Option<i64>,
}

impl Outlier {
    /// A run that failed or was killed. Its peak never counts: a run that
    /// goes wrong can take far more memory than the job needs.
    pub fn failed(&self) -> bool {
        self.exit.is_some_and(|e| e != 0)
    }
}

/// The highest recent peaks when they stand far above the rest: one run that
/// took 20 GB where the others took 5 GB. A single such run may be a one-off,
/// so it does not set the memory need at once: it counts by `weight`, which
/// grows each time it happens again, until it counts in full.
#[derive(Debug, Clone, PartialEq)]
pub struct Outliers {
    pub runs: Vec<Outlier>,
    /// The highest peak below them.
    pub normal_kb: u64,
    /// How much of the step from `normal_kb` to the highest outlier that did
    /// not fail counts: 0 is none, 1 is all.
    pub weight: f64,
    /// The memory need they leave: `normal_kb` plus that share.
    pub mem_kb: u64,
}

impl Outliers {
    /// How many outliers count towards the weight: those that did not fail.
    pub fn repeats(&self) -> usize {
        self.runs.iter().filter(|o| !o.failed()).count()
    }

    /// Why an outlier counts as it does, in words.
    pub fn why(&self, o: &Outlier) -> String {
        if o.failed() {
            return format!("ignored: the run failed (exit {})", o.exit.unwrap_or(-1));
        }
        let n = self.repeats();
        let times = if n == 1 { "once".to_string() } else { format!("{n} times") };
        match self.weight {
            w if w <= 0.0 => format!("ignored: seen {times}, maybe a one-off"),
            w if w >= 1.0 => format!("counts in full: seen {times}"),
            w => format!("counts {:.0}%: seen {times}", w * 100.0),
        }
    }
}

/// Find the outliers among `runs` (run id, peak, end, exit). The runs sorted
/// by peak, highest first, are cut at the first step down that is both more
/// than `outlier_ratio` times and `outlier_min_kb` apart: the runs above the
/// cut are the outliers. They must be fewer than the runs below it, and at
/// least two runs must be below it, or the step is just how the job is. The
/// peak below the cut must also be `outlier_min_kb` or more: a job that is
/// mostly cache hits of a few MB and now and then a full build of 2 GB has no
/// outliers, as its full build is not a one-off and holding it back would
/// let it start into a machine with no room for it.
pub fn find_outliers(runs: &[Outlier], rules: &Learn) -> Option<Outliers> {
    let mut by_peak: Vec<&Outlier> = runs.iter().collect();
    by_peak.sort_by_key(|o| std::cmp::Reverse(o.peak_kb));
    let peaks: Vec<f64> = by_peak.iter().map(|o| o.peak_kb as f64).collect();
    let cut = outlier_cut(&peaks, rules.outlier_ratio, rules.outlier_min_kb as f64)?;
    let normal_kb = by_peak[cut].peak_kb;
    let out: Vec<Outlier> = by_peak[..cut].iter().map(|o| (*o).clone()).collect();
    let weight = outlier_weight(out.iter().filter(|o| !o.failed()).count(), rules);
    let top = out.iter().filter(|o| !o.failed()).map(|o| o.peak_kb).max().unwrap_or(normal_kb);
    let mem_kb = normal_kb + ((top - normal_kb) as f64 * weight) as u64;
    Some(Outliers { runs: out, normal_kb, weight, mem_kb })
}

/// Where `values`, sorted highest first, step down to the normal level: the
/// values before the cut are outliers. The step must be more than `ratio`
/// times and at least `min` apart, the value below it at least `min`, the
/// outliers fewer than the rest, and at least two values below the cut.
fn outlier_cut(values: &[f64], ratio: f64, min: f64) -> Option<usize> {
    let n = values.len();
    if ratio <= 1.0 || n < 3 {
        return None;
    }
    (1..n).take_while(|j| *j < n - j && n - j >= 2).find(|&j| {
        let (above, below) = (values[j - 1], values[j]);
        above > below * ratio && above - below >= min && below >= min
    })
}

/// How much outliers count when `healthy` of them did not fail.
fn outlier_weight(healthy: usize, rules: &Learn) -> f64 {
    match healthy {
        0 => 0.0,
        h => rules.outlier_weights.get(h - 1).copied().unwrap_or(1.0).clamp(0.0, 1.0),
    }
}

/// A CPU outlier is also at least this many cores above the others, and
/// those use at least this many: a job that idles at 0.2 cores and now and
/// then compiles on 3 has no outliers.
const CPU_OUTLIER_MIN: f64 = 1.0;
/// A run counts half as much as one this many runs newer.
const CPU_HALF_LIFE_RUNS: f64 = 4.0;

/// One past run, as `cpu_need` reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuRun {
    /// The cores it used: never the time it spent waiting for a core.
    pub used: f64,
    /// It was starved of CPU, so it used less than it would have.
    pub starved: bool,
    pub failed: bool,
}

/// The CPU need from recent runs, newest first: the weighted median of the
/// cores they used. A run counts half as much as one `CPU_HALF_LIFE_RUNS`
/// newer, so the need follows a job that gets faster or slower. Runs far
/// above the others are outliers and count by `outlier_weights`, as memory
/// peaks do. Runs that were starved of CPU used less than the job needs, so
/// they are left out while at least three runs that were not starved remain;
/// one run on a quiet machine alone does not set the need. The need is never
/// more than a run used: what a starved run waited for does not count.
pub fn cpu_need(runs: &[CpuRun], rules: &Learn) -> Option<f64> {
    let mut w: Vec<f64> = (0..runs.len()).map(|i| 0.5f64.powf(i as f64 / CPU_HALF_LIFE_RUNS)).collect();
    let mut by_use: Vec<usize> = (0..runs.len()).collect();
    by_use.sort_by(|a, b| runs[*b].used.total_cmp(&runs[*a].used));
    let used: Vec<f64> = by_use.iter().map(|i| runs[*i].used).collect();
    if let Some(cut) = outlier_cut(&used, rules.outlier_ratio, CPU_OUTLIER_MIN) {
        let out = &by_use[..cut];
        let weight = outlier_weight(out.iter().filter(|i| !runs[**i].failed).count(), rules);
        for &i in out {
            w[i] *= if runs[i].failed { 0.0 } else { weight };
        }
    }
    let fed = |i: usize| !runs[i].starved && w[i] > 0.0;
    if (0..runs.len()).filter(|i| fed(*i)).count() >= 3 {
        for (i, r) in runs.iter().enumerate() {
            if r.starved {
                w[i] = 0.0;
            }
        }
    }
    weighted_median(by_use.iter().rev().map(|i| (runs[*i].used, w[*i])))
}

/// The median of `(value, weight)` pairs, sorted by value from low to high.
/// Where the weight below and above balance exactly, the two values are
/// averaged, as a plain median does.
fn weighted_median(pairs: impl Iterator<Item = (f64, f64)>) -> Option<f64> {
    let pairs: Vec<(f64, f64)> = pairs.filter(|p| p.1 > 0.0).collect();
    let half = pairs.iter().map(|p| p.1).sum::<f64>() / 2.0;
    let mut acc = 0.0;
    for (i, (v, wt)) in pairs.iter().enumerate() {
        acc += wt;
        if (acc - half).abs() < 1e-9 {
            return Some(pairs.get(i + 1).map_or(*v, |n| (v + n.0) / 2.0));
        }
        if acc > half {
            return Some(*v);
        }
    }
    None
}

/// Runs that `prune` moved out of the history, or would move.
#[derive(Debug, Clone, PartialEq)]
pub struct PrunedRun {
    pub id: i64,
    pub key: String,
    pub ended_at: Option<f64>,
    pub peak_mem_kb: Option<u64>,
    pub exit: Option<i64>,
    pub reason: String,
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
    /// The `name` in that package.json.
    pub package: Option<&'a str>,
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
    /// Seconds the run was paused for memory; not part of its duration.
    pub paused_s: f64,
    /// A long-lived job after its start-up: cores wanted on average, and
    /// the memory peak. None for other jobs.
    pub steady: Option<(f64, u64)>,
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
        // Databases made before auto_pause: how long a run was paused.
        let has_paused: bool = conn.prepare("SELECT 1 FROM pragma_table_info('runs') WHERE name = 'paused_s'")?.exists([])?;
        if !has_paused {
            conn.execute_batch("ALTER TABLE runs ADD COLUMN paused_s REAL")?;
        }
        // Databases made before first runs were guessed from the same package:
        // the package name of each run. The index needs the column, so it is
        // made here rather than in SCHEMA.
        let has_package: bool = conn.prepare("SELECT 1 FROM pragma_table_info('runs') WHERE name = 'package'")?.exists([])?;
        if !has_package {
            conn.execute_batch("ALTER TABLE runs ADD COLUMN package TEXT")?;
        }
        conn.execute_batch("CREATE INDEX IF NOT EXISTS runs_package ON runs(package, cmd)")?;
        // Databases made before long-lived jobs: what a run took after its start-up.
        for (col, kind) in [("steady_cores_wanted", "REAL"), ("steady_mem_kb", "INTEGER")] {
            let has: bool = conn.prepare(&format!("SELECT 1 FROM pragma_table_info('runs') WHERE name = '{col}'"))?.exists([])?;
            if !has {
                conn.execute_batch(&format!("ALTER TABLE runs ADD COLUMN {col} {kind}"))?;
            }
        }
        Ok(Db { conn })
    }

    pub fn learned(&self, key: &str, rules: &Learn) -> Result<Learned> {
        let (keep, boost_runs) = (rules.keep, rules.boost_runs);
        let mut stmt = self.conn.prepare_cached(
            "SELECT peak_mem_kb, coalesce(cores_used, cores_wanted), ended_at - started_at - coalesce(paused_s, 0), starved, steady_mem_kb,
                    steady_cores_wanted, id, ended_at, exit
             FROM runs WHERE key = ?1 AND ended_at IS NOT NULL AND peak_mem_kb > 0
             ORDER BY ended_at DESC, id DESC LIMIT ?2",
        )?;
        // peak memory, cores used, duration, starved, and after the start-up: memory peak, cores wanted
        type Row = (i64, Option<f64>, Option<f64>, Option<String>, Option<i64>, Option<f64>);
        let rows: Vec<(Row, Outlier)> = stmt
            .query_map(params![key, keep.max(boost_runs) as i64], |r| {
                let peak: i64 = r.get(0)?;
                let run = Outlier { run_id: r.get(6)?, peak_kb: peak as u64, ended_at: r.get(7)?, exit: r.get(8)? };
                Ok(((peak, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?), run))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let (rows, runs): (Vec<Row>, Vec<Outlier>) = rows.into_iter().unzip();
        let recent = &rows[..rows.len().min(keep)];
        let outliers = find_outliers(&runs[..recent.len()], rules);
        let mem = match &outliers {
            Some(o) => Some(o.mem_kb),
            None => recent.iter().map(|r| r.0 as u64).max(),
        };
        let cpu_runs: Vec<CpuRun> = recent
            .iter()
            .zip(&runs)
            .filter_map(|(r, o)| {
                let starved = matches!(r.3.as_deref(), Some("cpu" | "slowdown"));
                r.1.map(|used| CpuRun { used, starved, failed: o.failed() })
            })
            .collect();
        let cpu = cpu_need(&cpu_runs, rules);
        let dur = median_of(recent.iter().filter_map(|r| r.2).collect());
        let boosted = rows.iter().take(boost_runs).any(|r| r.3.as_deref() == Some("memory"));
        let steady_mem_kb = recent.iter().filter_map(|r| r.4).max().map(|m| m as u64);
        let steady_cpu = median_of(recent.iter().filter_map(|r| r.5).collect());
        Ok(Learned { runs: recent.len(), mem_kb: mem, outliers, cpu, dur_s: dur, mem_boosted: boosted, steady_mem_kb, steady_cpu })
    }

    /// Every key with finished runs, or those that match `pattern`: a key, or
    /// a glob with `*` and `?`.
    pub fn keys(&self, pattern: Option<&str>) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT DISTINCT key FROM runs WHERE ended_at IS NOT NULL AND (?1 IS NULL OR key GLOB ?1) ORDER BY key")?;
        let keys = stmt.query_map([pattern], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        Ok(keys)
    }

    /// Finished runs of the keys that match `pattern` that ended before `before`.
    pub fn runs_of(&self, pattern: &str, before: Option<f64>) -> Result<Vec<PrunedRun>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, key, ended_at, peak_mem_kb, exit FROM runs
             WHERE key GLOB ?1 AND ended_at IS NOT NULL AND (?2 IS NULL OR ended_at < ?2) ORDER BY key, ended_at",
        )?;
        let runs = stmt
            .query_map(params![pattern, before], |r| {
                Ok(PrunedRun {
                    id: r.get(0)?,
                    key: r.get(1)?,
                    ended_at: r.get(2)?,
                    peak_mem_kb: r.get::<_, Option<i64>>(3)?.map(|m| m as u64),
                    exit: r.get(4)?,
                    reason: String::new(),
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(runs)
    }

    /// Finished runs of exactly these keys, each with `reason`.
    pub fn runs_of_keys(&self, keys: &[String], reason: &str) -> Result<Vec<PrunedRun>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, key, ended_at, peak_mem_kb, exit FROM runs WHERE key = ?1 AND ended_at IS NOT NULL ORDER BY ended_at",
        )?;
        let mut out = Vec::new();
        for k in keys {
            let runs = stmt.query_map([k], |r| {
                Ok(PrunedRun {
                    id: r.get(0)?,
                    key: r.get(1)?,
                    ended_at: r.get(2)?,
                    peak_mem_kb: r.get::<_, Option<i64>>(3)?.map(|m| m as u64),
                    exit: r.get(4)?,
                    reason: reason.to_string(),
                })
            })?;
            for r in runs {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// Move runs out of the history into `pruned_runs`, each with its reason.
    /// Every version stops learning from them, as they are no longer in
    /// `runs`; `restore` puts them back. Returns how many moved.
    pub fn prune_runs(&self, runs: &[PrunedRun]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut moved = 0;
        for r in runs {
            let n = tx.execute(
                &format!(
                    "INSERT OR REPLACE INTO pruned_runs ({RUN_COLUMNS}, pruned_at, prune_reason)
                     SELECT {RUN_COLUMNS}, ?2, ?3 FROM runs WHERE id = ?1"
                ),
                params![r.id, now(), r.reason],
            )?;
            if n > 0 {
                tx.execute("DELETE FROM runs WHERE id = ?1", [r.id])?;
                moved += 1;
            }
        }
        tx.commit()?;
        Ok(moved)
    }

    /// Pruned runs of the keys that match `pattern`.
    pub fn pruned(&self, pattern: &str) -> Result<Vec<PrunedRun>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, key, ended_at, peak_mem_kb, exit, prune_reason FROM pruned_runs WHERE key GLOB ?1 ORDER BY key, ended_at",
        )?;
        let runs = stmt
            .query_map([pattern], |r| {
                Ok(PrunedRun {
                    id: r.get(0)?,
                    key: r.get(1)?,
                    ended_at: r.get(2)?,
                    peak_mem_kb: r.get::<_, Option<i64>>(3)?.map(|m| m as u64),
                    exit: r.get(4)?,
                    reason: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(runs)
    }

    /// Put pruned runs back into the history. A run whose id a newer run took
    /// meanwhile comes back under a new id. Returns how many came back.
    pub fn restore(&self, runs: &[PrunedRun]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut back = 0;
        let rest = RUN_COLUMNS.trim_start_matches("id,");
        for r in runs {
            let taken: bool = tx.prepare_cached("SELECT 1 FROM runs WHERE id = ?1")?.exists([r.id])?;
            let cols = if taken { rest } else { RUN_COLUMNS };
            let n = tx.execute(&format!("INSERT INTO runs ({cols}) SELECT {cols} FROM pruned_runs WHERE id = ?1"), [r.id])?;
            if n > 0 {
                tx.execute("DELETE FROM pruned_runs WHERE id = ?1", [r.id])?;
                back += 1;
            }
        }
        tx.commit()?;
        Ok(back)
    }

    pub fn set_need(&self, run_id: i64, cpu: f64, mem_kb: u64) -> Result<()> {
        self.conn.execute("UPDATE runs SET need_cpu = ?2, need_mem_kb = ?3 WHERE id = ?1", params![run_id, cpu, mem_kb as i64])?;
        Ok(())
    }

    pub fn insert_run(&self, r: &NewRun) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (ns, pool, key, label, cwd, cmd, pid, script, package_json, queued_at, now,
                               min_cpu, min_mem_kb, need_cpu, need_mem_kb, package)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
                r.need_mem_kb as i64,
                r.package
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
                machine_full_frac = ?12, paused_s = ?13, steady_cores_wanted = ?14, steady_mem_kb = ?15
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
                r.machine_full_frac,
                (r.paused_s > 0.0).then_some(r.paused_s),
                r.steady.filter(|_| r.measured).map(|s| s.0),
                r.steady.filter(|_| r.measured).map(|s| s.1 as i64)
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
    /// A guess for a job with no history, from jobs like it in the last 30
    /// days: first those of the same kind in the same pool, then the same
    /// kind, then the same pool, then the same program. Memory is the 90th
    /// percentile of their peaks and CPU the 75th of the cores they used:
    /// a guess that is too low lets a first run crowd the machine.
    /// A first guess for a job with no history of its own. `same` is the
    /// job's package name and command: the same command of the same package
    /// under another key, as after the package moved to another folder, is
    /// the closest guess. Its history stays under its old key; only the guess
    /// borrows from it.
    pub fn estimate(&self, same: Option<(&str, &str)>, label: Option<&str>, tool: &str, pool: Option<&str>) -> Result<Option<Estimate>> {
        let since = now() - 30.0 * 86400.0;
        // package and command, kind, pool, program, and the words for where the guess comes from
        type Tier<'a> = (Option<(&'a str, &'a str)>, Option<&'a str>, Option<&'a str>, Option<&'a str>, String);
        let mut tiers: Vec<Tier> = Vec::new();
        if let Some((pkg, cmd)) = same {
            tiers.push((Some((pkg, cmd)), None, None, None, format!("runs of {cmd:?} in {pkg}")));
        }
        if let (Some(l), Some(p)) = (label, pool) {
            tiers.push((None, Some(l), Some(p), None, format!("{l} {p} jobs")));
        }
        if let Some(l) = label {
            tiers.push((None, Some(l), None, None, format!("{l} jobs")));
        }
        if let Some(p) = pool {
            tiers.push((None, None, Some(p), None, format!("jobs in {p}")));
        }
        tiers.push((None, None, None, Some(tool), format!("{tool} jobs")));
        for (same, l, p, t, what) in tiers {
            let mut stmt = self.conn.prepare_cached(
                "SELECT max(peak_mem_kb), avg(coalesce(cores_used, cores_wanted)), avg(ended_at - started_at - coalesce(paused_s, 0)) FROM runs
                 WHERE peak_mem_kb > 0 AND ended_at > ?1
                   AND (?2 IS NULL OR label = ?2) AND (?3 IS NULL OR pool = ?3) AND (?4 IS NULL OR cmd LIKE ?4 || '%')
                   AND (?5 IS NULL OR (package = ?5 AND cmd = ?6))
                 GROUP BY key",
            )?;
            let (pkg, cmd) = same.unzip();
            let rows: Vec<(i64, Option<f64>, Option<f64>)> = stmt
                .query_map(params![since, l, p, t, pkg, cmd], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            if rows.is_empty() {
                continue;
            }
            let pct = |mut v: Vec<f64>, q: f64| -> Option<f64> {
                if v.is_empty() {
                    return None;
                }
                v.sort_by(|a, b| a.total_cmp(b));
                Some(v[((v.len() as f64 * q) as usize).min(v.len() - 1)])
            };
            let mem = pct(rows.iter().map(|r| r.0 as f64).collect(), 0.9).map(|m| m as u64);
            let cpu = pct(rows.iter().filter_map(|r| r.1).collect(), 0.75);
            let dur_s = pct(rows.iter().filter_map(|r| r.2).collect(), 0.75);
            let from = if same.is_some() { format!("the {what}") } else { format!("typical of {} {what}", rows.len()) };
            return Ok(Some(Estimate { mem_kb: mem, cpu, dur_s, from }));
        }
        Ok(None)
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
            "SELECT ended_at - started_at - coalesce(paused_s, 0) FROM runs WHERE key = ?1 AND id != ?2 AND ended_at IS NOT NULL
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
    /// One reading of the groups of programs outside taskguard: per group,
    /// cores, memory, the number of processes, and its biggest programs.
    pub fn group_samples(&self, ts: f64, rows: &[GroupReading]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("INSERT INTO group_samples (ts, grp, cores, mem_kb, procs, top) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
            for g in rows {
                stmt.execute(params![ts, g.group, g.cores, g.mem_kb as i64, g.procs as i64, g.top])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The newest reading per group, biggest memory first.
    pub fn latest_groups(&self) -> Result<Vec<GroupReading>> {
        let ts: Option<f64> = self.conn.query_row("SELECT max(ts) FROM group_samples", [], |r| r.get(0)).optional()?.flatten();
        let Some(ts) = ts else { return Ok(Vec::new()) };
        let mut stmt = self
            .conn
            .prepare_cached("SELECT grp, cores, mem_kb, procs, coalesce(top, '') FROM group_samples WHERE ts = ?1 ORDER BY mem_kb DESC")?;
        let rows = stmt
            .query_map([ts], |r| {
                Ok(GroupReading {
                    group: r.get(0)?,
                    cores: r.get(1)?,
                    mem_kb: r.get::<_, i64>(2)? as u64,
                    procs: r.get::<_, i64>(3)? as usize,
                    top: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

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
        self.conn.execute(
            "INSERT OR REPLACE INTO group_1m (minute, grp, cores_avg, mem_avg_kb)
             SELECT CAST(ts / 60 AS INTEGER) AS m, grp, avg(cores), avg(mem_kb)
             FROM group_samples WHERE ts >= ?1 * 60 AND ts < ?2 * 60 GROUP BY m, grp",
            params![last, upto_min],
        )?;
        Ok(())
    }

    /// File runs that older versions put under a wrong namespace under the
    /// right one (see `key::renamed_namespace`). Returns how many names moved.
    pub fn fix_namespaces(&self) -> Result<usize> {
        let mut stmt = self.conn.prepare("SELECT ns, max(cwd) FROM runs WHERE cwd IS NOT NULL GROUP BY ns")?;
        let names: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        let mut moved = 0;
        for (ns, cwd) in names {
            if let Some(right) = crate::key::renamed_namespace(Path::new(&cwd), &ns) {
                self.rename_ns(&ns, &right)?;
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// Move every run and every minute of load from one namespace to another.
    /// Both can have load in the same minute; their loads add up.
    pub fn rename_ns(&self, from: &str, to: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("UPDATE runs SET ns = ?2 WHERE ns = ?1", params![from, to])?;
        tx.execute(
            "INSERT INTO ns_1m (minute, ns, cores_avg, mem_avg_kb)
             SELECT minute, ?2, cores_avg, mem_avg_kb FROM ns_1m WHERE ns = ?1
             ON CONFLICT (minute, ns) DO UPDATE SET
               cores_avg = coalesce(cores_avg, 0) + coalesce(excluded.cores_avg, 0),
               mem_avg_kb = coalesce(mem_avg_kb, 0) + coalesce(excluded.mem_avg_kb, 0)",
            params![from, to],
        )?;
        tx.execute("DELETE FROM ns_1m WHERE ns = ?1", [from])?;
        tx.commit()?;
        Ok(())
    }

    pub fn prune(&self, raw_hours: u64, rollup_days: u64) -> Result<()> {
        let raw_cut = now() - raw_hours as f64 * 3600.0;
        let roll_cut = ((now() - rollup_days as f64 * 86400.0) / 60.0) as i64;
        self.conn.execute("DELETE FROM machine_samples WHERE ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM job_samples WHERE ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM top_procs WHERE ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM group_samples WHERE ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM group_1m WHERE minute < ?1", [roll_cut])?;
        self.conn.execute("DELETE FROM wait_spans WHERE to_ts < ?1", [raw_cut])?;
        self.conn.execute("DELETE FROM machine_1m WHERE minute < ?1", [roll_cut])?;
        self.conn.execute("DELETE FROM ns_1m WHERE minute < ?1", [roll_cut])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_stays_compatible() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut stmt = db
            .conn
            .prepare("SELECT m.name, p.name FROM sqlite_master m, pragma_table_info(m.name) p WHERE m.type = 'table' AND p.\"notnull\" AND p.dflt_value IS NULL AND NOT p.pk ORDER BY 1, 2")
            .unwrap();
        let required: Vec<String> = stmt
            .query_map([], |r| Ok(format!("{}.{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        // Exactly the NOT NULL columns of v0.1.0. A new one here breaks the
        // INSERTs of every older version still running on the machine.
        let v0_1_0 = [
            "adjustments.kind",
            "adjustments.key",
            "adjustments.reason",
            "adjustments.ts",
            "job_samples.cores_used",
            "job_samples.cores_wanted",
            "job_samples.mem_kb",
            "job_samples.pageins_per_s",
            "job_samples.run_id",
            "job_samples.ts",
            "machine_samples.cpu_busy",
            "machine_samples.mem_total_kb",
            "machine_samples.mem_used_kb",
            "machine_samples.ncpu",
            "machine_samples.running",
            "machine_samples.ts",
            "machine_samples.waiting",
            "runs.key",
            "runs.ns",
            "runs.queued_at",
            "top_procs.cores",
            "top_procs.mem_kb",
            "top_procs.name",
            "top_procs.pid",
            "top_procs.ts",
            "wait_spans.blocker",
            "wait_spans.from_ts",
            "wait_spans.run_id",
            "wait_spans.to_ts",
        ];
        // Tables added later: only versions that know them write to them.
        let added = [
            "group_samples.cores",
            "group_samples.grp",
            "group_samples.mem_kb",
            "group_samples.procs",
            "group_samples.ts",
            "pruned_runs.key",
            "pruned_runs.ns",
            "pruned_runs.prune_reason",
            "pruned_runs.pruned_at",
            "pruned_runs.queued_at",
            "receipt_posts.posted_at",
            "receipt_posts.receipt_row",
            "receipt_posts.sha",
            "receipts.cmd",
            "receipts.duration_s",
            "receipts.level",
            "receipts.ok",
            "receipts.receipt",
            "receipts.started_at",
            "receipts.tree",
        ];
        let mut want: Vec<String> = v0_1_0.iter().chain(added.iter()).map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(required, want);
    }

    #[test]
    fn rename_ns_merges_runs_and_minutes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("t.db")).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO runs (ns, key, queued_at) VALUES ('01ABC', 'k', 1), ('dalp', 'k', 2), ('shop', 'k', 3);
                 INSERT INTO ns_1m VALUES (10, '01ABC', 1.5, 100), (11, '01ABC', 2.0, 200), (10, 'dalp', 0.5, 50), (10, 'shop', 9, 9);",
            )
            .unwrap();
        db.rename_ns("01ABC", "dalp").unwrap();
        let ns: Vec<String> =
            db.conn.prepare("SELECT ns FROM runs ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(ns, ["dalp", "dalp", "shop"]);
        let rows: Vec<(i64, String, f64, f64)> = db
            .conn
            .prepare("SELECT minute, ns, cores_avg, mem_avg_kb FROM ns_1m ORDER BY minute, ns")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows, [(10, "dalp".into(), 2.0, 150.0), (10, "shop".into(), 9.0, 9.0), (11, "dalp".into(), 2.0, 200.0)]);
    }

    #[test]
    fn opens_a_database_from_before_package() {
        let path = std::env::temp_dir().join(format!("tg-old-pkg-{}.db", std::process::id()));
        // Today's database without the column, as an older version left it.
        drop(Db::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP INDEX runs_package; ALTER TABLE runs DROP COLUMN package").unwrap();
        drop(conn);
        let db = Db::open(&path).unwrap();
        assert!(db.conn.prepare("SELECT package FROM runs").is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn opens_a_database_from_before_span_s() {
        let path = std::env::temp_dir().join(format!("tg-old-{}.db", std::process::id()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE job_samples (ts REAL NOT NULL, run_id INTEGER NOT NULL, cores_used REAL NOT NULL, cores_wanted REAL NOT NULL, mem_kb INTEGER NOT NULL, pageins_per_s REAL NOT NULL)").unwrap();
        drop(conn);
        let db = Db::open(&path).unwrap();
        assert!(db.conn.prepare("SELECT span_s FROM job_samples").is_ok());
        std::fs::remove_file(&path).ok();
    }

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
        assert_eq!(db.learned("k", &Learn::default()).unwrap(), Learned::default());
        run(&db, "k", 1000, 1.0, 10.0, None);
        run(&db, "k", 5000, 2.0, 12.0, None);
        run(&db, "k", 3000, 8.0, 11.0, None);
        let l = db.learned("k", &Learn::default()).unwrap();
        assert_eq!(l.runs, 3);
        assert_eq!(l.mem_kb, Some(5000));
        assert_eq!(l.cpu, Some(2.0), "median, so one spike does not move it");
        assert_eq!(l.dur_s.map(|d| d.round()), Some(11.0));
        assert!(!l.mem_boosted);
        run(&db, "k", 3000, 2.0, 11.0, Some("memory"));
        assert!(db.learned("k", &Learn::default()).unwrap().mem_boosted);
        assert_eq!(db.starved_streak("k").unwrap(), 1);
    }

    /// A run that used `used` cores while it wanted `wanted`.
    fn run_cpu(db: &Db, key: &str, used: f64, wanted: f64, starved: Option<&str>) {
        let id = db.insert_run(&NewRun { ns: "n", key, ..Default::default() }).unwrap();
        db.mark_started(id, now() - 10.0, 0.0, None).unwrap();
        let r = RunResult {
            ended_at: now(),
            peak_mem_kb: 1000,
            cores_used: used,
            cores_wanted: wanted,
            measured: true,
            starved: starved.map(str::to_string),
            ..Default::default()
        };
        db.finish_run(id, &r).unwrap();
    }

    fn cpu(db: &Db) -> f64 {
        db.learned("k", &Learn::default()).unwrap().cpu.unwrap()
    }

    #[test]
    fn the_cpu_need_is_what_runs_used_not_what_they_waited_for() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        // tsgo on a busy machine: one thread per core, so it "wants" 17
        // cores while it gets 4.
        for _ in 0..5 {
            run_cpu(&db, "k", 4.0, 17.0, None);
        }
        assert_eq!(cpu(&db), 4.0);
        // Rows from before cores_used was kept fall back on cores wanted.
        db.conn.execute("UPDATE runs SET cores_used = NULL", []).unwrap();
        assert_eq!(cpu(&db), 17.0);
    }

    fn cpu_runs(spec: &[(f64, bool)]) -> Vec<CpuRun> {
        spec.iter().map(|&(used, starved)| CpuRun { used, starved, failed: false }).collect()
    }

    #[test]
    fn starved_runs_are_left_out_while_three_runs_that_were_not_starved_remain() {
        let rules = Learn::default();
        // Newest first: four starved runs at 2 cores, then three at 8.
        let three = cpu_runs(&[(2.0, true), (2.0, true), (2.0, true), (2.0, true), (8.0, false), (8.0, false), (8.0, false)]);
        assert_eq!(cpu_need(&three, &rules), Some(8.0), "what a starved run got does not pull the need down");
        // With only two runs that were not starved, the starved ones count.
        let two = cpu_runs(&[(2.0, true), (2.0, true), (2.0, true), (2.0, true), (8.0, false), (8.0, false)]);
        assert_eq!(cpu_need(&two, &rules), Some(2.0));
    }

    #[test]
    fn recent_runs_weigh_more_than_old_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for _ in 0..6 {
            run_cpu(&db, "k", 8.0, 8.0, None);
        }
        for _ in 0..3 {
            run_cpu(&db, "k", 2.0, 2.0, None);
        }
        assert_eq!(cpu(&db), 2.0, "three new runs outweigh six older ones");
    }

    #[test]
    fn a_run_far_above_the_others_counts_more_each_time_it_comes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for _ in 0..5 {
            run_cpu(&db, "k", 4.0, 4.0, None);
        }
        run_cpu(&db, "k", 16.0, 16.0, None);
        assert_eq!(cpu(&db), 4.0, "a one-off does not set the need");
        run_cpu(&db, "k", 16.0, 16.0, None);
        assert_eq!(cpu(&db), 4.0, "the second time it counts half");
        run_cpu(&db, "k", 16.0, 16.0, None);
        assert_eq!(cpu(&db), 16.0, "the third time in full");
    }

    #[test]
    fn a_starved_run_never_raises_the_cpu_need() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for _ in 0..4 {
            run_cpu(&db, "k", 4.0, 4.0, None);
        }
        run_cpu(&db, "k", 2.0, 17.0, Some("cpu"));
        assert_eq!(cpu(&db), 4.0, "neither what it wanted nor what it got");

        // On a crowded machine nearly every run is starved. The one run on a
        // quiet machine used every core; it alone does not set the need.
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for _ in 0..8 {
            run_cpu(&db, "k", 6.0, 17.0, Some("cpu"));
        }
        run_cpu(&db, "k", 18.0, 18.0, None);
        assert_eq!(cpu(&db), 6.0);
    }

    fn run_exit(db: &Db, key: &str, mem: u64, exit: i32) -> i64 {
        let id = db.insert_run(&NewRun { ns: "n", key, ..Default::default() }).unwrap();
        db.mark_started(id, now() - 10.0, 0.0, None).unwrap();
        db.finish_run(id, &RunResult { ended_at: now(), exit, peak_mem_kb: mem, cores_wanted: 1.0, measured: true, ..Default::default() })
            .unwrap();
        id
    }

    const GB: u64 = 1024 * 1024;

    #[test]
    fn one_far_higher_peak_counts_more_each_time_it_comes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for _ in 0..4 {
            run_exit(&db, "k", 5 * GB, 0);
        }
        let first = run_exit(&db, "k", 11 * GB, 0);
        let l = db.learned("k", &Learn::default()).unwrap();
        assert_eq!(l.mem_kb, Some(5 * GB), "a one-off does not set the need");
        let o = l.outliers.unwrap();
        assert_eq!((o.runs.len(), o.runs[0].run_id, o.normal_kb, o.weight), (1, first, 5 * GB, 0.0));
        assert_eq!(o.why(&o.runs[0]), "ignored: seen once, maybe a one-off");

        run_exit(&db, "k", 11 * GB, 0);
        let l = db.learned("k", &Learn::default()).unwrap();
        assert_eq!(l.mem_kb, Some(8 * GB), "the second time it counts half");
        assert_eq!(l.outliers.as_ref().map(|o| o.why(&o.runs[0])).as_deref(), Some("counts 50%: seen 2 times"));

        run_exit(&db, "k", 11 * GB, 0);
        assert_eq!(db.learned("k", &Learn::default()).unwrap().mem_kb, Some(11 * GB), "the third time in full");

        let off = Learn { outlier_ratio: 0.0, ..Learn::default() };
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for m in [5, 5, 5, 11] {
            run_exit(&db, "k", m * GB, 0);
        }
        assert_eq!(db.learned("k", &off).unwrap().mem_kb, Some(11 * GB), "outlier_ratio = 0 turns it off");
    }

    #[test]
    fn a_failed_run_far_above_the_rest_never_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for _ in 0..3 {
            run_exit(&db, "k", 5 * GB, 0);
        }
        run_exit(&db, "k", 12 * GB, 1);
        run_exit(&db, "k", 12 * GB, 137);
        let l = db.learned("k", &Learn::default()).unwrap();
        assert_eq!(l.mem_kb, Some(5 * GB));
        let o = l.outliers.unwrap();
        assert_eq!(o.repeats(), 0, "failed runs do not count as a repeat");
        assert_eq!(o.why(&o.runs[0]), "ignored: the run failed (exit 137)");
    }

    #[test]
    fn small_jobs_and_steady_steps_have_no_outliers() {
        let rules = Learn::default();
        let runs = |peaks: &[u64]| -> Vec<Outlier> {
            peaks.iter().enumerate().map(|(i, p)| Outlier { run_id: i as i64, peak_kb: *p, ended_at: i as f64, exit: Some(0) }).collect()
        };
        // Cache hits of a few MB and a full build now and then.
        assert_eq!(find_outliers(&runs(&[10_000, 10_000, 10_000, 10_000, 3 * GB]), &rules), None);
        // Less than 1 GB above the rest.
        let low = Learn { outlier_ratio: 1.5, ..Learn::default() };
        assert_eq!(find_outliers(&runs(&[3 * GB / 2, 3 * GB / 2, 3 * GB / 2, 12 * GB / 5]), &low), None);
        assert!(find_outliers(&runs(&[3 * GB / 2, 3 * GB / 2, 3 * GB / 2, 3 * GB]), &low).is_some());
        // Half the runs are high: that is how the job is.
        assert_eq!(find_outliers(&runs(&[2 * GB, 2 * GB, 9 * GB, 9 * GB]), &rules), None);
        // Too few runs to tell.
        assert_eq!(find_outliers(&runs(&[2 * GB, 9 * GB]), &rules), None);
        // The cut is at the step: two high runs above a lower group.
        let o = find_outliers(&runs(&[2 * GB, 2 * GB, 2 * GB, 2 * GB, 2 * GB, 9 * GB, 10 * GB]), &rules).unwrap();
        assert_eq!((o.runs.len(), o.normal_kb, o.mem_kb), (2, 2 * GB, 6 * GB));
    }

    #[test]
    fn pruned_runs_leave_the_history_and_come_back() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for _ in 0..3 {
            run_exit(&db, "a:tsc", 5 * GB, 0);
        }
        let big = run_exit(&db, "a:tsc", 20 * GB, 0);
        run_exit(&db, "b:tsc", GB, 0);
        let mut runs = db.runs_of("a:*", None).unwrap();
        assert_eq!(runs.len(), 4);
        assert!(db.runs_of("a:*", Some(0.0)).unwrap().is_empty(), "older than: nothing ended before 1970");
        runs.retain(|r| r.id == big);
        runs[0].reason = "outlier".into();
        assert_eq!(db.prune_runs(&runs).unwrap(), 1);
        assert_eq!(db.learned("a:tsc", &Learn::default()).unwrap().mem_kb, Some(5 * GB));
        assert_eq!(db.learned("b:tsc", &Learn::default()).unwrap().runs, 1, "other keys stay");
        let pruned = db.pruned("*").unwrap();
        assert_eq!((pruned.len(), pruned[0].reason.as_str(), pruned[0].peak_mem_kb), (1, "outlier", Some(20 * GB)));

        assert_eq!(db.restore(&pruned).unwrap(), 1);
        assert!(db.pruned("*").unwrap().is_empty());
        assert_eq!(db.learned("a:tsc", &Learn::default()).unwrap().runs, 4);

        // A run whose id was taken meanwhile comes back under a new id.
        let last = db.runs_of("b:*", None).unwrap();
        db.prune_runs(&last).unwrap();
        let newer = run_exit(&db, "c:tsc", GB, 0);
        assert_eq!(newer, last[0].id, "SQLite hands out the freed id again");
        assert_eq!(db.restore(&db.pruned("*").unwrap()).unwrap(), 1);
        assert_eq!((db.runs_of("b:*", None).unwrap().len(), db.runs_of("c:*", None).unwrap().len()), (1, 1));
    }

    #[test]
    fn a_new_job_is_estimated_from_similar_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        assert_eq!(db.estimate(None, Some("typecheck"), "tsc", None).unwrap(), None);
        let add = |key: &str, label: &str, pool: Option<&str>, cmd: &str, mem: u64, cores: f64| {
            let id = db.insert_run(&NewRun { ns: "n", key, label: Some(label), pool, cmd, ..Default::default() }).unwrap();
            db.mark_started(id, now() - 5.0, 0.0, None).unwrap();
            db.finish_run(
                id,
                &RunResult {
                    ended_at: now(),
                    peak_mem_kb: mem,
                    cores_used: cores,
                    cores_wanted: cores,
                    measured: true,
                    ..Default::default()
                },
            )
            .unwrap();
        };
        for (i, mem) in [300, 500, 700, 900, 1100, 1300, 1500, 1700, 1900, 4200].iter().enumerate() {
            add(&format!("tc{i}"), "typecheck", None, "tsc -p .", *mem, 1.0 + i as f64 / 10.0);
        }
        add("ci-a", "ci", Some("throttle-suite"), "sh -c x", 12_000_000, 9.0);
        add("ci-b", "ci", Some("throttle-suite"), "bun run ci:local", 10_000_000, 8.0);

        let e = db.estimate(None, Some("typecheck"), "tsc", None).unwrap().unwrap();
        assert_eq!(e.mem_kb, Some(4200), "the 90th percentile of ten peaks: the highest");
        assert_eq!(e.cpu.map(|c| (c * 10.0).round() / 10.0), Some(1.7), "CPU: the 75th percentile");
        assert_eq!(e.from, "typical of 10 typecheck jobs");
        assert!(e.dur_s.is_some_and(|d| (4.0..6.0).contains(&d)), "the duration they took: {:?}", e.dur_s);
        let e = db.estimate(None, Some("ci"), "sh", Some("throttle-suite")).unwrap().unwrap();
        assert_eq!((e.mem_kb, e.from.as_str()), (Some(12_000_000), "typical of 2 ci throttle-suite jobs"));
        let e = db.estimate(None, None, "sh", Some("throttle-suite")).unwrap().unwrap();
        assert_eq!(e.from, "typical of 2 jobs in throttle-suite", "without a kind, the pool decides");
        assert_eq!(db.estimate(None, None, "tsc", None).unwrap().unwrap().from, "typical of 10 tsc jobs", "then the program");
        assert_eq!(db.estimate(None, Some("test"), "vitest", None).unwrap(), None);
    }

    #[test]
    fn a_moved_package_is_guessed_from_its_runs_under_the_old_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        let add = |key: &str, package: &str, cmd: &str, mem: u64, cores: f64| {
            let id =
                db.insert_run(&NewRun { ns: "n", key, label: Some("test"), package: Some(package), cmd, ..Default::default() }).unwrap();
            db.mark_started(id, now() - 5.0, 0.0, None).unwrap();
            db.finish_run(
                id,
                &RunResult {
                    ended_at: now(),
                    peak_mem_kb: mem,
                    cores_used: cores,
                    cores_wanted: cores,
                    measured: true,
                    ..Default::default()
                },
            )
            .unwrap();
        };
        // Many small test jobs, and one big one under the package's old folder.
        for i in 0..10 {
            add(&format!("other{i}:vitest"), &format!("@x/other{i}"), "vitest run", 300_000, 1.0);
        }
        add("packages/dapp/custody:vitest", "@dapp/custody", "vitest run", 4_000_000, 3.5);
        add("packages/dapp/custody:tsc", "@dapp/custody", "tsc -p .", 2_000_000, 2.0);

        // Its new folder has no history: the guess is its own old runs, not the typical test job.
        let e = db.estimate(Some(("@dapp/custody", "vitest run")), Some("test"), "vitest", None).unwrap().unwrap();
        assert_eq!((e.mem_kb, e.cpu), (Some(4_000_000), Some(3.5)));
        assert_eq!(e.from, r#"the runs of "vitest run" in @dapp/custody"#);
        // The old key keeps its history: nothing moved.
        assert_eq!(db.learned("packages/dapp/custody:vitest", &Learn::default()).unwrap().runs, 1);
        // Another command, or another package, falls back on the typical job.
        let e = db.estimate(Some(("@dapp/custody", "vitest run --coverage")), Some("test"), "vitest", None).unwrap().unwrap();
        assert!(e.from.starts_with("typical of"), "{}", e.from);
        let e = db.estimate(Some(("@dapp/new", "vitest run")), Some("test"), "vitest", None).unwrap().unwrap();
        assert!(e.from.starts_with("typical of"), "{}", e.from);
    }

    #[test]
    fn a_long_lived_job_learns_its_start_up_peak_and_its_steady_state() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        for (peak, steady) in
            [((6_000_000, 11.0), (3_000_000, 0.3)), ((5_000_000, 10.0), (4_000_000, 0.5)), ((7_000_000, 12.0), (3_500_000, 0.2))]
        {
            let id = db.insert_run(&NewRun { ns: "n", key: "stack", ..Default::default() }).unwrap();
            db.mark_started(id, now() - 3600.0, 0.0, None).unwrap();
            let r = RunResult {
                ended_at: now(),
                peak_mem_kb: peak.0,
                cores_used: peak.1,
                cores_wanted: peak.1,
                measured: true,
                steady: Some((steady.1, steady.0)),
                ..Default::default()
            };
            db.finish_run(id, &r).unwrap();
        }
        let l = db.learned("stack", &Learn::default()).unwrap();
        assert_eq!((l.mem_kb, l.cpu), (Some(7_000_000), Some(11.0)), "it is admitted against its start-up peak");
        assert_eq!((l.steady_mem_kb, l.steady_cpu), (Some(4_000_000), Some(0.3)), "the steady state: max memory, median cores");
        // An ordinary job has no steady state.
        run(&db, "k", 1000, 1.0, 10.0, None);
        let l = db.learned("k", &Learn::default()).unwrap();
        assert_eq!((l.steady_mem_kb, l.steady_cpu), (None, None));
    }

    #[test]
    fn opens_a_database_from_before_long_lived_jobs() {
        let path = std::env::temp_dir().join(format!("tg-old-steady-{}.db", std::process::id()));
        drop(Db::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("ALTER TABLE runs DROP COLUMN steady_cores_wanted; ALTER TABLE runs DROP COLUMN steady_mem_kb").unwrap();
        drop(conn);
        let db = Db::open(&path).unwrap();
        assert!(db.conn.prepare("SELECT steady_cores_wanted, steady_mem_kb FROM runs").is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unmeasured_runs_teach_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        let id = db.insert_run(&NewRun { ns: "n", key: "k", ..Default::default() }).unwrap();
        db.mark_started(id, now(), 0.0, None).unwrap();
        db.finish_run(id, &RunResult { ended_at: now(), measured: false, ..Default::default() }).unwrap();
        assert_eq!(db.learned("k", &Learn::default()).unwrap().runs, 0);
    }
}
