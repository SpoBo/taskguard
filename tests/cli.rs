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
    let e = Env::new("mem_max = 1\nlearn_stagger = 0\n");
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
