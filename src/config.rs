//! Layered settings. Later layers win:
//!   1. built-in defaults
//!   2. the user config (~/.config/taskguard/config.toml)
//!   3. `[dir."<path>"]` sections of the user config, for jobs whose cwd is
//!      inside that path (the longest path wins, because it is applied last)
//!   4. a repo file `.taskguard.toml`, found walking up from the cwd to the
//!      checkout root
//!   5. command-line flags (applied by the caller)
//!
//! Settings live in files, not environment variables, because a task runner
//! such as turbo runs in a strict environment mode and drops variables it does
//! not know.

use crate::builtin;
use crate::matcher;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Layer {
    pub enabled: Option<bool>,
    pub hints: Option<bool>,
    pub cpu_max: Option<f64>,
    pub mem_max: Option<f64>,
    pub sample_every: Option<f64>,
    pub status_every: Option<u64>,
    pub hist_keep: Option<usize>,
    pub learn_stagger: Option<f64>,
    pub cpu_min_duration: Option<f64>,
    pub pressure_max: Option<f64>,
    pub priority: Option<i32>,
    pub auto_pause: Option<bool>,
    pub pause_at: Option<f64>,
    pub resume_at: Option<f64>,
    pub new_job_mem: Option<String>,
    pub max_bypass: Option<u64>,
    pub max_backfill: Option<u64>,
    pub boost_runs: Option<usize>,
    pub recorder_idle_exit: Option<u64>,
    pub retention_raw_hours: Option<u64>,
    pub retention_rollup_days: Option<u64>,
    #[serde(default)]
    pub pool: BTreeMap<String, PoolCfg>,
    #[serde(default)]
    pub label: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub passthrough: Vec<String>,
    #[serde(default)]
    pub job: Vec<JobRule>,
    /// Only read from the user config.
    #[serde(default)]
    pub dir: BTreeMap<String, Layer>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct PoolCfg {
    pub max_slots: Option<u32>,
    /// Take away a slot ceiling that an earlier layer set, such as a built-in
    /// pool's. Ignored when the same section sets max_slots. A separate key,
    /// because a version that does not know it ignores it and keeps the old
    /// ceiling, where `max_slots = 0` would block its jobs for ever.
    pub unlimited: Option<bool>,
    /// One pool per checkout rather than one for the machine. Default true.
    pub per_checkout: Option<bool>,
    #[serde(default, rename = "match")]
    pub patterns: Vec<String>,
    /// Its jobs run for hours, such as dev stacks: see `Config::long_lived`.
    pub long_lived: Option<bool>,
    pub startup: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct JobRule {
    #[serde(rename = "match")]
    pub pattern: Option<String>,
    pub key: Option<String>,
    pub min_cpu: Option<f64>,
    pub min_mem: Option<String>,
    pub now: Option<bool>,
    pub pool: Option<String>,
    pub priority: Option<i32>,
    pub long_lived: Option<bool>,
    pub startup: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Pool {
    pub name: String,
    pub max_slots: Option<u32>,
    pub per_checkout: bool,
    pub patterns: Vec<String>,
    pub long_lived: bool,
    pub startup: Option<u64>,
}

/// Seconds a long-lived job holds its start-up peak when no `startup` is set.
pub const STARTUP_S: u64 = 300;

#[derive(Debug, Clone)]
pub struct Config {
    /// Off: every command runs straight through, as with TASKGUARD_DISABLE=1.
    pub enabled: bool,
    pub hints: bool,
    pub cpu_max: f64,
    pub mem_max: f64,
    pub sample_every: f64,
    pub status_every: u64,
    pub hist_keep: usize,
    pub learn_stagger: f64,
    pub cpu_min_duration: f64,
    pub pressure_max: f64,
    /// The priority of a job that no rule or flag gives one.
    pub priority: i32,
    /// Pause running jobs when memory fills up, and resume them when it frees
    /// up. Off by default.
    pub auto_pause: bool,
    pub pause_at: f64,
    pub resume_at: f64,
    pub new_job_mem_kb: u64,
    pub max_bypass: u64,
    pub max_backfill: u64,
    pub boost_runs: usize,
    pub recorder_idle_exit: u64,
    pub retention_raw_hours: u64,
    pub retention_rollup_days: u64,
    pub pools: Vec<Pool>,
    pub labels: Vec<(String, Vec<String>)>,
    pub passthrough: Vec<String>,
    pub jobs: Vec<(JobRule, String)>,
    /// setting name -> the layer that set it last, for `doctor --explain`.
    pub origin: BTreeMap<String, String>,
    /// Every layer that was read, in order.
    pub layers: Vec<String>,
    /// Settings a file names that this version does not know. They are
    /// ignored, not fatal: a repo may pin a newer taskguard that knows them,
    /// and every version on the machine reads the same files.
    pub unknown: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let mut c = Config {
            enabled: true,
            hints: true,
            cpu_max: 100.0,
            mem_max: 85.0,
            sample_every: 2.0,
            status_every: 120,
            hist_keep: 10,
            learn_stagger: 5.0,
            cpu_min_duration: 5.0,
            pressure_max: 20.0,
            priority: 0,
            auto_pause: false,
            pause_at: 92.0,
            resume_at: 80.0,
            new_job_mem_kb: 1536 * 1024,
            max_bypass: 120,
            max_backfill: 0,
            boost_runs: 5,
            recorder_idle_exit: 1800,
            retention_raw_hours: 48,
            retention_rollup_days: 90,
            pools: builtin::POOLS
                .iter()
                .map(|(n, p)| Pool {
                    name: n.to_string(),
                    max_slots: Some(1),
                    per_checkout: true,
                    patterns: p.iter().map(|s| s.to_string()).collect(),
                    long_lived: false,
                    startup: None,
                })
                .collect(),
            labels: builtin::LABELS.iter().map(|(n, p)| (n.to_string(), p.iter().map(|s| s.to_string()).collect())).collect(),
            passthrough: builtin::PASSTHROUGH.iter().map(|s| s.to_string()).collect(),
            jobs: Vec::new(),
            origin: BTreeMap::new(),
            layers: vec!["built-in".into()],
            unknown: Vec::new(),
        };
        for k in [
            "enabled",
            "hints",
            "cpu_max",
            "mem_max",
            "sample_every",
            "status_every",
            "hist_keep",
            "learn_stagger",
            "max_bypass",
            "max_backfill",
            "boost_runs",
            "recorder_idle_exit",
        ] {
            c.origin.insert(k.into(), "built-in".into());
        }
        c
    }
}

/// A setting that the dashboard's Config view can change.
pub struct Setting {
    pub key: &'static str,
    pub what: &'static str,
    pub kind: SettingKind,
}

pub enum SettingKind {
    /// A number: step, lowest, highest, unit.
    Number(f64, f64, f64, &'static str),
    Bool,
    /// A size in megabytes: step, lowest, highest.
    Size(u64, u64, u64),
}

pub const SETTINGS: [Setting; 13] = [
    Setting {
        key: "enabled",
        what: "off: every command runs straight through, unqueued and unmeasured (as TASKGUARD_DISABLE=1)",
        kind: SettingKind::Bool,
    },
    Setting {
        key: "cpu_max",
        what: "CPU limit: jobs start while busy + promised + need stay under this share of all cores",
        kind: SettingKind::Number(5.0, 10.0, 100.0, "%"),
    },
    Setting {
        key: "mem_max",
        what: "memory limit: jobs start while memory stays under this share of RAM",
        kind: SettingKind::Number(5.0, 10.0, 95.0, "%"),
    },
    Setting {
        key: "pressure_max",
        what: "Linux: no new job while memory pressure (PSI) is above this",
        kind: SettingKind::Number(5.0, 5.0, 90.0, "%"),
    },
    Setting {
        key: "auto_pause",
        what: "pause the newest running job when memory reaches pause_at; resume it under resume_at",
        kind: SettingKind::Bool,
    },
    Setting {
        key: "pause_at",
        what: "auto_pause: pause a running job at this share of RAM in use",
        kind: SettingKind::Number(1.0, 50.0, 99.0, "%"),
    },
    Setting {
        key: "resume_at",
        what: "auto_pause: resume a paused job under this share of RAM in use",
        kind: SettingKind::Number(1.0, 30.0, 98.0, "%"),
    },
    Setting { key: "hints", what: "agent hints on the status lines of jobs", kind: SettingKind::Bool },
    Setting {
        key: "max_bypass",
        what: "a job that newer jobs passed for this long gets a reservation",
        kind: SettingKind::Number(30.0, 30.0, 1800.0, "s"),
    },
    Setting {
        key: "learn_stagger",
        what: "new jobs start in batches this far apart, so readings can catch up",
        kind: SettingKind::Number(1.0, 0.0, 60.0, "s"),
    },
    Setting {
        key: "cpu_min_duration",
        what: "jobs that usually end sooner than this skip the CPU check",
        kind: SettingKind::Number(1.0, 0.0, 60.0, "s"),
    },
    Setting {
        key: "new_job_mem",
        what: "memory a first run reserves when no similar job is known",
        kind: SettingKind::Size(256, 256, 16384),
    },
    Setting { key: "status_every", what: "a waiting job prints a status line this often", kind: SettingKind::Number(5.0, 5.0, 300.0, "s") },
];

impl Config {
    /// The value of a setting, as the Config view shows it.
    pub fn setting_text(&self, key: &str) -> String {
        match key {
            "cpu_max" => format!("{:.0}%", self.cpu_max),
            "mem_max" => format!("{:.0}%", self.mem_max),
            "pressure_max" => format!("{:.0}%", self.pressure_max),
            "enabled" => (if self.enabled { "on" } else { "off" }).into(),
            "hints" => (if self.hints { "on" } else { "off" }).into(),
            "auto_pause" => (if self.auto_pause { "on" } else { "off" }).into(),
            "pause_at" => format!("{:.0}%", self.pause_at),
            "resume_at" => format!("{:.0}%", self.resume_at),
            "max_bypass" => format!("{}s", self.max_bypass),
            "learn_stagger" => format!("{:.0}s", self.learn_stagger),
            "cpu_min_duration" => format!("{:.0}s", self.cpu_min_duration),
            "new_job_mem" => format!("{}M", self.new_job_mem_kb / 1024),
            "status_every" => format!("{}s", self.status_every),
            _ => "?".into(),
        }
    }

    fn setting_bool(&self, key: &str) -> bool {
        match key {
            "enabled" => self.enabled,
            "hints" => self.hints,
            "auto_pause" => self.auto_pause,
            _ => false,
        }
    }

    /// The pause rules, with resume_at kept under pause_at.
    pub fn pause_rules(&self) -> crate::queue::PauseRules {
        crate::queue::PauseRules { on: self.auto_pause, pause_at: self.pause_at, resume_at: self.resume_at.min(self.pause_at - 2.0) }
    }

    fn setting_number(&self, key: &str) -> f64 {
        match key {
            "cpu_max" => self.cpu_max,
            "mem_max" => self.mem_max,
            "pressure_max" => self.pressure_max,
            "pause_at" => self.pause_at,
            "resume_at" => self.resume_at,
            "max_bypass" => self.max_bypass as f64,
            "learn_stagger" => self.learn_stagger,
            "cpu_min_duration" => self.cpu_min_duration,
            "status_every" => self.status_every as f64,
            _ => 0.0,
        }
    }
}

/// The slot ceiling of a pool, as the Config view shows it.
pub fn slots_text(max: Option<u32>) -> String {
    match max {
        None => "no limit".into(),
        Some(1) => "1 slot".into(),
        Some(n) => format!("{n} slots"),
    }
}

/// The repo file that the Config view writes for this worktree: the one that
/// applies here, or a new one at the checkout root.
pub fn repo_file(cwd: &Path, checkout: &Path) -> PathBuf {
    find_repo_file(cwd, checkout).unwrap_or_else(|| checkout.join(".taskguard.toml"))
}

fn read_doc(path: &Path) -> Result<toml_edit::DocumentMut> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.parse().with_context(|| format!("reading {}", path.display()))
}

fn write_doc(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Move a pool's slot ceiling one step: no limit, 1, 2, 3 and so on. Write it
/// in `[pool.NAME]` of the file at `path`. Comments and other settings stay
/// as they are. Returns what changed, in words.
pub fn step_pool(path: &Path, cfg: &Config, name: &str, up: bool) -> Result<String> {
    let cur = cfg.pools.iter().find(|p| p.name == name).and_then(|p| p.max_slots);
    let next = match (cur, up) {
        (None, true) => Some(1),
        (None, false) | (Some(0 | 1), false) => None,
        (Some(n), true) => Some((n + 1).min(64)),
        (Some(n), false) => Some(n - 1),
    };
    let mut doc = read_doc(path)?;
    let pool = pool_table(&mut doc, name)?;
    match next {
        Some(n) => {
            pool.remove("unlimited");
            pool.insert("max_slots", toml_edit::value(n as i64));
        }
        None => {
            pool.remove("max_slots");
            pool.insert("unlimited", toml_edit::value(true));
        }
    }
    write_doc(path, &doc)?;
    Ok(format!("pool {name}: {}", slots_text(next)))
}

/// Add a pool with 1 slot in `[pool.NAME]` of the file at `path`. It matches
/// nothing until it gets a pattern. Names are letters, digits, `-` and `_`.
pub fn new_pool(path: &Path, cfg: &Config, name: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("a pool name is letters, digits, - and _");
    }
    if cfg.pools.iter().any(|p| p.name == name) {
        bail!("pool {name} already exists");
    }
    let mut doc = read_doc(path)?;
    pool_table(&mut doc, name)?.insert("max_slots", toml_edit::value(1));
    write_doc(path, &doc)
}

/// Write the full match list of a pool in `[pool.NAME]` of the file at
/// `path`. A layer's list replaces the lists before it, so it holds every
/// pattern, the built-in ones included. Comments and other settings stay.
pub fn set_pool_patterns(path: &Path, name: &str, patterns: &[String]) -> Result<()> {
    if patterns.is_empty() {
        bail!("pool {name} needs at least one pattern");
    }
    let mut doc = read_doc(path)?;
    let pool = pool_table(&mut doc, name)?;
    let list: toml_edit::Array = patterns.iter().map(String::as_str).collect();
    pool.insert("match", toml_edit::value(list));
    write_doc(path, &doc)
}

/// `[pool.NAME]` of a settings file, made if it is not there yet.
fn pool_table<'a>(doc: &'a mut toml_edit::DocumentMut, name: &str) -> Result<&'a mut dyn toml_edit::TableLike> {
    let pools = doc.entry("pool").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    });
    match pools.as_table_like_mut().and_then(|t| t.entry(name).or_insert(toml_edit::table()).as_table_like_mut()) {
        Some(t) => Ok(t),
        None => bail!("cannot write pool.{name}"),
    }
}

/// Move a setting one step up or down from its current value, and write it at
/// the top level of the file at `path`. Comments and other settings stay as
/// they are. Returns what changed, in words.
pub fn step_setting(path: &Path, cfg: &Config, set: &Setting, up: bool) -> Result<String> {
    let mut doc = read_doc(path)?;
    let value: toml_edit::Item = match set.kind {
        SettingKind::Bool => toml_edit::value(!cfg.setting_bool(set.key)),
        SettingKind::Number(step, lo, hi, _) => {
            let cur = cfg.setting_number(set.key);
            let next = ((cur / step).round() * step + if up { step } else { -step }).clamp(lo, hi);
            // Whole numbers are written as integers: some settings are integers.
            if next.fract() == 0.0 { toml_edit::value(next as i64) } else { toml_edit::value(next) }
        }
        SettingKind::Size(step, lo, hi) => {
            let cur = cfg.new_job_mem_kb / 1024;
            let next = if up { cur + step } else { cur.saturating_sub(step) }.clamp(lo, hi);
            toml_edit::value(format!("{next}M"))
        }
    };
    // Replace only the value, so the comments around it stay.
    match (doc.get_mut(set.key).and_then(|i| i.as_value_mut()), value.into_value()) {
        (Some(cur), Ok(mut new)) => {
            *new.decor_mut() = cur.decor().clone();
            *cur = new;
        }
        (_, Ok(new)) => {
            doc.insert(set.key, toml_edit::Item::Value(new));
        }
        (_, Err(_)) => bail!("cannot write {}", set.key),
    }
    write_doc(path, &doc)?;
    let shown = match set.kind {
        SettingKind::Bool => (if cfg.setting_bool(set.key) { "off" } else { "on" }).to_string(),
        _ => match doc.get(set.key).and_then(|v| v.as_value()) {
            Some(toml_edit::Value::Integer(i)) => i.value().to_string(),
            Some(toml_edit::Value::Float(f)) => f.value().to_string(),
            Some(toml_edit::Value::String(t)) => t.value().clone(),
            _ => String::new(),
        },
    };
    let unit = match set.kind {
        SettingKind::Number(_, _, _, u) => u,
        _ => "",
    };
    Ok(format!("{} = {shown}{unit}", set.key))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn user_config_path() -> PathBuf {
    std::env::var_os("TASKGUARD_CONF").map(PathBuf::from).unwrap_or_else(|| home().join(".config/taskguard/config.toml"))
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("TASKGUARD_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".cache/taskguard"))
}

/// "6G", "512M", "1.5g", "2048K". A bare number is megabytes.
pub fn parse_size_kb(s: &str) -> Result<u64> {
    let t = s.trim();
    let (num, mult) = match t.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&t[..t.len() - 1], 1.0),
        Some('M') => (&t[..t.len() - 1], 1024.0),
        Some('G') => (&t[..t.len() - 1], 1024.0 * 1024.0),
        Some('T') => (&t[..t.len() - 1], 1024.0 * 1024.0 * 1024.0),
        _ => (t, 1024.0),
    };
    let n: f64 = num.trim().trim_end_matches(['b', 'B']).parse().with_context(|| format!("bad size {s:?}"))?;
    if n < 0.0 {
        bail!("bad size {s:?}");
    }
    Ok((n * mult) as u64)
}

impl Config {
    /// Every layer that applies to a job running in `cwd`.
    pub fn load(cwd: &Path, checkout: &Path) -> Result<Config> {
        let mut c = Config::default();
        let user_path = user_config_path();
        let mut dirs: Vec<(String, Layer)> = Vec::new();
        if let Some(user) = read_layer(&user_path, &mut c.unknown)? {
            dirs = user.dir.clone().into_iter().collect();
            c.apply(&user, &user_path.display().to_string());
        }
        // Shortest first, so the longest (most specific) path is applied last.
        dirs.sort_by_key(|(p, _)| p.len());
        for (p, layer) in dirs {
            let p = expand_home(&p);
            if cwd.starts_with(&p) {
                c.apply(&layer, &format!("{} [dir.\"{}\"]", user_path.display(), p.display()));
            }
        }
        if let Some(repo_file) = find_repo_file(cwd, checkout)
            && let Some(layer) = read_layer(&repo_file, &mut c.unknown)?
        {
            if !layer.dir.is_empty() {
                bail!("{}: [dir] sections are only read from the user config", repo_file.display());
            }
            c.apply(&layer, &repo_file.display().to_string());
        }
        Ok(c)
    }

    fn apply(&mut self, l: &Layer, name: &str) {
        self.layers.push(name.to_string());
        macro_rules! set {
            ($f:ident) => {
                if let Some(v) = l.$f.clone() {
                    self.$f = v;
                    self.origin.insert(stringify!($f).into(), name.to_string());
                }
            };
        }
        set!(enabled);
        set!(hints);
        set!(cpu_max);
        set!(mem_max);
        set!(sample_every);
        set!(status_every);
        set!(hist_keep);
        set!(learn_stagger);
        set!(cpu_min_duration);
        set!(pressure_max);
        set!(priority);
        set!(auto_pause);
        set!(pause_at);
        set!(resume_at);
        if let Some(v) = &l.new_job_mem
            && let Ok(kb) = parse_size_kb(v)
        {
            self.new_job_mem_kb = kb;
            self.origin.insert("new_job_mem".into(), name.to_string());
        }
        set!(max_bypass);
        set!(max_backfill);
        set!(boost_runs);
        set!(recorder_idle_exit);
        set!(retention_raw_hours);
        set!(retention_rollup_days);
        for (n, p) in &l.pool {
            let existing = self.pools.iter().position(|x| &x.name == n);
            let base = existing.map(|i| self.pools[i].clone());
            let pool = Pool {
                name: n.clone(),
                max_slots: match (p.max_slots, p.unlimited) {
                    (Some(n), _) => Some(n),
                    (None, Some(true)) => None,
                    _ => base.as_ref().and_then(|b| b.max_slots),
                },
                per_checkout: p.per_checkout.or(base.as_ref().map(|b| b.per_checkout)).unwrap_or(true),
                long_lived: p.long_lived.or(base.as_ref().map(|b| b.long_lived)).unwrap_or(false),
                startup: p.startup.or(base.as_ref().and_then(|b| b.startup)),
                patterns: if p.patterns.is_empty() { base.map(|b| b.patterns).unwrap_or_default() } else { p.patterns.clone() },
            };
            match existing {
                Some(i) => self.pools[i] = pool,
                None => self.pools.push(pool),
            }
            self.origin.insert(format!("pool.{n}"), name.to_string());
        }
        for (n, pats) in &l.label {
            self.labels.retain(|(x, _)| x != n);
            // User labels are checked first.
            self.labels.insert(0, (n.clone(), pats.clone()));
        }
        self.passthrough.extend(l.passthrough.iter().cloned());
        for j in &l.job {
            self.jobs.push((j.clone(), name.to_string()));
        }
    }
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

/// Read one settings file. Settings it names that this version does not know
/// are added to `unknown`, as "file: setting".
fn read_layer(path: &Path, unknown: &mut Vec<String>) -> Result<Option<Layer>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (layer, ignored) = parse_layer(&text).with_context(|| format!("parsing {}", path.display()))?;
    unknown.extend(ignored.into_iter().map(|k| format!("{}: {k}", path.display())));
    Ok(Some(layer))
}

/// A settings file, and the settings in it that this version does not know.
fn parse_layer(text: &str) -> Result<(Layer, Vec<String>)> {
    let mut ignored = Vec::new();
    let de = toml::Deserializer::parse(text)?;
    let layer: Layer = serde_ignored::deserialize(de, |p| ignored.push(p.to_string()))?;
    Ok((layer, ignored))
}

fn find_repo_file(cwd: &Path, checkout: &Path) -> Option<PathBuf> {
    let mut d = Some(cwd);
    while let Some(dir) = d {
        let f = dir.join(".taskguard.toml");
        if f.is_file() {
            return Some(f);
        }
        if dir == checkout {
            break;
        }
        d = dir.parent();
    }
    None
}

/// What the config says about one command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Classified {
    pub passthrough: bool,
    pub pool: Option<(String, Option<u32>, bool)>,
    pub label: Option<String>,
    pub min_cpu: Option<f64>,
    pub min_mem_kb: Option<u64>,
    pub now: bool,
    pub priority: Option<i32>,
    /// Set by a [[job]] rule; otherwise the job's pool decides.
    pub long_lived: Option<bool>,
    pub startup: Option<u64>,
    /// For `doctor --explain`: which rule set what.
    pub why: Vec<String>,
}

impl Config {
    pub fn classify(&self, argv: &[String], key: &str) -> Result<Classified> {
        let mut c = Classified::default();
        if let Some(p) = self.passthrough.iter().find(|p| matcher::matches(p, argv)) {
            c.passthrough = true;
            c.why.push(format!("passthrough: matches {p:?}"));
        }
        if let Some(pool) = self.pools.iter().find(|pool| pool.patterns.iter().any(|p| matcher::matches(p, argv))) {
            c.pool = Some((pool.name.clone(), pool.max_slots, pool.per_checkout));
            c.why.push(format!("pool {}: matches one of {:?}", pool.name, pool.patterns));
        }
        let label_of =
            |argv: &[String]| self.labels.iter().find(|(_, pats)| pats.iter().any(|p| matcher::matches(p, argv))).map(|(n, _)| n.clone());
        c.label = label_of(argv);
        // A shell job is what the heaviest command in it is.
        if c.label.is_none() {
            let rank = |l: &str| builtin::HEAVIEST_FIRST.iter().position(|h| *h == l).unwrap_or(usize::MAX);
            let inside = matcher::shell_commands(argv).iter().filter_map(|cmd| label_of(cmd)).min_by_key(|l| rank(l));
            if let Some(l) = inside {
                c.why.push(format!("kind {l}: the heaviest command in the shell script"));
                c.label = Some(l);
            }
        }
        for (rule, layer) in &self.jobs {
            let hit = match (&rule.pattern, &rule.key) {
                (Some(p), _) if matcher::matches(p, argv) => true,
                (_, Some(k)) if matcher::glob(k, key) => true,
                _ => false,
            };
            if !hit {
                continue;
            }
            let what = rule.pattern.clone().or(rule.key.clone()).unwrap_or_default();
            if let Some(v) = rule.min_cpu {
                c.min_cpu = Some(v);
                c.why.push(format!("min_cpu {v} from [[job]] {what:?} in {layer}"));
            }
            if let Some(v) = &rule.min_mem {
                c.min_mem_kb = Some(parse_size_kb(v)?);
                c.why.push(format!("min_mem {v} from [[job]] {what:?} in {layer}"));
            }
            if let Some(v) = rule.priority {
                c.priority = Some(v);
                c.why.push(format!("priority {v} from [[job]] {what:?} in {layer}"));
            }
            if let Some(v) = rule.now {
                c.now = v;
                c.why.push(format!("now = {v} from [[job]] {what:?} in {layer}"));
            }
            if let Some(p) = &rule.pool {
                let found = self.pools.iter().find(|x| &x.name == p);
                c.pool = Some((p.clone(), found.and_then(|x| x.max_slots), found.map(|x| x.per_checkout).unwrap_or(true)));
                c.why.push(format!("pool {p} from [[job]] {what:?} in {layer}"));
            }
            if let Some(v) = rule.long_lived {
                c.long_lived = Some(v);
                c.why.push(format!("long_lived = {v} from [[job]] {what:?} in {layer}"));
            }
            if let Some(v) = rule.startup {
                c.startup = Some(v);
                c.why.push(format!("startup {v}s from [[job]] {what:?} in {layer}"));
            }
        }
        Ok(c)
    }

    /// A long-lived job, such as a dev stack, runs until someone stops it. It
    /// starts when there is room for its start-up peak, and holds that peak
    /// for its start-up phase. After that its needs follow what it uses, so
    /// a stack that idles at a few percent does not keep the start-up peak
    /// of every core for hours. Returns the start-up phase in seconds, or None
    /// for an ordinary job. A [[job]] rule wins over the job's pool.
    pub fn long_lived(&self, cls: &Classified, pool: Option<&str>) -> Option<f64> {
        let pool = pool.and_then(|p| self.pools.iter().find(|x| x.name == p));
        let on = cls.long_lived.or(pool.map(|p| p.long_lived)).unwrap_or(false);
        on.then(|| cls.startup.or(pool.and_then(|p| p.startup)).unwrap_or(STARTUP_S) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        matcher::split_shell(s)
    }

    /// The shipped example parses as is, and with every commented-out setting
    /// switched on, so it never documents a key that does not exist.
    #[test]
    fn the_example_config_is_valid() {
        let text = include_str!("../taskguard.example.toml");
        assert_eq!(parse_layer(text).expect("as shipped").1, Vec::<String>::new(), "every setting in it exists");
        let on: String = text
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest) if rest.starts_with('[') || rest.split_once(" = ").is_some_and(|(k, _)| !k.contains(' ')) => rest,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (layer, unknown) = parse_layer(&on).expect("with every setting on");
        assert_eq!(unknown, Vec::<String>::new(), "every commented-out setting exists");
        assert!(layer.pool.contains_key("docker"));
        assert_eq!(layer.job.len(), 3);
        assert_eq!(layer.dir.values().next().map(|d| d.job.len()), Some(1));
    }

    #[test]
    fn unknown_settings_are_ignored_and_named() {
        let (layer, unknown) =
            parse_layer("cpu_max = 90\nnew_thing = 3\n[pool.e2e]\nmax_slots = 1\nshiny = true\n[[job]]\nmatch = \"x\"\nlater = 1\n")
                .unwrap();
        assert_eq!(layer.cpu_max, Some(90.0), "the known settings still apply");
        assert_eq!(unknown, vec!["job.0.later", "new_thing", "pool.e2e.shiny"]);
        assert!(parse_layer("cpu_max = \"lots\"").is_err(), "a known setting with a wrong value is still an error");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size_kb("6G").unwrap(), 6 * 1024 * 1024);
        assert_eq!(parse_size_kb("512M").unwrap(), 512 * 1024);
        assert_eq!(parse_size_kb("1.5g").unwrap(), 1024 * 1024 * 3 / 2);
        assert_eq!(parse_size_kb("100").unwrap(), 100 * 1024);
        assert!(parse_size_kb("lots").is_err());
    }

    /// Common commands of a JavaScript monorepo land where they should. Script
    /// paths of one repo belong in that repo's .taskguard.toml, not here.
    #[test]
    fn common_commands() {
        let c = Config::default();
        let cases: &[(&str, Option<&str>, bool, Option<&str>)] = &[
            // command, pool, passthrough, label
            ("tsc -p .", None, false, Some("typecheck")),
            ("bunx --bun tsc --noEmit -p tsconfig.repo-tools.json", None, false, Some("typecheck")),
            ("oxlint --deny-warnings .", None, false, Some("lint")),
            (
                "bun --bun vitest run --config node_modules/@tools/vitest-config/vitest.config.mjs --configLoader native --project unit --testTimeout 60000 tests/unit",
                None,
                false,
                Some("test"),
            ),
            ("bun --bun vitest run --config c.mjs --project integration tests/integration", Some("integration"), false, Some("test")),
            ("bun test --timeout 60000 tests/unit", None, false, Some("test")),
            ("bun build src/main.ts --target bun --compile", None, false, Some("build")),
            ("bunx --bun vite build", None, false, Some("build")),
            ("bun tools/generate.ts", None, false, Some("codegen")),
            ("fumadocs-mdx", None, false, Some("codegen")),
            ("tsr generate", None, false, Some("codegen")),
            ("bun scripts/compile.ts", None, false, None),
            ("bun scripts/hardhat-runtime.ts --max-old-space-size=16384 compile", None, false, None),
            ("bun tools/dpm.ts build --all", None, false, None),
            ("bunx --bun playwright test --config=tests/e2e/playwright.config.ts", Some("e2e"), false, None),
            ("bun web/app/tools/run-app-e2e.ts", None, false, None),
            ("bunx cypress run --spec cypress/e2e", Some("e2e"), false, None),
            ("bun e2e/api/tests/run.ts", None, false, None),
            ("bun testkit/helpers/tools/run-integration.ts --task e2e", None, false, None),
            ("bun src/run-drizzle.ts migrate", None, false, None),
            ("npx prisma migrate deploy", Some("db"), false, None),
            ("bunx --bun drizzle-kit push", Some("db"), false, None),
            ("bun tools/check-boundaries.ts", None, false, Some("check")),
            ("bunx --bun stryker run", None, false, Some("test")),
            ("bunx --bun vite dev", None, true, None),
            ("bunx --bun vite preview", None, true, None),
            ("turbo watch codegen", None, true, None),
            ("devenv up", None, true, None),
            ("bunx --bun playwright test --ui", Some("e2e"), true, None),
            ("tsc -p . --watch", None, true, Some("typecheck")),
            ("tsc --showConfig", None, true, Some("typecheck")),
        ];
        for (cmd, pool, pass, label) in cases {
            let got = c.classify(&argv(cmd), "k").unwrap();
            assert_eq!(got.pool.as_ref().map(|p| p.0.as_str()), *pool, "pool of {cmd}");
            assert_eq!(got.passthrough, *pass, "passthrough of {cmd}");
            if let Some(l) = label {
                assert_eq!(got.label.as_deref(), Some(*l), "label of {cmd}");
            }
        }
    }

    #[test]
    fn layers_and_dir_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let pkg = root.join("packages/api");
        std::fs::create_dir_all(&pkg).unwrap();
        let user = tmp.path().join("config.toml");
        std::fs::write(
            &user,
            format!(
                "hints = false\ncpu_max = 90\n[dir.\"{r}\"]\nmem_max = 70\n[dir.\"{p}\"]\nmem_max = 60\n",
                r = root.display(),
                p = pkg.display()
            ),
        )
        .unwrap();
        std::fs::write(
            root.join(".taskguard.toml"),
            "hints = true\nmax_backfill = 1800\n[[job]]\nmatch = \"vitest run\"\nmin_cpu = 4\nmin_mem = \"6G\"\n",
        )
        .unwrap();
        // SAFETY: tests in this module do not read TASKGUARD_CONF concurrently.
        unsafe { std::env::set_var("TASKGUARD_CONF", &user) };
        let c = Config::load(&pkg, &root).unwrap();
        unsafe { std::env::remove_var("TASKGUARD_CONF") };
        assert!(c.hints, "the repo file beats the user config");
        assert_eq!(c.cpu_max, 90.0);
        assert_eq!(c.mem_max, 60.0, "the longest [dir] path wins");
        let cl = c.classify(&argv("bun --bun vitest run tests/unit"), "k").unwrap();
        assert_eq!(cl.min_cpu, Some(4.0));
        assert_eq!(cl.min_mem_kb, Some(6 * 1024 * 1024));
        assert!(c.origin["hints"].ends_with(".taskguard.toml"));
        assert_eq!((Config::default().max_backfill, c.max_backfill), (0, 1800), "backfill is off unless a layer sets it");
    }

    #[test]
    fn a_setting_changed_in_the_dashboard_keeps_the_rest_of_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "# my limits\ncpu_max = 80 # leave room for the editor\n\n[dir.\"/w\"]\nmem_max = 70\n").unwrap();
        let cfg = Config { cpu_max: 80.0, ..Default::default() };
        let set = SETTINGS.iter().find(|s| s.key == "cpu_max").unwrap();
        assert_eq!(step_setting(&path, &cfg, set, false).unwrap(), "cpu_max = 75%");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my limits") && text.contains("# leave room for the editor"), "{text}");
        assert!(text.contains("cpu_max = 75") && text.contains("mem_max = 70"), "{text}");
        let hints = SETTINGS.iter().find(|s| s.key == "hints").unwrap();
        assert_eq!(step_setting(&path, &cfg, hints, true).unwrap(), "hints = off");
        let pause = SETTINGS.iter().find(|s| s.key == "auto_pause").unwrap();
        assert_eq!(step_setting(&path, &cfg, pause, true).unwrap(), "auto_pause = on");
        let size = SETTINGS.iter().find(|s| s.key == "new_job_mem").unwrap();
        assert_eq!(step_setting(&path, &cfg, size, true).unwrap(), "new_job_mem = 1792M");
        let c = read_layer(&path, &mut Vec::new()).unwrap().unwrap();
        assert_eq!((c.cpu_max, c.hints, c.new_job_mem.as_deref()), (Some(75.0), Some(false), Some("1792M")));
        assert_eq!(c.auto_pause, Some(true));
    }

    #[test]
    fn a_pool_ceiling_steps_through_no_limit_and_keeps_the_rest_of_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".taskguard.toml");
        std::fs::write(&path, "# repo limits\ncpu_max = 80\n\n[pool.e2e]\nmatch = [\"playwright test\"] # ours\n").unwrap();
        let load = || {
            let mut c = Config::default();
            c.apply(&read_layer(&path, &mut Vec::new()).unwrap().unwrap(), "repo");
            c
        };
        let slots = |c: &Config, n: &str| c.pools.iter().find(|p| p.name == n).unwrap().max_slots;
        assert_eq!(step_pool(&path, &load(), "db", false).unwrap(), "pool db: no limit");
        assert_eq!(slots(&load(), "db"), None, "unlimited = true takes away the built-in ceiling");
        assert_eq!(step_pool(&path, &load(), "db", true).unwrap(), "pool db: 1 slot");
        assert_eq!(step_pool(&path, &load(), "db", true).unwrap(), "pool db: 2 slots");
        assert_eq!(step_pool(&path, &load(), "e2e", true).unwrap(), "pool e2e: 2 slots");
        let c = load();
        assert_eq!((slots(&c, "db"), slots(&c, "e2e"), slots(&c, "integration")), (Some(2), Some(2), Some(1)));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# repo limits") && text.contains("# ours") && text.contains("cpu_max = 80"), "{text}");
        assert!(!text.contains("unlimited"), "a number replaces unlimited:\n{text}");
        assert_eq!(c.pools.iter().find(|p| p.name == "e2e").unwrap().patterns, vec!["playwright test"]);
        let (_, unknown) = parse_layer(&text).unwrap();
        assert_eq!(unknown, Vec::<String>::new());
    }

    #[test]
    fn long_lived_comes_from_the_pool_or_a_job_rule() {
        let mut c = Config::default();
        let (layer, unknown) = parse_layer(
            "[pool.stack]\nmax_slots = 4\nlong_lived = true\nmatch = [\"dev-env up\"]\n\
             [pool.browser]\nlong_lived = true\nstartup = 60\n\
             [[job]]\nmatch = \"dev-env up --seed-only\"\nlong_lived = false\n\
             [[job]]\nmatch = \"storybook-server\"\nlong_lived = true\nstartup = 30\n",
        )
        .unwrap();
        assert_eq!(unknown, Vec::<String>::new());
        c.apply(&layer, "repo");
        let long = |c: &Config, cmd: &str, pool: Option<&str>| c.long_lived(&c.classify(&argv(cmd), "k").unwrap(), pool);
        assert_eq!(long(&c, "dev-env up", Some("stack")), Some(STARTUP_S as f64), "the pool's, with the default start-up");
        assert_eq!(long(&c, "scenarios", Some("browser")), Some(60.0), "a pool named with --id");
        assert_eq!(long(&c, "dev-env up --seed-only", Some("stack")), None, "a [[job]] rule wins over its pool");
        assert_eq!(long(&c, "storybook-server", None), Some(30.0));
        assert_eq!(long(&c, "tsc -p .", None), None, "ordinary jobs are not");
        assert_eq!(long(&c, "tsc -p .", Some("db")), None);
        // A later layer that only changes the ceiling keeps the mode.
        c.apply(&parse_layer("[pool.stack]\nmax_slots = 2\n").unwrap().0, "user");
        assert_eq!(long(&c, "dev-env up", Some("stack")), Some(STARTUP_S as f64));
    }

    #[test]
    fn max_slots_wins_over_unlimited_in_one_section() {
        let mut c = Config::default();
        c.apply(&parse_layer("[pool.db]\nmax_slots = 3\nunlimited = true\n[pool.e2e]\nunlimited = true\n").unwrap().0, "x");
        let slots = |n: &str| c.pools.iter().find(|p| p.name == n).unwrap().max_slots;
        assert_eq!((slots("db"), slots("e2e"), slots("integration")), (Some(3), None, Some(1)));
    }

    #[test]
    fn a_shell_script_gets_the_kind_of_its_heaviest_command() {
        let c = Config::default();
        let kind = |line: &str| c.classify(&argv(line), "k").unwrap().label;
        assert_eq!(kind("bun run ci:local").as_deref(), Some("ci"));
        assert_eq!(
            kind(
                "sh -c 'git fetch -q origin main && git merge --no-edit origin/main && bun install --frozen-lockfile && bun run ci:local'"
            )
            .as_deref(),
            Some("ci")
        );
        assert_eq!(kind("bash -c 'cd packages/api && bunx tsc --noEmit && bunx vitest run'").as_deref(), Some("test"));
        assert_eq!(kind("bunx vitest run").as_deref(), Some("test"));
        assert_eq!(kind("sh -c 'git fetch'"), None);
    }
}
