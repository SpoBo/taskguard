# Changelog

What changed in each release of taskguard. The newest release is first.

Each release has a section `## [VERSION] - DATE`. Work that is not released
yet goes under `## [Unreleased]`. A release renames that section to the new
version. The release workflow uses the section as the text of the GitHub
release. The format follows [Keep a Changelog](https://keepachangelog.com),
and the versions follow [semver](https://semver.org).

## [Unreleased]

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

[Unreleased]: https://github.com/SpoBo/taskguard/compare/v0.7.1...HEAD
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
