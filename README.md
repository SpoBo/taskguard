# taskguard

A `sem` that learns what each job needs.

Put `taskguard` in front of a command. The command starts only when the
machine has room for the CPU and memory that the same command used on its past
runs. Every call on the machine shares one queue, from any terminal, worktree,
task runner or agent.

![taskguard top: machine CPU and memory over time, with the jobs taskguard started stacked per repo](docs/screenshots/overview.png)

The dashboard above shows a demo: two repos, `shop` and `billing`, share one
laptop. Coloured areas are the jobs that taskguard started, one colour per
repo. The grey area is everything else on the machine. The strip at the bottom
shows how many jobs waited, and why. To see it yourself, run
[the demo](#try-it-with-the-demo).

macOS and Linux. One binary, written in Rust.

## The problem

A task runner caps tasks inside one run only. Two terminals, two worktrees, an
editor and a coding agent can each start a full set of compiles and test
suites. Nothing on the machine holds a shared ceiling, so a large monorepo
pushes the machine into swap, and the operating system starts to kill
processes.

A fixed limit does not fix this. `turbo --concurrency=2` still lets two jobs
that each want 20 GB run side by side, and it holds back twenty small jobs that
would fit easily. GNU `sem` has the same gap: it counts slots, and a 25 GB
compile and a 50 MB lint each take one slot.

`taskguard` counts what the jobs really use.

## How it decides

A job starts when both of these fit:

```
CPU busy     + CPU still promised to running jobs     + this job's CPU need     <= cpu_max  (100% of cores)
memory held  + memory still promised to running jobs  + this job's memory need  <= mem_max  (85% of RAM)
```

A job that misses by only a little, or only because of programs outside
taskguard, starts anyway with a warning (see "A little short is close enough"
below).

- **Needs are learned.** Each run is measured, and the result is kept per
  command. The memory need is the highest peak of the last 10 runs, because
  running out of memory kills processes. The CPU need is the cores the job
  *used* in its last 10 runs, because too little CPU only makes a job slower.
  It is a median in which a run counts half as much as one four runs newer, so
  it follows a job that gets faster or slower. Time a job's threads spent
  waiting for a core does not count: `tsgo` and `oxlint` start one thread per
  core, so on a busy machine they wait on every core. Runs far above the
  others count as outliers do for memory (below). A run that was starved of
  CPU used less than it needs, so starved runs are left out while three runs
  that were not starved remain; what a starved run waited for never raises
  the need. A memory need never passes `mem_max`: a job that once took more
  is capped there, and says so.
- **One wild run does not set the need.** A peak more than `outlier_ratio`
  (1.5) times the next peak below it, and at least `outlier_min` (1 GB) above
  it, is an outlier: a CI run that took 12 GB where it usually takes 4.5 GB.
  The first time it does not count, the second time it counts half, the third
  time in full (`outlier_weights`). A run that failed (exit not 0) never
  raises the need above the runs that passed, outlier or not, and with any
  number of runs: a run that goes wrong can take far more than the job needs,
  and on a real machine most outliers were such runs. When every recent run
  failed, the need is the second-highest failed peak, at least `new_job_mem`
  but never more than the highest: one failed peak alone does not set it.
  A failed run that was starved of memory adds the 25% only when its peak
  stayed within that 25% of its need. Jobs whose usual peak is under `outlier_min` have
  no outliers: there a jump is mostly a full build after cache hits, and
  holding it back would start the next full build into a full machine. The
  job's line says when its run was an outlier, `taskguard outliers` lists
  them, and `taskguard prune` drops runs from the history.
- **A first run reserves a cautious guess.** A job with no history counts as
  needing what jobs like it needed in the last 30 days: the 90th percentile of
  their memory peaks and the 75th of the cores they wanted. "Like it" means the
  same command of the same package first (its `package.json` name), so a
  package that moved to another folder starts from what it used before; its
  history itself stays with the old folder. Then the same kind and pool (a
  `ci` run in DALP's `throttle-suite`), then the same kind, then the same
  pool, then the same program; 1.5 GB without any (`new_job_mem`). A shell
  job gets the kind of the heaviest command it runs: `sh -c "git fetch && bun
  install && bun run ci:local"` is a `ci` run.
- **First runs start in groups, and a group must settle.** At most a group of
  first runs may still grow at once: one job per free core, and only as many
  as fit in the free memory at their guess. A first run has settled when its
  memory has not risen by 10% for 20 seconds, or when it has run for 2
  minutes; then the next one may start. At most a group of first runs starts
  per `learn_stagger` seconds. A first run that similar jobs finish within 5
  seconds (`cpu_min_duration`), such as a lint, is over before it could grow:
  it neither waits nor counts. Memory is checked for every job. With an empty
  history, DALP's full `ci` graph took 372 s this way, against 671 s when
  first runs went one at a time.
- **A running job's needs follow its peak.** When a job passes its needs, its
  memory need becomes its peak plus 25% and its CPU need what it wants, so what
  it still promises to take keeps up with what it takes. The Queue shows in red
  how far jobs are above the needs they started with.
- **Memory pressure stops everything new.** When the kernel reports memory
  pressure (macOS: critical; Linux: PSI above 20%), nothing new starts until
  it eases, whatever the estimates say. macOS keeps its warning level for a
  long time, also with half of memory free, for example while a dev stack
  runs. At warning, jobs still start, but only while memory fits without any
  leeway, and not while a first run still grows: a first run books a guess,
  so the next job waits until it has shown what it takes (at most two
  minutes). A job with a history has its whole need booked from its first
  second, so the next job may start at once. So the job that needs that
  stack does not wait behind it. When nothing runs but long-lived jobs past
  their start-up, the rule below for nothing running decides at warning too,
  up to all of RAM. `warn_admit = false` makes warning stop every new job
  again.
- **Memory is read at its recent peak.** Admission uses the highest reading
  of the last 10 seconds, so a job never starts in a short dip.
- **Short jobs skip the CPU reading, not the CPU budget.** A job that usually
  ends within 5 seconds is over before the CPU reading could react to it, so
  the reading does not hold it back. It still waits while the cores that
  running jobs were promised, with its own, would pass `cpu_max`. Without this,
  a task runner with a high concurrency starts every short job at once and
  fills every core. Memory is checked for every job.
- **"Promised" is the growth still to come.** A compile that sits at 2 GB
  but peaked at 20 GB last time still has 18 GB to take. Without this, a second
  job is started into space the first one is about to use.
- **The machine readings count everything.** Your browser and your editor
  count too, not only jobs that taskguard started.
- **A little short is close enough.** A job short of memory by at most
  `noise_mem` (2%) of RAM, or of CPU by at most `noise_cpu` (0.5) cores,
  starts anyway: readings move by that much from one second to the next.
  So does a job that only programs outside taskguard keep out, while
  taskguard's own jobs fit under the limits (`outside_admit`). Memory stays a
  hard rule at `pause_at` (92%), or, when `mem_max` is at or above `pause_at`,
  halfway from `mem_max` to all of RAM (97.5% for `mem_max = 95`). Each such
  start prints a warning with what the job lacks and the rule that let it
  start.
- **Nothing running means the first job in line that fits starts,** whatever
  the CPU reading says and up to all of RAM, so the queue cannot deadlock on
  a wrong reading or on programs outside taskguard. The same holds when
  nothing runs but long-lived jobs past their start-up. A job that would not
  fit in RAM even so waits for memory and holds nobody back: the jobs behind
  it that fit start. Next to long-lived jobs, a long-lived job must fit under
  `mem_max` (with `noise_mem`), or the ceiling `outside_admit` allows: it does
  not end by itself, so starting it over the limit is no progress but load
  that stays. Memory pressure is the one exception:
  then the load comes from other programs, and jobs wait until it eases
  (at macOS warning, see above).
- **A job that only programs outside taskguard keep out starts over RAM
  once it has waited (`overcommit`).** With nothing of taskguard's running,
  a job short of all of RAM waits for programs outside taskguard to free
  memory, and nothing taskguard runs would make that room: on a 32 GB Mac a
  dev stack that books its browser run waited for good once other programs
  held 18 GB. So the only job in the queue may start with memory over RAM
  after `overcommit_after` (120) seconds, and the first in line after
  `overcommit_stuck_after` (1800) seconds while other jobs queue behind it.
  Work for a running stack counts as alone next to it. Work that takes over
  the room its running stack declared for it (not a guess of the history),
  and needs no more than `noise_mem` beyond it, starts so at once only when
  that stack itself started by over-commit: it started over RAM with that
  room booked. Otherwise the work waits as any other job does. Memory may reach `overcommit_max_macos` or
  `overcommit_max_linux` (120) percent of RAM, and the part over RAM must
  fit in half the free swap. macOS compresses cold memory and swaps to files
  that grow while their volume has space, so there the swap counts that
  free space too; over RAM costs speed, not processes. Linux has only its
  swap: with none, nothing starts over RAM. Never at critical pressure, and
  on Linux never at `pressure_max`. A second stack next to a running one
  still waits. On Linux a job that starts over RAM, by this rule, by `--st`
  or by hand, gets `oom_score_adj` 800: when memory runs out, the OOM killer
  takes it, not the browser or the agents that ran first. `taskguard top`
  marks it `OVER`, `taskguard status` `OVER-RAM`. `overcommit = false` turns
  it off.
- **A long wait for memory says who holds it.** A job that has waited five
  minutes for memory with nothing of taskguard's running gets a line in
  `taskguard top` and `taskguard status` that names the programs outside
  taskguard that hold the most, how far the job is short of the ceiling it
  really waits for, and when over-commit starts it or how to start it by
  hand.
- **Any order, oldest first.** A small job may pass a big one that does not
  fit yet. This is safe: a job only reaches taskguard when its launcher has
  decided that it may run. Turbo starts a task only after the tasks it depends
  on are done, and `a && b` starts `b` only after `a` ends.
- **One run at a time.** A job started by a task runner (`turbo`, `nx`,
  `make`, `moon`, `lage`, `just`) belongs to that runner's run. When several
  runs wait, a job of the run that queued first goes first, as long as it can
  start. Twenty pipelines that each move a little all finish late; twenty in
  turn finish one after the other, and the last one no later. A job of the
  older run that does not fit holds nothing back, so no room is left idle.
  Priorities still come first. A waiting job with a higher priority holds the
  others back as a reserved job does: while it could start, and with backfill
  (below) not while it cannot use the room.
- **No starvation.** A job that newer jobs have passed for 2 minutes
  (`max_bypass`) gets a reservation. Nothing behind it in line starts until it
  has started, except by backfill (below).
- **Part of its CPU need is enough for a job that cannot fit.** A job whose
  CPU need is above the whole limit never fits, and a job that waited past
  `max_bypass` is the one others are held for. Such a job starts on part of
  its CPU need: at least `partial_fit` (0.5) of it must be free, and the
  machine must be near its lowest CPU use of the last two minutes (within one
  core, or a tenth of the cores), so it starts at a quiet moment and has the
  best chance to finish well. It then books the cores it got, not its whole
  need, so it does not hold every other job out while it runs; from there it
  follows what it uses. Memory stays a hard rule. `partial_fit = 0` turns it
  off; set it per machine in `~/.config/taskguard/config.toml`.
- **One line, no circles.** Every rule that holds a job back for another
  follows one fixed order: a higher priority, then the older run (a job outside
  any run counts from when it queued), then the older ticket. A job only ever
  waits for a job ahead of it, so jobs cannot wait for each other in a circle
  while the room they need sits free.
- **No waiting for a job that does not start.** A job ahead in line that may
  start does so within a tenth of a second. One that could start by these
  rules but still waits after 10 seconds has an owner that decides by other
  rules (an older taskguard, see Mixed versions) or not at all (stopped with
  Ctrl-Z). The job behind it marks it as stalled, says so with what the owner
  itself reports it waits for, and from then on no job waits for it. It may
  still start by itself. `taskguard status` shows the mark, and what an owner
  says when that differs from what it should do.
- **Backfill.** A reservation for a job that does not fit
  holds room it cannot use: a 29 GB compile that waits for memory keeps a
  0.2 GB install waiting too. So a reserved job holds
  its turn only while it could start: until it has waited `max_backfill`
  seconds, newer jobs that fit start while it cannot, and nothing newer starts
  once it fits. After `max_backfill`, a newer job that fits still starts if
  its learned duration says it ends before the reserved job could start
  anyway: before the running jobs have freed the room it lacks, or, when
  programs outside taskguard keep it out, before none of them runs (then the
  first in line that fits in RAM starts). Such a job takes no room
  the reserved one could use, so a steady stream of small jobs cannot keep it
  out, and a lint is not held up for half an hour behind a job that waits for
  two long test runs. A job with no learned duration waits. For a reserved job
  that may start on part of its need, "could start" means once that part is
  free. The drain always ends with the reserved job running: at the latest
  when no other job runs, also when its need is above the whole limit. On by
  default for 10 minutes (`max_backfill = 600`); `0` turns it off, `1800`
  lets every job that fits through for half an hour.
- **A job that only a stopped stack can make room for holds nobody back.**
  A job ahead in line that would not fit by the rule for nothing running even
  once every job that ends by itself has ended can only start when a
  long-lived job is stopped, or when programs outside taskguard free memory.
  Holding the jobs behind it gains it nothing, so it holds none of them back,
  whatever its priority or reservation: on Omarchy one 5.5 GB lint that only
  fit once a dev stack stopped kept two dozen others out, among them a test of
  189 MB.
- **Pools** add a slot ceiling where jobs share something: one database, one
  set of services, one lock file. A slot ceiling only stops such jobs from
  running at the same time and breaking each other; CPU and memory are always
  shared by the whole machine. Pools count per worktree unless
  `per_checkout = false`, so other worktrees are never held up.
  `taskguard --id e2e -j1 ...`, a `[pool.NAME]` section, or the Config view
  of `taskguard top`.
- **Long-lived jobs** run until someone stops them, such as a dev stack. Mark
  them with `long_lived = true` in a `[pool.NAME]` or `[[job]]` section. Such
  a job starts when there is room for its start-up peak, and holds that peak
  for its start-up (`startup`, 300 seconds by default). Then its needs start
  again from what it uses: its CPU need is what it wanted over the last 10
  seconds, up or down, and its memory need what it holds plus 25%, up or
  down with each reading, so an idle stack releases its booking. A DALP dev
  stack wants almost every core while it
  starts and seeds, then idles at a few percent; without this it kept every
  core for hours. Its history keeps both: the start-up peak, which the next
  run is admitted against, and what it took after. Past its start-up it
  counts as load from outside taskguard: no job waits for it to end, and when
  nothing else runs, the first in line that fits starts. A waiting long-lived
  job itself starts that way next to it only under `mem_max`, or, when only
  programs outside taskguard keep it out, up to the ceiling `outside_admit`
  allows. With nothing of taskguard's running at all, it starts up to all of
  RAM: nothing taskguard runs would free that memory. `auto_pause` leaves it
  alone. `taskguard status` marks it `STEADY`, and a job it keeps out says it
  starts when the stack is stopped, not when it finishes.
- **Work for a long-lived job goes first.** The caller that starts a dev
  stack usually knows what will run on it: the browser run it brings the
  stack up for. It can say so, with `--expect WORK` on the stack's job
  (repeatable) or `TASKGUARD_EXPECT=WORK,WORK` in its environment, so it
  passes through scripts such as `devenv up`. WORK is a pool name, a job key,
  or a command pattern as in `[[job]] match`. The stack then books room for
  each piece of work, learned as that work learns its own need: the need
  jobs of the pool typically started with (their peak with headroom, or a
  minimum), or the history or first guess of the command. It
  starts only when the machine has room for itself and that work, so the
  whole load is known before it starts. A job that queues in the same
  checkout and matches WORK is that work: it goes one priority above the
  stack, so it starts before the jobs that queued for room meanwhile and
  before a next stack, and when it starts it takes over the room booked for
  it, which the stack then drops. `taskguard status` shows both ends: `(for
  stack dev-env:stack)` on the work, `(expects browser, books 4.3 GB)` on the
  stack. Work that queues while its stack still waits waits for the stack and
  holds nobody back meanwhile. A stack with work queued for it, and work whose
  stack runs, hold their turn as a strict reservation, also while they do not
  fit yet: lints that fit do not take the room meanwhile, unless the stack
  could only start once another stack is stopped. A stack started without
  `--expect` books only what its history infers (below); the variable means
  nothing to any other job.
- **Without a declaration, the history says what work follows a stack.**
  When at least `infer_work_min` (3) of a stack's last `infer_work_of` (5)
  runs in its checkout had work of one kind queue there within
  `infer_work_within` (900) seconds of their start, that work is expected
  again. A kind of work is a pool, or the key of a job outside any pool. The
  stack books that work's usual need, which `taskguard status` shows as
  `(expects browser from history, books 4.3 GB)` until work queues for
  it, and while the guess is open the work links and goes first as declared
  work does. It is a guess, so it is bounded:
  - it books room for twice the work's usual delay after the stack started,
    ten minutes at most, and drops it once work for the stack starts or the
    stack stops;
  - once it has expired or its work has started, no work links to it, and
    work linked to it drops back to its own priority and holds its turn no
    more;
  - no job that ends by itself waits for it by the rule for nothing running,
    so it can never stop the queue, and such a job waits for it at most up
    to all of RAM;
  - another stack respects it while it is open, also one whose caller
    declared no work, so the room is there when the work comes. Such a stack
    leaves only its own guess out, starts at once when it still fits next to
    the open guess, and otherwise waits at most until the guess expires or
    its work starts;
  - a declaration always wins: a stack with `--expect` books only what was
    declared, and `--expect none` expects nothing at all;
  - `infer_work = false` turns it off.

  Once the work has queued and linked to it, its room is no guess any more
  and is booked as declared work is, by every rule, also the rule for
  nothing running: it does not expire while that work waits, and it ends
  when the work starts, or, past its time, when the work leaves the queue.
- **A stack waits a few seconds for its work.** A caller may start the stack
  and its work at the same moment, not one after the other. So a long-lived
  job that expects work, declared or from its history, waits up to
  `expect_wait` (5) seconds, before it starts, for that work to queue, and
  starts as soon as it has: with room for both, and the work right after it.
  A stack that expects nothing starts at once, as before: sometimes you only
  want the stack. `expect_wait = 0` turns the wait off.

### What is measured

| | macOS | Linux |
| --- | --- | --- |
| Job memory | physical footprint of the whole process tree | PSS of the whole process tree |
| Job CPU used | user + system time | `schedstat` run time |
| Job CPU wanted | used + time spent ready but waiting for a core (`ri_runnable_time`) | used + run-queue wait (`schedstat`) |
| Machine CPU | host tick counters | `/proc/stat` |
| Machine memory | the higher of active + wired + compressed, and the kernel's `memorystatus_level` | total - available |
| Memory pressure | `memorystatus_vm_pressure_level` | `/proc/pressure/memory` |

On macOS, active + wired + compressed falls exactly when the machine is in
trouble: under pressure the kernel moves pages to the inactive list and writes
compressed pages to swap. The kernel's own figure rises instead, so taskguard
takes the higher of the two.

Physical footprint is used on macOS because resident size hides compressed
pages. One measured `tsc` reported 3.8 GB resident while it really held 22 GB.
All readings are syscalls. They cost microseconds, not the 120 ms per process
of `/usr/bin/footprint`.

The whole process tree is measured, because the memory often lives in a
grandchild: a JavaScript entry point is a node process that starts the native
compiler.

## Install

What changed in each version: [`CHANGELOG.md`](CHANGELOG.md).

Prebuilt binaries for macOS (Apple silicon, Intel) and Linux (x86_64, ARM,
static) are on the [releases page](https://github.com/SpoBo/taskguard/releases):

```sh
# pick the target for your machine: aarch64-apple-darwin, x86_64-apple-darwin,
# x86_64-unknown-linux-musl or aarch64-unknown-linux-musl
target=aarch64-apple-darwin
curl -fsSL "https://github.com/SpoBo/taskguard/releases/latest/download/taskguard-$target.tar.gz" | tar -xz
install -m 755 "taskguard-$target/taskguard" ~/.local/bin/taskguard
```

Or build it from source:

```sh
git clone https://github.com/SpoBo/taskguard.git
cd taskguard
cargo install --path .
```

This puts `taskguard` in `~/.cargo/bin`. It needs Rust 1.89 or newer.

## Use

Put the prefix in the package scripts:

```json
{
  "scripts": {
    "typecheck": "taskguard tsc -p .",
    "test": "taskguard bun --bun vitest run",
    "build": "taskguard vite build"
  }
}
```

With turbo, prefix the package scripts, not the root `turbo run` command, and
give turbo a high `--concurrency`. Turbo then only sets the upper limit, and
taskguard decides what runs. A prefixed `turbo run` would hold one slot for the
whole run.

### Syntax

The shape follows GNU `sem`. There is one difference: taskguard runs in the
foreground by default, because turbo and npm need the command's exit code.

```
taskguard [options] [--] COMMAND [ARGS...]
taskguard --wait [--id NAME]
```

| Option | What it does |
| --- | --- |
| `-j N`, `+N`, `-N`, `N%` | Slot ceiling for this pool, as in sem. None by default. |
| `--id NAME` | Pool name (sem's semaphore id). |
| `--ns NAME` | Namespace for the dashboard. The default is the repo name, the same for every worktree. |
| `--st SECS` | `SECS > 0`: run anyway after SECS. `SECS < 0`: give up after -SECS, exit 124. |
| `--st-exit N` | The exit code when `--st` gives up, instead of 124. |
| `--key KEY` | History key. The default is the project path plus the command. |
| `--min-cpu N` | Never start with fewer than N free cores. |
| `--min-mem SIZE` | Never start with less than SIZE free (`6G`, `512M`). |
| `--priority N` | Higher starts first. The default is 0, or `priority` from the config. |
| `--expect WORK` | For a long-lived job: work that will run on it (a pool name, job key or command pattern). It books room for it, and that work goes first. Repeatable; also `TASKGUARD_EXPECT=WORK,WORK`. `none`: no work, not even what its history suggests. |
| `--now` | Skip the queue, but still measure and learn. |
| `--bg` | Wait for room, then return and let the job run on. |
| `--pipe` | With `--bg`: pass stdin to the command. |
| `-q` | Print only waits longer than the status interval. |
| `--hints`, `--no-hints` | Agent hints on or off for this call. |

The `--` is optional. Options stop at the first word that is not an option.
Use `taskguard run ...` for a command that has the same name as a subcommand.

`TASKGUARD_DISABLE=1` skips taskguard for one command. `enabled = false` in a
config file does the same for every command it applies to: all of them run
straight through, unqueued and unmeasured. A call nested inside a
running job starts at once and takes no second slot, even when a task runner in
between drops environment variables.

A command that wraps other taskguard jobs, such as `taskguard bun run ci:local`
around a turbo run whose package scripts start with `taskguard`, runs straight
through from its second run on. Queued as one job, it booked the memory of its
biggest run, a full build, also when turbo's cache left almost nothing to do,
and the jobs inside it ran unqueued under its claim. Now each nested call
leaves a note for the job it runs in, and a command whose recent runs started
taskguard jobs inside them runs straight through, with one line saying so. The
jobs inside it then queue on their own, each by its own need. A long-lived job,
a job in a pool and a job with a minimum stay queued: their claim is on
purpose. `taskguard prune 'KEY' --apply` forgets the history, so the command
queues again. When a queued run had jobs inside it, taskguard prints advice to
pass `--wraps`: `taskguard --wraps bun run ci:local` runs straight through
from the first run, with nothing to learn first.

Long-running commands are never queued: watch modes, dev servers, `--version`
and `--help` run straight through (the list is in `src/builtin.rs`). Add your
own with `passthrough = ["storybook", "my-dev-server"]` in a config file; it
adds to the built-in list. See `taskguard doctor --explain "COMMAND"`.

### What a waiting job prints

Status lines go to stderr, so the command's own output stays clean:

```
[taskguard] queued packages/api:tsc - learned: 3.7 cores, 2.5 GB (5 runs)
[taskguard] waiting 15s web:build - blocked by CPU: would use 9.8 of 8.0 cores, 1.8 cores short. It starts when packages/api:tsc finishes; this is not a hang
[taskguard] start web:build - waited 19s; fits: CPU 2.7+1.3+4.4 of 8.0 cores, memory 63% of 85%
[taskguard] done web:build - 7s, used 4.4 cores sustained (wanted 4.8), 1.2 GB peak, exit 0 (0 still queued)
```

A job that CPU keeps out and that cannot fit now, because its need is above
the whole limit or it has waited past `max_bypass`, also prints advice:

```
[taskguard] advice web:typecheck needs 30.0 cores; the limit is 27.0 cores (cpu_max 150% of 18 cores), so it never fully fits
[taskguard] advice the room is held by web:lint 15.0 cores, about 4m00s left; web:lint-css 15.0 cores, about 6m10s left
[taskguard] advice it has waited 1m00s; 46 jobs wait behind it
[taskguard] advice it starts by itself on 15.0 cores (partial_fit 50% of its need) once that much is free and the machine is near its recent low
[taskguard] advice to start it now anyway: taskguard start 4242. It then runs on the cores it gets, slower, and no check holds it back, memory included
[taskguard] advice to change the outcome: raise cpu_max, lower partial_fit, or if its need comes from old runs, check taskguard history and run taskguard prune 'web:typecheck' --apply
```

It prints once when it applies and again with each status line, with hints
off too: an agent only sees this output, and this is how it learns to get the
job going.

**Agent hints** ("this is not a hang", "starts when X finishes") are on by
default, so an LLM agent does not kill a command that only waits. Set
`hints = false` per repository or directory. `--hints` and `--no-hints` override
it for one call.

**For agents:** `taskguard help --all` prints every command with all its
options in one text. Point an agent at it, or put its output in the agent's
instructions, so it knows the whole tool at once. `taskguard help COMMAND`, or
`COMMAND --help`, shows one command.

### Starved runs

taskguard also measures whether a job was held back while it ran: its threads
waited for a core more than half as long as they ran, it paged memory in while the machine was under memory pressure, or it
took 1.5 times its usual time while the machine was full. A run starved of
memory that reached 90% of its need teaches a higher need for the next run
(+25%); one starved far below its need was squeezed by others, and more room
would not have helped it. A run starved of CPU does not
raise the CPU need (see "Needs are learned"). With hints on it prints advice
that can be pasted:

```
[taskguard] warning packages/api:vitest_run - possibly starved: waited on CPU 64% of its run time (wanted 6.1 cores, got 2.2)
[taskguard] advice already done: the next run reserves 6.1 cores (was 3.0)
[taskguard] advice to pin it: script "test" in packages/api/package.json -> "taskguard --min-cpu 7 bun --bun vitest run"
[taskguard] advice or let vitest fit the room it gets: add --maxWorkers=2
```

The advice names the exact script and file, because package managers tell each
script its name and package.json path. It knows the parallelism flags of
vitest, jest, playwright and turbo, and the node heap flag.

## The dashboard

`taskguard top` is a terminal dashboard in the style of btop.

**Queue.** What runs and what waits. The Why panel checks every rule for
the selected job, with numbers, and says what will unblock it.

![Queue view: three jobs wait on CPU, and the Why panel shows the sum that fails](docs/screenshots/queue.png)

**Warnings.** Runs that were starved, what taskguard changed for the next
run, and the minimum to pin when a job starves again and again.

![Warnings view: starved runs with the evidence and a suggested --min-cpu](docs/screenshots/warnings.png)

<details>
<summary>More views: runs, job, trends, namespaces, and the <code>status</code> command</summary>

![Runs view: past runs with wait time, cores used and wanted, peak memory and flags](docs/screenshots/runs.png)

![Job view: the learned needs of one command and its last runs](docs/screenshots/job.png)

![Trends view: a test suite whose memory grew 73%](docs/screenshots/trends.png)

![Namespaces view: CPU-hours, GB-hours, runs and waits per repo](docs/screenshots/namespaces.png)

![taskguard status: the same queue as plain text, for scripts and agents](docs/screenshots/status.png)

</details>

| View | What it shows |
| --- | --- |
| 1 Overview | Machine CPU and memory over time. The load of jobs taskguard started is stacked in colour per namespace. The load it did not start is split into groups: agents (with everything they started), browsers, editors, dev services such as databases, containers, and chat and other apps. The legend names the biggest programs in each group, so you can see what to close to make room. A strip shows how many jobs waited, coloured by reason, and `!` marks a starved run. |
| 2 Queue | Running jobs, then waiting ones, as a tree: the jobs of one task-runner run sit under one row named by the command that started it (`bun run ci:local`), and the taskguard calls running inside a job (a dev stack's start-up builds) under that job, with their CPU and memory. `←` folds a row, `→` unfolds it, and `Enter` on a run's row does both. The bars show in red the load that running jobs have above their estimates. The Why panel checks every rule for the selected job, with numbers, and says what will unblock it and when. `g` starts a waiting job now, and `e` sets a job's needs for this run only (for example `2 4G`), so it can fit into room you know it will not outgrow. |
| 3 Runs | Past runs, sortable and filterable, with the reasons each one waited. |
| 4 Tasks | Every task taskguard knows, with its namespace, runs, last run, and what it learned. `n` (namespace), `/` (text) and `a` (not run for 1, 7, 30 or 90 days) filter the list. `x` prunes every run of the selected task; `X` prunes every task the filters leave, and needs a filter. Both ask first. Pruned runs move aside, as with `taskguard prune`, and `taskguard prune --undo --apply` puts them back. In a job's view, `x` prunes the selected run. |
| 5 Trends | Commands whose memory, CPU or duration grows: the last 10 runs against the 10 before. |
| 6 Warnings | Starved runs with the evidence, suggested minimums, trend alerts, and `--now` runs that went over a limit. |
| 7 Namespaces | CPU-hours, GB-hours, runs, waits and starved runs per namespace. |
| 8 Config | The limits, other settings and the slot ceiling of each pool. `←`/`→` change one. `Enter` on a pool lists its match patterns: `a` adds one, `d` removes one. `n` adds a new pool. The first change asks where to save it: your user config (every repo) or this worktree's `.taskguard.toml`; `w` switches later. Comments are kept. Jobs that already wait follow the change. |

`Enter` on a row opens that job: its learned needs and where each comes
from, sparklines over its runs, and what taskguard changed on its own. For a
run that is going on now, it also shows CPU and memory over the run and live
signs of starvation. `Esc` or `Backspace` goes back to the same row, and `j`
opens the last job again.

`?` lists every key of the view you are in, and the keys of every view; the
bottom line starts with it and shows the keys of the view that fit, with
the version of taskguard at the right when room is left. `?` or `Esc`
closes the list, and any other key closes it and does its work.

Keys: `1`-`9`, `Tab`, or Shift, Option or Cmd with `←`/`→` change views. `t`
time range (5m to 7d), `n` namespace, `/` filter, `←`/`→` time cursor,
`c`/`m`/`b` charts, `o` hide the load taskguard did not start, `s`/`r` sort,
`q` quit. `PgUp`/`PgDn` jump most of a screen; the selected row keeps its
place on screen. Long lists say how many rows are hidden above and below.

On the Overview, `PgDn` moves the chart back in time and `PgUp` forward, by
80% of its width; the time cursor keeps its column. `l` (or `End`) goes back
to live. `Enter` at the time cursor picks one of the jobs that ran then: `↑`/`↓`
select a run, `Enter` opens it in the job view, `Esc` goes back to the chart.

The mouse
works on the tabs, the keys in the bottom line, and the rows; the wheel
scrolls lists and zooms the Overview.

A job started by hand or with other needs is still measured, so its next run
learns from what it really used. Jobs from taskguard before 0.1.3 cannot be
started or changed from the dashboard.

`taskguard top --print [--view NAME] [--range 1h] [--keys]` prints one frame as plain
text, for scripts and agents.

The history comes from a small recorder process. The first job starts it, it
samples the machine and the groups of other programs every 2 seconds, and it
exits by itself 30 minutes after the last job. Nothing is installed as a
service. On Linux it reads the PSS of another program in full every two
minutes, and sooner when its resident size moves by more than a quarter; in
between it moves the last reading by the change in resident size. The kernel
walks every page of a process to add up PSS, so reading it for every process
every 2 seconds took about half a core on a machine with 1,700 processes.

## Try it with the demo

`scripts/demo.sh` fills a scratch state with about four minutes of made-up
monorepo work, in two repos. The jobs are busy loops and a Python process that
holds memory, so taskguard really measures, learns and queues them. The demo
sets a low CPU limit (60%), so jobs have to wait.

```sh
scripts/demo.sh                                        # terminal 1
TASKGUARD_DIR=/tmp/taskguard-demo/state taskguard top  # terminal 2
```

The demo keeps its state in `/tmp/taskguard-demo`. Your real queue and
history are not touched, so a plain `taskguard top` does not show the demo
jobs. Set `TASKGUARD=/path/to/taskguard` to try a local build.

## Commands

| Command | What it does |
| --- | --- |
| `taskguard [options] COMMAND` | Run a command when the machine has room for it |
| `taskguard --wait [--id NAME]` | Wait until no job of the pool runs or waits |
| `taskguard top` | The dashboard |
| `taskguard status [--json]` | What runs, what waits, and why |
| `taskguard pause JOB`, `taskguard resume JOB` | Pause or resume a running job by hand. JOB is a pid, a key, or a unique part of a key |
| `taskguard start JOB` | Start a waiting job now, whatever the limits say, as `g` in `taskguard top` does. JOB is a pid, a ticket, a key, or a unique part of a key |
| `taskguard history` | Learned needs per command |
| `taskguard outliers [PATTERN] [--ratio R] [--min SIZE]` | Memory peaks far above a job's other runs, and how much each counts. `--ratio 1.5` finds more, `3` fewer |
| `taskguard prune PATTERN [--older-than 30d]` | Drop the runs of the keys that match from the history. PATTERN is a key from `history`; `*` matches anything (`'packages/api:*'`). Without `--apply` it only shows what it would do |
| `taskguard prune --outliers [PATTERN] [--ratio R] [--min SIZE]` | Drop the outliers that `outliers` lists, with the same options |
| `taskguard prune --undo [PATTERN]` | Put pruned runs back |
| `taskguard doctor` | Configuration, live readings, and leftover tsc-queue shims |
| `taskguard doctor --explain "COMMAND"` | How one command is matched, pooled and learned |
| `taskguard dump PATTERN \| --cmd "COMMAND" \| --run ID` | Everything taskguard knows about a job, as JSON: how it matches, what its next run reserves and why, step by step, and each recent run with what it waited for and what it did to the need. Attach it to a bug report |
| `taskguard import-history` | Load tsc-queue's memory history |
| `taskguard help COMMAND`, `taskguard COMMAND --help` | Help for one command: what it does, its options, examples |
| `taskguard help --all` | Every command in full, in one text, for LLM agents |

`--help` after the command you run belongs to that command:
`taskguard -- tsc --help` shows tsc's help.

## Configuration

Settings come from files, in layers. Later layers win:

1. built-in defaults
2. `~/.config/taskguard/config.toml`
3. `[dir."<path>"]` sections in that file, for jobs inside that path
4. `.taskguard.toml` in the repository
5. command-line flags

Files are used, not environment variables, because turbo's default strict env
mode drops variables it does not know.

[`taskguard.example.toml`](taskguard.example.toml) lists every setting. The
most used ones:

```toml
cpu_max = 100          # percent of all cores
mem_max = 85           # percent of RAM
hints = true           # agent hints on status lines
outlier_ratio = 1.5    # a peak this many times the next one is an outlier; 0 = off
outside_admit = true   # start jobs that only other programs keep out
partial_fit = 0.5      # a job that cannot fit starts on half its CPU need; 0 = off
max_backfill = 600     # small jobs pass a stuck job this long, then the machine drains
expect_wait = 5        # a dev stack waits this long for the work it expects to queue
infer_work = true      # a dev stack books the work its history says follows it
overcommit = true      # a lone job that only other programs keep out starts over RAM after a while

[pool.e2e]             # a slot ceiling for one kind of job, per worktree
max_slots = 1

[pool.db]              # take away a built-in pool's ceiling
unlimited = true

[[job]]                # settings for one command
match = "vitest run --project integration"
min_cpu = 4
min_mem = "6G"

[pool.stack]           # dev stacks: four at a time, each holding its
max_slots = 4          # start-up peak for 10 minutes, then what it uses
long_lived = true
startup = 600
```

The built-in defaults name common JavaScript tools, not the scripts of one
repo. Put a repo's own scripts in its `.taskguard.toml`:

- single-slot pools per checkout for database migrations (`drizzle-kit`,
  `prisma`, `knex`, `sequelize`, `typeorm`), e2e runs (`playwright test`,
  `cypress run`) and integration runs (`vitest --project integration`)
- launchers such as `bun --bun`, `bunx`, `npx`, `pnpm exec`, `devenv shell --`
  and `node` are stripped before matching
- watch modes and dev servers run straight through

## Known limits

- By default taskguard only decides when a job starts. When another program
  suddenly takes a lot of memory after taskguard's jobs have started, the
  machine can still fill up; taskguard then starts nothing new until the
  pressure eases. With `auto_pause = true` it also pauses (SIGSTOP) its newest
  running jobs when memory reaches `pause_at`, and resumes them (SIGCONT) under
  `resume_at`. A paused job keeps its memory: the pause stops it from growing
  and lets the system compress it or move it to swap. It does not free it.
- A job can also be paused by hand: `taskguard pause JOB`, or `p` in the
  dashboard's Queue view. It stays paused until `taskguard resume JOB` or `p`
  again; auto_pause leaves it alone. New jobs may start while it is paused, and
  they may use its CPU. Its memory stays reserved. taskguard also sees a job
  that someone stopped with `kill -STOP` and treats it as paused by hand. It
  cannot see a Ctrl-Z in the terminal: that stops taskguard itself too.
- A job with no history reserves an estimate. A first run far bigger than
  similar jobs can still take more than its estimate before it is learned.
- An outlier that is real growth is learned late: the next run still
  reserves the usual peak, and only the third high run counts in full. A run
  that failed is not learned above the runs that passed, even when it really
  needed that memory: a job that only reaches its full size when it fails,
  such as a type check that passes on small changes, can be under-reserved. Set
  `outlier_weights = [0.5]` to learn sooner, or `outlier_ratio = 0` to learn
  every peak at once.
- A job that starts early on `noise_mem` or `outside_admit` can push memory
  past `mem_max`, up to `pause_at` (or halfway from `mem_max` to all of RAM
  when `mem_max` is at or above `pause_at`). Set `outside_admit = false` and
  `noise_mem = 0` to keep `mem_max` a hard rule.
- Over-commit (`overcommit`) lets a job take memory over RAM. On macOS that
  makes the machine slower until the job ends; on Linux it goes into swap
  and, should that run out, the OOM killer takes the job. It reads the free
  swap when it decides, not what other programs swap out after.
- A stack that expects work (`--expect`) books room for it until it starts,
  however long that takes: work that never comes keeps that room booked
  while the stack runs. A stack plus its work that do not fit next to another
  stack wait for room, where before the stack started and its work waited.
  Work only links to a stack in its own checkout.
- A guess of the history that does not come true books room for up to ten
  minutes per stack: while it does, jobs that do not fit next to it wait,
  unless nothing but long-lived jobs runs. In the model, when no run follows
  any stack, about a quarter fewer lints ran at 14.5 GB held by other
  programs, and none waited more than a minute longer.

## Moving from tsc-queue

This repository used to be `SpoBo/tsc-queue`, a bash tool that replaced the
compiler inside `node_modules` with a shim. GitHub redirects the old URL. taskguard needs no shims.

```sh
tsc-queue unload         # stop its launchd jobs first, or they re-wrap the compilers
tsc-queue uninstall      # put the original compilers back
cargo install --path .
taskguard import-history # its memory history becomes the first guess for tsc runs
```

Then add the prefix to the scripts. `taskguard doctor` reports shims that are
still installed.

## Files

Everything lives in `~/.cache/taskguard` (or `TASKGUARD_DIR`):

| Path | What it is |
| --- | --- |
| `wait/`, `run/` | One file per waiting or running job, named after its process |
| `stall/` | One file per stalled waiting job: who saw it, and what its owner said it waited for |
| `lock` | The queue lock; the kernel releases it when a process dies |
| `machine` | The shared machine reading |
| `taskguard.db` | Runs, samples and rollups (SQLite) |

A job that is killed leaves a file behind. The next process that looks sees
that its owner is gone and removes it, so a Ctrl-C never wedges the queue.

## Mixed versions

Different versions of taskguard can run on one machine at the same time. For
example, each worktree of a repo can pin its own version. They all share one
queue and one history in the state directory, so each version must read what
the others write:

- The queue entries and the `machine` file are JSON. Fields are only added,
  never removed, renamed or retyped, and a missing field gets a default.
- `taskguard prune` moves runs into the table `pruned_runs`, so every
  version stops learning from them; `prune --undo` moves them back. Without
  `--apply`, prune only shows what it would do.
- The database only gains tables and columns. A new column is nullable or has
  a default, so older versions can still insert rows.
- A job gets the same history key in every version.
- `TASKGUARD_HELD` keeps its meaning, so a nested call never takes a second
  slot, whatever version the outer call was.

A setting that a version does not know is ignored with a warning, not an
error: a repo can pin a newer taskguard that knows it, while an older one runs
from another worktree. `taskguard doctor` lists them. Versions before 0.2.1
stop with an error instead.

A version only follows the rules it knows. A version that does not know
`long_lived` runs such a job as any other: it holds its start-up peak until it
ends. Its waiting jobs do read the lower needs of a long-lived job that a newer
version runs, but they still wait for it to end when nothing else runs.
Versions up to 0.10.0 start the first in line whatever the readings say when
nothing runs but long-lived jobs past their start-up, also a second dev stack
over `mem_max`, and skip that rule at macOS warning; their jobs decide so
whatever newer versions do. They do not know `--expect` either: they ignore
`TASKGUARD_EXPECT`, do not see what a newer stack books for its work,
declared or from its history, and may start their own jobs into that room.
Nor do they know `expect_wait`: they see a stack that waits a few seconds for
its work as one that could start, and mark it as stalled only after 10
seconds, so keep `expect_wait` under 10 while such versions share the
machine. The priority a newer job takes as work
for a stack is an ordinary priority, so their waiting jobs do let it go
first.
Versions up to 0.11.0-rc.2 do not over-commit: a job of theirs waits where a
newer version would start it over RAM. With nothing of taskguard's running a
job that does not fit in RAM holds nobody back in any version, but while it
is first in line, a newer job behind it does not over-commit either.
A version before 0.4.0 does not
know runs: its jobs never step aside for an older run. A version before
0.2.0 does not
read priorities: its waiting jobs start in ticket order and do not step aside
for a job with a higher priority. It does not know auto_pause either: its jobs
are never paused, and its waiting jobs may start while another job is paused.

Versions before 0.4.1 keep the line in ticket order; 0.4.1 and later order it
by the age of a job's run first. With nothing running, an old and a new owner
can each see the other as the job first in line and wait for it, and nothing
starts. In versions after 0.4.1 the waiting job behind such a pair marks the
one that does not start as stalled after 10 seconds and passes it. Two jobs of
0.4.1 or older can still wait for each other until a job of a newer version
queues or until one of them is stopped.

Tests hold each of these rules. A change that cannot follow them must use a
new state directory. Versions that use different directories do not see each
other's jobs. They still see the load of those jobs in the machine readings.

A version that does not know partial fit reads the `machine` file and leaves
the CPU history in it (`recent_cpu`) out when it writes the file. While its
recorder runs, newer versions see no recent low and start a partial fit as
soon as there is room for it. Such a version never starts its own job on part
of its need; a newer job behind it may then mark it as stalled (see above).
In 0.7.1 and older `max_backfill` is 0 by default, so such a version keeps
every reservation unless a settings file sets it.

Versions before 0.2.1 read settings files strictly: they reject a file with a
setting they do not know. The user config and a repo's `.taskguard.toml` are
read by every version that runs there, so set a new setting such as
`max_backfill` in them only once every such version is 0.2.1 or newer. The
dashboard's Config view does not offer `max_backfill` for that reason. It
writes `unlimited = true` only when you take a pool down to no limit; a
version that does not know `unlimited` keeps that pool's old ceiling.

## Uninstall

```sh
cargo uninstall taskguard
rm -rf ~/.cache/taskguard ~/.config/taskguard
```
