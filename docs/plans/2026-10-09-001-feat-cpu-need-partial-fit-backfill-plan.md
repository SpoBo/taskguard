---
title: "feat: learn CPU need from use, partial fit, bounded backfill, advice for agents"
type: feat
status: active
date: 2026-10-09
issue: https://github.com/SpoBo/taskguard/issues/19
product_contract_source: ce-plan-bootstrap
---

# feat: learn CPU need from use, partial fit, bounded backfill, advice for agents

## Goal Capsule

- Objective: one big CPU job (a `tsgo` typecheck) must never hold the whole queue for half an hour again. Small jobs keep moving, and the big job always ends up running.
- Means: learn a realistic CPU need, let a job that can never fully fit start on part of its need at a quiet moment, turn backfill on with a drain cut-off, and tell agents what is going on and how to force a start.
- Done when: all four parts of issue #19 ship with tests, docs, a CHANGELOG line, and no change to stored data formats beyond optional growth.

## Product Contract

### Summary

Fix the four causes in issue #19 inside taskguard: CPU need learned from cores used (recency-weighted, outliers weighted down by `outlier_weights`, no ever-rising starved rule); partial fit with a per-machine `partial_fit` setting that starts near a recent low point of machine CPU use; backfill on by default with a drain that always ends with the big job running; an advice block in the wait output plus `taskguard start JOB`.

### Problem Frame

On Voyager (18 cores, `cpu_max = 150`) a `tsgo` typecheck at the head of the queue held 46 jobs back for up to 38 minutes. The learned CPU need (17-18 cores) came from the peak of cpu + runnable time, and runnable time is time spent waiting for a core, which one-thread-per-core tools always do on a busy machine. A CPU-starved run raised the need to what it "wanted", so the number only went up. The job could not fit, became `Reserved` after `max_bypass`, and then even 1-core test jobs could not start. Backfill was off by default. Agents only see wait lines, and had no way to start the job by hand outside `taskguard top`.

### Requirements

- R1. A job's CPU need is learned from the cores it used (never from runnable wait).
- R2. Recent runs weigh more than older runs.
- R3. Runs whose CPU use stands far above the others are weighted down by the existing outlier rules (`outlier_ratio`, `outlier_weights`): the first such run counts 0, the second half, repeats in full.
- R4. A CPU-starved run no longer raises the need to what it wanted. The need is bounded by what runs used.
- R5. No new stored fields; the database schema, queue entries and job keys stay the same. The shared `machine` file may only gain an optional field.
- R6. A job that cannot fit now (its need is above the whole CPU limit, or it has waited past `max_bypass`) may start when the machine can give it at least a set share of its CPU need (`partial_fit`, per machine, 0 turns it off). Memory stays a hard rule for every automatic start; only a start by hand (R10) skips it, and the advice says so.
- R7. Such a start happens only when machine CPU use is near its recent low point.
- R8. Backfill is on by default. Past `max_backfill` only jobs that end before the big job could start still pass, and the drain always ends with the big job running, also when its need is larger than the whole CPU limit. This rests on the existing rule that the first job in line starts when no job runs, whatever it lacks, so it holds for memory-blocked jobs too (only memory pressure, a kernel alarm, still holds it).
- R9. When a waiting job cannot fit (need over the whole limit, or held by CPU past `max_bypass`), its wait output shows an advice block: need, limit, what holds the room and for how long, how many jobs it holds back, how long it waited, what happens when forced, the command to force it, and the settings that change the outcome.
- R10. `taskguard start JOB` starts a waiting job by hand (pid, ticket, key or part of a key).
- R11. A CHANGELOG line under `## [Unreleased]`; README and `taskguard.example.toml` describe the new behavior and settings.

### Scope Boundaries

- Capping tool thread counts (`--threads`, `GOMAXPROCS`) is out of scope (issue #20).
- `taskguard outliers` and `prune --outliers` keep listing memory outliers only.

#### Deferred to Follow-Up Work

- Listing CPU outliers in `taskguard outliers` / `prune --outliers`. CPU outliers are already weighted down automatically by learning, so there is little to prune by hand.

## Planning Contract

### Key Technical Decisions

- KTD1. Per-run CPU value is `coalesce(cores_used, cores_wanted)`. `cores_used` is the existing peak 10-second window of used CPU (floored at cpu time / wall) and excludes runnable wait. session-settled (user-directed): learn from cores used, not from cpu + runnable. Rejected: peak of cpu + runnable. Using the stored column needs no new data (R5); old rows without it fall back to `cores_wanted`.
- KTD2. The learned CPU need is a weighted median. Each run's weight is `0.5^(i / 4)` (i = 0 for the newest run), times its outlier weight. session-settled (user-directed): recency-weighted with outliers left out. Rejected: plain median of the last `hist_keep` runs. A weighted median keeps the median's resistance to single odd runs while following trends.
- KTD3. CPU outliers reuse the memory outlier cut. The cut in `find_outliers` is generalised over a value and a minimum; for CPU both minimums (the step and the level below it) are 1 core (constant). The weight is count-based as for memory: with k healthy outliers in the window, every healthy outlier run gets `outlier_weights[k-1]` (1.0 past the list), multiplied with its recency weight in the weighted median; a failed outlier run counts 0.
- KTD4. The "starved run raises the need" rule is removed. Order: the outlier cut (KTD3) runs over all recent runs first. Then CPU-starved runs (`starved` in `cpu`, `slowdown`) are left out only when at least 3 non-starved runs remain; otherwise every run's used value counts. One quiet-machine run can then not set the need alone, and the need can never rise above any run's actual use.
- KTD4b. Running jobs follow cores used, not cores wanted: `Entry::follow` raises `need_cpu` from the reading's `used_recent` (10 s window), so a running job's reserve never books runnable wait. The CPU `--min-cpu` suggestion after a starved run (`insight::advice`, `after_starved` `suggest_min`) uses cores used too, so the advice never brings the inflated number back.
- KTD5. The first-run estimate from similar jobs reads `coalesce(cores_used, cores_wanted)` too, so it is not inflated.
- KTD6. "Recent low point" comes from a new optional `recent_cpu: Vec<(f64, f64)>` field in `MachineSample` (time, smoothed busy cores) over the last `LOW_WINDOW = 120` s. Near the low means `cpu_busy <= low + max(1.0, 0.1 * ncpu)`. A file written by an older version has no list; then the current reading is the low, so behavior falls back to "start when it passes".
- KTD7. Partial fit lives in `Room::early`, next to `noise_cpu` and `outside_admit`. It applies only when every blocker is CPU, the job is not short, and the job can never fully fit: its need is above the whole CPU limit, or it has waited longer than `max_bypass`. It starts when `cpu_limit - busy - reserve >= partial_fit * need` and the machine is near its recent low. The start prints the existing "started before it fully fits" warning. A job started by partial fit books the room it started on (`cpu_limit - busy - reserve` at admission, at least `partial_fit * need`) as its `need_cpu` and `start_need_cpu`, carried out of `decide` in an optional `booked_cpu` field of `Decision::Admit`; from then on it grows only from measured use. So a partially started job never books more than the limit, and small jobs keep moving.
- KTD8. New setting `partial_fit` (fraction of the CPU need, default `0.5`, `0` turns it off), set like every other top-level key, so the user config is per machine and a repo may override it. Carried in `queue::Limits`.
- KTD9. `max_backfill` default becomes `600` s (10 minutes). The existing drain logic (`holds` + `soonest_start`) already falls back to "when no job runs" for a CPU blocker no running job can free; this is verified by a test with need > limit. For a head that qualifies for partial fit, `soonest_start` measures its CPU shortfall against `partial_fit * need` instead of the full need.
- KTD10. The advice block is built by a pure function in `report.rs` and printed by the waiting loop once when the condition first holds and again with each `status_every` line. It is not gated by `hints` (agents need it most), only by `--quiet`.
- KTD11. A start by hand skips every check, memory included, as `g` in `taskguard top` does today: it is a person or agent overriding taskguard. `taskguard start JOB` writes the existing `start` nudge and waits up to 10 s for the job to move to running. A job owned by a version that does not read nudges (`version` is `None`) is refused with a clear message. The admit reason becomes "started by hand".

### Assumptions

- Default `partial_fit = 0.5`: a job may start on half its CPU need.
- The recency half-life of 4 runs and the 120 s low-point window are constants, not settings.
- Partial fit counts as "can never fully fit" when the need is above the whole limit or the job waited past `max_bypass`; other short-on-CPU jobs keep the full check.
- Default `max_backfill = 600`.
- The advice prints with hints off too, since it carries the only way out for an agent.

### Risks

- Learning from used CPU lowers needs, so more jobs start at once on a crowded machine. Mitigation: they still pass the CPU check, and the weighted median follows a job that gets slower.
- Turning backfill on by default changes order for every user. Mitigation: the drain past `max_backfill` is bounded, and it is documented in CHANGELOG and README.
- `start` becomes a reserved subcommand name: `taskguard start` no longer runs a program named `start`. `taskguard run -- start` still does. Note in CHANGELOG.
- Mixed versions: an older recorder rewrites the `machine` file without `recent_cpu` for as long as it runs, so the near-the-low gate is off then and partial fit starts whenever room passes. A head job owned by an older version never takes a partial fit itself; newer waiters may mark it stalled. Both are documented under "Mixed versions" in `README.md`.

## Implementation Units

### U1. Learn CPU need from cores used, recency-weighted, outliers out

- Goal: R1-R5.
- Files: `src/db.rs` (`Learned`, `Learn`, `find_outliers`, `Db::learned`, `Db::estimate`, tests), `src/queue.rs` (`Entry::follow`), `src/runner.rs` (`follow` call, `after_starved`), `src/insight.rs` (`advice` min-cpu value).
- Approach: select `coalesce(cores_used, cores_wanted)` and `starved` per run; generalise the outlier cut; weighted median per KTD2-KTD4; remove the starved raise. Update doc comments.
- Test scenarios (`src/db.rs` tests):
  - A job whose runs used 4 cores but wanted 17 learns about 4.
  - A recent change wins: 6 old runs at 8 cores then 3 new runs at 2 cores gives a need near 2, not 8.
  - One run at 16 cores among runs at 4 is left out; seen three times it counts.
  - A CPU-starved latest run does not raise the need; all-starved history uses their used values.
  - 8 starved runs that used 6 cores and 1 non-starved run that used 18 learn about 6.
  - Old rows with `cores_used` NULL fall back to `cores_wanted`.
  - A running job whose readings show 4 cores used and 17 wanted books about 4 (`src/queue.rs` test).
  - The starved-run advice for a run that used 4 and wanted 17 cores suggests `--min-cpu 4` (`src/insight.rs` test).
  - The estimate from similar jobs uses cores used.

### U2. Recent low point of machine CPU use

- Goal: R5, R7.
- Files: `src/machine.rs`.
- Approach: add `recent_cpu` (optional by `#[serde(default)]`), kept for `LOW_WINDOW` seconds in `measure`; add `cpu_low()` and `near_cpu_low()` helpers.
- Test scenarios: a file without `recent_cpu` reads; the low is the minimum of the window; old readings fall out of the window; with no list, the current reading counts as the low.

### U3. Partial fit

- Goal: R6, R7.
- Files: `src/queue.rs` (`Limits.partial_fit`, `Room::early`, tests), `src/config.rs` (setting, default, layer merge, origin, tests), `src/commands.rs` and `src/runner.rs` (`limits`), `src/sim.rs` (LIM).
- Approach: per KTD7-KTD8.
- Test scenarios (`src/queue.rs` tests):
  - A 30-core job on a 27-core limit with 16 cores free and the machine at its low starts with an early reason naming partial_fit.
  - The same job with only 10 cores free waits.
  - The same job when the machine is well above its recent low waits.
  - Memory short still blocks a partial fit.
  - `partial_fit = 0` keeps the old behavior.
  - A job under the limit that has not waited past `max_bypass` gets no partial fit.
  - A partially started job books the room it started on, so a 1-core short job can still start after it.

### U4. Backfill on by default, drain ends with the big job

- Goal: R8.
- Files: `src/config.rs` (default, test that says backfill is off), `src/queue.rs` tests, `src/sim.rs` if a scenario assumes 0.
- Test scenarios: default config has `max_backfill = 600`; a reserved job whose need is above the whole limit lets jobs that end before the drain pass and holds the rest past `max_backfill`; with nothing running it starts.

### U5. Advice block for agents

- Goal: R9.
- Files: `src/report.rs` (pure `cpu_advice` builder + test), `src/runner.rs` (print in the waiting loop).
- Content: need vs limit; running jobs that hold CPU with need and time left; how many waiting jobs are behind; waited time; forced start runs slower and skips every check including memory; `taskguard start <pid>`; settings `cpu_max`, `partial_fit`, `max_backfill`, and `taskguard prune <key>` for an old inflated history.
- Test scenarios: a job needing 30 of 27 cores gets lines with "30.0", "27.0", the holder key, "taskguard start", "partial_fit"; a job that fits gets no advice.

### U6. `taskguard start JOB`

- Goal: R10.
- Files: `src/commands.rs` (`start`), `src/main.rs` (SUBCOMMANDS, usage, dispatch), `src/help.rs` (help text), `src/runner.rs` (admit reason), `tests/cli.rs`.
- Test scenarios (`tests/cli.rs`): a job waiting behind a `--min-cpu` too large to fit starts after `taskguard start <pid>`; `taskguard start nothing` fails with a list of waiting jobs.

### U7. Docs

- Goal: R11.
- Files: `CHANGELOG.md`, `README.md`, `taskguard.example.toml`.

## Verification Contract

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` in this repo.
- No DALP turbo batches.

## Definition of Done

- All units done, tests pass, CHANGELOG line under `## [Unreleased]`, README "Mixed versions" notes the new `machine` field and setting.
