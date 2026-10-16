# Changelog

What changed in each release of taskguard. The newest release is first.

Each release has a section `## [VERSION] - DATE`. Work that is not released
yet goes under `## [Unreleased]`. A release renames that section to the new
version. The release workflow uses the section as the text of the GitHub
release. The format follows [Keep a Changelog](https://keepachangelog.com),
and the versions follow [semver](https://semver.org).

## [Unreleased]

### Added

- `taskguard dump PATTERN | --cmd "COMMAND" | --run ID` prints everything
  taskguard knows about a job as one JSON document: how the command matches,
  the rules in force and which file set them, what the next run reserves and
  why, step by step (history, boost, first guess, caps, minimums), each recent
  run with what it waited for, what it changed, and its role in the need
  (outlier, failed and ignored, boost), pruned runs, and the live queue entry.
  Use it to find out why a job reserved what it did.
- Each run now stores how its need was made (`runs.need_why`), so `dump`
  can explain an old run too, not only the next one.
- A long-lived job can declare the work that will run on it:
  `--expect WORK` (repeatable) or `TASKGUARD_EXPECT=WORK,WORK`, where WORK is
  a pool name, a job key or a command pattern. The stack books room for that
  work (learned as the work learns its own need) and starts only with room
  for both. A job in the same checkout that matches is its work: it goes one
  priority above the stack, takes over the booked room when it starts, and
  waits while its stack still waits. `taskguard status` shows `(for stack
  KEY)` on the work and `(expects WORK, books SIZE)` on the stack. Without a
  declaration, only what the history infers applies (`infer_work`, below).
- A long-lived job with no declared work books the work its history says
  follows it: work of one kind (a pool, or a key outside any pool) that
  queued in its checkout within `infer_work_within` (900) seconds of its
  start on at least `infer_work_min` (3) of its last `infer_work_of` (5)
  runs there. It books that work's usual need, shown as `(expects WORK from
  history, ...)`, for twice its usual delay and ten minutes at most, and
  drops it once work for the stack starts. While the guess is open the
  stack waits `expect_wait` for the work, and matching work links to it and
  goes first; an expired or started guess links nothing, and work linked to
  it drops back to its own priority. The guess never stops the rule for
  nothing running. Another stack respects it while it is open, also one
  started with no declared work, so such a stack waits at most until the
  guess expires; a stack that still fits next to it starts at once. A
  declaration wins; `--expect none` expects nothing; `infer_work = false`
  turns it off.
- `expect_wait` (5 seconds by default, 0 turns it off): a long-lived job that
  expects work, declared or from its history, waits that long before it
  starts for the work to queue, so a stack and a run started together start
  with room for both. A stack that expects nothing starts at once.

### Changed

- With nothing running, or nothing but long-lived jobs past their start-up,
  the first job in line that fits starts, up to all of RAM, rather than the
  first in line whatever the memory reading says. A long-lived job, such as a
  second dev stack, starts that way only under `mem_max`, or, when only
  programs outside taskguard keep it out, up to the ceiling `outside_admit`
  allows. A job that does not fit holds nobody back, so the jobs behind it
  that fit start. At macOS
  warning this rule still decides when only long-lived jobs run.
- A waiting job that would not fit even once every job that ends by itself
  has ended holds nobody back, whatever its priority or reservation: it can
  only start once a long-lived job is stopped or other programs free memory,
  and the small jobs behind it start meanwhile.
- A stack with work queued for it, and work whose stack runs, hold their turn
  as a strict reservation, also while they do not fit yet, rather than letting
  jobs that fit start meanwhile (backfill).

### Fixed

- A second dev stack is no longer started over memory while the first one's
  run waits: the rule for "nothing runs but long-lived jobs" started the head
  of the line whatever the readings said, and skipped itself at macOS
  warning, so the run waited until its caller gave up.
- The NEXT line names a long-lived job that holds the room as one that must
  be stopped, not as one that will finish.
- `outside_admit` works again when `mem_max` is at or above `pause_at`
  (92% by default), as with `mem_max = 95`. It capped memory at `pause_at`,
  under the limit, so it could never let a job start; the cap is now
  halfway from `mem_max` to all of RAM in that case.

## [0.10.0] - 2026-10-16

### Fixed

- A run that failed no longer raises a job's memory need above what its
  passing runs took, also with fewer than three runs and when it is no
  outlier. One `tsc` run that failed at 14.5 GB made every later run reserve
  18 GB. When every recent run failed, one high peak alone does not set the
  need: the job reserves `new_job_mem` until a peak comes back.
- A failed run starved of memory adds the 25% boost only when its peak stayed
  within that 25% of its need.
- `taskguard outliers`, `doctor --explain` and the job's end line show failed
  runs whose peak is ignored, and why.

### Changed

- `outlier_ratio` defaults to 1.5 (was 2): a one-off 17 GB peak where a job
  usually takes 9 GB no longer sets the need for its next 10 runs. A job with
  two sizes, such as a cache hit and a full build, still reserves the big one.

## [0.9.0] - 2026-10-12

### Added

- `taskguard top`: the Queue view is a tree. The jobs of one task-runner
  run sit under one row, named by the command that started it, with their
  counts, CPU and memory. The taskguard calls running inside a job, such as a
  dev stack's start-up builds, show under that job with their own CPU and
  memory. `←` folds a row, `→` unfolds it, and `Enter` on a run's row does
  both. Running jobs keep their place,
  oldest first.
- `--wraps`: a command that runs other taskguard jobs runs straight through
  from its first run, and the jobs inside it queue on their own. A queued run
  that had jobs inside it prints advice to pass it.
- The Config view of `taskguard top` lists `warn_admit`.

### Changed

- A command that wraps other taskguard jobs, such as `taskguard bun run
  ci:local` around a turbo run, runs straight through from its second run on,
  and the jobs inside it queue on their own. Queued as one job, it booked its
  biggest run, up to 20 GB, also when turbo's cache left almost nothing to do;
  on one machine such wrappers waited 995 minutes in 17 days. A long-lived
  job, a job in a pool, and a job with a minimum stay queued.
  `taskguard prune 'KEY' --apply` queues the command again. (#24)
- A run starved of memory raises the next run's need by 25% only when it
  reached 90% of its need. One starved far below its need was squeezed by
  other programs, and the extra room went unused. (#24)
- macOS: the kernel's memory warning level no longer stops every new job.
  At warning, jobs start while memory fits without any leeway, and not while
  a first run still grows (at most two minutes). A dev stack kept the level
  at warning with half of memory free, and the browser proof that needed the
  stack waited behind it for up to an hour. Critical still stops every new
  job. `warn_admit = false` brings back the old rule. (#22)

## [0.8.0] - 2026-10-09

### Added

- Partial fit: a job that cannot fit now, because its CPU need is above the
  whole limit or because it has waited past `max_bypass`, starts on part of
  its CPU need. It needs at least `partial_fit` (0.5) of its need free, and
  the machine near its lowest CPU use of the last two minutes, so it starts
  at a quiet moment. It then books the cores it got, not its whole need, so
  other jobs keep moving. Memory stays a hard rule. `partial_fit = 0` turns
  it off. Set it per machine in `~/.config/taskguard/config.toml`.
- A waiting job that CPU keeps out, and that cannot fit now (its need is
  above the whole limit, or it has waited past `max_bypass`), prints advice
  lines in its wait output: what it needs against the limit, what holds the
  room and how long that should last, how many jobs wait behind it, how long
  it has waited, when partial fit starts it, the command to start it now and
  what that costs, and the settings that change the outcome. They print with
  hints off too; `--quiet` hides them.
- `taskguard start JOB` starts a waiting job now, whatever the limits say, as
  `g` in `taskguard top` does. JOB is a pid, a ticket, a key, or a unique
  part of a key. A job that CPU keeps out and starts this way (or at a
  `--st` timeout) books only the cores that were free, so the jobs behind it
  keep moving. `start` is now a subcommand: to run a program named `start`,
  use `taskguard run -- start`.

### Changed

- Backfill is on by default: `max_backfill` is 600 (10 minutes) instead of
  0. Small jobs that fit pass a reserved job that cannot start yet. After 10
  minutes only jobs that end before the reserved job could start still pass,
  so the machine drains for it, and it starts at the latest once no other job
  runs, also when its need is above the whole limit. The same holds for a
  waiting job with a higher priority: for its first 10 minutes, jobs with a
  lower priority that fit start while it cannot. Set `max_backfill = 0` for
  the old behavior.

### Fixed

- The CPU need is learned from the cores a job used, not from the time its
  threads waited for a core. Tools that start one thread per core (`tsgo`,
  `oxlint`) waited on every core of a busy machine, so they learned a need of
  the whole machine and blocked the queue. Recent runs count more: a run
  counts half as much as one four runs newer. A run far above the others is
  an outlier and counts by `outlier_weights`, as memory peaks do. A
  CPU-starved run no longer raises the need to what it waited for; starved
  runs are left out while three runs that were not starved remain. A running
  job books the cores it uses, and the `--min-cpu` advice after a starved run
  pins the learned need. Run `taskguard prune KEY --apply` on a job whose need
  is still inflated to start again from its next runs.

## [0.7.1] - 2026-10-07

### Fixed

- Steady long-lived jobs reserve recent CPU use plus a margin instead of
  runnable wait, and current memory plus 25% instead of a historical floor.
  Historical CPU needs are clamped to the host's logical core count.

## [0.7.0] - 2026-10-06

### Added

- One run that takes far more memory than the job's other runs no longer sets
  its memory need at once. A peak more than `outlier_ratio` (2) times the next
  peak below it, and at least `outlier_min` (1 GB) above it, is an outlier.
  It counts by `outlier_weights`: the first time not at all, the second time
  half, the third time in full. A run that failed never counts. Jobs whose
  usual peak is under `outlier_min` have no outliers. The job prints a warning
  after an outlier run, with the reason, and the reason is kept in the history.
- `taskguard outliers [PATTERN] [--ratio R] [--min SIZE]` lists the outliers,
  and how much each one counts.
- `taskguard prune` drops runs from the history: of the keys that match a
  pattern, older than an age (`--older-than 30d`), or the outliers
  (`--outliers`). Without `--apply` it only shows what it would do. Pruned
  runs are kept aside, and `prune --undo` puts them back.
- A job short of memory by at most `noise_mem` (2%) of RAM, or of CPU by at
  most `noise_cpu` (0.5) cores, starts anyway. So does a job that only
  programs outside taskguard keep out (`outside_admit`, on by default), while
  memory stays under `pause_at`. Each such start prints a warning with what
  the job lacks.
- Help for each command: `taskguard help COMMAND`, or `COMMAND --help`.
  `taskguard help --all` prints every command in full in one text, for LLM
  agents.
- `doctor --explain` shows a command's outliers.
- `taskguard top` has a Tasks view (tab 4): every task taskguard knows, with
  its namespace, runs, last run and learned needs. Filter by namespace (`n`),
  text (`/`) and age (`a`: not run for 1, 7, 30 or 90 days). `x` prunes every
  run of the selected task, `X` every task the filters leave. In a job's view,
  `x` prunes the selected run. Both ask first, and `taskguard prune --undo`
  puts the runs back.

### Changed

- In `taskguard top`, `?` lists every key of the view you are in, and the keys
  of every view. The bottom line starts with `? keys` and shows only the keys
  of the view that fit, so it no longer runs off the screen.
- The tabs of `taskguard top` after Runs move one up: Trends is 5, Warnings 6,
  Namespaces 7, Config 8, Help 9.
- A memory need never passes `mem_max`, as a CPU need never passes `cpu_max`.
  A job that once took more is capped there, and says so. Before, it could
  only start on an empty machine.

## [0.6.0] - 2026-10-05

### Fixed

- Jobs in worktrees of a bare repository get their name from the repository,
  not from the worktree. Their old runs move to the new name.

### Changed

- A waiting job decides once a second, or at once when the queue changes. A
  long queue took a whole core before.
- In `taskguard top`, a mouse move or a key no longer reloads everything for
  2.5 seconds.

## [0.5.0] - 2026-10-05

### Added

- Long-lived jobs, such as dev stacks (`long_lived = true`). They hold their
  start-up peak only through their start-up. After it, their needs follow
  what they use.

## [0.4.4] - 2026-10-05

### Fixed

- The recorder reads the PSS of other programs every two minutes, not every
  2 seconds. On Linux this took about half a core.

## [0.4.3] - 2026-10-05

### Fixed

- Past `max_backfill`, jobs that end before a reserved job could start still
  start.

## [0.4.2] - 2026-10-04

### Fixed

- A job ahead in line that does not start holds nobody up.
- A waiting job is never held back by its own entry.

## [0.4.1] - 2026-10-04

### Fixed

- Jobs hold each other back only in one fixed line, so they cannot wait for
  each other in a circle.

## [0.4.0] - 2026-10-02

### Added

- First runs start in groups. A first run guessed to be short never waits.
- One task-runner run at a time: the jobs of an older `turbo` or `nx` run go
  first.
- Short jobs count in the CPU budget.
- A first run is guessed from the same command in the same package.

## [0.3.0] - 2026-10-01

### Added

- Edit pools in the Config view of `taskguard top`.
- Generic built-in pools.
- `enabled = false` lets every command run straight through.

## [0.2.1] - 2026-09-28

### Added

- `max_backfill`: newer jobs may start while a reserved job cannot.

### Fixed

- An unknown setting is a warning, not a fatal error. A repository can pin a
  newer taskguard that knows the setting.

## [0.2.0] - 2026-09-28

### Added

- `taskguard pause JOB` and `taskguard resume JOB`. taskguard also sees a job
  that was stopped from outside.
- `auto_pause` (off by default): pause running jobs when memory fills up.
- Job priorities (`--priority N`, `priority = N`).
- A waiting job says when it may start.
- The job view of `taskguard top` charts the selected past run.

## [0.1.7] - 2026-09-28

### Added

- `taskguard top`: page keys, a chart window in the past, and picking a run
  at the cursor.

## [0.1.6] - 2026-09-26

### Fixed

- A waiting job prints a still-waiting line every 120 seconds, not every 15.

## [0.1.5] - 2026-09-25

### Fixed

- `taskguard top`: the memory-in-use line runs through every column, over the
  layer colour.

## [0.1.4] - 2026-09-25

### Added

- Cautious guesses for first runs. A first run waits until the one before it
  has settled.

### Fixed

- `taskguard top` draws every memory layer in full, also above the machine
  total.
- The Why panel names memory pressure.

## [0.1.3] - 2026-09-25

### Added

- `taskguard top` groups the load that taskguard did not start: agents,
  browsers, editors and more.
- Start or resize a waiting job by hand from `taskguard top`, and a Config
  view.

### Fixed

- The shared state stays readable when different versions run on one machine.

## [0.1.2] - 2026-09-25

### Added

- Screenshots of the dashboard, a demo script, and install from prebuilt
  binaries.

## [0.1.1] - 2026-09-25

### Added

- `--st-exit N` sets the exit code when `--st` gives up.

## [0.1.0] - 2026-09-25

### Added

- taskguard: tsc-queue rewritten in Rust. A `sem` that learns what each job
  needs, and starts it when the machine has room for it.
- Prebuilt binaries for macOS and Linux on each version tag.

[Unreleased]: https://github.com/SpoBo/taskguard/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/SpoBo/taskguard/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/SpoBo/taskguard/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/SpoBo/taskguard/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/SpoBo/taskguard/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/SpoBo/taskguard/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/SpoBo/taskguard/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/SpoBo/taskguard/compare/v0.4.4...v0.5.0
[0.4.4]: https://github.com/SpoBo/taskguard/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/SpoBo/taskguard/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/SpoBo/taskguard/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/SpoBo/taskguard/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/SpoBo/taskguard/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/SpoBo/taskguard/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/SpoBo/taskguard/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/SpoBo/taskguard/compare/v0.1.7...v0.2.0
[0.1.7]: https://github.com/SpoBo/taskguard/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/SpoBo/taskguard/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/SpoBo/taskguard/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/SpoBo/taskguard/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/SpoBo/taskguard/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/SpoBo/taskguard/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/SpoBo/taskguard/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/SpoBo/taskguard/releases/tag/v0.1.0
