//! A model of dev stacks and the runs they exist for, run as tests.
//!
//! On a MacBook (10 cores, 32 GB, mem_max 95) two agents each brought up a
//! dev stack and, once it was up, queued the browser run that needs it.
//! Meanwhile an older turbo run kept queueing lints. The second stack was
//! admitted while the first one's run waited, the runs never fit next to
//! both stacks, and each agent gave up on its run after its 10-minute command
//! timeout and took its stack down. This model replays that morning second
//! by second through the real `decide()`, `Entry::follow()` and
//! `link_work()`, so a change to the rules shows what it does to it:
//!
//! - `the_macbook_morning`: the agents queue the run once the stack is up,
//!   with and without declaring it on the stack, at 11, 12.8 and 14.5 GB
//!   held by programs outside taskguard.
//! - `a_stack_and_its_run_queued_together`: the agents queue the run two
//!   seconds after the stack, with no declaration, a declaration, or only
//!   the history to go on.
//! - `a_large_job_at_the_front_on_omarchy`: a second stack that does not fit
//!   at the front of 25 jobs that do.
//!
//! `cargo test stall -- --nocapture --test-threads 1` prints the tables.

use crate::machine::{MachineSample, PEAK_WINDOW};
use crate::queue::{Decision, Entry, Expect, Limits, LongLived, decide, expire_guesses, link_lapsed, link_work};

const GB: u64 = 1024 * 1024;
const TICK: f64 = 1.0;

/// The machine and the load outside taskguard.
#[derive(Clone, Copy)]
pub struct Host {
    pub total_kb: u64,
    pub ncpu: usize,
    /// Memory programs outside taskguard hold, on average; it swings by
    /// `outside_swing_kb` around that.
    pub outside_kb: u64,
    pub outside_swing_kb: u64,
    /// macOS: the kernel's warning level above this share of RAM in use, and
    /// critical above `critical_pct`. Linux reports no levels: None.
    pub warning_pct: Option<f64>,
    pub critical_pct: f64,
}

impl Host {
    fn outside(&self, t: f64) -> u64 {
        let swing = (t / 37.0).sin() * 0.7 + (t / 11.0).sin() * 0.3;
        (self.outside_kb as f64 + swing * self.outside_swing_kb as f64) as u64
    }

    fn pressure(&self, pct: f64) -> f64 {
        match self.warning_pct {
            Some(_) if pct > self.critical_pct => 100.0,
            Some(w) if pct > w => 50.0,
            _ => 0.0,
        }
    }
}

/// The MacBook of the morning: 10 cores, 32 GB, the kernel at warning above
/// 78% held (where that morning's readings flipped), critical above 97%.
pub fn macbook(outside_gb: f64) -> Host {
    Host {
        total_kb: 32 * GB,
        ncpu: 10,
        outside_kb: (outside_gb * GB as f64) as u64,
        outside_swing_kb: 6 * GB / 10,
        warning_pct: Some(78.0),
        critical_pct: 97.0,
    }
}

/// The MacBook's limits: mem_max 95 (its user config), DALP's
/// max_backfill 1800, and the built-in rest.
pub fn macbook_limits() -> Limits {
    Limits {
        cpu_max_pct: 100.0,
        mem_max_pct: 95.0,
        learn_stagger: 5.0,
        max_bypass: 120.0,
        max_backfill: 1800.0,
        cpu_min_duration: 5.0,
        pressure_max: 20.0,
        warn_admit: true,
        noise_mem_pct: 2.0,
        noise_cpu: 0.5,
        outside_admit: true,
        outside_mem_max_pct: crate::commands::outside_ceiling(95.0, 92.0),
        partial_fit: 0.5,
    }
}

/// What a job is, for the model.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// The dev stack of agent `n`.
    Stack(usize),
    /// The browser run of agent `n`, against its stack.
    Run(usize),
    Lint,
    /// `Scenario::jobs[n]`.
    Job(usize),
}

/// A job that queues at `at` (or, before 0, already runs since then): it
/// holds `peak_kb` after a few seconds and ends after `secs`. A long-lived
/// one runs to the end.
#[derive(Clone)]
pub struct Job {
    pub at: f64,
    pub key: String,
    pub need_kb: u64,
    pub peak_kb: u64,
    pub cpu: f64,
    pub secs: f64,
    pub long_lived: bool,
    pub priority: i32,
}

/// One agent: it brings up a stack, queues its browser run once the stack is
/// up, gives up on the run after `give_up_s` in the queue and takes the stack
/// down, or takes it down `down_after_s` after the run ended. Then it starts
/// over after `pause_s`.
#[derive(Clone, Copy)]
pub struct Agent {
    pub first_at: f64,
    /// Memory the stack holds once it has started, and how long that takes.
    pub stack_idle_kb: u64,
    pub stack_ramp_s: f64,
    /// How much the stack grows while its run drives it.
    pub stack_grows: f64,
    pub stack_need_kb: u64,
    pub run_need_kb: u64,
    pub run_peak_kb: u64,
    pub run_cpu: f64,
    pub run_s: f64,
    pub give_up_s: f64,
    pub down_after_s: f64,
    pub pause_s: f64,
    /// The agent declares its browser run when it brings the stack up
    /// (`TASKGUARD_EXPECT=browser`), and the stack books for it what browser
    /// runs typically started with: the run's need.
    pub declares: bool,
    /// The agent queues its run 2 s after its stack, not once the stack is up.
    pub with_stack: bool,
    /// Undeclared, but the history says a browser run follows its stacks
    /// (`infer_work`): the stack books the run's usual need, for at most
    /// ten minutes.
    pub implied: bool,
    /// A bad guess: the agent only wants the stack, for `hold_s`, and never
    /// queues a run.
    pub no_run: bool,
    pub hold_s: f64,
}

pub fn agent(first_at: f64, idle_gb: f64) -> Agent {
    Agent {
        first_at,
        stack_idle_kb: (idle_gb * GB as f64) as u64,
        stack_ramp_s: 360.0,
        stack_grows: 1.13,
        stack_need_kb: 94 * GB / 10,
        run_need_kb: 43 * GB / 10,
        run_peak_kb: 3 * GB,
        run_cpu: 3.0,
        run_s: 240.0,
        give_up_s: 600.0,
        down_after_s: 20.0,
        pause_s: 30.0,
        declares: false,
        with_stack: false,
        implied: false,
        no_run: false,
        hold_s: 1200.0,
    }
}

/// Lints of a turbo run that began an hour earlier: it keeps `concurrency`
/// lints queued or running, each one `need_kb` (in turn), peaking at
/// `peak_kb` for `secs`.
#[derive(Clone)]
pub struct Lints {
    pub concurrency: usize,
    pub need_kb: Vec<u64>,
    pub peak_kb: u64,
    pub cpu: f64,
    pub secs: f64,
}

pub fn macbook_lints() -> Lints {
    Lints {
        concurrency: 3,
        need_kb: [46, 58, 66, 53, 73].iter().map(|g| g * GB / 10).collect(),
        peak_kb: 46 * GB / 10,
        cpu: 3.5,
        secs: 10.0,
    }
}

pub struct Scenario {
    pub host: Host,
    pub lim: Limits,
    pub agents: Vec<Agent>,
    pub lints: Option<Lints>,
    pub jobs: Vec<Job>,
    pub secs: f64,
    /// The scout's fallback, for comparison: a steady stack books what it
    /// holds, without room to grow.
    pub steady_books_live: bool,
    /// `expect_wait`.
    pub expect_wait: f64,
}

/// What came of it.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub runs_done: usize,
    pub runs_given_up: usize,
    /// The longest a run waited in the queue, started or not.
    pub worst_run_wait: f64,
    /// Seconds stacks stood up while their run was not running.
    pub stack_idle_s: f64,
    pub max_stacks: usize,
    pub worst_mem_pct: f64,
    /// Seconds memory in use was above the limit.
    pub over_limit_s: f64,
    pub lints_done: usize,
    pub worst_stack_wait: f64,
    /// Runs that started while their stack did not run: they fail.
    pub runs_without_stack: usize,
    /// The longest a lint waited.
    pub worst_lint_wait: f64,
    /// When each of `Scenario::jobs` started, if it did.
    pub job_started: Vec<Option<f64>>,
}

struct Live {
    e: Entry,
    kind: Kind,
    started: f64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Idle {
        until: f64,
    },
    /// The stack waits or starts; the run queues at `run_at` when the agent
    /// queues it together with the stack.
    StackQueued {
        run_at: Option<f64>,
    },
    StackUp,
    RunQueued {
        since: f64,
    },
    RunRunning {
        until: f64,
    },
    Down {
        at: f64,
    },
}

pub fn run(s: &Scenario) -> Outcome {
    let mut out = Outcome { job_started: vec![None; s.jobs.len()], ..Default::default() };
    let mut waiting: Vec<(Entry, Kind)> = Vec::new();
    let mut running: Vec<Live> = Vec::new();
    let mut phase: Vec<Phase> = s.agents.iter().map(|a| Phase::Idle { until: a.first_at }).collect();
    let mut ticket = 0u64;
    let mut recent: Vec<(f64, u64, u64)> = Vec::new();
    let mut lint_n = 0usize;
    let long = Some(LongLived { startup: 120.0, steady_mem_kb: None });
    let checkout = |n: usize| format!("/w/dalp/{n}");
    let job_entry = |n: usize, j: &Job, ticket: u64| Entry {
        ticket,
        pid: 8000 + n as i32,
        key: j.key.clone(),
        checkout: "/w/dalp/main".into(),
        queued_at: j.at,
        need_cpu: j.cpu,
        need_mem_kb: j.need_kb,
        known: true,
        est_dur_s: (!j.long_lived).then_some(j.secs),
        priority: j.priority,
        long_lived: j.long_lived,
        ..Default::default()
    };
    for (n, j) in s.jobs.iter().enumerate().filter(|(_, j)| j.at < 0.0) {
        ticket += 1;
        let e = Entry { started_at: Some(j.at), ..job_entry(n, j, ticket) };
        running.push(Live { e, kind: Kind::Job(n), started: j.at });
        out.job_started[n] = Some(j.at);
    }
    let mut t = 0.0;
    while t < s.secs {
        for (n, j) in s.jobs.iter().enumerate().filter(|(_, j)| j.at >= t && j.at < t + TICK) {
            ticket += 1;
            waiting.push((job_entry(n, j, ticket), Kind::Job(n)));
        }
        // Jobs that end by themselves.
        running.retain(|r| {
            let done = match r.kind {
                Kind::Lint => t - r.started >= s.lints.as_ref().map_or(0.0, |l| l.secs),
                Kind::Job(n) => !s.jobs[n].long_lived && t - r.started >= s.jobs[n].secs,
                _ => false,
            };
            if done {
                out.lints_done += 1;
            }
            !done
        });
        // The agents.
        for (n, a) in s.agents.iter().enumerate() {
            let stack_pid = 1000 + n as i32;
            match phase[n] {
                Phase::Idle { until } if t >= until => {
                    ticket += 1;
                    let expects = if a.declares {
                        vec![Expect { work: "browser".into(), mem_kb: a.run_need_kb, ..Default::default() }]
                    } else if a.implied {
                        vec![Expect {
                            work: "browser".into(),
                            mem_kb: a.run_need_kb,
                            implied: true,
                            expire_after_s: Some(600.0),
                            ..Default::default()
                        }]
                    } else {
                        Vec::new()
                    };
                    let e = Entry {
                        ticket,
                        pid: stack_pid,
                        key: "dev-env:stack".into(),
                        pool: Some("stack".into()),
                        checkout: checkout(n),
                        queued_at: t,
                        need_cpu: 3.0,
                        need_mem_kb: a.stack_need_kb,
                        known: true,
                        priority: 10,
                        long_lived: true,
                        expect_until: (!expects.is_empty() && s.expect_wait > 0.0).then_some(t + s.expect_wait),
                        expects,
                        ..Default::default()
                    };
                    waiting.push((e, Kind::Stack(n)));
                    phase[n] = Phase::StackQueued { run_at: (a.with_stack && !a.no_run).then_some(t + 2.0) };
                }
                Phase::StackQueued { run_at } => {
                    if run_at.is_some_and(|at| t >= at) {
                        ticket += 1;
                        waiting.push((run_entry(n, a, ticket, t), Kind::Run(n)));
                        phase[n] = Phase::RunQueued { since: t };
                    } else if let Some(r) = running.iter().find(|r| r.kind == Kind::Stack(n))
                        && t - r.started >= a.stack_ramp_s
                    {
                        phase[n] = Phase::StackUp;
                    }
                }
                Phase::StackUp if a.no_run => {
                    let started = running.iter().find(|r| r.kind == Kind::Stack(n)).map_or(t, |r| r.started);
                    phase[n] = Phase::Down { at: started + a.hold_s };
                }
                Phase::StackUp => {
                    ticket += 1;
                    waiting.push((run_entry(n, a, ticket, t), Kind::Run(n)));
                    phase[n] = Phase::RunQueued { since: t };
                }
                Phase::RunQueued { since } => {
                    if let Some(r) = running.iter().find(|r| r.kind == Kind::Run(n)) {
                        out.worst_run_wait = out.worst_run_wait.max(r.started - since);
                        phase[n] = Phase::RunRunning { until: r.started + a.run_s };
                    } else if t - since >= a.give_up_s {
                        // The agent's command timeout: the run leaves the queue
                        // and the agent takes its stack down.
                        out.worst_run_wait = out.worst_run_wait.max(t - since);
                        out.runs_given_up += 1;
                        waiting.retain(|(_, k)| *k != Kind::Run(n));
                        phase[n] = Phase::Down { at: t };
                    }
                }
                Phase::RunRunning { until } if t >= until => {
                    running.retain(|r| r.kind != Kind::Run(n));
                    out.runs_done += 1;
                    phase[n] = Phase::Down { at: t + a.down_after_s };
                }
                Phase::Down { at } if t >= at => {
                    running.retain(|r| r.kind != Kind::Stack(n));
                    waiting.retain(|(_, k)| *k != Kind::Stack(n));
                    phase[n] = Phase::Idle { until: t + a.pause_s };
                }
                _ => {}
            }
        }
        // The turbo run keeps its lints queued.
        if let Some(l) = &s.lints {
            let open = waiting.iter().filter(|(_, k)| *k == Kind::Lint).count() + running.iter().filter(|r| r.kind == Kind::Lint).count();
            for _ in open..l.concurrency {
                ticket += 1;
                lint_n += 1;
                let e = Entry {
                    ticket,
                    pid: 5000 + lint_n as i32,
                    key: format!("pkg{}:oxlint", lint_n % 40),
                    checkout: "/w/dalp/main".into(),
                    queued_at: t,
                    need_cpu: l.cpu,
                    need_mem_kb: l.need_kb[lint_n % l.need_kb.len()],
                    known: true,
                    est_dur_s: Some(l.secs),
                    pipeline: Some("turbo:1".into()),
                    pipeline_since: Some(-3600.0),
                    ..Default::default()
                };
                waiting.push((e, Kind::Lint));
            }
        }
        // What each running job holds and uses now.
        let queued: Vec<Entry> = waiting.iter().map(|(e, _)| e.clone()).collect();
        for r in running.iter_mut() {
            let age = t - r.started;
            let (mem, cpu) = match r.kind {
                Kind::Stack(n) => {
                    let a = &s.agents[n];
                    let busy = running_kind_any(&phase, n);
                    let grow = if busy { a.stack_grows } else { 1.0 };
                    let ramp = (age / a.stack_ramp_s).min(1.0);
                    ((a.stack_idle_kb as f64 * ramp * grow) as u64, if age < a.stack_ramp_s { 3.0 } else { 0.1 })
                }
                Kind::Run(n) => {
                    let a = &s.agents[n];
                    ((a.run_peak_kb as f64 * (age / 20.0).min(1.0)) as u64, a.run_cpu)
                }
                Kind::Lint => {
                    let l = s.lints.as_ref().expect("lints");
                    ((l.peak_kb as f64 * (age / 3.0).min(1.0)) as u64, l.cpu)
                }
                Kind::Job(n) => {
                    let j = &s.jobs[n];
                    let ramp = if j.long_lived { 300.0 } else { 3.0 };
                    ((j.peak_kb as f64 * (age / ramp).min(1.0)) as u64, if j.long_lived && age > ramp { 0.1 } else { j.cpu })
                }
            };
            r.e.live_mem_kb = mem;
            r.e.live_cpu = cpu;
            let lived = match r.kind {
                Kind::Stack(_) => long,
                Kind::Job(n) if s.jobs[n].long_lived => long,
                _ => None,
            };
            r.e.follow(mem, cpu, s.host.ncpu as f64, t, lived);
            // As the owner does: a guess of the history that did not come
            // true in time stops booking.
            expire_guesses(&mut r.e, &queued, t);
            if s.steady_books_live && r.e.steady() {
                r.e.need_mem_kb = mem;
            }
        }
        let ours: u64 = running.iter().map(|r| r.e.live_mem_kb).sum();
        let used = s.host.outside(t) + ours;
        let pct = used as f64 * 100.0 / s.host.total_kb as f64;
        out.worst_mem_pct = out.worst_mem_pct.max(pct);
        if pct > s.lim.mem_max_pct {
            out.over_limit_s += TICK;
        }
        let stacks =
            running.iter().filter(|r| matches!(r.kind, Kind::Stack(_)) || matches!(r.kind, Kind::Job(n) if s.jobs[n].long_lived)).count();
        out.max_stacks = out.max_stacks.max(stacks);
        for (n, p) in phase.iter().enumerate() {
            if matches!(p, Phase::RunQueued { .. }) && running.iter().any(|r| r.kind == Kind::Stack(n)) {
                out.stack_idle_s += TICK;
            }
        }
        recent.retain(|(ts, _, _)| t - ts < PEAK_WINDOW);
        recent.push((t, used, ours));
        let busy = (1.0 + running.iter().map(|r| r.e.live_cpu).sum::<f64>()).min(s.host.ncpu as f64);
        let m = MachineSample {
            ts: t,
            cpu_busy: busy,
            ncpu: s.host.ncpu,
            mem_used_kb: used,
            mem_total_kb: s.host.total_kb,
            mem_pressure: Some(s.host.pressure(pct)),
            recent_mem: recent.clone(),
            ..Default::default()
        };
        // Every waiting job decides once per tick, as the runners do.
        let mut i = 0;
        while i < waiting.len() {
            let run_now: Vec<Entry> = running.iter().map(|r| r.e.clone()).collect();
            let all: Vec<Entry> = waiting.iter().map(|(e, _)| e.clone()).collect();
            // As the runner does: work links to the stack that expects it,
            // and goes one priority above it, until the stack is gone or only
            // guessed it and the guess has expired or come true.
            let me = waiting[i].0.clone();
            if link_lapsed(&me, &run_now, &all, |w| me.pool.as_deref() == Some(w)) {
                waiting[i].0.work_for = None;
                waiting[i].0.priority = match waiting[i].1 {
                    Kind::Job(n) => s.jobs[n].priority,
                    _ => 0,
                };
            }
            if waiting[i].0.work_for.is_none() {
                let me = waiting[i].0.clone();
                if let Some((link, p)) = link_work(&me, &run_now, &all, |w| me.pool.as_deref() == Some(w)) {
                    waiting[i].0.priority = me.priority.max(p + 1);
                    waiting[i].0.work_for = Some(link);
                }
            }
            // As the runner does: a waiting stack books at least what the
            // work that queued for it needs.
            if waiting[i].0.long_lived {
                let pid = waiting[i].0.pid;
                for x in 0..waiting[i].0.expects.len() {
                    let theirs = all
                        .iter()
                        .filter(|w| w.work_for.as_ref().is_some_and(|l| l.pid == pid && l.item == Some(x as u32)))
                        .map(|w| w.need_mem_kb)
                        .max()
                        .unwrap_or(0);
                    let item = &mut waiting[i].0.expects[x];
                    item.mem_kb = item.mem_kb.max(theirs);
                    if theirs > 0 {
                        (item.implied, item.expire_after_s) = (false, None);
                    }
                }
            }
            let all: Vec<Entry> = waiting.iter().map(|(e, _)| e.clone()).collect();
            let (me, kind) = waiting[i].clone();
            let d = decide(&m, &s.lim, &run_now, &all, &me, t, &[]);
            if matches!(d, Decision::Admit { .. }) {
                // The nudge to the stack: its work has started.
                if let Some(w) = &me.work_for
                    && let Some(i) = w.item
                    && let Some(st) = running.iter_mut().find(|r| r.e.pid == w.pid)
                {
                    st.e.expects[i as usize].started = true;
                    for x in st.e.expects.iter_mut().filter(|x| x.implied) {
                        x.started = true;
                    }
                }
                for (w, _) in waiting.iter_mut().filter(|(w, _)| w.ahead_of(&me) && w.bypassed_since.is_none()) {
                    w.bypassed_since = Some(t);
                }
                if let Kind::Stack(_) = kind {
                    out.worst_stack_wait = out.worst_stack_wait.max(t - me.queued_at);
                }
                if let Kind::Job(n) = kind {
                    out.job_started[n] = Some(t);
                }
                if kind == Kind::Lint {
                    out.worst_lint_wait = out.worst_lint_wait.max(t - me.queued_at);
                }
                if let Kind::Run(n) = kind
                    && !running.iter().any(|r| r.kind == Kind::Stack(n))
                {
                    out.runs_without_stack += 1;
                }
                let e = Entry { started_at: Some(t), start_need_cpu: Some(me.need_cpu), start_need_mem_kb: Some(me.need_mem_kb), ..me };
                running.push(Live { e, kind, started: t });
                waiting.remove(i);
            } else {
                i += 1;
            }
        }
        t += TICK;
    }
    out
}

/// The browser run of agent `n`.
fn run_entry(n: usize, a: &Agent, ticket: u64, t: f64) -> Entry {
    Entry {
        ticket,
        pid: 2000 + n as i32,
        key: format!("e2e/agentic:bun_src/pr.ts_--pr_{}", 17397 + n),
        pool: Some("browser".into()),
        checkout: format!("/w/dalp/{n}"),
        queued_at: t,
        need_cpu: a.run_cpu,
        need_mem_kb: a.run_need_kb,
        known: true,
        raised_by_min: true,
        est_dur_s: Some(a.run_s),
        ..Default::default()
    }
}

/// Does agent `n`'s run run now?
fn running_kind_any(phase: &[Phase], n: usize) -> bool {
    matches!(phase[n], Phase::RunRunning { .. })
}

/// The MacBook morning with programs outside taskguard at `outside_gb`.
pub fn morning(outside_gb: f64) -> Scenario {
    Scenario {
        host: macbook(outside_gb),
        lim: macbook_limits(),
        agents: vec![agent(0.0, 7.0), agent(380.0, 4.6)],
        lints: Some(macbook_lints()),
        jobs: Vec::new(),
        secs: 7200.0,
        steady_books_live: false,
        expect_wait: 5.0,
    }
}

/// The same morning, with each agent declaring its browser run on its stack.
pub fn declared_morning(outside_gb: f64) -> Scenario {
    let mut s = morning(outside_gb);
    for a in &mut s.agents {
        a.declares = true;
    }
    s
}

/// The same morning, undeclared, with the history inferring each run.
pub fn inferred_morning(outside_gb: f64) -> Scenario {
    let mut s = morning(outside_gb);
    for a in &mut s.agents {
        a.implied = true;
    }
    s
}

/// A bad guess: the history says a run follows each stack, but the agents
/// only want their stacks, for 20 minutes each, while the lints keep coming.
pub fn bad_guess(outside_gb: f64, infer: bool) -> Scenario {
    let mut s = morning(outside_gb);
    for a in &mut s.agents {
        (a.implied, a.no_run) = (infer, true);
    }
    s
}

/// Omarchy (24 cores, 31.3 GB, Linux, mem_max 95, DALP's max_backfill 1800):
/// a stack has run for an hour, a second stack of 9.1 GB queues that does
/// not fit next to it, and 25 jobs queue behind it over five minutes: lints
/// of about 5 GB, which fit only one at a time, and small tests, which fit.
pub fn head_of_line() -> Scenario {
    let gb = |g: f64| (g * GB as f64) as u64;
    let mut jobs = vec![
        Job {
            at: -3600.0,
            key: "dev-env:stack".into(),
            need_kb: gb(10.2),
            peak_kb: gb(8.2),
            cpu: 3.0,
            secs: 0.0,
            long_lived: true,
            priority: 10,
        },
        Job {
            at: 0.0,
            key: "dev-env:stack".into(),
            need_kb: gb(9.1),
            peak_kb: gb(7.0),
            cpu: 3.0,
            secs: 0.0,
            long_lived: true,
            priority: 10,
        },
    ];
    for i in 0..25 {
        let at = 2.0 + 12.0 * i as f64;
        jobs.push(if i % 5 < 3 {
            let need = [4.7, 5.0, 5.5, 6.1, 4.9][i % 5];
            Job {
                at,
                key: format!("pkg{i}:oxlint"),
                need_kb: gb(need),
                peak_kb: gb(4.5),
                cpu: 3.0,
                secs: 60.0,
                long_lived: false,
                priority: 0,
            }
        } else {
            let need = [0.2, 0.9, 0.5][i % 3];
            Job {
                at,
                key: format!("pkg{i}:bun_test"),
                need_kb: gb(need),
                peak_kb: gb(need),
                cpu: 1.5,
                secs: 30.0,
                long_lived: false,
                priority: 0,
            }
        });
    }
    let host =
        Host { total_kb: gb(31.3), ncpu: 24, outside_kb: gb(15.8), outside_swing_kb: gb(0.3), warning_pct: None, critical_pct: 100.0 };
    let lim = Limits { warn_admit: false, ..macbook_limits() };
    Scenario { host, lim, agents: Vec::new(), lints: None, jobs, secs: 1800.0, steady_books_live: false, expect_wait: 5.0 }
}

/// The longest a small job (under 1 GB) of `s` waited, or the scenario's
/// length for one that never started.
pub fn worst_small_wait(s: &Scenario, o: &Outcome) -> f64 {
    s.jobs
        .iter()
        .zip(&o.job_started)
        .filter(|(j, _)| j.at >= 0.0 && j.need_kb < GB)
        .map(|(j, t)| t.unwrap_or(s.secs) - j.at)
        .fold(0.0, f64::max)
}

/// How many of the jobs of `s` that end by themselves started.
pub fn jobs_started(s: &Scenario, o: &Outcome) -> usize {
    s.jobs.iter().zip(&o.job_started).filter(|(j, t)| !j.long_lived && t.is_some()).count()
}

pub fn header() -> String {
    format!(
        "{:<34} {:>5} {:>6} {:>9} {:>8} {:>6} {:>6} {:>6} {:>6} {:>9}",
        "variant", "runs", "gave", "worst run", "stack", "stacks", "worst", "over", "lints", "worst"
    ) + &format!(
        "\n{:<34} {:>5} {:>6} {:>9} {:>8} {:>6} {:>6} {:>6} {:>6} {:>9}",
        "", "done", "up", "wait", "idle", "at once", "mem", "limit", "done", "stack wait"
    )
}

pub fn row(name: &str, o: &Outcome) -> String {
    format!(
        "{name:<34} {:>5} {:>6} {:>8.0}s {:>7.0}s {:>6} {:>5.1}% {:>5.0}s {:>6} {:>8.0}s",
        o.runs_done,
        o.runs_given_up,
        o.worst_run_wait,
        o.stack_idle_s,
        o.max_stacks,
        o.worst_mem_pct,
        o.over_limit_s,
        o.lints_done,
        o.worst_stack_wait
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_macbook_morning() {
        eprintln!("{}", header());
        for outside in [11.0, 12.8, 14.5] {
            let o = run(&morning(outside));
            eprintln!("{}", row(&format!("outside {outside} GB"), &o));
            // 0.10.0 also capped outside_admit at pause_at, under mem_max.
            let old = Scenario { lim: Limits { outside_mem_max_pct: 92.0, ..macbook_limits() }, ..morning(outside) };
            eprintln!("{}", row(&format!("outside {outside} GB, outside cap 92%"), &run(&old)));
            let declared = run(&declared_morning(outside));
            eprintln!("{}", row(&format!("outside {outside} GB, declared"), &declared));
            let inferred = run(&inferred_morning(outside));
            eprintln!("{}", row(&format!("outside {outside} GB, inferred"), &inferred));
            // The scout's fallback, not taken: a steady stack books what it
            // holds. It gives runs up at 12.8 GB.
            let live = Scenario { steady_books_live: true, ..morning(outside) };
            eprintln!("{}", row(&format!("outside {outside} GB, steady books live"), &run(&live)));
            let both = Scenario { steady_books_live: true, ..declared_morning(outside) };
            eprintln!("{}", row(&format!("outside {outside} GB, declared + live"), &run(&both)));

            // A caller that declares its run never gives it up, and memory
            // stays under the limit.
            assert_eq!(declared.runs_given_up, 0, "{outside} GB declared: {declared:?}");
            assert!(declared.runs_done >= 10, "{outside} GB declared: {declared:?}");
            assert!(declared.worst_mem_pct <= 95.0 && declared.over_limit_s == 0.0, "{outside} GB declared: {declared:?}");
            // Nor does one that does not declare, whose run the history infers.
            assert_eq!(inferred.runs_given_up, 0, "{outside} GB inferred: {inferred:?}");
            assert!(inferred.worst_mem_pct <= 95.0, "{outside} GB inferred: {inferred:?}");
            // A caller that does not declare: the progress rule alone clears
            // the morning's load, never a second stack over the limit.
            if outside < 14.0 {
                assert_eq!(o.runs_given_up, 0, "{outside} GB: {o:?}");
            }
            assert!(o.worst_mem_pct <= 95.0, "{outside} GB: {o:?}");
        }
    }

    #[test]
    fn a_stack_and_its_run_queued_together() {
        eprintln!("{}", header());
        for outside in [11.0, 12.8, 14.5] {
            for (name, declares, implied, wait) in [
                ("nothing", false, false, 5.0),
                ("history implies", false, true, 5.0),
                ("declared", true, false, 5.0),
                ("declared, no wait", true, false, 0.0),
            ] {
                let mut s = Scenario { expect_wait: wait, ..morning(outside) };
                for a in &mut s.agents {
                    (a.with_stack, a.declares, a.implied) = (true, declares, implied);
                }
                let o = run(&s);
                eprintln!("{} {:>3} without stack", row(&format!("together {outside} GB, {name}"), &o), o.runs_without_stack);
                if declares || implied {
                    // The run waits for its stack, and the two fit together.
                    assert_eq!((o.runs_given_up, o.runs_without_stack), (0, 0), "{outside} GB, {name}: {o:?}");
                    assert!(o.worst_mem_pct <= 95.0, "{outside} GB, {name}: {o:?}");
                }
                if implied {
                    // A run the history implied waits at most a few seconds for
                    // its stack after the stack has started.
                    assert!(o.stack_idle_s <= 60.0 || outside < 12.0, "{outside} GB, {name}: {o:?}");
                }
            }
        }
    }

    #[test]
    fn a_bad_guess_of_the_history_starves_nothing() {
        eprintln!("{:<34} {:>6} {:>10} {:>6} {:>9} {:>11}", "variant", "lints", "worst lint", "worst", "stacks", "worst stack");
        eprintln!("{:<34} {:>6} {:>10} {:>6} {:>9} {:>11}", "", "done", "wait", "mem", "at once", "wait");
        for outside in [11.0, 12.8, 14.5] {
            let plain = run(&bad_guess(outside, false));
            let guessed = run(&bad_guess(outside, true));
            for (name, o) in [("no run follows, no inference", &plain), ("no run follows, inferred", &guessed)] {
                eprintln!(
                    "{:<34} {:>6} {:>9.0}s {:>5.1}% {:>9} {:>10.0}s",
                    format!("{outside} GB, {name}"),
                    o.lints_done,
                    o.worst_lint_wait,
                    o.worst_mem_pct,
                    o.max_stacks,
                    o.worst_stack_wait
                );
            }
            // The guess books room for ten minutes at most and never stops
            // the progress rule: no lint waits longer than without it, give
            // or take a minute, though fewer run while a guess books room.
            assert!(guessed.worst_lint_wait <= plain.worst_lint_wait + 60.0, "{outside} GB: {guessed:?} vs {plain:?}");
            assert!(guessed.lints_done * 10 >= plain.lints_done * 7, "{outside} GB: {guessed:?} vs {plain:?}");
            // A stack started with no work respects the other stack's open
            // guess: it waits at most until that guess expires.
            assert!(guessed.worst_stack_wait <= plain.worst_stack_wait + 600.0 + 60.0, "{outside} GB: {guessed:?} vs {plain:?}");
        }
    }

    #[test]
    fn a_large_job_at_the_front_on_omarchy() {
        let s = head_of_line();
        let o = run(&s);
        eprintln!(
            "head of line: second stack at {:?}, {} of 25 jobs started, small jobs waited up to {:.0}s, worst memory {:.1}%",
            o.job_started[1],
            jobs_started(&s, &o),
            worst_small_wait(&s, &o),
            o.worst_mem_pct
        );
        // 0.10.0 started the second stack at once, to 114% of RAM, and small
        // jobs waited 1000 s. It does not fit next to the first: it waits,
        // and holds nobody back.
        assert_eq!(o.job_started[1], None);
        assert!(o.worst_mem_pct < 100.0, "{o:?}");
        assert!(jobs_started(&s, &o) >= 24, "{o:?}");
        assert!(worst_small_wait(&s, &o) < 300.0, "{o:?}");
    }
}
