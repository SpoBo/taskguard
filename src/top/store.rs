//! What the dashboard keeps between loads. The history is read once; after
//! that a load reads only the rows past the newest it has, and the runs that
//! were still going on. With nothing new, a load reads next to nothing:
//! reading it all again went through the whole history every second.
//!
//! A change that new rows do not show (a run deleted, put back, or changed
//! after its end, or samples deleted) moves a count in the `changes` table
//! (see `db::SCHEMA`), and the store then reads that part again in full.

use crate::dash::{self, Bucket, NsRow, RunAt, RunRow, TaskRow, TaskRun, TrendRow, TrendRun, Warning};
use crate::db::{self, Db, Learn};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// The newest row of each table the dashboard reads, and the counts of the
/// changes that new rows do not show.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Marks {
    pub runs_changed: i64,
    pub samples_changed: i64,
    /// Every update of a run, also of one that is going on.
    pub runs_updated: i64,
    pub run: i64,
    pub adjustment: i64,
    pub machine: i64,
    pub job_sample: i64,
    pub group: i64,
    pub wait: i64,
}

impl Marks {
    /// Each is one look at the end of a b-tree.
    pub fn read(db: &Db) -> Result<Marks> {
        let mut s = db.conn.prepare_cached(
            "SELECT (SELECT n FROM changes WHERE name = 'runs'), (SELECT n FROM changes WHERE name = 'samples'),
                    (SELECT n FROM changes WHERE name = 'updates'), (SELECT max(id) FROM runs), (SELECT max(id) FROM adjustments), (SELECT max(rowid) FROM machine_samples),
                    (SELECT max(rowid) FROM job_samples), (SELECT max(rowid) FROM group_samples), (SELECT max(rowid) FROM wait_spans)",
        )?;
        Ok(s.query_row([], |r| {
            let g = |i| r.get::<_, Option<i64>>(i).map(|v| v.unwrap_or(0));
            Ok(Marks {
                runs_changed: g(0)?,
                samples_changed: g(1)?,
                runs_updated: g(2)?,
                run: g(3)?,
                adjustment: g(4)?,
                machine: g(5)?,
                job_sample: g(6)?,
                group: g(7)?,
                wait: g(8)?,
            })
        })?)
    }
}

/// The newest one-minute rollups, which change once a minute.
fn rollup_marks(db: &Db) -> Result<(i64, i64, i64)> {
    let mut s = db.conn.prepare_cached(
        "SELECT coalesce((SELECT max(minute) FROM machine_1m), 0), coalesce((SELECT max(rowid) FROM ns_1m), 0),
                coalesce((SELECT max(rowid) FROM group_1m), 0)",
    )?;
    Ok(s.query_row([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
}

/// One run as the history reads it.
#[derive(Debug, Clone, PartialEq)]
struct Run {
    id: i64,
    key: String,
    ns: String,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    exit: Option<i64>,
    peak_mem_kb: Option<i64>,
    cores_used: Option<f64>,
    cores_wanted: Option<f64>,
    imported: bool,
}

const RUN_SQL: &str =
    "SELECT id, key, ns, started_at, ended_at, exit, peak_mem_kb, cores_used, cores_wanted, imported FROM runs WHERE id > ?1 ORDER BY id";

fn run_row(r: &rusqlite::Row) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        key: r.get(1)?,
        ns: r.get(2)?,
        started_at: r.get(3)?,
        ended_at: r.get(4)?,
        exit: r.get(5)?,
        peak_mem_kb: r.get(6)?,
        cores_used: r.get(7)?,
        cores_wanted: r.get(8)?,
        imported: r.get::<_, i64>(9)? != 0,
    })
}

/// The trends of one key: its runs that count, oldest first, and its row.
#[derive(Default)]
struct Trend {
    runs: Vec<TrendRun>,
    row: Option<TrendRow>,
    stale: bool,
}

/// Every run, folded into what the Tasks view, the trends and the
/// namespace filter show.
#[derive(Default)]
struct History {
    /// `Marks::runs_changed` when it was read in full.
    changed: i64,
    /// The newest run read.
    last: i64,
    /// The runs that had not ended when last read, read again after an update.
    going: BTreeMap<i64, Run>,
    /// Per key: how many runs ended, and the newest TASK_RUNS of them.
    tasks: BTreeMap<String, (usize, Vec<TaskRun>)>,
    trends: BTreeMap<String, Trend>,
    ns: BTreeSet<String>,
    task_rows: Vec<TaskRow>,
    trend_rows: Vec<TrendRow>,
}

impl History {
    fn read(db: &Db, changed: i64) -> Result<History> {
        let mut h = History { changed, ..Default::default() };
        h.add_new(db)?;
        h.finish();
        Ok(h)
    }

    /// Read the runs past the newest one read. Whether there were any.
    fn add_new(&mut self, db: &Db) -> Result<bool> {
        let mut s = db.conn.prepare_cached(RUN_SQL)?;
        let runs: Vec<Run> = s.query_map([self.last], run_row)?.collect::<std::result::Result<_, _>>()?;
        for r in &runs {
            self.last = self.last.max(r.id);
            if !r.imported {
                self.ns.insert(r.ns.clone());
            }
        }
        let any = !runs.is_empty();
        for r in runs {
            if r.ended_at.is_some() {
                self.add_ended(&r);
            } else {
                self.going.insert(r.id, r);
            }
        }
        Ok(any)
    }

    /// Read again the runs that were going on. Whether any of them changed.
    fn check_going(&mut self, db: &Db) -> Result<bool> {
        if self.going.is_empty() {
            return Ok(false);
        }
        let mut s = db.conn.prepare_cached(
            "SELECT id, key, ns, started_at, ended_at, exit, peak_mem_kb, cores_used, cores_wanted, imported FROM runs WHERE id = ?1",
        )?;
        let mut changed = false;
        for id in self.going.keys().copied().collect::<Vec<_>>() {
            let now = s.query_map([id], run_row)?.next().transpose()?;
            if now.as_ref() == self.going.get(&id) {
                continue;
            }
            changed = true;
            match now {
                Some(r) if r.ended_at.is_some() => {
                    self.going.remove(&id);
                    self.add_ended(&r);
                }
                Some(r) => {
                    self.going.insert(id, r);
                }
                // Gone: the count in `changes` moved too, so the next load reads it all.
                None => {
                    self.going.remove(&id);
                }
            }
        }
        Ok(changed)
    }

    fn add_ended(&mut self, r: &Run) {
        let ended_at = r.ended_at.unwrap_or(0.0);
        let (runs, newest) = self.tasks.entry(r.key.clone()).or_default();
        *runs += 1;
        let run = TaskRun {
            ended_at,
            id: r.id,
            ns: r.ns.clone(),
            exit: r.exit,
            peak_mem_kb: r.peak_mem_kb,
            cores: r.cores_used.or(r.cores_wanted),
        };
        let at = newest.partition_point(|o| (o.ended_at, o.id) > (ended_at, r.id));
        if at < dash::TASK_RUNS {
            newest.insert(at, run);
            newest.truncate(dash::TASK_RUNS);
        }
        if !r.imported && r.exit == Some(0) && r.peak_mem_kb.is_some_and(|m| m > 0) {
            let t = self.trends.entry(r.key.clone()).or_default();
            let run = TrendRun {
                ended_at,
                id: r.id,
                ns: r.ns.clone(),
                cores: r.cores_wanted,
                mem_kb: r.peak_mem_kb.unwrap_or(0),
                dur: r.started_at.map(|s| ended_at - s),
            };
            let at = t.runs.partition_point(|o| (o.ended_at, o.id) < (ended_at, r.id));
            t.runs.insert(at, run);
            t.stale = true;
        }
    }

    /// Make the rows again from what changed.
    fn finish(&mut self) {
        let mut tasks: Vec<TaskRow> = self.tasks.iter().map(|(k, (runs, newest))| dash::task_row(k, *runs, newest)).collect();
        tasks.sort_by(|a, b| b.last_end.total_cmp(&a.last_end));
        self.task_rows = tasks;
        for (k, t) in self.trends.iter_mut().filter(|(_, t)| t.stale) {
            t.row = dash::trend_row(k, &t.runs, 10);
            t.stale = false;
        }
        let mut trends: Vec<TrendRow> = self.trends.values().filter_map(|t| t.row.clone()).collect();
        dash::sort_trends(&mut trends);
        self.trend_rows = trends;
    }
}

/// Rows of one sample table from `from` on, kept between loads, ordered by
/// time and then row id, as the index on the time reads them.
struct Window<R> {
    from: f64,
    /// The newest row id read.
    last: i64,
    rows: Vec<(f64, i64, R)>,
}

impl<R> Window<R> {
    /// The window from `from` on: rows before it dropped, rows past `last`
    /// added. A window that starts earlier than the one kept is read again.
    fn update(
        slot: &mut Option<Window<R>>,
        from: f64,
        last: i64,
        all: impl FnOnce(f64) -> Result<Vec<(f64, i64, R)>>,
        since: impl FnOnce(i64) -> Result<Vec<(f64, i64, R)>>,
    ) -> Result<&Window<R>> {
        match slot {
            Some(w) if w.from <= from => {
                if last > w.last {
                    let new = since(w.last)?;
                    w.last = new.iter().map(|r| r.1).fold(w.last, i64::max);
                    w.rows.extend(new);
                    if !w.rows.is_sorted_by(|a, b| (a.0, a.1) <= (b.0, b.1)) {
                        w.rows.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                    }
                }
                w.rows.retain(|r| r.0 >= from);
                w.from = from;
            }
            _ => {
                let rows = all(from)?;
                let last = rows.iter().map(|r| r.1).fold(last, i64::max);
                *slot = Some(Window { from, last, rows });
            }
        }
        Ok(slot.as_ref().expect("just set"))
    }

    /// The rows before `to`.
    fn before(&self, to: f64) -> impl Iterator<Item = &(f64, i64, R)> {
        self.rows.iter().take_while(move |r| r.0 < to)
    }
}

/// `f`'s value for `key`, made again only when the key changed.
fn memo<K: PartialEq, V>(slot: &mut Slot<K, V>, key: K, f: impl FnOnce() -> Result<V>) -> Result<&V> {
    if slot.as_ref().is_none_or(|(k, _)| *k != key) {
        *slot = Some((key, f()?));
    }
    Ok(&slot.as_ref().expect("just set").1)
}

/// The parts of the Job view that change only with the history.
pub struct JobHistory {
    pub runs: Vec<RunRow>,
    pub min_cpu: Option<f64>,
    pub min_mem_kb: Option<u64>,
    pub learned: db::Learned,
    pub adjustments: Vec<dash::Adjustment>,
}

type Series = BTreeMap<String, Vec<(f64, f64)>>;

/// A value of `memo`, with the key it was made for.
type Slot<K, V> = Option<(K, V)>;

/// A job's samples: cores used, memory, seconds covered, by namespace.
type JobRow = (String, f64, f64, Option<f64>);

/// The changes a value made from the runs and the samples depends on.
type Seen = (i64, i64, i64);

/// The rollups, by minute: the machine, the namespaces, the groups.
type Minutes = (Vec<Bucket>, Series, Series);

/// The warnings from the database, and the oldest moment they were read from.
type Stored = (Vec<Warning>, f64);

#[derive(Default)]
pub struct Store {
    pub marks: Marks,
    /// Counts the changes to the runs, for the values made from them.
    version: u64,
    history: Option<History>,
    machine: Option<Window<(f64, f64)>>,
    jobs: Option<Window<JobRow>>,
    groups: Option<Window<(String, f64, f64)>>,
    waits: Option<Window<(f64, String)>>,
    machine_size: Slot<i64, (usize, f64)>,
    minutes: Slot<(u64, u64, usize, Seen, i64, u64), Minutes>,
    warnings: Slot<(u64, i64, u64, u64), Stored>,
    runs: Slot<(u64, Option<String>, String), Vec<RunRow>>,
    runs_at: Slot<(u64, u64), Vec<RunAt>>,
    markers: Slot<(u64, u64, u64), Vec<(f64, dash::Marker)>>,
    namespaces: Slot<(u64, i64, i64, u64, u64), Vec<NsRow>>,
    learned: Slot<(u64, String, Learn), db::Learned>,
    job: Slot<(u64, i64, String, Learn), JobHistory>,
    samples: Slot<Seen, Vec<dash::Sample>>,
    live_samples: Slot<Seen, Vec<dash::Sample>>,
    spans: Slot<Seen, Vec<(f64, String, String)>>,
    groups_now: Slot<(i64, i64), Vec<db::GroupReading>>,
}

impl Store {
    /// Read the marks, and the runs that are new or ended since the last load.
    pub fn update(&mut self, db: &Db) -> Result<()> {
        let marks = Marks::read(db)?;
        if marks.runs_changed != self.marks.runs_changed || marks.samples_changed != self.marks.samples_changed {
            // A run's namespace may have changed, which the samples of its jobs are filed under.
            (self.machine, self.jobs, self.groups, self.waits) = (None, None, None, None);
        }
        let changed = match &mut self.history {
            Some(h) if h.changed == marks.runs_changed => {
                let new = h.add_new(db)?;
                // A run that was going on changes only by an update.
                let ended = marks.runs_updated != self.marks.runs_updated && h.check_going(db)?;
                if new || ended {
                    h.finish();
                }
                new || ended
            }
            _ => {
                self.history = Some(History::read(db, marks.runs_changed)?);
                true
            }
        };
        if changed {
            self.version += 1;
        }
        self.marks = marks;
        Ok(())
    }

    fn history(&self) -> &History {
        self.history.as_ref().expect("update reads it first")
    }

    pub fn tasks(&self) -> &[TaskRow] {
        &self.history().task_rows
    }

    pub fn trends(&self) -> &[TrendRow] {
        &self.history().trend_rows
    }

    pub fn ns_list(&self) -> Vec<String> {
        self.history().ns.iter().cloned().collect()
    }

    /// The warnings of the last 7 days, newest first, as `dash::warnings_with`
    /// makes them, with the trends dated `now`.
    pub fn warnings(&mut self, db: &Db, now: f64, cpu_max: f64, mem_max: f64) -> Result<Vec<Warning>> {
        let since = now - 7.0 * 86400.0;
        let key = (self.version, self.marks.adjustment, cpu_max.to_bits(), mem_max.to_bits());
        // The window moves with the clock: read again once its oldest warning leaves it.
        if self.warnings.as_ref().is_some_and(|(_, (_, oldest))| *oldest < since) {
            self.warnings = None;
        }
        let (stored, _) = memo(&mut self.warnings, key, || dash::stored_warnings(db, since, cpu_max, mem_max))?;
        let mut out = stored.clone();
        out.extend(dash::trend_warnings(&self.history().trend_rows, now));
        out.sort_by(|a, b| b.ts.total_cmp(&a.ts));
        Ok(out)
    }

    /// The machine, the namespaces and the groups per bucket, as
    /// `dash::machine_series`, `dash::ns_series` and `dash::group_series`.
    pub fn series(&mut self, db: &Db, from: f64, to: f64, n: usize, sample_every: f64) -> Result<(Vec<Bucket>, Series, Series)> {
        if !dash::use_raw(from) {
            (self.machine, self.jobs, self.groups) = (None, None, None);
            let key = (from.to_bits(), to.to_bits(), n, rollup_marks(db)?, self.marks.samples_changed, self.version);
            let v = memo(&mut self.minutes, key, || {
                Ok((
                    dash::machine_buckets(dash::machine_minutes(db, from, to)?, 60.0, from, to, n),
                    dash::load_buckets(dash::ns_minutes(db, from, to)?, from, to, n),
                    dash::load_buckets(dash::group_minutes(db, from, to)?, from, to, n),
                ))
            })?;
            return Ok(v.clone());
        }
        self.minutes = None;
        let m = self.marks;
        let end = to + 60.0;
        let machine = Window::update(
            &mut self.machine,
            from,
            m.machine,
            |from| {
                let mut s = db
                    .conn
                    .prepare_cached("SELECT ts, rowid, cpu_busy, mem_used_kb FROM machine_samples WHERE ts >= ?1 ORDER BY ts, rowid")?;
                Ok(s.query_map([from], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get::<_, i64>(3)? as f64))))?
                    .collect::<std::result::Result<_, _>>()?)
            },
            |last| {
                let mut s = db.conn.prepare_cached("SELECT ts, rowid, cpu_busy, mem_used_kb FROM machine_samples WHERE rowid > ?1")?;
                Ok(s.query_map([last], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get::<_, i64>(3)? as f64))))?
                    .collect::<std::result::Result<_, _>>()?)
            },
        )?;
        let machine = dash::machine_buckets(machine.before(end).map(|(t, _, (c, m))| (*t, *c, *m)), 2.0, from, to, n);
        let jobs = Window::update(
            &mut self.jobs,
            from,
            m.job_sample,
            |from| {
                let mut s = db.conn.prepare_cached(
                    "SELECT js.ts, js.rowid, r.ns, js.cores_used, js.mem_kb, js.span_s FROM job_samples js JOIN runs r ON r.id = js.run_id
                     WHERE js.ts >= ?1 ORDER BY js.ts, js.rowid",
                )?;
                Ok(s.query_map([from], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get(3)?, r.get::<_, i64>(4)? as f64, r.get(5)?))))?
                    .collect::<std::result::Result<_, _>>()?)
            },
            |last| {
                let mut s = db.conn.prepare_cached(
                    "SELECT js.ts, js.rowid, r.ns, js.cores_used, js.mem_kb, js.span_s FROM job_samples js JOIN runs r ON r.id = js.run_id
                     WHERE js.rowid > ?1",
                )?;
                Ok(s.query_map([last], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get(3)?, r.get::<_, i64>(4)? as f64, r.get(5)?))))?
                    .collect::<std::result::Result<_, _>>()?)
            },
        )?;
        let ns = dash::load_buckets(
            jobs.before(end).map(|(t, _, (ns, c, m, span))| (*t, ns, *c, *m, span.unwrap_or(sample_every))),
            from,
            to,
            n,
        );
        let groups = Window::update(
            &mut self.groups,
            from,
            m.group,
            |from| {
                let mut s =
                    db.conn.prepare_cached("SELECT ts, rowid, grp, cores, mem_kb FROM group_samples WHERE ts >= ?1 ORDER BY ts, rowid")?;
                Ok(s.query_map([from], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get(3)?, r.get::<_, i64>(4)? as f64))))?
                    .collect::<std::result::Result<_, _>>()?)
            },
            |last| {
                let mut s = db.conn.prepare_cached("SELECT ts, rowid, grp, cores, mem_kb FROM group_samples WHERE rowid > ?1")?;
                Ok(s.query_map([last], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get(3)?, r.get::<_, i64>(4)? as f64))))?
                    .collect::<std::result::Result<_, _>>()?)
            },
        )?;
        let groups = dash::load_buckets(groups.before(end).map(|(t, _, (g, c, m))| (*t, g, *c, *m, crate::recorder::EVERY)), from, to, n);
        Ok((machine, ns, groups))
    }

    /// The CPU count and the memory of the machine, as `dash::machine_size`.
    pub fn machine_size(&mut self, db: &Db) -> Result<(usize, f64)> {
        Ok(*memo(&mut self.machine_size, self.marks.machine, || Ok(dash::machine_size(db)))?)
    }

    /// As `dash::waiting_series`.
    pub fn waiting(&mut self, db: &Db, from: f64, to: f64, n: usize) -> Result<Vec<(usize, String)>> {
        let w = Window::update(
            &mut self.waits,
            from,
            self.marks.wait,
            |from| {
                let mut s = db
                    .conn
                    .prepare_cached("SELECT to_ts, rowid, from_ts, blocker FROM wait_spans WHERE to_ts >= ?1 ORDER BY to_ts, rowid")?;
                Ok(s.query_map([from], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get(3)?))))?.collect::<std::result::Result<_, _>>()?)
            },
            |last| {
                let mut s = db.conn.prepare_cached("SELECT to_ts, rowid, from_ts, blocker FROM wait_spans WHERE rowid > ?1")?;
                Ok(s.query_map([last], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get(3)?))))?.collect::<std::result::Result<_, _>>()?)
            },
        )?;
        let spans: Vec<(f64, f64, &str)> = w.rows.iter().filter(|r| r.2.0 < to).map(|(t, _, (f, b))| (*f, *t, b.as_str())).collect();
        Ok(dash::waiting_buckets(&spans, from, to, n))
    }

    pub fn markers(&mut self, db: &Db, from: f64, to: f64) -> Result<Vec<(f64, dash::Marker)>> {
        Ok(memo(&mut self.markers, (self.version, from.to_bits(), to.to_bits()), || dash::markers(db, from, to))?.clone())
    }

    pub fn runs(&mut self, db: &Db, ns: Option<&str>, text: &str) -> Result<Vec<RunRow>> {
        let key = (self.version, ns.map(str::to_string), text.to_string());
        Ok(memo(&mut self.runs, key, || dash::runs(db, ns, text, 500))?.clone())
    }

    pub fn runs_at(&mut self, db: &Db, t: f64) -> Result<Vec<RunAt>> {
        Ok(memo(&mut self.runs_at, (self.version, t.to_bits()), || dash::runs_at(db, t))?.clone())
    }

    pub fn namespaces(&mut self, db: &Db, from: f64, sample_every: f64) -> Result<Vec<NsRow>> {
        let m = self.marks;
        let key = (self.version, m.job_sample, m.samples_changed, from.to_bits(), sample_every.to_bits());
        Ok(memo(&mut self.namespaces, key, || dash::namespaces(db, from, sample_every))?.clone())
    }

    pub fn learned(&mut self, db: &Db, key: &str, rules: &Learn) -> Result<db::Learned> {
        let k = (self.version, key.to_string(), rules.clone());
        Ok(memo(&mut self.learned, k, || db.learned(key, rules))?.clone())
    }

    /// The runs, the needs and the adjustments of the Job view's job.
    pub fn job(&mut self, db: &Db, key: &str, rules: &Learn) -> Result<&JobHistory> {
        let k = (self.version, self.marks.adjustment, key.to_string(), rules.clone());
        memo(&mut self.job, k, || {
            let (min_cpu, min_mem_kb) = db
                .conn
                .query_row("SELECT min_cpu, min_mem_kb FROM runs WHERE key = ?1 ORDER BY id DESC LIMIT 1", [key], |r| {
                    Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<i64>>(1)?.map(|v| v as u64)))
                })
                .unwrap_or((None, None));
            Ok(JobHistory {
                runs: dash::key_runs(db, key, 30)?,
                min_cpu,
                min_mem_kb,
                learned: db.learned(key, rules)?,
                adjustments: dash::adjustments(db, Some(key), 0.0)?,
            })
        })
    }

    /// The samples of a run, as `dash::run_samples`.
    pub fn run_samples(&mut self, db: &Db, run_id: i64, live: bool) -> Result<Vec<dash::Sample>> {
        let key = (run_id, self.marks.job_sample, self.marks.samples_changed);
        let slot = if live { &mut self.live_samples } else { &mut self.samples };
        Ok(memo(slot, key, || dash::run_samples(db, run_id))?.clone())
    }

    pub fn wait_spans(&mut self, db: &Db, run_id: i64) -> Result<Vec<(f64, String, String)>> {
        let key = (run_id, self.marks.wait, self.marks.samples_changed);
        Ok(memo(&mut self.spans, key, || dash::wait_spans(db, run_id))?.clone())
    }

    pub fn groups_now(&mut self, db: &Db) -> Vec<db::GroupReading> {
        let key = (self.marks.group, self.marks.samples_changed);
        memo(&mut self.groups_now, key, || Ok(db.latest_groups().unwrap_or_default())).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// The Tasks view as one query read it before the store kept it.
    fn tasks_by_query(db: &Db) -> Vec<TaskRow> {
        let mut s = db
            .conn
            .prepare(
                "WITH r AS (
                   SELECT key, ns, peak_mem_kb, coalesce(cores_used, cores_wanted) AS cores, exit, ended_at,
                          row_number() OVER (PARTITION BY key ORDER BY ended_at DESC, id DESC) AS rn
                   FROM runs WHERE ended_at IS NOT NULL)
                 SELECT key, max(CASE WHEN rn = 1 THEN ns END), count(*), max(ended_at), max(CASE WHEN rn = 1 THEN exit END),
                        max(CASE WHEN rn <= 10 THEN peak_mem_kb END), avg(CASE WHEN rn <= 10 THEN cores END)
                 FROM r GROUP BY key ORDER BY max(ended_at) DESC",
            )
            .unwrap();
        s.query_map([], |r| {
            Ok(TaskRow {
                key: r.get(0)?,
                ns: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                runs: r.get::<_, i64>(2)? as usize,
                last_end: r.get(3)?,
                last_exit: r.get(4)?,
                mem_kb: r.get::<_, Option<i64>>(5)?.filter(|m| *m > 0).map(|m| m as u64),
                cpu: r.get(6)?,
            })
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    fn ns_by_query(db: &Db) -> Vec<String> {
        let mut s = db.conn.prepare("SELECT DISTINCT ns FROM runs WHERE imported = 0 ORDER BY ns").unwrap();
        s.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    /// Everything the store makes, as text, to compare two stores.
    fn state(store: &mut Store, db: &Db, now: f64) -> String {
        let (from, to) = (now - 3600.0, now);
        let series = store.series(db, from, to, 60, 2.0).unwrap();
        let warnings = store.warnings(db, now, 100.0, 95.0).unwrap();
        let waiting = store.waiting(db, from, to, 60).unwrap();
        let markers = store.markers(db, from, to).unwrap();
        let runs = store.runs(db, None, "").unwrap();
        let namespaces = store.namespaces(db, from, 2.0).unwrap();
        format!(
            "{:?}\n{:?}\n{:?}\n{warnings:?}\n{series:?}\n{waiting:?}\n{markers:?}\n{runs:?}\n{namespaces:?}",
            store.tasks(),
            store.trends(),
            store.ns_list(),
        )
    }

    /// The series as the queries that read them in full make them.
    fn series_by_query(db: &Db, now: f64) -> String {
        let (from, to) = (now - 3600.0, now);
        let (machine, ..) = dash::machine_series(db, from, to, 60).unwrap();
        let (ns, groups) =
            (dash::ns_series(db, from, to, 60, 2.0).unwrap(), dash::group_series(db, from, to, 60, crate::recorder::EVERY).unwrap());
        format!("{:?}\n{:?}", (machine, ns, groups), dash::waiting_series(db, from, to, 60).unwrap())
    }

    fn series_of(store: &mut Store, db: &Db, now: f64) -> String {
        let (from, to) = (now - 3600.0, now);
        format!("{:?}\n{:?}", store.series(db, from, to, 60, 2.0).unwrap(), store.waiting(db, from, to, 60).unwrap())
    }

    fn fresh(db: &Db, now: f64) -> String {
        let mut s = Store::default();
        s.update(db).unwrap();
        state(&mut s, db, now)
    }

    #[test]
    fn the_store_equals_a_full_read_after_every_change() {
        use crate::db::{NewRun, RunResult};
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        let now = db::now();
        let finish = |id: i64, at: f64, exit: i32, peak: u64, starved: Option<&str>| {
            let r = RunResult {
                ended_at: at,
                exit,
                peak_mem_kb: peak,
                cores_used: 1.5,
                cores_wanted: 2.0,
                measured: true,
                starved: starved.map(str::to_string),
                ..Default::default()
            };
            db.finish_run(id, &r).unwrap();
        };
        let run = |ns: &str, key: &str, start: f64| {
            let id = db.insert_run(&NewRun { ns, key, ..Default::default() }).unwrap();
            db.mark_started(id, start, 1.0, Some("memory")).unwrap();
            id
        };
        // A key with a growing trend, and some history.
        for i in 0..20 {
            let id = run("dalp", "api:test", now - 3000.0 + i as f64 * 10.0);
            finish(id, now - 2995.0 + i as f64 * 10.0, 0, (1 << 20) + i * (1 << 18), None);
        }
        let going = run("shop", "web:build", now - 600.0);
        let sample = |id: i64, t: f64| db.job_sample(id, t, 2.0, 1.0, 2.0, 4000, 0.0).unwrap();
        for i in 0..10 {
            sample(going, now - 590.0 + i as f64 * 2.0);
            let s = crate::machine::MachineSample {
                ts: now - 590.0 + i as f64 * 2.0,
                cpu_inst: 2.0,
                ncpu: 8,
                mem_used_kb: 1000,
                mem_total_kb: 4000,
                ..Default::default()
            };
            db.machine_sample(&s, 1, 1).unwrap();
        }
        let g = |grp: &str| db::GroupReading { group: grp.into(), cores: 0.5, mem_kb: 100, procs: 2, top: String::new() };
        db.group_samples(now - 580.0, &[g("agents"), g("browsers")]).unwrap();

        let mut store = Store::default();
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now));
        assert_eq!(store.tasks(), tasks_by_query(&db).as_slice(), "the same rows as the query it replaces");
        assert_eq!(series_of(&mut store, &db, now), series_by_query(&db, now));

        // New rows: samples, a wait, a run that ends starved, an adjustment.
        sample(going, now - 500.0);
        db.wait_span(going, now - 700.0, now - 600.0, "memory", "").unwrap();
        let s = crate::machine::MachineSample {
            ts: now - 500.0,
            cpu_inst: 4.0,
            ncpu: 8,
            mem_used_kb: 2000,
            mem_total_kb: 4000,
            ..Default::default()
        };
        db.machine_sample(&s, 0, 1).unwrap();
        db.group_samples(now - 500.0, &[g("agents")]).unwrap();
        let id = run("dalp", "api:lint", now - 400.0);
        finish(id, now - 300.0, 1, 1 << 19, Some("cpu"));
        db.adjustment("api:lint", id, "cpu_need", Some(1.0), Some(2.0), "starved").unwrap();
        // A running run that starts and then ends.
        let later = db.insert_run(&NewRun { ns: "shop", key: "web:e2e", ..Default::default() }).unwrap();
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now));
        db.mark_started(later, now - 200.0, 3.0, None).unwrap();
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now), "an update of a running run");
        finish(going, now - 100.0, 0, 3 << 20, None);
        finish(later, now - 50.0, 0, 1 << 20, None);
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now), "runs that ended");

        // Imported runs come with old end times.
        db.conn
            .execute(
                "INSERT INTO runs (ns, key, cmd, queued_at, started_at, ended_at, exit, peak_mem_kb, imported, label)
                 VALUES ('imported', 'tsc-queue:x', 'tsc', ?1, ?1, ?2, 0, 5000, 1, 'typecheck')",
                params![now - 90000.0, now - 89000.0],
            )
            .unwrap();
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now), "an import");

        // Changes that new rows do not show: a prune, its undo, a rename,
        // and samples deleted.
        let pruned = db.runs_of_keys(&["api:lint".to_string()], "test").unwrap();
        db.prune_runs(&pruned).unwrap();
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now), "a prune");
        db.restore(&db.pruned("*").unwrap()).unwrap();
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now), "an undo");
        db.rename_ns("shop", "store").unwrap();
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now), "a rename");
        db.conn.execute("DELETE FROM machine_samples WHERE ts < ?1", [now - 550.0]).unwrap();
        store.update(&db).unwrap();
        assert_eq!(state(&mut store, &db, now), fresh(&db, now), "samples deleted");
        assert_eq!(store.tasks(), tasks_by_query(&db).as_slice());
        assert_eq!(store.ns_list(), ns_by_query(&db));
        assert_eq!(series_of(&mut store, &db, now), series_by_query(&db, now));
        assert_eq!(store.trends(), dash::trends(&db, 10).unwrap().as_slice());
    }

    #[test]
    fn a_load_with_nothing_new_reads_next_to_nothing() {
        use crate::db::{NewRun, RunResult};
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_dir(tmp.path()).unwrap();
        let now = db::now();
        let tx = db.conn.unchecked_transaction().unwrap();
        for i in 0..3000 {
            let id = db.insert_run(&NewRun { ns: "dalp", key: &format!("pkg{}:test", i % 300), ..Default::default() }).unwrap();
            db.mark_started(id, now - 5000.0 + i as f64, 0.0, None).unwrap();
            let r = RunResult {
                ended_at: now - 4990.0 + i as f64,
                peak_mem_kb: 1 << 20,
                cores_wanted: 1.0,
                measured: true,
                ..Default::default()
            };
            db.finish_run(id, &r).unwrap();
        }
        // Runs that never ended, as a crash leaves them.
        for i in 0..10 {
            let id = db.insert_run(&NewRun { ns: "dalp", key: "crashed", ..Default::default() }).unwrap();
            db.mark_started(id, now - 100.0 + i as f64, 0.0, None).unwrap();
        }
        for i in 0..1800 {
            let s = crate::machine::MachineSample {
                ts: now - 3600.0 + i as f64 * 2.0,
                cpu_inst: 2.0,
                ncpu: 8,
                mem_used_kb: 1000,
                mem_total_kb: 4000,
                ..Default::default()
            };
            db.machine_sample(&s, 0, 0).unwrap();
        }
        tx.commit().unwrap();
        let mut store = Store::default();
        let read = |store: &mut Store| {
            let before = db.pages_read();
            store.update(&db).unwrap();
            let _ = state(store, &db, now);
            db.pages_read() - before
        };
        let first = read(&mut store);
        let again = read(&mut store);
        assert!(first > 500, "the first load reads the history: {first} pages");
        assert!(again <= 20, "a load with nothing new reads only the marks: {again} pages, the first {first}");
    }
}
