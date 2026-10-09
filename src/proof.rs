//! Proof receipts: record that a command passed on an exact tree of files, and
//! publish that as a GitHub commit status, so CI can skip the work a machine
//! already did. See `taskguard help proof`.
//!
//! `--receipt ID` is the opt-in, per command. The receipt name is the
//! contract: a run under `--receipt unit-tests` says "this is the Unit Tests
//! job". The tree is hashed as it is on disk, uncommitted and untracked files
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

/// Commit status contexts are `taskguard/<id>`.
pub const CONTEXT: &str = "taskguard/";
/// GitHub cuts a status description at 140 characters.
const DESC_MAX: usize = 140;
/// How long a laptop's background publish watches the branch on GitHub.
const PUBLISH_WATCH_S: u64 = 3600;
/// How long `proof check` in GitHub Actions waits for a status that is missing.
const CHECK_WAIT_CI_S: u64 = 30;

fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        bail!("receipt id {id:?} may only hold letters, digits, - _ and .");
    }
    Ok(())
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

/// A short hash of the command line, so a status says which command passed.
/// git's object hash: no hashing crate needed, and the same on every machine.
fn fingerprint(root: &Path, argv: &[String]) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running git")?;
    child.stdin.take().context("git stdin")?.write_all(argv.join("\0").as_bytes())?;
    let out = child.wait_with_output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().chars().take(12).collect())
}

// --- the machine and GitHub Actions ---------------------------------------------

fn in_actions() -> bool {
    std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true")
}

fn host() -> String {
    if in_actions() {
        return "ci".into();
    }
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    let name = if ok { String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(0)]).into_owned() } else { String::new() };
    let name = name.split('.').next().unwrap_or("").to_string();
    if name.is_empty() { "unknown".into() } else { name }
}

/// The PR head sha and its labels from the GitHub Actions event, when this
/// runs for a pull request. CI checks out a merge of the head onto its base;
/// the status belongs on the head.
#[derive(Debug, Default, PartialEq)]
pub struct Event {
    pub head_sha: Option<String>,
    pub labels: Vec<String>,
}

pub fn read_event(json: &str) -> Event {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let pr = &v["pull_request"];
    Event {
        head_sha: pr["head"]["sha"].as_str().map(str::to_string),
        labels: pr["labels"]
            .as_array()
            .map(|a| a.iter().filter_map(|l| l["name"].as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
    }
}

fn actions_event() -> Event {
    std::env::var("GITHUB_EVENT_PATH").ok().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| read_event(&t)).unwrap_or_default()
}

// --- receipts -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Receipt {
    pub row: i64,
    pub receipt: String,
    pub tree: String,
    pub head: Option<String>,
    pub ok: bool,
    pub exit: Option<i32>,
    pub cmd: String,
    pub fingerprint: String,
    pub host: String,
    pub os: String,
    pub arch: String,
    pub started_at: f64,
    pub duration_s: f64,
}

impl Receipt {
    /// The status description, in the fixed shape `parse_description` reads
    /// back: `f:3fa1c0d2e4b1, macos/aarch64, 12m03s, host`. The host comes
    /// last, so a long one is what gets cut.
    pub fn description(&self) -> String {
        let s = format!("f:{}, {}/{}, {}, {}", self.fingerprint, self.os, self.arch, dur(self.duration_s), self.host);
        if s.chars().count() > DESC_MAX { s.chars().take(DESC_MAX - 1).collect::<String>() + "…" } else { s }
    }

    pub fn state(&self) -> &'static str {
        if self.ok { "success" } else { "failure" }
    }
}

/// The OS of a `taskguard/<id>` status, from its description.
pub fn parse_description(d: &str) -> Option<String> {
    let os_arch = d.split(", ").nth(1)?;
    let (os, _) = os_arch.split_once('/')?;
    (!os.is_empty()).then(|| os.to_string())
}

fn insert(db: &Db, id: &str, r: &Receipt, repo: &Path) -> Result<i64> {
    db.conn.execute(
        "INSERT INTO receipts (receipt, tree, head, repo, level, ok, exit, cmd, fingerprint, host, os, arch, version, started_at, duration_s)
         VALUES (?1, ?2, ?3, ?4, 'full', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            r.tree,
            r.head,
            repo.display().to_string(),
            r.ok,
            r.exit,
            r.cmd,
            r.fingerprint,
            r.host,
            r.os,
            r.arch,
            env!("CARGO_PKG_VERSION"),
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
        "SELECT id, receipt, tree, head, ok, exit, cmd, fingerprint, host, os, arch, started_at, duration_s
         FROM receipts WHERE tree = ?1",
    )?;
    for t in trees {
        let rows = stmt.query_map(params![t], |r| {
            Ok(Receipt {
                row: r.get(0)?,
                receipt: r.get(1)?,
                tree: r.get(2)?,
                head: r.get(3)?,
                ok: r.get(4)?,
                exit: r.get(5)?,
                cmd: r.get(6)?,
                fingerprint: r.get::<_, Option<String>>(7)?.unwrap_or_else(|| "?".into()),
                host: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                os: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
                arch: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
                started_at: r.get(11)?,
                duration_s: r.get(12)?,
            })
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    out.sort_by(|a, b| b.started_at.partial_cmp(&a.started_at).unwrap_or(std::cmp::Ordering::Equal).then(b.row.cmp(&a.row)));
    Ok(out)
}

/// The newest receipt per id.
fn newest_per_id(rs: Vec<Receipt>) -> Vec<Receipt> {
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

/// What `taskguard --receipt` was given besides the command.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ReceiptOpts {
    pub id: String,
    pub allow_env_files: Vec<String>,
    pub no_publish: bool,
}

/// `taskguard --receipt ID [--allow-env-file GLOB]... [--no-publish] -- COMMAND`:
/// run the command and, when the files did not change while it ran, keep a
/// receipt of the result and publish it. The command runs straight away,
/// outside the queue: it is mostly a task runner whose own leaves queue.
pub fn run_receipt(o: &ReceiptOpts, cmd: &[String]) -> Result<i32> {
    valid_id(&o.id)?;
    if cmd.is_empty() {
        bail!("--receipt {} needs the command to run: taskguard --receipt {} -- COMMAND", o.id, o.id);
    }
    let id = o.id.as_str();
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let root = toplevel(&cwd)?;
    let env = ignored_env_files(&root, &o.allow_env_files)?;
    if !env.is_empty() {
        bail!(
            "no receipt: ignored settings files can change the result, and the proof cannot see them: {}\nMove them away, or pass --allow-env-file GLOB for one that may stay.",
            env.join(", ")
        );
    }

    let before = work_tree(&root)?;
    let head = commit_of(&root, "HEAD").ok();
    let started_at = db::now();
    let t0 = Instant::now();
    let code = spawn_and_wait(cmd)?;
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
    let mut r = Receipt {
        row: 0,
        receipt: id.to_string(),
        tree: before,
        head,
        ok: exit == 0,
        exit: Some(exit),
        cmd: cmd.join(" "),
        fingerprint: fingerprint(&root, cmd)?,
        host: host(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        started_at,
        duration_s,
    };
    let db = Db::open_dir(&config::state_dir())?;
    r.row = insert(&db, id, &r, &root)?;
    let what = format!("receipt {id}: {} on tree {} ({})", if r.ok { "passed" } else { "failed" }, short(&r.tree), dur(duration_s));
    if o.no_publish {
        crate::report::say(&format!("{what}. Kept here only (--no-publish); `taskguard proof publish` posts it."));
    } else if in_actions() {
        // In CI the commit is on GitHub already: post now, on the PR head.
        let sha = actions_event().head_sha.or_else(|| std::env::var("GITHUB_SHA").ok()).unwrap_or_else(|| "HEAD".into());
        match commit_of(&root, &sha).and_then(|sha| unposted(&db, &root, &sha, false).map(|rs| (sha, rs))) {
            Ok((sha, rs)) if !rs.is_empty() => match post_all(&db, &root, &sha, rs, true) {
                Ok(()) => crate::report::say(&format!("{what}. Posted {CONTEXT}{id} on {}.", short(&sha))),
                Err(e) => crate::report::say(&format!("{what}. Could not post it: {e:#}")),
            },
            Ok((sha, _)) => crate::report::say(&format!("{what}. Not posted: {} has other files than the ones tested.", short(&sha))),
            Err(e) => crate::report::say(&format!("{what}. Could not post it: {e:#}")),
        }
    } else {
        match spawn_publish(&root, &r.tree) {
            Ok(how) => crate::report::say(&format!("{what}. {how}")),
            Err(e) => crate::report::say(&format!("{what}. Not published: {e:#}. `taskguard proof publish` after the push posts it.")),
        }
    }
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

/// Publish in the background. On a branch it watches that branch on GitHub
/// for the tested files, so the checkout may go away after the push (as
/// no-mistakes does). GH_REPO names the repo when the checkout's remote is not
/// GitHub. The output goes to a log in the temp folder.
fn spawn_publish(root: &Path, tree: &str) -> Result<String> {
    use std::os::unix::process::CommandExt;
    let branch = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).context("HEAD is not on a branch")?;
    let repo = std::env::var("GH_REPO")
        .ok()
        .filter(|r| !r.is_empty())
        .or_else(|| {
            let o = Command::new(gh_bin())
                .args(["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
                .current_dir(root)
                .output()
                .ok()?;
            let r = String::from_utf8_lossy(&o.stdout).trim().to_string();
            (o.status.success() && !r.is_empty()).then_some(r)
        })
        .context("no GitHub repo for this checkout (set GH_REPO=OWNER/REPO)")?;
    let log = std::env::temp_dir().join("taskguard-proof-publish.log");
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["proof", "publish", "--tree", tree, "--branch", &branch, "--repo", &repo, "--wait", &PUBLISH_WATCH_S.to_string(), "-q"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting the background publish")?;
    Ok(format!("It is posted when {branch} on {repo} has these files, within {} (log: {}).", dur(PUBLISH_WATCH_S as f64), log.display()))
}

// --- GitHub ---------------------------------------------------------------------

fn gh_bin() -> String {
    std::env::var("TASKGUARD_GH").unwrap_or_else(|_| "gh".into())
}

/// `gh api ...` in the checkout, so `{owner}/{repo}` resolves from its remote
/// (or from GH_REPO).
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
    fn secs(&self, f: &str) -> Result<Option<u64>> {
        self.one(f).map(|v| v.parse().with_context(|| format!("{f} needs a number of seconds"))).transpose()
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
        "" => bail!("proof needs a command: check, publish, show or log; see `taskguard help proof`"),
        other => bail!("no proof command {other:?}: check, publish, show or log"),
    }
}

fn root_here() -> Result<PathBuf> {
    toplevel(&std::env::current_dir().context("reading the current directory")?)
}

/// `proof publish [--sha SHA]... [--wait SECS] [--again]`, and the background
/// form `proof publish --tree TREE --branch BRANCH [--repo OWNER/REPO] --wait SECS`.
///
/// Without --sha it follows HEAD for up to --wait seconds: it posts as soon as
/// HEAD has a receipt and is on GitHub.
fn publish(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &["--sha", "--wait", "--tree", "--branch", "--repo"], &["--again", "-q", "--quiet"])?;
    let quiet = a.has("-q") || a.has("--quiet");
    let again = a.has("--again");
    let wait = a.secs("--wait")?.unwrap_or(0);
    if let Some(tree) = a.one("--tree") {
        let branch = a.one("--branch").context("--tree goes with --branch")?;
        return publish_tree(tree, branch, a.one("--repo"), wait, again, quiet);
    }
    let root = root_here()?;
    let db = Db::open_dir(&config::state_dir())?;
    let t0 = Instant::now();
    let explicit = a.all("--sha");
    if explicit.is_empty() {
        loop {
            let head = commit_of(&root, "HEAD")?;
            let rs = unposted(&db, &root, &head, again)?;
            if !rs.is_empty() && on_github(&root, &head) {
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
        let rs = unposted(&db, &root, &sha, again)?;
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

/// The newest receipt per id for the files of `sha` that is not posted on it yet.
fn unposted(db: &Db, root: &Path, sha: &str, again: bool) -> Result<Vec<Receipt>> {
    let mut trees = vec![tree_of(root, sha)?];
    // CI checks out a merge of the PR head onto its base: what passed there
    // passed on the head merged with the newest base, which counts for the head.
    let head = commit_of(root, "HEAD")?;
    let parents = git(root, &["rev-parse", "HEAD^@"]).unwrap_or_default();
    if sha != head && parents.lines().count() >= 2 && parents.lines().any(|p| p == sha) {
        trees.push(tree_of(root, "HEAD")?);
    }
    Ok(newest_per_id(receipts_for(db, &trees)?).into_iter().filter(|r| again || !posted_to(db, r.row, sha)).collect())
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

/// Watch the branch on GitHub and post the receipts of TREE on its head once
/// that head has those files. Needs no checkout.
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
            let rs: Vec<Receipt> = newest_per_id(receipts_for(&db, &[tree.to_string()])?)
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

/// What CI does for one receipt id, given the newest status on the commit.
pub fn decide(id: &str, os: &str, status: Option<&Status>, labels: &[String]) -> (bool, String) {
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
    let Some(got) = parse_description(&desc) else {
        return (false, format!("a status this taskguard cannot read: {desc:?}"));
    };
    if os != "any" && os != got {
        return (false, format!("proof on {got}, this check wants {os}"));
    }
    (true, format!("{desc} (posted by {})", s.who()))
}

/// `proof check ID... [--sha SHA] [--os OS] [--labels L,L] [--wait SECS] [--github-output]`
fn check(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &["--sha", "--labels", "--wait", "--os"], &["--github-output"])?;
    if a.pos.is_empty() {
        bail!("name the receipt ids to check: taskguard proof check unit-tests");
    }
    for id in &a.pos {
        valid_id(id)?;
    }
    let os = a.one("--os").unwrap_or("any");
    if !matches!(os, "any" | "linux" | "macos") {
        bail!("--os is any, linux or macos, not {os:?}");
    }
    let root = root_here()?;
    let event = if in_actions() { actions_event() } else { Event::default() };
    let sha = match a.one("--sha").map(str::to_string).or(event.head_sha) {
        Some(s) if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) => s,
        Some(s) => commit_of(&root, &s)?,
        None => commit_of(&root, "HEAD")?,
    };
    let mut labels: Vec<String> =
        a.all("--labels").iter().flat_map(|l| l.split(',')).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    labels.extend(event.labels);
    let wait = a.secs("--wait")?.unwrap_or(if in_actions() { CHECK_WAIT_CI_S } else { 0 });
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
        let d: Vec<(String, bool, String)> = a
            .pos
            .iter()
            .map(|id| {
                let (skip, why) = decide(id, os, got.get(id), &labels);
                (id.clone(), skip, why)
            })
            .collect();
        let missing = a.pos.iter().any(|id| !got.contains_key(id));
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
    let head = commit_of(&root, "HEAD")?;
    let head_tree = tree_of(&root, "HEAD")?;
    let work = work_tree(&root)?;
    let db = Db::open_dir(&config::state_dir())?;
    println!("HEAD {} (tree {})", short(&head), short(&head_tree));
    if work != head_tree {
        println!("files on disk differ from HEAD (tree {}): commit them as they are to use their receipts", short(&work));
    }
    let local = receipts_for(&db, &[head_tree.clone(), work.clone()])?;
    let shown = if a.has("--history") { local } else { newest_per_id(local) };
    println!("\nlocal receipts:");
    if shown.is_empty() {
        println!("  none");
    }
    for r in &shown {
        let on = if r.tree == head_tree { "HEAD" } else { "files on disk" };
        let posted = if posted_to(&db, r.row, &head) { ", posted" } else { "" };
        println!("  {:<16} {} on {on}: {}{posted}\n  {:<16} {}", r.receipt, r.state(), r.description(), "", r.cmd);
    }
    println!("\nGitHub statuses on HEAD:");
    match statuses(&root, &head) {
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
    Failed,
    /// Green on the PR head, whose tree is not the tree of the merge commit.
    OtherTree,
    Green,
}

impl Proof {
    fn of(s: Option<&Status>, same_tree: bool) -> Proof {
        match s {
            None => Proof::None,
            Some(s) if s.state != "success" => Proof::Failed,
            Some(_) if same_tree => Proof::Green,
            Some(_) => Proof::OtherTree,
        }
    }

    fn text(&self) -> &'static str {
        match self {
            Proof::None => "-",
            Proof::Failed => "red",
            Proof::OtherTree => "green~",
            Proof::Green => "green",
        }
    }

    /// Suspects when a full run goes red: the least proven first.
    pub fn rank(&self) -> u8 {
        match self {
            Proof::None => 0,
            Proof::Failed => 1,
            Proof::OtherTree => 2,
            Proof::Green => 3,
        }
    }
}

/// `proof log ID... [--since SHA] [-n N] [--branch REF]`
fn log(rest: &[String]) -> Result<i32> {
    let a = parse(rest, &["--since", "-n", "--branch"], &[])?;
    if a.pos.is_empty() {
        bail!("name the receipt ids: taskguard proof log unit-tests");
    }
    let ids = a.pos.clone();
    let root = root_here()?;
    let branch = match a.one("--branch") {
        Some(b) => b.to_string(),
        None => git(&root, &["rev-parse", "--abbrev-ref", "origin/HEAD"]).unwrap_or_else(|_| "origin/main".into()),
    };
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
    if ids.len() == 1 {
        rows.sort_by_key(|r| r.cells[0].rank());
        println!("commits on {branch}, the least proven first (~ = green on a different tree, the PR head before the squash):");
    } else {
        println!("proof per commit on {branch} (~ = green on a different tree, the PR head before the squash):");
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_description_reads_back() {
        let r = Receipt {
            row: 1,
            receipt: "unit".into(),
            tree: "t".into(),
            head: None,
            ok: true,
            exit: Some(0),
            cmd: "x".into(),
            fingerprint: "3fa1c0d2e4b1".into(),
            host: "h".repeat(200),
            os: "macos".into(),
            arch: "aarch64".into(),
            started_at: 0.0,
            duration_s: 723.0,
        };
        let d = r.description();
        assert!(d.starts_with("f:3fa1c0d2e4b1, macos/aarch64, 12m03s, hhh"), "{d}");
        assert_eq!(d.chars().count(), DESC_MAX, "a long host is cut");
        assert_eq!(parse_description(&d).as_deref(), Some("macos"));
        // Statuses of the first demo builds read too.
        assert_eq!(parse_description("full, linux/x86_64, 9m, ci").as_deref(), Some("linux"));
        assert_eq!(parse_description("Build passed"), None);
    }

    #[test]
    fn ci_skips_only_on_green_proof_from_an_accepted_os() {
        let green = status("success", "f:3fa1c0d2e4b1, macos/aarch64, 12m03s, mbp");
        assert!(decide("unit", "any", Some(&green), &[]).0);
        assert!(!decide("unit", "any", None, &[]).0, "no proof: run");
        assert!(!decide("unit", "any", Some(&status("failure", "f:1, macos/aarch64, 1m, mbp")), &[]).0, "red: CI runs it");
        assert!(!decide("unit", "any", Some(&status("pending", "")), &[]).0);
        assert!(!decide("unit", "any", Some(&status("success", "all good")), &[]).0, "a status taskguard did not write");
        assert!(!decide("unit", "any", Some(&green), &["taskguard:ci".into()]).0, "forced for all");
        assert!(!decide("unit", "any", Some(&green), &["taskguard:ci:unit".into()]).0, "forced for one");
        assert!(decide("unit", "any", Some(&green), &["taskguard:ci:e2e".into()]).0, "another id's label");
        assert!(!decide("unit", "linux", Some(&green), &[]).0, "the check wants Linux");
        assert!(decide("unit", "linux", Some(&status("success", "f:1, linux/x86_64, 9m, ci")), &[]).0);
    }

    #[test]
    fn the_newest_status_per_context_counts() {
        let mut old = status("failure", "f:1, macos/aarch64, 1m, mbp");
        old.created_at = "2026-10-09T09:00:00Z".into();
        let new = status("success", "f:1, linux/x86_64, 9m, ci");
        let mut all = vec![old, new.clone()];
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        assert_eq!(newest(&all).get("unit"), Some(&new));
    }

    #[test]
    fn the_event_gives_the_pr_head_and_labels() {
        let e = read_event(r#"{"pull_request":{"head":{"sha":"abc"},"labels":[{"name":"bug"},{"name":"taskguard:ci"}]}}"#);
        assert_eq!(e, Event { head_sha: Some("abc".into()), labels: vec!["bug".into(), "taskguard:ci".into()] });
        assert_eq!(read_event(r#"{"ref":"refs/heads/main"}"#), Event::default(), "a push event has no PR");
        assert_eq!(read_event("not json"), Event::default());
    }

    #[test]
    fn suspects_rank_least_proven_first() {
        let mut v = [Proof::Green, Proof::OtherTree, Proof::None, Proof::Failed];
        v.sort_by_key(Proof::rank);
        assert_eq!(v.iter().map(Proof::text).collect::<Vec<_>>(), ["-", "red", "green~", "green"]);
    }
}
