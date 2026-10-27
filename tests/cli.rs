//! End-to-end tests: the real binary, a scratch state directory per test.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Env {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    cwd: PathBuf,
    conf: PathBuf,
}

impl Env {
    fn new(conf: &str) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        let conf_path = tmp.path().join("config.toml");
        std::fs::write(&conf_path, format!("status_every = 1\n{conf}")).unwrap();
        Env { dir: tmp.path().join("state"), cwd, conf: conf_path, _tmp: tmp }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_taskguard"));
        c.args(args)
            .current_dir(&self.cwd)
            .env("TASKGUARD_DIR", &self.dir)
            .env("TASKGUARD_CONF", &self.conf)
            .env("TASKGUARD_RECORDER_IDLE_EXIT", "3")
            // Memory pressure on the test machine would hold every job back.
            .env("TASKGUARD_PRESSURE", "0")
            .env_remove("TASKGUARD_HELD")
            .env_remove("npm_lifecycle_event")
            .env_remove("npm_package_json");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.cmd(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.join("taskguard.db")).unwrap()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.cwd.join(name)
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Wait until a job holds its slot, so the next call really has to queue.
fn wait_for(p: &Path) {
    let t = Instant::now();
    while !p.exists() {
        assert!(t.elapsed() < Duration::from_secs(10), "{} never appeared", p.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_one_slot_pool_runs_jobs_one_after_the_other() {
    let e = Env::new("");
    let job = "echo start >> log; sleep 2; echo end >> log";
    // Start the second job only once the first holds the slot. A fixed sleep
    // is not enough when many tests start processes at the same moment.
    let a = e.spawn(&["-j1", "--id", "serial", "--", "sh", "-c", &format!("touch held; {job}")]);
    wait_for(&e.file("held"));
    let b = e.spawn(&["-j1", "--id", "serial", "--", "sh", "-c", job]);
    // While B waits, the status screen names the reason.
    let t = Instant::now();
    let status = loop {
        let s = String::from_utf8_lossy(&e.run(&["status"]).stdout).into_owned();
        if s.contains("WAITING (1)") || t.elapsed() > Duration::from_secs(2) {
            break s;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.contains("pool serial: 1 of 1 busy"), "{status}");
    assert!(a.wait_with_output().unwrap().status.success());
    let b = b.wait_with_output().unwrap();
    assert!(b.status.success());
    let log = std::fs::read_to_string(e.file("log")).unwrap();
    assert_eq!(log, "start\nend\nstart\nend\n");
    assert!(stderr(&b).contains("queued"), "{}", stderr(&b));
}

#[test]
fn exit_code_and_stdin_pass_through() {
    let e = Env::new("");
    let mut c = e.cmd(&["--", "sh", "-c", "read x; echo got $x; exit 7"]);
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "got hello\n", "the command's stdout stays clean");
}

#[test]
fn a_negative_timeout_gives_up_with_124() {
    let e = Env::new("");
    let holder = e.spawn(&["-j1", "--id", "t", "--", "sh", "-c", "touch held; sleep 3"]);
    wait_for(&e.file("held"));
    let out = e.run(&["--st", "-1", "-j1", "--id", "t", "--", "true"]);
    assert_eq!(out.status.code(), Some(124), "{}", stderr(&out));
    assert!(stderr(&out).contains("timeout"));
    // A caller with its own meaning for "busy" sets the code, as DALP's throttle does with 75.
    let out = e.run(&["--st", "-1", "--st-exit", "75", "-j1", "--id", "t", "--", "true"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    holder.wait_with_output().unwrap();
}

#[test]
fn bg_returns_at_once_and_wait_blocks_until_done() {
    let e = Env::new("");
    let t = Instant::now();
    // Not output(): the job keeps the inherited stdout open, as with sem --bg,
    // so reading it to the end would wait for the job.
    let st = e
        .cmd(&["--bg", "--id", "b", "--", "sh", "-c", "sleep 1.5; touch done"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    assert!(t.elapsed() < Duration::from_millis(1200), "--bg returned after {:?}", t.elapsed());
    assert!(!e.file("done").exists());
    let out = e.run(&["--wait", "--id", "b"]);
    assert!(out.status.success());
    assert!(e.file("done").exists(), "--wait returned before the job ended");
}

#[test]
fn now_skips_the_queue_but_is_still_recorded() {
    let e = Env::new("");
    let holder = e.spawn(&["-j1", "--id", "t", "--", "sh", "-c", "touch held; sleep 3"]);
    wait_for(&e.file("held"));
    let t = Instant::now();
    let out = e.run(&["--now", "-j1", "--id", "t", "--", "true"]);
    assert!(out.status.success());
    assert!(t.elapsed() < Duration::from_secs(2), "--now waited");
    assert!(stderr(&out).contains("queue skipped on request, still measured"));
    let now: i64 = e.db().query_row("SELECT count(*) FROM runs WHERE now = 1 AND ended_at IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(now, 1);
    holder.wait_with_output().unwrap();
}

#[test]
fn hints_are_on_by_default_and_set_per_repo_or_per_call() {
    let e = Env::new("");
    let holder = e.spawn(&["-j1", "--id", "t", "--", "sh", "-c", "touch held; sleep 4"]);
    wait_for(&e.file("held"));
    let waiting = |extra: &[&str]| {
        let mut args = vec!["--st", "-1.5", "-j1", "--id", "t"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", "true"]);
        stderr(&e.run(&args))
    };
    assert!(waiting(&[]).contains("this is not a hang"), "on by default");
    std::fs::write(e.cwd.join(".taskguard.toml"), "hints = false\n").unwrap();
    assert!(!waiting(&[]).contains("this is not a hang"), "off for this repo");
    assert!(waiting(&["--hints"]).contains("this is not a hang"), "--hints wins over the repo file");
    std::fs::remove_file(e.cwd.join(".taskguard.toml")).unwrap();
    assert!(!waiting(&["--no-hints"]).contains("this is not a hang"), "--no-hints wins over the default");
    holder.wait_with_output().unwrap();
}

#[test]
fn a_minimum_makes_a_job_wait_for_room() {
    let e = Env::new("");
    let holder = e.spawn(&["--", "sh", "-c", "touch held; sleep 3"]);
    wait_for(&e.file("held"));
    let out = e.run(&["--st", "-2", "--min-cpu", "100000", "--", "true"]);
    assert_eq!(out.status.code(), Some(124), "{}", stderr(&out));
    // CPU blocks; on a machine that is also short of memory, memory is named
    // first and CPU follows under "also".
    let err = stderr(&out);
    assert!(err.contains("blocked by CPU") || err.contains("also: cpu"), "{err}");
    holder.wait_with_output().unwrap();
}

/// A job started with memory over RAM, here by --st, is the OOM killer's
/// first pick; a job that fits keeps the score it inherits.
#[cfg(target_os = "linux")]
#[test]
fn only_a_start_over_ram_raises_the_oom_score() {
    let e = Env::new("");
    let own = std::fs::read_to_string("/proc/self/oom_score_adj").unwrap().trim().to_string();
    let fits = e.run(&["--", "sh", "-c", "cat /proc/self/oom_score_adj"]);
    assert_eq!(String::from_utf8_lossy(&fits.stdout).trim(), own, "{}", stderr(&fits));
    let over = e.run(&["--st", "1", "--min-mem", "100000G", "--", "sh", "-c", "cat /proc/self/oom_score_adj"]);
    assert!(over.status.success(), "{}", stderr(&over));
    assert_eq!(String::from_utf8_lossy(&over.stdout).trim(), "800", "{}", stderr(&over));
    assert!(stderr(&over).contains("OOM killer's first pick"), "{}", stderr(&over));
}

#[test]
fn a_nested_call_does_not_take_a_second_slot() {
    let e = Env::new("");
    let me = env!("CARGO_BIN_EXE_taskguard");
    // With one slot, a nested call that queued again would wait forever.
    let out = e.run(&["--st", "-5", "-j1", "--id", "n", "--", me, "-j1", "--id", "n", "--", "true"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn passthrough_commands_run_straight_through() {
    let e = Env::new("");
    let out = e.run(&["--", "sh", "-c", "exit 3", "sh", "--watch"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(stderr(&out).is_empty(), "passthrough prints nothing: {}", stderr(&out));
    assert!(
        !e.dir.join("taskguard.db").exists() || e.db().query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0)).unwrap() == 0
    );
}

#[test]
fn a_run_is_learned_and_explained() {
    let e = Env::new("");
    let out = e.run(&["--key", "k", "--", "sh", "-c", "sleep 0.3"]);
    assert!(out.status.success());
    let hist = String::from_utf8_lossy(&e.run(&["history"]).stdout).into_owned();
    assert!(hist.contains('k') && hist.contains("MB"), "{hist}");
    let explain = String::from_utf8_lossy(&e.run(&["doctor", "--explain", "bunx --bun playwright test --workers 2"]).stdout).into_owned();
    assert!(explain.contains("pool:       e2e (1 slot(s), one per checkout)"), "{explain}");
    let status = e.run(&["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(v["machine"]["ncpu"].as_u64().unwrap() >= 1);
}

#[test]
fn dump_shows_what_a_job_reserves_and_why() {
    let e = Env::new("");
    assert!(e.run(&["--key", "k", "--", "sh", "-c", "exit 0"]).status.success());
    let dump = |args: &[&str]| -> serde_json::Value {
        let o = e.run(args);
        assert!(o.status.success(), "{args:?}: {}", stderr(&o));
        serde_json::from_slice(&o.stdout).unwrap()
    };
    let v = dump(&["dump", "k"]);
    let job = &v["jobs"][0];
    assert_eq!(job["key"], "k");
    // The run stored how its need was made: a first run guesses.
    let run = &job["runs"][0];
    assert_eq!(run["need_why"]["steps"][0]["step"], "first_guess", "{run}");
    assert_eq!(run["role"]["in_window"], true);
    // The next run reserves from the history, and says so.
    assert_eq!(job["next_need"]["steps"][0]["step"], "history", "{job}");
    assert!(job["config"]["rules"]["keep"].as_u64().is_some());
    // The same job by one of its runs.
    let id = run["id"].as_i64().unwrap().to_string();
    let by_run = dump(&["dump", "--run", &id]);
    assert_eq!(by_run["jobs"][0]["key"], "k");
    assert_eq!(by_run["focus_run"].as_i64().unwrap().to_string(), id);
    // A command, matched as doctor --explain does.
    let by_cmd = dump(&["dump", "--cmd", "bunx --bun playwright test"]);
    assert_eq!(by_cmd["jobs"][0]["match"]["pool"]["name"], "e2e");
    assert!(!e.run(&["dump", "nope*"]).status.success());
}

#[test]
fn a_cpu_starved_run_is_flagged_and_advised() {
    let e = Env::new("");
    let n = std::thread::available_parallelism().unwrap().get() * 3;
    // Three busy loops per core for 11 s: every thread waits on a core most of
    // the time. The loops start no processes, so all their time is CPU or
    // waiting for a core.
    let script = format!("pids=''; for i in $(seq {n}); do ( while :; do :; done ) & pids=\"$pids $!\"; done; sleep 11; kill $pids");
    let mut c = e.cmd(&["--key", "hog", "--", "sh", "-c", &script]);
    c.env("npm_lifecycle_event", "test").env("npm_package_json", e.cwd.join("package.json"));
    let out = c.output().unwrap();
    let err = stderr(&out);
    assert!(err.contains("possibly starved: waited on CPU"), "{err}");
    assert!(err.contains("advice to pin it: script \"test\" in package.json -> \"taskguard --min-cpu"), "{err}");
    let (starved, adjust): (String, i64) = e
        .db()
        .query_row("SELECT starved, (SELECT count(*) FROM adjustments WHERE key = 'hog') FROM runs WHERE key = 'hog'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(starved, "cpu");
    assert_eq!(adjust, 1);
}

#[test]
fn the_recorder_samples_the_machine_while_jobs_run() {
    let e = Env::new("");
    let out = e.run(&["--", "sh", "-c", "sleep 5"]);
    assert!(out.status.success());
    let n: i64 = e.db().query_row("SELECT count(*) FROM machine_samples", [], |r| r.get(0)).unwrap();
    assert!(n >= 2, "the recorder wrote {n} machine samples during a 5 s job");
}

#[test]
fn a_job_shorter_than_one_sample_still_shows_its_load() {
    let e = Env::new("");
    // About 0.8 s of busy work: shorter than the 2 s sample interval.
    let out = e.run(&["--key", "short", "--", "sh", "-c", "i=0; while [ $i -lt 400000 ]; do i=$((i+1)); done"]);
    assert!(out.status.success());
    let (covered, cpu): (f64, f64) = e
        .db()
        .query_row(
            "SELECT sum(js.cores_used * js.span_s), r.cpu_seconds FROM job_samples js JOIN runs r ON r.id = js.run_id WHERE r.key = 'short'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(cpu > 0.1, "the run did some work: {cpu}");
    assert!((covered - cpu).abs() < cpu * 0.3, "the samples cover the run's CPU: {covered} of {cpu}");
}

/// Not a test of its own: `a_mapped_file_is_not_learned_as_memory` runs
/// this binary with `TG_TEST_MAP` set, and then it maps that file, reads
/// every page and holds it for a few seconds.
#[test]
fn helper_reads_a_mapped_file() {
    let Some(path) = std::env::var_os("TG_TEST_MAP") else { return };
    let f = std::fs::File::open(path).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    use std::os::fd::AsRawFd;
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0) };
    assert_ne!(p, libc::MAP_FAILED);
    let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, len) };
    let sum: u64 = bytes.iter().step_by(4096).map(|b| *b as u64).sum();
    std::thread::sleep(Duration::from_secs(3));
    assert!(sum > 0);
}

/// macOS charges a process the memory it holds itself (physical footprint),
/// not the pages of files and binaries it maps, which its resident size
/// counts: a compiled Bun host held 3.4 GB resident against 585 MB of its
/// own. On Linux PSS counts mapped pages too, so the test is for macOS.
#[cfg(target_os = "macos")]
#[test]
fn a_mapped_file_is_not_learned_as_memory() {
    let e = Env::new("sample_every = 0.2\n");
    let file = e.file("mapped");
    std::fs::write(&file, vec![1u8; 192 << 20]).unwrap();
    let me = std::env::current_exe().unwrap();
    let me = me.to_str().unwrap();
    let mut c = e.cmd(&["--key", "mapped", "--", me, "helper_reads_a_mapped_file", "--exact", "--test-threads=1"]);
    let out = c.env("TG_TEST_MAP", &file).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let peak: i64 = e.db().query_row("SELECT peak_mem_kb FROM runs WHERE key = 'mapped'", [], |r| r.get(0)).unwrap();
    assert!(peak < 64 << 10, "it held {peak} kB of its own, not the 192 MB file it mapped");
}

#[test]
fn a_nested_call_runs_inside_the_outer_slot() {
    // Every version sets and honours TASKGUARD_HELD=1, so a nested call from
    // any version never waits for the slot its own parent holds.
    let e = Env::new("");
    let inner = format!("{} -j1 --id one -- sh -c 'echo $TASKGUARD_HELD > inner'", env!("CARGO_BIN_EXE_taskguard"));
    let o = e.run(&["--st", "-10", "-j1", "--id", "one", "--", "sh", "-c", &inner]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(std::fs::read_to_string(e.file("inner")).unwrap().trim(), "1");
    let runs: i64 = e.db().query_row("SELECT count(*) FROM runs", [], |r| r.get(0)).unwrap();
    assert_eq!(runs, 1, "the nested call takes no slot and records no run");
}

#[test]
fn a_command_that_wraps_taskguard_jobs_runs_straight_through_next_time() {
    // The first run queues as one job; its nested call leaves a note. The
    // next run of the same command runs straight through, and the call
    // inside it queues as a job of its own.
    let e = Env::new("");
    let inner = format!("{} -- sh -c 'echo inner'", env!("CARGO_BIN_EXE_taskguard"));
    let first = e.run(&["--", "sh", "-c", &inner]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(!stderr(&first).contains("] straight through"), "{}", stderr(&first));
    assert!(stderr(&first).contains("taskguard --wraps sh -c"), "it advises the flag: {}", stderr(&first));
    let nested: i64 = e.db().query_row("SELECT nested_jobs FROM runs", [], |r| r.get(0)).unwrap();
    assert_eq!(nested, 1);

    let second = e.run(&["--", "sh", "-c", &inner]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(stderr(&second).contains("] straight through"), "{}", stderr(&second));
    let keys: Vec<String> =
        e.db().prepare("SELECT key FROM runs ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(keys.len(), 2, "the wrapper once, then the job inside it: {keys:?}");
    assert_ne!(keys[0], keys[1]);
}

#[test]
fn wraps_runs_straight_through_from_the_first_run() {
    let e = Env::new("");
    let inner = format!("{} -- sh -c 'echo inner'", env!("CARGO_BIN_EXE_taskguard"));
    let o = e.run(&["--wraps", "--", "sh", "-c", &inner]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stderr(&o).contains("straight through") && stderr(&o).contains("--wraps"), "{}", stderr(&o));
    let keys: Vec<String> =
        e.db().prepare("SELECT key FROM runs").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(keys.len(), 1, "only the job inside it is recorded: {keys:?}");
    assert!(!keys[0].contains("taskguard"), "{keys:?}");
}

/// Wait until a waiting entry exists for `pid`, so a nudge reaches a job that waits.
fn wait_until_queued(e: &Env, pid: u32) {
    let t = Instant::now();
    while !std::fs::read_dir(e.dir.join("wait")).unwrap().flatten().any(|f| f.file_name().to_string_lossy().ends_with(&format!(".{pid}"))) {
        assert!(t.elapsed() < Duration::from_secs(10), "job {pid} never queued");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_waiting_job_started_by_hand_runs_at_once() {
    let e = Env::new("");
    let holder = e.spawn(&["-j1", "--id", "h", "--", "sh", "-c", "touch held; sleep 4"]);
    wait_for(&e.file("held"));
    let waiter = e.spawn(&["-j1", "--id", "h", "--", "touch", "started"]);
    wait_until_queued(&e, waiter.id());
    // What the dashboard's "g" writes.
    std::fs::write(e.dir.join("nudge").join(waiter.id().to_string()), r#"{"start":true}"#).unwrap();
    let out = waiter.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("started by hand"), "{}", stderr(&out));
    assert!(e.file("started").exists());
    let t = Instant::now();
    holder.wait_with_output().unwrap();
    assert!(t.elapsed() > Duration::from_millis(500), "the job started while the holder still ran");
}

#[test]
fn taskguard_start_starts_a_waiting_job() {
    let e = Env::new("");
    let holder = e.spawn(&["-j1", "--id", "h", "--", "sh", "-c", "touch held; sleep 4"]);
    wait_for(&e.file("held"));
    let waiter = e.spawn(&["-j1", "--id", "h", "--", "touch", "started"]);
    wait_until_queued(&e, waiter.id());
    let nothing = e.run(&["start", "no-such-job"]);
    assert!(!nothing.status.success());
    assert!(stderr(&nothing).contains("no waiting job matches"), "{}", stderr(&nothing));
    let out = e.run(&["start", &waiter.id().to_string()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("started"), "{}", String::from_utf8_lossy(&out.stdout));
    let out = waiter.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("started by hand"), "{}", stderr(&out));
    assert!(e.file("started").exists());
    let t = Instant::now();
    holder.wait_with_output().unwrap();
    assert!(t.elapsed() > Duration::from_millis(500), "the job started while the holder still ran");
}

#[test]
fn a_job_that_can_never_fit_prints_advice_with_the_start_command() {
    let e = Env::new("");
    let holder = e.spawn(&["--", "sh", "-c", "touch held; sleep 4"]);
    wait_for(&e.file("held"));
    let out = e.run(&["--min-cpu", "1000", "--st", "-2", "--", "true"]);
    assert_eq!(out.status.code(), Some(124), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("advice ") && err.contains("never fully fits"), "{err}");
    // On a machine short of memory (a busy CI runner), the advice says so
    // instead of offering a start by hand.
    assert!(err.contains("taskguard start ") || err.contains("memory keeps it out too"), "{err}");
    holder.wait_with_output().unwrap();
}

#[test]
fn needs_set_by_hand_let_a_job_fit() {
    let e = Env::new("");
    let holder = e.spawn(&["--", "sh", "-c", "touch held; sleep 4"]);
    wait_for(&e.file("held"));
    // 100 TB never fits next to a running job.
    let waiter = e.spawn(&["--min-mem", "100000000M", "--", "touch", "started"]);
    wait_until_queued(&e, waiter.id());
    std::thread::sleep(Duration::from_millis(600));
    assert!(!e.file("started").exists());
    // What the dashboard's "e" writes for "1 64M".
    std::fs::write(e.dir.join("nudge").join(waiter.id().to_string()), r#"{"need_cpu":0.1,"need_mem_kb":65536}"#).unwrap();
    let out = waiter.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(e.file("started").exists());
    let need: i64 = e.db().query_row("SELECT need_mem_kb FROM runs WHERE cmd = 'touch started'", [], |r| r.get(0)).unwrap();
    assert_eq!(need, 65536, "the run records the needs it really waited for");
    holder.wait_with_output().unwrap();
}

#[test]
fn a_waiting_job_follows_a_limit_changed_in_the_config() {
    // At 1% of RAM no job fits while another one runs.
    // The leeway would let it start: the room it lacks is held by other programs.
    let e = Env::new("mem_max = 1\nlearn_stagger = 0\noutside_admit = false\nnoise_mem = 0\n");
    // One run first: a job known to be short skips the CPU check, so only
    // memory decides, however busy the test machine is.
    assert!(e.run(&["--", "touch", "started"]).status.success());
    std::fs::remove_file(e.file("started")).unwrap();
    let holder = e.spawn(&["--", "sh", "-c", "touch held; sleep 5"]);
    wait_for(&e.file("held"));
    let waiter = e.spawn(&["--", "touch", "started"]);
    wait_until_queued(&e, waiter.id());
    std::thread::sleep(Duration::from_millis(600));
    assert!(!e.file("started").exists(), "the job waits for memory");
    std::fs::write(&e.conf, "status_every = 1\nlearn_stagger = 0\nmem_max = 99\n").unwrap();
    let out = waiter.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(e.file("started").exists());
    let t = Instant::now();
    holder.wait_with_output().unwrap();
    assert!(t.elapsed() > Duration::from_millis(500), "the job started while the holder still ran");
}

#[test]
fn nothing_starts_under_memory_pressure() {
    let e = Env::new("");
    // Even with nothing running: the load comes from other programs.
    let out = e.cmd(&["--st", "-2", "--", "true"]).env("TASKGUARD_PRESSURE", "100").output().unwrap();
    assert_eq!(out.status.code(), Some(124), "{}", stderr(&out));
    assert!(stderr(&out).contains("memory pressure"), "{}", stderr(&out));
}

#[test]
fn auto_pause_pauses_the_newest_job_and_resumes_it_when_the_rest_is_done() {
    // Memory is always "full" at 1%, and never low enough to resume on its
    // own: the paused job resumes because nothing else runs any more.
    let e = Env::new("auto_pause = true\npause_at = 1\nresume_at = 0\nsample_every = 0.2\n");
    let a = e.spawn(&["--", "sh", "-c", "touch a_started; sleep 1; echo a >> log"]);
    wait_for(&e.file("a_started"));
    // A --now job is never paused, so the other one, A, is.
    let b = e.spawn(&["--now", "--", "sh", "-c", "sleep 3; echo b >> log"]);
    let (a, b) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    let err = stderr(&a);
    assert!(err.contains("paused") && err.contains("resumed"), "{err}");
    assert!(!stderr(&b).contains("paused"), "{}", stderr(&b));
    let log = std::fs::read_to_string(e.file("log")).unwrap();
    assert_eq!(log, "b\na\n", "the short job only went on after the long one ended");
    let paused: f64 = e.db().query_row("SELECT paused_s FROM runs WHERE cmd LIKE '%a_started%'", [], |r| r.get(0)).unwrap();
    assert!(paused > 1.0, "the pause is recorded: {paused}");
}

/// The running job's entry, from `taskguard status --json`.
fn running_job(e: &Env) -> Option<serde_json::Value> {
    let out = e.run(&["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["running"].as_array().and_then(|r| r.first().cloned())
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let t = Instant::now();
    while !ok() {
        assert!(t.elapsed() < Duration::from_secs(10), "{what} never happened");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_job_is_paused_and_resumed_by_hand() {
    let e = Env::new("sample_every = 0.2\n");
    let job = e.spawn(&["--", "sh", "-c", "touch started; sleep 4; echo done >> log"]);
    wait_for(&e.file("started"));
    let out = e.run(&["pause", "sleep_4"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("paused"), "{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    let status = String::from_utf8_lossy(&e.run(&["status"]).stdout).into_owned();
    assert!(status.contains("PAUSED") && status.contains("by hand"), "{status}");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!e.file("log").exists(), "a paused job does not go on");
    let out = e.run(&["resume", "sleep_4"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("resumed"), "{}", stderr(&out));
    let out = job.wait_with_output().unwrap();
    assert!(e.file("log").exists());
    assert!(stderr(&out).contains("paused") && stderr(&out).contains("by hand"), "{}", stderr(&out));
    assert!(!String::from_utf8_lossy(&e.run(&["pause", "nothing-like-this"]).stderr).is_empty());
}

#[test]
fn a_job_stopped_from_outside_counts_as_paused_by_hand() {
    let e = Env::new("sample_every = 0.2\n");
    // exec: the command is one process, so one signal stops all of it.
    let mut job = e.spawn(&["--", "sh", "-c", "touch started; exec sleep 30"]);
    wait_for(&e.file("started"));
    wait_until("the command's pid is known", || running_job(&e).is_some_and(|r| r["child_pid"].as_i64().unwrap_or(0) > 0));
    let child = running_job(&e).unwrap()["child_pid"].as_i64().unwrap() as i32;
    unsafe { libc::kill(child, libc::SIGSTOP) };
    wait_until("the pause is seen", || running_job(&e).is_some_and(|r| r["paused_by_hand"] == true));
    unsafe { libc::kill(child, libc::SIGCONT) };
    wait_until("the resume is seen", || running_job(&e).is_some_and(|r| r["paused_since"].is_null()));
    let _ = job.kill();
    let _ = job.wait();
    unsafe { libc::kill(child, libc::SIGKILL) };
}

#[test]
fn a_long_lived_job_holds_its_peak_only_through_its_start_up() {
    let e = Env::new("sample_every = 0.2\n[pool.stack]\nlong_lived = true\nstartup = 1\n");
    // A busy start-up, then it idles.
    let job = e.spawn(&["--id", "stack", "--", "sh", "-c", "( while :; do :; done ) & p=$!; sleep 1.5; kill $p; touch idle; sleep 4"]);
    wait_until("the start-up is over", || running_job(&e).is_some_and(|r| r["steady_since"].is_f64()));
    let r = running_job(&e).unwrap();
    assert!(r["est_dur_s"].is_null(), "no one counts on it ending: {r}");
    let status = String::from_utf8_lossy(&e.run(&["status"]).stdout).into_owned();
    assert!(status.contains("STEADY"), "{status}");
    wait_for(&e.file("idle"));
    // It needed a core while it started; over the last 10 s it wants less and less.
    wait_until("its CPU need follows what it uses", || running_job(&e).is_some_and(|r| r["need_cpu"].as_f64().unwrap() < 0.5));
    let out = job.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(stderr(&out).contains("[taskguard] steady ") && stderr(&out).contains("start-up over after"), "{}", stderr(&out));
    let (peak, steady, steady_mem): (f64, f64, i64) = e
        .db()
        .query_row("SELECT cores_wanted, steady_cores_wanted, steady_mem_kb FROM runs", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    assert!(steady < peak, "the history keeps the start-up peak ({peak:.2}) and the steady state ({steady:.2}) apart");
    assert!(steady_mem > 0);
    let explain = String::from_utf8_lossy(&e.run(&["doctor", "--explain", "sh -c x"]).stdout).into_owned();
    assert!(!explain.contains("long-lived"), "only the stack pool is: {explain}");
}

/// Not a test of its own: other tests run this binary with `TG_TEST_HOLD`
/// set to `MB:SECONDS,MB:SECONDS...`, and then it holds that much memory of
/// its own for that long, step by step.
#[test]
fn helper_holds_memory() {
    let Ok(steps) = std::env::var("TG_TEST_HOLD") else { return };
    for step in steps.split(',') {
        let (mb, secs) = step.split_once(':').unwrap();
        let mut held = vec![0u8; mb.parse::<usize>().unwrap() << 20];
        for b in held.iter_mut().step_by(4096) {
            *b = 1;
        }
        std::thread::sleep(Duration::from_secs_f64(secs.parse().unwrap()));
        assert!(std::hint::black_box(&held).iter().step_by(4096).all(|b| *b == 1));
    }
}

/// `taskguard ARGS -- <this binary> helper_holds_memory` with `hold` steps.
fn holding(e: &Env, args: &[&str], hold: &str) -> Output {
    let me = std::env::current_exe().unwrap();
    let mut argv: Vec<&str> = args.to_vec();
    argv.extend(["--", me.to_str().unwrap(), "helper_holds_memory", "--exact", "--test-threads=1"]);
    e.cmd(&argv).env("TG_TEST_HOLD", hold).output().unwrap()
}

#[test]
fn a_stack_that_grows_after_its_start_up_is_admitted_against_its_start_up_peak() {
    let e = Env::new("sample_every = 0.2\n[pool.stack]\nlong_lived = true\nstartup = 2\n");
    // 40 MB while it starts, then it grows to 160 MB while it serves.
    let out = holding(&e, &["--id", "stack", "--key", "stack"], "40:3,160:3");
    assert!(out.status.success(), "{}", stderr(&out));
    let (peak, startup): (i64, i64) =
        e.db().query_row("SELECT peak_mem_kb, startup_mem_kb FROM runs", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert!(peak > 150 << 10 && startup < 100 << 10, "whole run {peak} kB, start-up {startup} kB");
    let dump: serde_json::Value = serde_json::from_slice(&e.run(&["dump", "stack"]).stdout).unwrap();
    let next = dump["jobs"][0]["next_need"]["mem_kb"].as_i64().unwrap();
    assert_eq!(next, startup, "the next run is admitted against the start-up peak, not the {peak} kB it grew to");
}

#[test]
fn a_stack_books_the_work_it_expects_and_that_work_goes_first() {
    let e = Env::new(
        "sample_every = 0.2\nnew_job_mem = \"700M\"\nexpect_wait = 0.5\n[pool.stack]\nlong_lived = true\nstartup = 1\n[pool.browser]\nunlimited = true\n",
    );
    // The caller of the stack says a browser run will follow (DALP passes the
    // variable through `devenv up`).
    let mut stack = e.cmd(&["--id", "stack", "--priority", "10", "--", "sh", "-c", "touch up; sleep 30"]);
    stack.env("TASKGUARD_EXPECT", "browser");
    let mut stack = stack.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    wait_for(&e.file("up"));
    let r = running_job(&e).unwrap();
    assert_eq!(r["expects"][0]["work"], "browser", "{r}");
    assert_eq!(r["expects"][0]["mem_kb"], 700 * 1024, "no browser run is known yet: new_job_mem: {r}");
    let status = String::from_utf8_lossy(&e.run(&["status"]).stdout).into_owned();
    assert!(status.contains("(expects browser, books 700 MB)"), "{status}");

    // The run queues in the same checkout: it is the work the stack expects,
    // and goes one priority above it.
    let run = e.spawn(&["--id", "browser", "--", "sh", "-c", "touch ran; sleep 1"]);
    wait_for(&e.file("ran"));
    let out = e.run(&["status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let r = v["running"].as_array().unwrap().iter().find(|r| r["pool"] == "browser").cloned().unwrap();
    assert_eq!((r["priority"].as_i64(), r["work_for"]["item"].as_i64()), (Some(11), Some(0)), "{r}");
    let run = run.wait_with_output().unwrap();
    assert!(run.status.success() && stderr(&run).contains("for stack "), "{}", stderr(&run));
    // Once it has started, the stack books nothing for it any more.
    wait_until("the stack drops its booking", || running_job(&e).is_some_and(|r| r["expects"][0]["started"] == true));

    // Any other job ignores the variable; --expect on it says so.
    let other = e.run(&["--expect", "browser", "--", "true"]);
    assert!(stderr(&other).contains("--expect is for a long-lived job"), "{}", stderr(&other));
    unsafe { libc::kill(stack.id() as i32, libc::SIGTERM) };
    let _ = stack.wait();
}

#[test]
fn a_stack_waits_for_the_work_it_expects_when_both_queue_together() {
    // Both are first runs and book new_job_mem each. Small, so the stack and
    // its run fit together also on a CI runner whose memory is mostly in use:
    // a lone stack that does not fit in RAM waits, and this test is about the
    // stack waiting for its work, not about room.
    let e = Env::new(
        "sample_every = 0.2\nexpect_wait = 20\nnew_job_mem = \"64M\"\n[pool.stack]\nlong_lived = true\nstartup = 1\n[pool.browser]\nunlimited = true\n",
    );
    let mut stack = e.cmd(&["--id", "stack", "--expect", "browser", "--", "sh", "-c", "touch up; sleep 30"]);
    let mut stack = stack.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let t = Instant::now();
    // The run queues a moment later: the stack, which waits for it, starts
    // at once, and the run after it.
    wait_until("the stack queues", || {
        let out = e.run(&["status", "--json"]);
        serde_json::from_slice::<serde_json::Value>(&out.stdout).is_ok_and(|v| v["waiting"].as_array().is_some_and(|w| !w.is_empty()))
    });
    let run = e.run(&["--id", "browser", "--", "true"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(t.elapsed() < Duration::from_secs(15), "the stack did not wait out expect_wait: {:?}", t.elapsed());
    assert!(stderr(&run).contains("for stack "), "{}", stderr(&run));
    let started = |pool: &str| -> f64 { e.db().query_row("SELECT started_at FROM runs WHERE pool = ?1", [pool], |r| r.get(0)).unwrap() };
    assert!(started("browser") >= started("stack"), "the run starts only once its stack runs");
    unsafe { libc::kill(stack.id() as i32, libc::SIGTERM) };
    let _ = stack.wait();
}

#[test]
fn enabled_false_runs_every_command_straight_through() {
    // A one-slot pool would queue the second job; switched off, nothing queues
    // and nothing is recorded.
    let e = Env::new("enabled = false\n");
    let o = e.run(&["-j1", "--id", "one", "--", "sh", "-c", "echo $TASKGUARD_HELD > out"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(std::fs::read_to_string(e.file("out")).unwrap().trim(), "", "no slot was taken");
    assert!(!e.dir.join("taskguard.db").exists(), "nothing was recorded");
}

#[test]
fn every_command_has_help_and_help_all_has_them_all() {
    let e = Env::new("");
    let out = |args: &[&str]| {
        let o = e.cmd(args).output().unwrap();
        assert!(o.status.success(), "{args:?}: {}", stderr(&o));
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    // --help anywhere among a command's options, and help COMMAND, say the same.
    let h = out(&["outliers", "--ratio", "1.2", "--help"]);
    assert!(h.starts_with("taskguard outliers"), "{h}");
    assert_eq!(out(&["help", "outliers"]), h);
    for cmd in ["top", "status", "pause", "resume", "history", "prune", "dump", "doctor", "import-history", "version", "help"] {
        let h = out(&[cmd, "--help"]);
        assert!(h.contains(&format!("taskguard {cmd}")), "{cmd}: {h}");
    }
    assert!(out(&["run", "--help"]).contains("--min-mem SIZE"));
    assert!(out(&["--wait", "--help"]).starts_with("taskguard --wait"));
    // One text for an agent: the overview, then every command in full.
    let all = out(&["help", "--all"]);
    for part in ["usage:", "--min-mem SIZE", "taskguard outliers [PATTERN]", "taskguard prune --undo", "taskguard top [--view", "--explain"]
    {
        assert!(all.contains(part), "help --all lacks {part:?}");
    }
    assert!(!e.cmd(&["help", "nope"]).output().unwrap().status.success());
    // After the command, --help belongs to the command.
    assert_eq!(out(&["--now", "--", "sh", "-c", "echo got $1", "x", "--help"]), "got --help\n");
}
