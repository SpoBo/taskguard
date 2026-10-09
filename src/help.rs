//! Help for each subcommand: `taskguard help COMMAND`, or `COMMAND --help`.

/// The options of the run form, shared by `taskguard --help` and `help run`.
pub const RUN_OPTIONS: &str = "\
options (sem style; the default is to run in the foreground):
  -j N | +N | -N | N%     slot ceiling for this pool (as in sem); none by default
  --id NAME               pool name (sem's semaphore id)
  --ns NAME               namespace for the dashboard (default: the repo name)
  --st SECS               SECS > 0: run anyway after SECS; SECS < 0: give up (exit 124)
  --st-exit N             the exit code when --st gives up, instead of 124
  --key KEY               history key (default: project path + command)
  --min-cpu N             never start with fewer than N free cores
  --min-mem SIZE          never start with less than SIZE free (6G, 512M)
  --priority N            higher starts first (default 0; negative is allowed)
  --now                   skip the queue, but still measure and learn
  --bg                    wait for room, then return and let the job run on
  --fg                    run in the foreground (the default)
  --pipe                  with --bg: pass stdin to the command
  -q, --quiet             only print waits longer than the status interval
  --hints / --no-hints    agent hints on or off for this call
  --receipt ID            run for a proof receipt (taskguard help proof); no
                          COMMAND runs the policy command of ID
  --partial REASON        with --receipt: a chosen subset, and why it is enough
  --publish SECS          with --receipt: publish in the background once HEAD has
                          these files and is on GitHub, for up to SECS
";

const RUN: &str = "\
taskguard [options] [--] COMMAND [ARGS...]
taskguard run [options] -- COMMAND [ARGS...]

Run COMMAND when the machine has room for it. The room it needs is learned
from its past runs: memory is the highest peak of the last 10 runs, CPU the
cores it used, the newest runs counting most. Use the `run` form for a
command named like a subcommand, such as a script called `top`.

A job that is short of room by only a little (noise_mem, noise_cpu), or only
because programs outside taskguard hold the room (outside_admit), starts
anyway and prints a warning that says what it lacks.

Options go before the command. Words after the command, --help too, belong
to the command.

";

const WAIT: &str = "\
taskguard --wait [--id NAME]

Wait until no job of the pool NAME runs or waits. Without --id, wait for the
jobs that have no pool, as sem --wait does. For scripts that must not go on
while a batch of jobs is still busy.
";

const TOP: &str = "\
taskguard top [--view NAME] [--range RANGE] [--job KEY] [--print [--width N] [--height N] [--keys]]

The dashboard: the machine, the queue, past runs, every known task, trends
and warnings. Inside it, ? lists the keys of the view you are in. In the
Tasks view (tab 4), filter tasks and prune their history: x one task, X every
task shown.

options:
  --view NAME     open on a view: overview, queue, runs, tasks, trends,
                  warnings, namespaces, config, help, or job (with --job)
  --queue         the same as --view queue
  --range RANGE   the time range of the charts: 5m, 15m, 1h, 6h, 24h or 7d
  --job KEY       the job view for this history key
  --print         print one frame as plain text and exit, for scripts and agents
  --keys          with --print: show the list of keys that ? opens
  --width N       with --print: the frame width (default: the terminal, at least 100)
  --height N      with --print: the frame height (default: the terminal, at least 30)
";

const STATUS: &str = "\
taskguard status [--json]

What runs, what waits, and why each waiting job waits. Also the warnings of
the last 24 hours.

options:
  --json          the same as JSON, for scripts
";

const PAUSE: &str = "\
taskguard pause JOB
taskguard resume JOB

Pause a running job (SIGSTOP on the command and every process below it), or
let it go on (SIGCONT). JOB is a pid, a history key, or a unique part of a
key. A job paused by hand keeps its memory but gives up its CPU, and
auto_pause leaves it alone. Its paused time does not count as run time.
";

const START: &str = "\
taskguard start JOB

Start a waiting job now, whatever the limits say, as g in taskguard top does.
JOB is a pid, a ticket, a history key, or a unique part of a key. The job
runs on the cores it gets, so it may be slower, and no check holds it back:
not CPU, not memory. A job that cannot fit prints this command in its wait
output, with what it needs and what holds the room.

Use it when a job waits for room it will not get soon. To fix the cause
instead, see cpu_max, partial_fit and max_backfill in taskguard.example.toml,
or taskguard prune for a need learned from old runs.
";

const HISTORY: &str = "\
taskguard history

The learned needs per history key: how many runs, the cores and the memory
the next run reserves, and how the last run went. The keys are the PATTERN
that `outliers` and `prune` take.
";

const OUTLIERS: &str = "\
taskguard outliers [PATTERN] [--ratio R] [--min SIZE]

List the memory peaks that stand far above a job's other runs, in the last
hist_keep (10) runs of each key, and how much each one counts.

A peak is an outlier when it is more than R times the next peak below it, and
at least SIZE above it. Jobs whose usual peak is under SIZE have no outliers:
there a jump is mostly a full build after cache hits. Outliers count by
outlier_weights: the first time not at all, the second time half, the third
time in full. A run that failed (exit not 0) never counts.

arguments:
  PATTERN         the keys to look at, as `taskguard history` shows them;
                  * and ? match anything: 'packages/api:*'. Quote it, so the
                  shell leaves the * alone. Default: every key.

options:
  --ratio R       how many times the next peak (default outlier_ratio, 2).
                  1.5 finds more outliers, 3 fewer.
  --min SIZE      how far above the next peak (default outlier_min, 1G).

columns:
  run             the run id, for `prune`
  usual           the highest peak below the outliers
  x               the peak divided by usual
  next run        the memory the next run reserves
  why             how much the run counts, and why

examples:
  taskguard outliers
  taskguard outliers 'root:*' --ratio 1.5
";

const PRUNE: &str = "\
taskguard prune PATTERN [--older-than AGE] [--apply]
taskguard prune --outliers [PATTERN] [--ratio R] [--min SIZE] [--older-than AGE] [--apply]
taskguard prune --undo [PATTERN] [--apply]

Drop runs from the history, so the next runs learn without them. Use it after
a change that makes old runs wrong, such as a big refactor, or to drop
outliers that you know were one-offs.

Without --apply, prune only shows what it would do. With --apply, the runs
move to a separate table: every taskguard version stops learning from them,
and `prune --undo` puts them back. When runs leave the history, older runs
move into the last hist_keep runs that count.

arguments:
  PATTERN         the keys, as `taskguard history` shows them; * and ? match
                  anything: 'packages/api:*'. Quote it. Needed for a plain
                  prune ('*' for all keys); with --outliers or --undo the
                  default is every key.

options:
  --older-than AGE  only runs that ended longer ago than AGE: 30d, 12h, 90m,
                    45s. A bare number is days.
  --outliers        only the runs that `taskguard outliers` lists
  --ratio R         with --outliers: as in `taskguard outliers`
  --min SIZE        with --outliers: as in `taskguard outliers`
  --undo            put pruned runs back
  --apply           do it; without it, only show what would happen

examples:
  taskguard prune --outliers                        what would go
  taskguard prune --outliers --ratio 3 --apply      drop only far outliers
  taskguard prune 'packages/api:*' --older-than 30d --apply
  taskguard prune --undo 'packages/api:*' --apply
";

const DOCTOR: &str = "\
taskguard doctor [--explain \"COMMAND\"]

The settings and where each one comes from, the live readings, settings this
version does not know, and leftover tsc-queue shims.

options:
  --explain \"COMMAND\"  how one command is matched, which pool and history key
                       it gets, and what it learned, outliers included. Run it
                       in the directory the command runs in.
";

const IMPORT: &str = "\
taskguard import-history

Load tsc-queue's memory history (~/.cache/tsc-queue/history.tsv, or
$TSC_QUEUE_DIR/history.tsv) into taskguard's history, once.
";

const PROOF: &str = "\
taskguard --receipt ID [--partial REASON] [--publish SECS] [-- COMMAND]
taskguard proof publish [--sha SHA]... [--wait SECS] [--again] [-q]
taskguard proof show [--history]
taskguard proof check [ID...] [--sha SHA] [--labels L,L] [--wait SECS] [--github-output]
taskguard proof log [--id ID] [--since SHA] [-n N] [--branch REF]
taskguard proof install [ID --command \"CMD\" [--os OS] [--levels L,L]]
taskguard proof uninstall [ID] [--purge-local]

Proof receipts: publish what passed on this machine as GitHub commit statuses
(taskguard/ID), so CI can skip work that is already proven.

1. Run. `taskguard --receipt ID` runs the command that .taskguard/proof.toml
   names for ID. It hashes the files on disk before and after the run,
   uncommitted and untracked files included, ignored files left out. When the
   hash did not change, it keeps a receipt: passed or failed, on that tree.
   A file that changed during the run means no receipt. The command runs at
   once, outside the queue: it is mostly a task runner whose leaves queue.
   Another command proves only a part: give --partial \"REASON\".
2. Publish. After the push, `proof publish` posts each receipt whose tree is
   the tree of the pushed commit, as status taskguard/ID: green or red, with
   \"LEVEL, OS/ARCH, DURATION, HOST\" as its description. A commit of other
   files gets nothing. The push hook from `proof install` does this.
3. Check. In CI, `proof check` reads the statuses of the PR head and says per
   ID: skip (green, at a level and on an OS the policy accepts) or run (no
   proof, red, another level or OS, a taskguard:ci or taskguard:ci:ID label).
   It cannot read the statuses: run. Exit 0 when all skip; with
   --github-output it writes ID=skip|run to $GITHUB_OUTPUT and exits 0, and
   a missing policy or ID means run, not an error.

The policy, .taskguard/proof.toml:

  [receipt.unit-tests]
  command = \"bun install --frozen-lockfile && turbo run test:unit\"
  os = \"any\"             # any, linux or macos: whose proof CI accepts
  levels = [\"full\"]      # full and/or partial: what lets a PR skip CI
  env_files = \"deny\"     # deny: no receipt while an ignored .env* file exists
  env_allow = []          # globs of ignored .env* files that may exist

The command must rebuild what the hash cannot see: install dependencies from
the lockfile, and let the task runner build dist and generated code.

commands:
  publish         post receipts for HEAD (or each --sha). --wait SECS waits
                  for the commit to reach GitHub; without --sha it follows
                  HEAD meanwhile, so later commits and the push may come
                  after it starts. A full receipt wins over a partial one on
                  the same files. CI checks out a merge of the PR head:
                  --sha HEAD_SHA posts its receipts on the PR head.
  show            this checkout: local receipts and the statuses on HEAD
  check           CI: skip or run per ID. --wait SECS waits for statuses that
                  are missing, for a push whose proof is still on its way.
  log             proof per commit on the main branch; a squash merge reads
                  the PR head (~ marks proof on a different tree). With --id,
                  the least proven commits first: the suspects when a full
                  run goes red.
  install         write a policy entry, the pre-push hook, and print what
                  else is missing: the CI check job, and the ruleset that
                  makes the statuses a merge gate.
  uninstall       remove one ID from the policy, or the policy and the hook.

GitHub is reached through the gh CLI (GH_TOKEN in CI). TASKGUARD_GH names
another gh.
";

const VERSION: &str = "\
taskguard version

Print the version. The same as taskguard --version.
";

const HELP: &str = "\
taskguard help [COMMAND]
taskguard help --all

Help for one command. COMMAND --help shows the same. With --all, the overview
and the help of every command in one text: what an LLM agent should read
before it uses taskguard.
";

/// The help for one subcommand, or None when there is no such command.
pub fn text(cmd: &str) -> Option<String> {
    Some(match cmd {
        "run" => format!("{RUN}{RUN_OPTIONS}"),
        "wait" | "--wait" => WAIT.into(),
        "top" => TOP.into(),
        "status" => STATUS.into(),
        "pause" | "resume" => PAUSE.into(),
        "start" => START.into(),
        "history" => HISTORY.into(),
        "outliers" => OUTLIERS.into(),
        "prune" => PRUNE.into(),
        "doctor" => DOCTOR.into(),
        "import-history" => IMPORT.into(),
        "proof" | "receipt" => PROOF.into(),
        "version" => VERSION.into(),
        "help" => HELP.into(),
        _ => return None,
    })
}

/// Every command in the order `help --all` prints them.
pub const ALL: [&str; 13] =
    ["run", "wait", "top", "status", "pause", "start", "history", "outliers", "prune", "doctor", "import-history", "proof", "version"];

/// The full help, every command in one text: for LLM agents and for reading
/// it all at once. `usage` is the overview that comes first.
pub fn all(usage: &str) -> String {
    let mut out = String::from(usage);
    for cmd in ALL {
        out.push_str(&format!("\n{}\n\n", "-".repeat(78)));
        out.push_str(&text(cmd).unwrap_or_default());
    }
    out
}

/// Whether the arguments of a subcommand ask for help.
pub fn asked(args: &[String]) -> bool {
    args.iter().any(|a| a == "-h" || a == "--help")
}
