//! Proof receipts: record that a command passed on an exact tree of files, and
//! publish that as a GitHub commit status, so CI can skip the work a machine
//! already did. See `taskguard help proof`.
//!
//! The tree is hashed as it is on disk, uncommitted and untracked files
//! included, through a temporary index: the same hash a commit of exactly
//! those files gets. A receipt only counts for a commit whose tree is that
//! hash. GitHub talks go through the `gh` CLI (`TASKGUARD_GH` overrides it).

use crate::config;
use crate::db::{self, Db};
use crate::matcher;
use crate::report::dur;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The policy file, at the root of the checkout.
pub const POLICY: &str = ".taskguard/proof.toml";
/// Commit status contexts are `taskguard/<id>`.
pub const CONTEXT: &str = "taskguard/";
/// The first line of the push hook that `install` writes; it marks the hook as ours.
const HOOK_MARK: &str = "# taskguard proof: publish receipts once the pushed commits reach GitHub.";
/// GitHub cuts a status description at 140 characters.
const DESC_MAX: usize = 140;

// --- policy ---------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct Policy {
    #[serde(default)]
    pub receipt: BTreeMap<String, Rule>,
}

/// What one receipt id runs, and what proof of it CI accepts.
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Rule {
    /// The full command. A receipt of exactly this command is `full`.
    pub command: String,
    /// The OS whose proof CI accepts: any, linux or macos.
    #[serde(default = "default_os")]
    pub os: String,
    /// The levels that let a PR skip CI: full, partial.
    #[serde(default = "default_levels")]
    pub levels: Vec<String>,
    /// deny: no receipt while an ignored `.env*` file exists; allow: ignore them.
    #[serde(default = "default_env_files")]
    pub env_files: String,
    /// Ignored `.env*` files that do not count with `env_files = "deny"` (globs).
    #[serde(default)]
    pub env_allow: Vec<String>,
}

fn default_os() -> String {
    "any".into()
}
fn default_levels() -> Vec<String> {
    vec!["full".into()]
}
fn default_env_files() -> String {
    "deny".into()
}

impl Policy {
    pub fn parse(text: &str, origin: &str) -> Result<Policy> {
        let p: Policy = toml::from_str(text).with_context(|| format!("reading {origin}"))?;
        for (id, r) in &p.receipt {
            if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                bail!("{origin}: receipt id {id:?} may only hold letters, digits, - _ and .");
            }
            if r.command.trim().is_empty() {
                bail!("{origin}: [receipt.{id}] needs a command");
            }
            if !matches!(r.os.as_str(), "any" | "linux" | "macos") {
                bail!("{origin}: [receipt.{id}] os is any, linux or macos, not {:?}", r.os);
            }
            if r.levels.is_empty() || r.levels.iter().any(|l| !matches!(l.as_str(), "full" | "partial")) {
                bail!("{origin}: [receipt.{id}] levels lists full and/or partial");
            }
            if !matches!(r.env_files.as_str(), "deny" | "allow") {
                bail!("{origin}: [receipt.{id}] env_files is deny or allow, not {:?}", r.env_files);
            }
        }
        Ok(p)
    }

    pub fn load(root: &Path) -> Result<Policy> {
        let path = root.join(POLICY);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("no {POLICY} in {}; `taskguard proof install ID --command \"CMD\"` writes one", root.display()))?;
        Policy::parse(&text, &path.display().to_string())
    }

    pub fn rule(&self, id: &str) -> Result<&Rule> {
        self.receipt.get(id).with_context(|| {
            let known: Vec<&str> = self.receipt.keys().map(String::as_str).collect();
            format!("no receipt id {id:?} in {POLICY} (it has: {})", if known.is_empty() { "none".into() } else { known.join(", ") })
        })
    }
}

/// The level of a receipt for `cmd`: full when it is the policy command, as
/// argv or as the `sh -c` line that `taskguard --receipt ID` runs.
pub fn level_of(rule: &Rule, cmd: &[String], partial: Option<&str>) -> Result<(&'static str, Option<String>)> {
    if let Some(reason) = partial {
        if reason.trim().is_empty() {
            bail!("--partial needs a reason: what ran, and why that is enough");
        }
        return Ok(("partial", Some(reason.trim().to_string())));
    }
    let full = cmd.is_empty()
        || *cmd == matcher::split_shell(&rule.command)
        || (cmd.len() == 3 && cmd[0] == "sh" && cmd[1] == "-c" && cmd[2] == rule.command);
    if !full {
        bail!(
            "this is not the policy command, so it proves nothing in full\n  policy:  {}\n  command: {}\nRun `taskguard --receipt ID` alone to run the policy command, or add --partial \"REASON\" for a chosen subset.",
            rule.command,
            cmd.join(" ")
        );
    }
    Ok(("full", None))
}

// --- git ------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).output().context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

pub fn toplevel(cwd: &Path) -> Result<PathBuf> {
    let top = git(cwd, &["rev-parse", "--show-toplevel"]).context("a receipt needs a git checkout")?;
    Ok(PathBuf::from(top))
}

fn tree_of(root: &Path, rev: &str) -> Result<String> {
    git(root, &["rev-parse", &format!("{rev}^{{tree}}")])
}

fn commit_of(root: &Path, rev: &str) -> Result<String> {
    git(root, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

/// The tree hash of the files on disk, uncommitted and untracked files
/// included, ignored files left out. A copy of the index takes the `git add`,
/// so the real index does not change.
pub fn work_tree(root: &Path) -> Result<String> {
    let index = PathBuf::from(git(root, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("taskguard-index-{}-{nanos}", std::process::id()));
    if index.exists() {
        std::fs::copy(&index, &tmp).with_context(|| format!("copying {}", index.display()))?;
    }
    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_INDEX_FILE", &tmp)
            .stdin(Stdio::null())
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let tree = run(&["add", "-A", "."]).and_then(|_| run(&["write-tree"]));
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("lock"));
    tree
}

/// Ignored `.env*` files: local settings that change a result but are not in
/// the tree hash. Ignored folders count as a whole (`node_modules/`), so a
/// `.env` inside one does not show.
fn ignored_env_files(root: &Path, allow: &[String]) -> Result<Vec<String>> {
    let out = git(root, &["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory", "--no-empty-directory"])?;
    Ok(out
        .split('\0')
        .filter(|p| !p.is_empty() && !p.ends_with('/'))
        .filter(|p| p.rsplit('/').next().is_some_and(|name| name.starts_with(".env")))
        .filter(|p| !allow.iter().any(|g| matcher::glob(g, p)))
        .map(str::to_string)
        .collect())
}

// --- the machine ----------------------------------------------------------------

fn host() -> String {
    if std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true") {
        return "ci".into();
    }
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    let name = if ok { String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(0)]).into_owned() } else { String::new() };
    let name = name.split('.').next().unwrap_or("").to_string();
    if name.is_empty() { "unknown".into() } else { name }
}

// --- receipts -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Receipt {
    pub row: i64,
    pub receipt: String,
    pub tree: String,
    pub head: Option<String>,
    pub repo: Option<String>,
    pub level: String,
    pub reason: Option<String>,
    pub ok: bool,
    pub exit: Option<i32>,
    pub cmd: String,
    pub host: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub started_at: f64,
    pub duration_s: f64,
}

impl Receipt {
    /// The status description, in the fixed shape `parse_description` reads
    /// back: `full, macos/aarch64, 12m03s, host` and `: reason` for a partial.
    pub fn description(&self) -> String {
        let mut s = format!("{}, {}/{}, {}, {}", self.level, self.os, self.arch, dur(self.duration_s), self.host);
        if let Some(r) = &self.reason {
            s.push_str(": ");
            s.push_str(r);
        }
        if s.chars().count() > DESC_MAX {
            s = s.chars().take(DESC_MAX - 1).collect::<String>() + "…";
        }
        s
    }

    pub fn state(&self) -> &'static str {
        if self.ok { "success" } else { "failure" }
    }
}

/// The level and OS of a `taskguard/<id>` status, from its description.
pub fn parse_description(d: &str) -> Option<(String, String)> {
    let mut parts = d.splitn(3, ", ");
    let level = parts.next()?;
    let os = parts.next()?.split('/').next()?;
    matches!(level, "full" | "partial").then(|| (level.to_string(), os.to_string()))
}

fn insert(db: &Db, r: &Receipt) -> Result<i64> {
    db.conn.execute(
        "INSERT INTO receipts (receipt, tree, head, repo, level, reason, ok, exit, cmd, host, os, arch, version, started_at, duration_s)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            r.receipt,
            r.tree,
            r.head,
            r.repo,
            r.level,
            r.reason,
            r.ok,
            r.exit,
            r.cmd,
            r.host,
            r.os,
            r.arch,
            r.version,
            r.started_at,
            r.duration_s
        ],
    )?;
    Ok(db.conn.last_insert_rowid())
}

/// The receipts made on any of these trees, newest first.
pub fn receipts_for(db: &Db, trees: &[String]) -> Result<Vec<Receipt>> {
    let mut out = Vec::new();
    let mut stmt = db.conn.prepare_cached(
        "SELECT id, receipt, tree, head, repo, level, reason, ok, exit, cmd, host, os, arch, version, started_at, duration_s
         FROM receipts WHERE tree = ?1",
    )?;
    for t in trees {
        let rows = stmt.query_map(params![t], |r| {
            Ok(Receipt {
                row: r.get(0)?,
                receipt: r.get(1)?,
                tree: r.get(2)?,
                head: r.get(3)?,
                repo: r.get(4)?,
                level: r.get(5)?,
                reason: r.get(6)?,
                ok: r.get(7)?,
                exit: r.get(8)?,
                cmd: r.get(9)?,
                host: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                os: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                arch: r.get::<_, Option<String>>(12)?.unwrap_or_default(),
                version: r.get::<_, Option<String>>(13)?.unwrap_or_default(),
                started_at: r.get(14)?,
                duration_s: r.get(15)?,
            })
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    out.sort_by(|a, b| b.started_at.partial_cmp(&a.started_at).unwrap_or(std::cmp::Ordering::Equal).then(b.row.cmp(&a.row)));
    Ok(out)
}

/// One receipt per id: the strongest level, then the newest. A partial run
/// on the same files does not hide a full one; a newer full run, green or
/// red, does replace an older one.
fn strongest_per_id(mut rs: Vec<Receipt>) -> Vec<Receipt> {
    rs.sort_by_key(|r| r.level != "full");
    let mut seen = std::collections::HashSet::new();
    rs.into_iter().filter(|r| seen.insert(r.receipt.clone())).collect()
}

fn posted_to(db: &Db, row: i64, sha: &str) -> bool {
    db.conn
        .prepare_cached("SELECT 1 FROM receipt_posts WHERE receipt_row = ?1 AND sha = ?2")
        .and_then(|mut s| s.exists(params![row, sha]))
        .unwrap_or(false)
}

fn mark_posted(db: &Db, row: i64, sha: &str) -> Result<()> {
    db.conn.execute("INSERT INTO receipt_posts (receipt_row, sha, posted_at) VALUES (?1, ?2, ?3)", params![row, sha, db::now()])?;
    Ok(())
}

// --- the receipt run ------------------------------------------------------------

/// `taskguard --receipt ID [--partial REASON] [-- COMMAND]`: run the command
/// (the policy command when none is given) and, when the files did not change
/// while it ran, keep a receipt of the result. The command runs straight away,
/// outside the queue: it is mostly a task runner whose own leaves queue.
pub fn run_receipt(id: &str, partial: Option<&str>, publish: Option<u64>, cmd: &[String]) -> Result<i32> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let root = toplevel(&cwd)?;
    let policy = Policy::load(&root)?;
    let rule = policy.rule(id)?;
    let (level, reason) = level_of(rule, cmd, partial)?;
    if rule.env_files == "deny" {
        let env = ignored_env_files(&root, &rule.env_allow)?;
        if !env.is_empty() {
            bail!(
                "no receipt: ignored settings files can change the result, and the proof cannot see them: {}\nMove them away, or list them in env_allow of [receipt.{id}] in {POLICY}.",
                env.join(", ")
            );
        }
    }
    let argv: Vec<String> = if cmd.is_empty() { vec!["sh".into(), "-c".into(), rule.command.clone()] } else { cmd.to_vec() };

    let before = work_tree(&root)?;
    let head = commit_of(&root, "HEAD").ok();
    let started_at = db::now();
    let t0 = Instant::now();
    let code = spawn_and_wait(&argv)?;
    let duration_s = t0.elapsed().as_secs_f64();
    let after = work_tree(&root)?;

    let Some(exit) = code else {
        crate::report::say(&format!("no receipt for {id}: the command was stopped by a signal"));
        return Ok(130);
    };
    if before != after {
        crate::report::say(&format!(
            "no receipt for {id}: files changed while it ran (tree {} before, {} after), so nobody knows which version {}",
            short(&before),
            short(&after),
            if exit == 0 { "passed" } else { "failed" }
        ));
        return Ok(exit);
    }
    let r = Receipt {
        row: 0,
        receipt: id.to_string(),
        tree: before.clone(),
        head,
        repo: Some(root.display().to_string()),
        level: level.to_string(),
        reason,
        ok: exit == 0,
        exit: Some(exit),
        cmd: argv.join(" "),
        host: host(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at,
        duration_s,
    };
    let db = Db::open_dir(&config::state_dir())?;
    insert(&db, &r)?;
    let next = match publish {
        Some(secs) => {
            let (log, how) = spawn_publish(&root, &r.tree, secs)?;
            format!("It is posted {how}, for up to {} (log: {}).", dur(secs as f64), log.display())
        }
        None => "After you push, `taskguard proof publish` posts it.".into(),
    };
    crate::report::say(&format!(
        "receipt {id}: {} {} on tree {} ({}). {next}",
        r.level,
        if r.ok { "passed" } else { "failed" },
        short(&r.tree),
        dur(duration_s)
    ));
    Ok(exit)
}

/// Run argv with the terminal's stdio. None when a signal stopped it.
fn spawn_and_wait(argv: &[String]) -> Result<Option<i32>> {
    use std::os::unix::process::ExitStatusExt;
    let term = Arc::new(AtomicBool::new(false));
    let int = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(libc::SIGTERM, term.clone());
    let _ = signal_hook::flag::register(libc::SIGHUP, term.clone());
    // Ctrl-C reaches the command through the terminal; taskguard stays to see it end.
    let _ = signal_hook::flag::register(libc::SIGINT, int.clone());
    let mut child = Command::new(&argv[0]).args(&argv[1..]).spawn().with_context(|| format!("cannot run {}", argv[0]))?;
    let mut sent = false;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(match (st.code(), st.signal()) {
                (Some(c), _) => Some(c),
                _ => None,
            });
        }
        if term.load(Ordering::Relaxed) && !sent {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            sent = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// --- GitHub ---------------------------------------------------------------------

fn gh_bin() -> String {
    std::env::var("TASKGUARD_GH").unwrap_or_else(|_| "gh".into())
}

/// `gh api ...` in the checkout, so `{owner}/{repo}` resolves from its remote.
fn gh_api(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(gh_bin())
        .arg("api")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {} (the GitHub CLI)", gh_bin()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = String::from_utf8_lossy(&out.stdout);
        bail!("gh api {}: {}", args.first().copied().unwrap_or(""), if err.trim().is_empty() { msg.trim() } else { err.trim() });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Status {
    pub context: String,
    pub state: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub target_url: Option<String>,
    #[serde(default)]
    pub creator: Option<Creator>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct Creator {
    pub login: String,
}

impl Status {
    fn who(&self) -> &str {
        self.creator.as_ref().map(|c| c.login.as_str()).unwrap_or("?")
    }
}

/// Every `taskguard/*` status of a commit, newest first.
fn statuses(root: &Path, sha: &str) -> Result<Vec<Status>> {
    let out = gh_api(root, &["--paginate", &format!("repos/{{owner}}/{{repo}}/commits/{sha}/statuses"), "--jq", ".[]"])?;
    let mut v = Vec::new();
    for s in serde_json::Deserializer::from_str(&out).into_iter::<Status>() {
        let s = s.context("reading the statuses from GitHub")?;
        if s.context.starts_with(CONTEXT) {
            v.push(s);
        }
    }
    v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(v)
}

/// The newest status per context: the one GitHub shows and requires.
fn newest(statuses: &[Status]) -> BTreeMap<String, Status> {
    let mut m = BTreeMap::new();
    for s in statuses {
        m.entry(s.context.trim_start_matches(CONTEXT).to_string()).or_insert_with(|| s.clone());
    }
    m
}

fn on_github(root: &Path, sha: &str) -> bool {
    gh_api(root, &[&format!("repos/{{owner}}/{{repo}}/commits/{sha}"), "--jq", ".sha"]).is_ok()
}

fn post_status(root: &Path, sha: &str, id: &str, state: &str, desc: &str) -> Result<()> {
    let path = format!("repos/{{owner}}/{{repo}}/statuses/{sha}");
    let (st, ctx, de) = (format!("state={state}"), format!("context={CONTEXT}{id}"), format!("description={desc}"));
    let mut args = vec!["-X", "POST", &path, "-f", &st, "-f", &ctx, "-f", &de, "--silent"];
    let url;
    if let (Ok(server), Ok(repo), Ok(run)) =
        (std::env::var("GITHUB_SERVER_URL"), std::env::var("GITHUB_REPOSITORY"), std::env::var("GITHUB_RUN_ID"))
    {
        url = format!("target_url={server}/{repo}/actions/runs/{run}");
        args.extend(["-f", &url]);
    }
    gh_api(root, &args).map(|_| ())
}

// --- subcommands ----------------------------------------------------------------

struct Args {
    flags: BTreeMap<String, Vec<String>>,
    pos: Vec<String>,
}

/// Flags with values; `switches` take none.
fn parse(rest: &[String], with_value: &[&str], switches: &[&str]) -> Result<Args> {
    let mut a = Args { flags: BTreeMap::new(), pos: Vec::new() };
    let mut i = 0;
    while i < rest.len() {
        let s = rest[i].as_str();
        let (name, inline) = match s.split_once('=') {
            Some((n, v)) if s.starts_with("--") => (n, Some(v.to_string())),
            _ => (s, None),
        };
        if with_value.contains(&name) {
            let v = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    rest.get(i).cloned().with_context(|| format!("{name} needs a value"))?
                }
            };
            a.flags.entry(name.to_string()).or_default().push(v);
        } else if switches.contains(&name) {
            a.flags.entry(name.to_string()).or_default();
        } else if s.starts_with('-') {
            bail!("unknown option {s}; see `taskguard help proof`");
        } else {
            a.pos.push(s.to_string());
        }
        i += 1;
    }
    Ok(a)
}

impl Args {
    fn has(&self, f: &str) -> bool {
        self.flags.contains_key(f)
    }
    fn one(&self, f: &str) -> Option<&str> {
        self.flags.get(f).and_then(|v| v.last()).map(String::as_str)
    }
    fn all(&self, f: &str) -> Vec<String> {
        self.flags.get(f).cloned().unwrap_or_default()
    }
    fn secs(&self, f: &str) -> Result<u64> {
        self.one(f).map(|v| v.parse().with_context(|| format!("{f} needs a number of seconds"))).transpose().map(|v| v.unwrap_or(0))
    }
}

pub fn dispatch(rest: &[String]) -> Result<i32> {
    let sub = rest.first().map(String::as_str).unwrap_or("");
    let args = &rest[1.min(rest.len())..];
    match sub {
        "publish" => publish(args),
        "show" => show(args),
        "check" => check(args),
        "log" => log(args),
        "install" => install(args),
        "uninstall" => uninstall(args),
        "" => bail!("proof needs a command: publish, show, check, log, install or uninstall; see `taskguard help proof`"),
        other => bail!("no proof command {other:?}: publish, show, check, log, install or uninstall"),
    }
}

fn root_here() -> Result<PathBuf> {
    toplevel(&std::env::current_dir().context("reading the current directory")?)
}

/// `proof publish [--sha SHA]... [--wait SECS] [--again]`
///
/// Without --sha it follows HEAD for up to --wait seconds: it posts as soon as
/// HEAD has a receipt and is on GitHub. That fits a pipeline that runs the
/// receipt, then may add commits, then pushes from the same checkout.
fn publish(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &["--sha", "--wait", "--tree", "--branch", "--repo"], &["--again", "-q", "--quiet"])?;
    let quiet = a.has("-q") || a.has("--quiet");
    let again = a.has("--again");
    if let Some(tree) = a.one("--tree") {
        let branch = a.one("--branch").context("--tree goes with --branch")?;
        return publish_tree(tree, branch, a.one("--repo"), a.secs("--wait")?, again, quiet);
    }
    let root = root_here()?;
    let policy = Policy::load(&root)?;
    let wait = a.secs("--wait")?;
    let db = Db::open_dir(&config::state_dir())?;
    let t0 = Instant::now();
    let explicit = a.all("--sha");
    if explicit.is_empty() {
        loop {
            let head = commit_of(&root, "HEAD")?;
            let rs = unposted(&db, &policy, &root, &head, again)?;
            let ready = !rs.is_empty() && on_github(&root, &head);
            if ready {
                post_all(&db, &root, &head, rs, quiet)?;
                return Ok(0);
            }
            if t0.elapsed().as_secs() >= wait {
                if rs.is_empty() {
                    if !quiet {
                        println!("{}: nothing to publish: no new receipt for its files", short(&head));
                    }
                    return Ok(0);
                }
                bail!("commit {} is not on GitHub yet; push it first, or give --wait SECS", short(&head));
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }
    for s in explicit {
        let sha = commit_of(&root, &s)?;
        let rs = unposted(&db, &policy, &root, &sha, again)?;
        if rs.is_empty() {
            if !quiet {
                println!("{}: nothing to publish: no new receipt for its files", short(&sha));
            }
            continue;
        }
        while !on_github(&root, &sha) {
            if t0.elapsed().as_secs() >= wait {
                bail!("commit {} is not on GitHub yet; push it first, or give --wait SECS", short(&sha));
            }
            std::thread::sleep(Duration::from_secs(3));
        }
        post_all(&db, &root, &sha, rs, quiet)?;
    }
    Ok(0)
}

/// The receipts for the files of `sha` that are not posted on it yet.
fn unposted(db: &Db, policy: &Policy, root: &Path, sha: &str, again: bool) -> Result<Vec<Receipt>> {
    let mut trees = vec![tree_of(root, sha)?];
    // CI checks out a merge of the PR head onto its base: what passed there
    // passed on the head merged with the newest base, which counts for the head.
    let head = commit_of(root, "HEAD")?;
    let parents = git(root, &["rev-parse", "HEAD^@"]).unwrap_or_default();
    if sha != head && parents.lines().count() >= 2 && parents.lines().any(|p| p == sha) {
        trees.push(tree_of(root, "HEAD")?);
    }
    Ok(strongest_per_id(receipts_for(db, &trees)?)
        .into_iter()
        .filter(|r| policy.receipt.contains_key(&r.receipt))
        .filter(|r| again || !posted_to(db, r.row, sha))
        .collect())
}

fn post_all(db: &Db, root: &Path, sha: &str, rs: Vec<Receipt>, quiet: bool) -> Result<()> {
    for r in rs {
        let desc = r.description();
        post_status(root, sha, &r.receipt, r.state(), &desc)?;
        mark_posted(db, r.row, sha)?;
        if !quiet {
            println!("{}: posted {CONTEXT}{} {}: {desc}", short(sha), r.receipt, r.state());
        }
    }
    Ok(())
}

/// `proof publish --tree TREE --branch BRANCH [--repo OWNER/REPO] --wait SECS`:
/// no checkout needed. Watch the branch on GitHub and post the receipts of
/// TREE on its head once that head has those files. A pipeline that deletes
/// its checkout right after the push (no-mistakes) can still publish.
fn publish_tree(tree: &str, branch: &str, repo: Option<&str>, wait: u64, again: bool, quiet: bool) -> Result<i32> {
    if let Some(r) = repo {
        // gh fills {owner}/{repo} from GH_REPO; nothing else runs yet.
        unsafe { std::env::set_var("GH_REPO", r) };
    }
    let dir = std::env::temp_dir();
    let db = Db::open_dir(&config::state_dir())?;
    let t0 = Instant::now();
    loop {
        let head = gh_api(
            &dir,
            &[&format!("repos/{{owner}}/{{repo}}/branches/{branch}"), "--jq", ".commit.sha + \" \" + .commit.commit.tree.sha"],
        );
        if let Ok(line) = &head
            && let Some((sha, head_tree)) = line.trim().split_once(' ')
            && head_tree == tree
        {
            let rs: Vec<Receipt> = strongest_per_id(receipts_for(&db, &[tree.to_string()])?)
                .into_iter()
                .filter(|r| again || !posted_to(&db, r.row, sha))
                .collect();
            if rs.is_empty() && !quiet {
                println!("{}: nothing to publish: no new receipt for its files", short(sha));
            }
            post_all(&db, &dir, sha, rs, quiet)?;
            return Ok(0);
        }
        if t0.elapsed().as_secs() >= wait {
            let now = head.map(|l| l.trim().to_string()).unwrap_or_else(|e| format!("{e:#}"));
            bail!("{branch} on GitHub never had tree {} within {wait}s (last seen: {now})", short(tree));
        }
        std::thread::sleep(Duration::from_secs(10));
    }
}

/// `--receipt ID --publish SECS`: publish in the background, for up to SECS.
/// On a branch it watches that branch on GitHub for the tested files, so the
/// checkout may go away after the push; detached, it follows HEAD in the
/// checkout. GH_REPO names the repo when the checkout's remote is not GitHub.
/// The output goes to a log in the temp folder.
fn spawn_publish(root: &Path, tree: &str, secs: u64) -> Result<(PathBuf, String)> {
    use std::os::unix::process::CommandExt;
    let log = std::env::temp_dir().join("taskguard-proof-publish.log");
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
    let branch = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    let repo = std::env::var("GH_REPO").ok().filter(|r| !r.is_empty()).or_else(|| {
        let o = Command::new(gh_bin())
            .args(["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
            .current_dir(root)
            .output()
            .ok()?;
        let r = String::from_utf8_lossy(&o.stdout).trim().to_string();
        (o.status.success() && !r.is_empty()).then_some(r)
    });
    let how = match (&branch, &repo) {
        (Some(b), Some(r)) => {
            cmd.args(["proof", "publish", "--tree", tree, "--branch", b, "--repo", r, "--wait", &secs.to_string()])
                .current_dir(std::env::temp_dir());
            format!("when {b} on {r} has these files")
        }
        _ => {
            cmd.args(["proof", "publish", "--wait", &secs.to_string()]).current_dir(root);
            "when HEAD has these files and is on GitHub".to_string()
        }
    };
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting the background publish")?;
    Ok((log, how))
}

/// What CI does for one receipt id, given the newest status on the commit.
pub fn decide(id: &str, rule: &Rule, status: Option<&Status>, labels: &[String]) -> (bool, String) {
    if labels.iter().any(|l| l == "taskguard:ci" || *l == format!("taskguard:ci:{id}")) {
        return (false, "forced by a taskguard:ci label".into());
    }
    let Some(s) = status else { return (false, "no proof yet".into()) };
    let desc = s.description.clone().unwrap_or_default();
    match s.state.as_str() {
        "success" => {}
        "failure" | "error" => return (false, format!("red from {}: {desc}; CI runs it to see for itself", s.who())),
        other => return (false, format!("{other} from {}", s.who())),
    }
    let Some((level, os)) = parse_description(&desc) else {
        return (false, format!("a status this taskguard cannot read: {desc:?}"));
    };
    if !rule.levels.contains(&level) {
        return (false, format!("{level} proof, the policy accepts {}", rule.levels.join(", ")));
    }
    if rule.os != "any" && rule.os != os {
        return (false, format!("proof on {os}, the policy wants {}", rule.os));
    }
    (true, format!("{desc} (posted by {})", s.who()))
}

/// `proof check [ID...] [--sha SHA] [--labels L,L] [--wait SECS] [--github-output]`
fn check(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &["--sha", "--labels", "--wait"], &["--github-output"])?;
    let root = root_here()?;
    // With --github-output a missing policy or id is not an error: it means
    // nothing is proven, so CI runs. A branch made before the policy has none.
    let policy = match Policy::load(&root) {
        Err(e) if a.has("--github-output") => {
            eprintln!("taskguard: {e:#}; so CI runs");
            Policy::default()
        }
        p => p?,
    };
    let ids: Vec<String> = if a.pos.is_empty() { policy.receipt.keys().cloned().collect() } else { a.pos.clone() };
    for id in &ids {
        if let Err(e) = policy.rule(id) {
            if !a.has("--github-output") {
                return Err(e);
            }
            eprintln!("taskguard: {e:#}; so CI runs {id}");
        }
    }
    let sha = match a.one("--sha") {
        Some(s) if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) => s.to_string(),
        Some(s) => commit_of(&root, s)?,
        None => commit_of(&root, "HEAD")?,
    };
    let labels: Vec<String> =
        a.all("--labels").iter().flat_map(|l| l.split(',')).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    let wait = a.secs("--wait")?;
    let t0 = Instant::now();
    let decisions = loop {
        let mut failed = false;
        let got = match statuses(&root, &sha) {
            Ok(s) => newest(&s),
            Err(e) => {
                failed = true;
                // Unsure means run: CI must not skip work it cannot see the proof of.
                eprintln!("taskguard: cannot read the statuses of {}, so CI runs everything: {e:#}", short(&sha));
                BTreeMap::new()
            }
        };
        let d: Vec<(String, bool, String)> = ids
            .iter()
            .map(|id| {
                let (skip, why) = match policy.receipt.get(id) {
                    Some(rule) => decide(id, rule, got.get(id), &labels),
                    None => (false, format!("no receipt id {id} in {POLICY}")),
                };
                (id.clone(), skip, why)
            })
            .collect();
        let missing = ids.iter().any(|id| policy.receipt.contains_key(id) && !got.contains_key(id));
        // A gh error means run; waiting longer would only delay that.
        if !missing || failed || t0.elapsed().as_secs() >= wait {
            break d;
        }
        std::thread::sleep(Duration::from_secs(5));
    };
    let mut out = String::new();
    for (id, skip, why) in &decisions {
        println!("{id}: {} - {why}", if *skip { "skip" } else { "run" });
        out.push_str(&format!("{id}={}\n", if *skip { "skip" } else { "run" }));
    }
    if a.has("--github-output") {
        let path = std::env::var("GITHUB_OUTPUT").context("--github-output needs $GITHUB_OUTPUT (set inside GitHub Actions)")?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&path).with_context(|| format!("opening {path}"))?;
        f.write_all(out.as_bytes())?;
        return Ok(0);
    }
    Ok(if decisions.iter().all(|d| d.1) { 0 } else { 1 })
}

/// `proof show [--history]`
fn show(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &[], &["--history"])?;
    let root = root_here()?;
    let policy = Policy::load(&root).ok();
    let head = commit_of(&root, "HEAD")?;
    let head_tree = tree_of(&root, "HEAD")?;
    let work = work_tree(&root)?;
    let db = Db::open_dir(&config::state_dir())?;
    println!("HEAD {} (tree {})", short(&head), short(&head_tree));
    if work != head_tree {
        println!("files on disk differ from HEAD (tree {}): commit them as they are to use their receipts", short(&work));
    }
    let mut ids: Vec<String> = policy.as_ref().map(|p| p.receipt.keys().cloned().collect()).unwrap_or_default();
    let local = receipts_for(&db, &[head_tree.clone(), work.clone()])?;
    for r in &local {
        if !ids.contains(&r.receipt) {
            ids.push(r.receipt.clone());
        }
    }
    let remote = statuses(&root, &head);
    println!("\nlocal receipts:");
    for id in &ids {
        let mine: Vec<&Receipt> = local.iter().filter(|r| &r.receipt == id).collect();
        if mine.is_empty() {
            println!("  {id:<16} none");
        }
        for r in mine.iter().take(if a.has("--history") { usize::MAX } else { 1 }) {
            let on = if r.tree == head_tree { "HEAD" } else { "files on disk" };
            let posted = if posted_to(&db, r.row, &head) { ", posted" } else { "" };
            println!("  {id:<16} {} on {on}: {}{posted}", r.state(), r.description());
        }
    }
    println!("\nGitHub statuses on HEAD:");
    match remote {
        Err(e) => println!("  cannot read them: {e:#}"),
        Ok(all) if all.is_empty() => println!("  none"),
        Ok(all) => {
            let list: Vec<Status> = if a.has("--history") { all } else { newest(&all).into_values().collect() };
            for s in list {
                println!("  {:<26} {:<8} {} ({}, {})", s.context, s.state, s.description.as_deref().unwrap_or(""), s.who(), s.created_at);
            }
        }
    }
    Ok(0)
}

/// How one commit on main is proven for one receipt id.
#[derive(Debug, Clone, PartialEq)]
pub enum Proof {
    None,
    /// Proof on the PR head, whose tree is not the tree of the merge commit.
    OtherTree(String),
    Failed,
    Level(String),
}

impl Proof {
    fn of(s: Option<&Status>, same_tree: bool) -> Proof {
        let Some(s) = s else { return Proof::None };
        let level = s.description.as_deref().and_then(parse_description).map(|(l, _)| l).unwrap_or_else(|| "?".into());
        if s.state != "success" {
            return Proof::Failed;
        }
        if same_tree { Proof::Level(level) } else { Proof::OtherTree(level) }
    }

    fn text(&self) -> String {
        match self {
            Proof::None => "-".into(),
            Proof::OtherTree(l) => format!("{l}~"),
            Proof::Failed => "red".into(),
            Proof::Level(l) => l.clone(),
        }
    }

    /// Suspects when a full run goes red: the least proven first.
    pub fn rank(&self) -> u8 {
        match self {
            Proof::None => 0,
            Proof::Failed => 1,
            Proof::OtherTree(_) => 2,
            Proof::Level(l) if l == "partial" => 3,
            Proof::Level(_) => 4,
        }
    }
}

/// `proof log [--since SHA] [--id ID] [-n N] [--branch REF]`
fn log(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &["--since", "--id", "-n", "--branch"], &[])?;
    let root = root_here()?;
    let policy = Policy::load(&root).ok();
    let branch = match a.one("--branch") {
        Some(b) => b.to_string(),
        None => git(&root, &["rev-parse", "--abbrev-ref", "origin/HEAD"]).unwrap_or_else(|_| "origin/main".into()),
    };
    let ids: Vec<String> = match a.one("--id") {
        Some(id) => vec![id.to_string()],
        None => policy.map(|p| p.receipt.keys().cloned().collect()).unwrap_or_default(),
    };
    if ids.is_empty() {
        bail!("no receipt ids: give --id ID, or add a {POLICY}");
    }
    let n = a.one("-n").map(|v| v.parse::<usize>().context("-n needs a number")).transpose()?;
    let mut rl = vec!["rev-list".to_string(), "--first-parent".into()];
    if let Some(n) = n.or(if a.has("--since") { None } else { Some(20) }) {
        rl.push(format!("-n{n}"));
    }
    rl.push(match a.one("--since") {
        Some(s) => format!("{s}..{branch}"),
        None => branch.clone(),
    });
    let rl: Vec<&str> = rl.iter().map(String::as_str).collect();
    let commits: Vec<String> = git(&root, &rl)?.lines().map(str::to_string).collect();

    struct Row {
        sha: String,
        pr: String,
        subject: String,
        cells: Vec<Proof>,
    }
    let mut rows = Vec::new();
    for sha in &commits {
        let subject = git(&root, &["log", "-1", "--format=%s", sha]).unwrap_or_default();
        let own = newest(&statuses(&root, sha).unwrap_or_default());
        let (mut got, mut same, mut pr) = (own, true, String::new());
        if !ids.iter().any(|id| got.contains_key(id)) {
            // A squash merge: the proof sits on the PR head.
            let pulls = gh_api(
                &root,
                &[&format!("repos/{{owner}}/{{repo}}/commits/{sha}/pulls"), "--jq", ".[0] // empty | \"\\(.number) \\(.head.sha)\""],
            )
            .unwrap_or_default();
            if let Some((num, head)) = pulls.trim().split_once(' ') {
                pr = format!("#{num}");
                let head_tree = tree_of(&root, head)
                    .or_else(|_| {
                        gh_api(&root, &[&format!("repos/{{owner}}/{{repo}}/git/commits/{head}"), "--jq", ".tree.sha"])
                            .map(|s| s.trim().to_string())
                    })
                    .unwrap_or_default();
                same = head_tree == tree_of(&root, sha).unwrap_or_default();
                got = newest(&statuses(&root, head).unwrap_or_default());
            }
        }
        let cells = ids.iter().map(|id| Proof::of(got.get(id), same)).collect();
        rows.push(Row { sha: sha.clone(), pr, subject, cells });
    }
    if a.has("--id") {
        rows.sort_by_key(|r| r.cells[0].rank());
        println!("suspects for {}, the least proven first (~ = proof on a different tree):", ids[0]);
    } else {
        println!("proof per commit on {branch} (~ = proof on a different tree, the PR head before the squash):");
    }
    println!("{:<10} {:<7} {} subject", "commit", "pr", ids.iter().map(|i| format!("{i:<12}")).collect::<String>());
    for r in rows {
        let subject: String = r.subject.chars().take(60).collect();
        println!(
            "{:<10} {:<7} {} {}",
            short(&r.sha),
            r.pr,
            r.cells.iter().map(|c| format!("{:<12}", c.text())).collect::<String>(),
            subject
        );
    }
    Ok(0)
}

fn hook_path(root: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git(root, &["rev-parse", "--path-format=absolute", "--git-path", "hooks/pre-push"])?))
}

fn hook_text() -> String {
    format!(
        "#!/bin/sh\n{HOOK_MARK}\n# Written by `taskguard proof install`; `taskguard proof uninstall` removes it.\n\
command -v taskguard >/dev/null 2>&1 || exit 0\n\
shas=\"\"\n\
while read -r _ sha _ _; do\n  case \"$sha\" in *[!0]*) shas=\"$shas --sha $sha\" ;; esac\ndone\n\
[ -n \"$shas\" ] || exit 0\n\
# The commits reach GitHub only after this hook, so publish waits for them in the background.\n\
nohup taskguard proof publish $shas --wait 120 >\"${{TMPDIR:-/tmp}}/taskguard-proof-publish.log\" 2>&1 &\n\
exit 0\n"
    )
}

/// `proof install [ID --command CMD [--os OS] [--levels L,L]]`
fn install(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &["--command", "--os", "--levels"], &[])?;
    let root = root_here()?;
    let path = root.join(POLICY);
    if let Some(cmd) = a.one("--command") {
        let [id] = a.pos.as_slice() else { bail!("--command goes with exactly one receipt id") };
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            "# Proof receipts: what `taskguard --receipt ID` runs, and what proof CI accepts.\n# See `taskguard help proof`.\n".into()
        });
        let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("reading {}", path.display()))?;
        let receipts = doc.entry("receipt").or_insert(toml_edit::table()).as_table_mut().context("receipt is not a table")?;
        receipts.set_implicit(true);
        let t = receipts.entry(id).or_insert(toml_edit::table()).as_table_mut().context("not a table")?;
        t["command"] = toml_edit::value(cmd);
        if let Some(os) = a.one("--os") {
            t["os"] = toml_edit::value(os);
        }
        if let Some(l) = a.one("--levels") {
            t["levels"] = toml_edit::value(l.split(',').map(str::trim).collect::<toml_edit::Array>());
        }
        Policy::parse(&doc.to_string(), POLICY)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, doc.to_string())?;
    } else if !a.pos.is_empty() {
        bail!("give the command of a new receipt id: taskguard proof install ID --command \"CMD\"");
    }

    let mut todo = 0;
    let policy = match Policy::load(&root) {
        Ok(p) if !p.receipt.is_empty() => {
            println!(
                "✓ policy      {POLICY} ({} receipt ids: {})",
                p.receipt.len(),
                p.receipt.keys().cloned().collect::<Vec<_>>().join(", ")
            );
            p
        }
        Ok(p) => {
            todo += 1;
            println!("✗ policy      {POLICY} has no receipt ids; add one: taskguard proof install ID --command \"CMD\"");
            p
        }
        Err(e) => {
            todo += 1;
            println!("✗ policy      {e:#}");
            Policy::default()
        }
    };

    let hook = hook_path(&root)?;
    match std::fs::read_to_string(&hook) {
        Ok(t) if t == hook_text() => println!("✓ push hook   {} publishes after each push", hook.display()),
        Ok(t) if t.contains(HOOK_MARK) || t.trim().is_empty() => {
            write_hook(&hook)?;
            println!("✓ push hook   {} updated", hook.display());
        }
        Ok(_) => {
            todo += 1;
            println!(
                "✗ push hook   {} is not taskguard's; run `taskguard proof publish` after each push, or call it from that hook",
                hook.display()
            );
        }
        Err(_) => {
            write_hook(&hook)?;
            println!("✓ push hook   {} written", hook.display());
        }
    }

    match Command::new(gh_bin()).args(["auth", "status"]).stdout(Stdio::null()).stderr(Stdio::null()).status() {
        Ok(s) if s.success() => println!("✓ gh          signed in"),
        _ => {
            todo += 1;
            println!("✗ gh          the GitHub CLI is missing or not signed in: gh auth login");
        }
    }

    let contexts: Vec<String> = policy.receipt.keys().map(|id| format!("{CONTEXT}{id}")).collect();
    let rulesets = gh_api(&root, &["repos/{owner}/{repo}/rulesets", "--jq", ".[] | select(.name == \"taskguard\") | .id"]);
    match rulesets.as_deref().map(str::trim) {
        Ok(id) if !id.is_empty() => println!("✓ ruleset     taskguard (id {id}); check that it requires {}", contexts.join(", ")),
        _ => {
            println!("• ruleset     none named taskguard. Optional: make the statuses a merge gate. A repo admin runs:");
            println!("{}", indent(&ruleset_command(&contexts), 16));
        }
    }

    let workflows = read_workflows(&root);
    if workflows.contains("taskguard proof check") {
        println!("✓ CI check    a workflow runs `taskguard proof check`");
    } else {
        todo += 1;
        println!("✗ CI check    no workflow runs `taskguard proof check`. A first job, then `if:` on the heavy ones:");
        println!("{}", indent(&check_snippet(&policy), 16));
    }
    for id in policy.receipt.keys() {
        if workflows.contains(&format!("--receipt {id}")) || workflows.contains(&format!("--receipt={id}")) {
            println!("✓ CI {id:<9} runs `taskguard --receipt {id}`");
        } else {
            println!("• CI {id:<9} optional: let CI post {CONTEXT}{id} too, so a rerun skips what passed:");
            println!(
                "{}",
                indent(
                    &format!(
                        "permissions:\n  statuses: write\n...\n- run: taskguard --receipt {id} && taskguard proof publish --sha \"$HEAD_SHA\"\n  env:\n    GH_TOKEN: ${{{{ github.token }}}}\n    HEAD_SHA: ${{{{ github.event.pull_request.head.sha || github.sha }}}}"
                    ),
                    16
                )
            );
        }
    }
    if todo > 0 {
        println!("\n{todo} to do. Run `taskguard proof install` again after you fix them.");
    }
    Ok(0)
}

fn write_hook(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, hook_text())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn indent(s: &str, n: usize) -> String {
    s.lines().map(|l| format!("{}{l}", " ".repeat(n))).collect::<Vec<_>>().join("\n")
}

fn read_workflows(root: &Path) -> String {
    let dir = root.join(".github/workflows");
    let mut all = String::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "yml" || x == "yaml") {
            all.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
        }
    }
    all
}

fn ruleset_command(contexts: &[String]) -> String {
    let checks: Vec<String> = contexts.iter().map(|c| format!("{{\"context\":\"{c}\"}}")).collect();
    format!(
        "gh api -X POST repos/{{owner}}/{{repo}}/rulesets --input - <<'JSON'\n\
{{\"name\":\"taskguard\",\"target\":\"branch\",\"enforcement\":\"evaluate\",\n \
\"conditions\":{{\"ref_name\":{{\"include\":[\"~DEFAULT_BRANCH\"],\"exclude\":[]}}}},\n \
\"rules\":[{{\"type\":\"required_status_checks\",\"parameters\":{{\"strict_required_status_checks_policy\":false,\n   \
\"required_status_checks\":[{}]}}}}]}}\nJSON",
        checks.join(",")
    )
}

fn check_snippet(policy: &Policy) -> String {
    let ids: Vec<&String> = policy.receipt.keys().collect();
    let first = ids.first().map(|s| s.as_str()).unwrap_or("ID");
    format!(
        "proof:\n  runs-on: ubuntu-latest\n  permissions: {{ contents: read, statuses: read }}\n  outputs:\n    {first}: ${{{{ steps.check.outputs['{first}'] }}}}\n  steps:\n    - uses: actions/checkout@v4\n      with: {{ sparse-checkout: .taskguard }}\n    - id: check\n      run: taskguard proof check --sha \"$HEAD_SHA\" --labels \"$LABELS\" --wait 30 --github-output\n      env:\n        GH_TOKEN: ${{{{ github.token }}}}\n        HEAD_SHA: ${{{{ github.event.pull_request.head.sha || github.sha }}}}\n        LABELS: ${{{{ join(github.event.pull_request.labels.*.name, ',') }}}}\n{first}-job:\n  needs: [proof]\n  if: needs.proof.outputs['{first}'] != 'skip'"
    )
}

/// `proof uninstall [ID] [--purge-local]`
fn uninstall(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &[], &["--purge-local"])?;
    let root = root_here()?;
    let path = root.join(POLICY);
    let policy = Policy::load(&root).unwrap_or_default();
    if let [id] = a.pos.as_slice() {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut doc: toml_edit::DocumentMut = text.parse()?;
        let removed = doc.get_mut("receipt").and_then(|r| r.as_table_mut()).and_then(|t| t.remove(id)).is_some();
        if !removed {
            bail!("no receipt id {id:?} in {POLICY}");
        }
        std::fs::write(&path, doc.to_string())?;
        println!("removed [receipt.{id}] from {POLICY}");
        println!("still to do by hand: drop {CONTEXT}{id} from the taskguard ruleset, and the CI lines that check or run {id}");
    } else if a.pos.is_empty() {
        if std::fs::remove_file(&path).is_ok() {
            println!("removed {POLICY}");
        }
        let hook = hook_path(&root)?;
        if std::fs::read_to_string(&hook).is_ok_and(|t| t.contains(HOOK_MARK)) {
            std::fs::remove_file(&hook)?;
            println!("removed the push hook {}", hook.display());
        }
        let ids: Vec<String> = policy.receipt.keys().map(|id| format!("{CONTEXT}{id}")).collect();
        println!(
            "still to do by hand: the taskguard ruleset{} and the CI lines that run `taskguard proof check` or `--receipt`",
            if ids.is_empty() { String::new() } else { format!(" ({})", ids.join(", ")) }
        );
    } else {
        bail!("uninstall takes at most one receipt id");
    }
    if a.has("--purge-local") {
        let db = Db::open_dir(&config::state_dir())?;
        let n = db.conn.execute("DELETE FROM receipts", [])?;
        db.conn.execute("DELETE FROM receipt_posts", [])?;
        println!("dropped {n} local receipts");
    } else {
        println!("local receipts stay; --purge-local drops them");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(cmd: &str) -> Rule {
        Rule { command: cmd.into(), os: "any".into(), levels: vec!["full".into()], env_files: "deny".into(), env_allow: vec![] }
    }

    fn status(state: &str, desc: &str) -> Status {
        Status {
            context: "taskguard/unit".into(),
            state: state.into(),
            description: Some(desc.into()),
            created_at: "2026-10-09T10:00:00Z".into(),
            target_url: None,
            creator: Some(Creator { login: "vincent".into() }),
        }
    }

    fn v(s: &str) -> Vec<String> {
        matcher::split_shell(s)
    }

    #[test]
    fn policy_defaults_and_checks() {
        let p = Policy::parse("[receipt.unit]\ncommand = \"turbo run test:unit\"\n", "t").unwrap();
        assert_eq!(p.rule("unit").unwrap(), &rule("turbo run test:unit"));
        assert!(Policy::parse("[receipt.unit]\ncommand = \"x\"\nos = \"windows\"\n", "t").is_err());
        assert!(Policy::parse("[receipt.unit]\ncommand = \"x\"\nlevels = [\"some\"]\n", "t").is_err());
        assert!(Policy::parse("[receipt.\"a b\"]\ncommand = \"x\"\n", "t").is_err());
        assert!(Policy::parse("[receipt.unit]\ncommand = \" \"\n", "t").is_err());
        assert!(p.rule("e2e").unwrap_err().to_string().contains("it has: unit"));
    }

    #[test]
    fn only_the_policy_command_is_full() {
        let r = rule("turbo run test:unit --affected");
        assert_eq!(level_of(&r, &[], None).unwrap().0, "full");
        assert_eq!(level_of(&r, &v("turbo run test:unit --affected"), None).unwrap().0, "full");
        assert_eq!(level_of(&r, &v("sh -c 'turbo run test:unit --affected'"), None).unwrap().0, "full");
        assert!(level_of(&r, &v("turbo run test:unit --filter api"), None).is_err(), "a subset needs --partial");
        let (l, why) = level_of(&r, &v("turbo run test:unit --filter api"), Some("only api changed")).unwrap();
        assert_eq!((l, why.as_deref()), ("partial", Some("only api changed")));
        assert!(level_of(&r, &[], Some(" ")).is_err(), "a partial says why");
    }

    #[test]
    fn the_description_reads_back() {
        let mut r = Receipt {
            row: 1,
            receipt: "unit".into(),
            tree: "t".into(),
            head: None,
            repo: None,
            level: "full".into(),
            reason: None,
            ok: true,
            exit: Some(0),
            cmd: "x".into(),
            host: "mbp".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            version: "0.8.0".into(),
            started_at: 0.0,
            duration_s: 723.0,
        };
        assert_eq!(r.description(), "full, macos/aarch64, 12m03s, mbp");
        assert_eq!(parse_description(&r.description()), Some(("full".into(), "macos".into())));
        r.level = "partial".into();
        r.reason = Some("changed the transfer form, ran the transfer specs, ".repeat(5));
        let d = r.description();
        assert_eq!(d.chars().count(), DESC_MAX);
        assert_eq!(parse_description(&d), Some(("partial".into(), "macos".into())));
        assert_eq!(parse_description("Build passed"), None);
    }

    #[test]
    fn ci_skips_only_accepted_green_proof() {
        let mut r = rule("x");
        let green = status("success", "full, macos/aarch64, 12m03s, mbp");
        assert!(decide("unit", &r, Some(&green), &[]).0);
        assert!(!decide("unit", &r, None, &[]).0, "no proof: run");
        assert!(!decide("unit", &r, Some(&status("failure", "full, macos/aarch64, 1m, mbp")), &[]).0, "red: CI runs it");
        assert!(!decide("unit", &r, Some(&status("pending", "")), &[]).0);
        assert!(!decide("unit", &r, Some(&status("success", "all good")), &[]).0, "a status taskguard did not write");
        assert!(!decide("unit", &r, Some(&status("success", "partial, macos/aarch64, 1m, mbp: api only")), &[]).0);
        assert!(!decide("unit", &r, Some(&green), &["taskguard:ci".into()]).0, "forced for all");
        assert!(!decide("unit", &r, Some(&green), &["taskguard:ci:unit".into()]).0, "forced for one");
        assert!(decide("unit", &r, Some(&green), &["taskguard:ci:e2e".into()]).0, "another id's label");
        r.os = "linux".into();
        assert!(!decide("unit", &r, Some(&green), &[]).0, "the policy wants Linux");
        assert!(decide("unit", &r, Some(&status("success", "full, linux/x86_64, 9m, ci")), &[]).0);
        r.levels = vec!["full".into(), "partial".into()];
        assert!(decide("unit", &r, Some(&status("success", "partial, linux/x86_64, 1m, box: api only")), &[]).0);
    }

    #[test]
    fn a_partial_receipt_does_not_hide_a_full_one() {
        let mk = |row: i64, level: &str, ok: bool| Receipt {
            row,
            receipt: "unit".into(),
            tree: "t".into(),
            head: None,
            repo: None,
            level: level.into(),
            reason: None,
            ok,
            exit: Some(0),
            cmd: "x".into(),
            host: "h".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            version: "v".into(),
            started_at: row as f64,
            duration_s: 1.0,
        };
        // Newest first, as receipts_for gives them.
        let got = strongest_per_id(vec![mk(3, "partial", true), mk(2, "full", true), mk(1, "full", false)]);
        assert_eq!(got.iter().map(|r| r.row).collect::<Vec<_>>(), [2]);
        let got = strongest_per_id(vec![mk(3, "full", false), mk(2, "full", true)]);
        assert_eq!(got[0].row, 3, "a newer full run counts, red too");
    }

    #[test]
    fn the_newest_status_per_context_counts() {
        let mut old = status("failure", "full, macos/aarch64, 1m, mbp");
        old.created_at = "2026-10-09T09:00:00Z".into();
        let new = status("success", "full, linux/x86_64, 9m, ci");
        let mut all = vec![old, new.clone()];
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        assert_eq!(newest(&all).get("unit"), Some(&new));
    }

    #[test]
    fn suspects_rank_least_proven_first() {
        let mut v =
            [Proof::Level("full".into()), Proof::Level("partial".into()), Proof::OtherTree("full".into()), Proof::None, Proof::Failed];
        v.sort_by_key(Proof::rank);
        assert_eq!(v.iter().map(Proof::text).collect::<Vec<_>>(), ["-", "red", "full~", "partial", "full"]);
    }

    #[test]
    fn the_ruleset_command_is_json() {
        let c = ruleset_command(&["taskguard/unit".into(), "taskguard/e2e".into()]);
        let json = c.split("<<'JSON'\n").nth(1).unwrap().trim_end_matches("JSON");
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["enforcement"], "evaluate");
        assert_eq!(v["rules"][0]["parameters"]["required_status_checks"][1]["context"], "taskguard/e2e");
    }
}
