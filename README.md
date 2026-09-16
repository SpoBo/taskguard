# tsc-queue

A machine-wide admission queue for TypeScript compiles on macOS.

It stops your laptop from running out of memory when several `tsc` processes
start at the same time. Each compile waits for a free slot, and it also waits
until the machine has room for the amount of memory that this exact project
needed on its past runs.

```
$ tsc-queue status
memory 61% of 32 GB held, 14% reserved for growth   limit 85%   max slots 6

RUNNING
  packages_api_server                    47s   wants 18422 MB (3910 MB resident)   predicted 25600 MB
  packages_ui                            12s   wants  1204 MB ( 980 MB resident)   predicted  1600 MB

WAITING
  packages_worker                        31s in the queue

2 of max 6 compiling, 1 queued, 19626 MB wanted by them
room for another: no - a typical job would land at 91%, over 85%
```

## The problem

A task runner starts one `tsc` process per task, and it only caps tasks inside
a single run. Two terminals, two worktrees, or an editor and a CI script can
each start a full set of compiles. Nothing on the machine holds a shared
ceiling, so a large monorepo pushes the machine into swap and macOS starts
killing processes.

Raising or lowering the task runner concurrency does not fix this. One large
package can want 25 GB on its own. A limit of one task still lets that one task
take the whole machine.

`tsc-queue` puts one gate in front of every `tsc` process on the machine,
whoever started it.

## How it decides

A compile is admitted when two conditions hold.

1. Fewer than `TSC_QUEUE_MAX_SLOTS` compiles are running. This is a ceiling,
   not a target.
2. `memory in use` + `growth reserve` + `this job's predicted peak` stays under
   `TSC_QUEUE_MEM_MAX` percent of RAM.

The growth reserve is the part the running compiles have not taken yet:
`sum(max(0, predicted_peak - current_footprint))`. Without it, a second job is
admitted into space the first one is about to eat.

The prediction is the largest of the project's last ten recorded runs. Every
run appends one line to `~/.cache/tsc-queue/history.tsv`. A project with no
history is assumed to need `TSC_QUEUE_ASSUME_MB`.

A run is always admitted when nothing else is running, so the queue can never
deadlock on a busy machine.

### Memory is measured as physical footprint, not resident size

macOS compresses a process's pages under memory pressure. One measured `tsc`
reported 3.8 GB resident while it really held 22 GB. Activity Monitor shows
both numbers and looks self-contradictory: the list column is the physical
footprint, and the per-process info panel is the resident set.

Physical footprint charges compressed pages at their original size, so it is
demand, not occupancy. Demand is what an admission gate must reserve, because
resident size only looks small once the machine is already out of room.

`tsc-queue` samples `/usr/bin/footprint` every two seconds. That call costs
about 120 ms. `top -l 1 -pid` and `vmmap --summary` both cost about 1.5 s.

The sample covers the whole process tree, not only the process the shim
started. The JavaScript entry point is a node process that spawns the native
compiler, so the memory lives in the grandchild. Measuring the direct child
alone reported 4 MB for a compile that really held 1793 MB.

## Install

```sh
git clone https://github.com/<you>/tsc-queue.git
cd tsc-queue
./install.sh ~/code/my-monorepo
```

The installer copies the script to `~/.local/bin/tsc-queue`, writes
`~/.config/tsc-queue.conf`, wraps the compiler in each checkout you named, and
loads two launchd jobs. Run it again at any time; it never overwrites a config
file you already have.

Install somewhere else with `PREFIX=/usr/local ./install.sh`.

To manage several checkouts, or a directory full of git worktrees, edit
`~/.config/tsc-queue.conf` and run `tsc-queue install`.

## Uninstall

```sh
./uninstall.sh            # restore every compiler, keep config and history
./uninstall.sh --purge    # also delete the config file and the history
```

The uninstaller stops the launchd jobs before it restores the compilers. That
order matters: a repair job that is still loaded would re-wrap them seconds
later.

## Commands

| Command | What it does |
| --- | --- |
| `tsc-queue status` | What is compiling, what is waiting, and whether there is room for one more |
| `tsc-queue status --watch [secs]` | The same report, redrawn live. Default 2 seconds |
| `tsc-queue history` | Peak memory and run count per project |
| `tsc-queue doctor` | Configuration, and which checkouts are wrapped |
| `tsc-queue install` | Wrap the compiler in every configured checkout |
| `tsc-queue uninstall` | Put the original compilers back |
| `tsc-queue repair` | Load the launchd job that re-wraps after a package install |
| `tsc-queue watch` | Load the launchd job that reacts to a new compiler binary |
| `tsc-queue unload` | Stop and remove both launchd jobs |

Set `TSC_QUEUE_DISABLE=1` to bypass the queue for one command.

## Configuration

All settings live in `~/.config/tsc-queue.conf`. See
[`tsc-queue.conf.example`](tsc-queue.conf.example) for the full list.

```sh
TSC_QUEUE_ROOTS="$HOME/code/my-monorepo"   # checkouts to manage
TSC_QUEUE_SCAN="$HOME/worktrees"           # directories full of checkouts
TSC_QUEUE_MAX_SLOTS=6                      # ceiling on concurrent compiles
TSC_QUEUE_MEM_MAX=85                       # percent of RAM not to exceed
TSC_QUEUE_ASSUME_MB=1500                   # guess for a project with no history
```

**Settings must be in the file, not in your shell.** A task runner such as
turbo runs its tasks in a strict environment mode and drops every
`TSC_QUEUE_*` variable. Exporting them looks like it works and silently does
nothing. The shim reads the file from disk on every call.

## Where it hooks in

`tsc-queue` renames each checkout's real compiler to `tsc-real` and writes a
small bash shim in its place.

That is the only interception point that catches every caller: `npm run`,
`pnpm`, `bun run`, `npx`, a task runner, an editor, and a direct call. A `PATH`
shim does not work, because a package manager puts `node_modules/.bin` ahead of
`PATH`.

It also means the tool touches nothing outside the checkouts you name. No other
project's compiler is modified.

The wrap uses a hardlink, then an atomic rename. A plain `mv` would leave the
compiler path empty for an instant, and anything invoking `tsc` in that window
would die with "No such file or directory".

The saved original is called `tsc-real`, with no file extension. That detail is
load-bearing. The JavaScript entry point is loaded by node, node picks its
loader from the file extension, and a name such as `tsc.real` makes every call
fail with `ERR_UNKNOWN_FILE_EXTENSION`. The native Go compiler does not care,
so the fault only shows on npm, pnpm and yarn layouts. An older `tsc.real` is
renamed on the next `tsc-queue install`.

These layouts are found by default:

```
node_modules/typescript/bin/tsc
node_modules/.pnpm/typescript@*/node_modules/typescript/bin/tsc
node_modules/.bun/typescript@*/node_modules/typescript/bin/tsc
node_modules/@typescript/typescript-darwin-*/lib/tsc
node_modules/.pnpm/@typescript+typescript-darwin-*@*/.../lib/tsc
node_modules/.bun/@typescript+typescript-darwin-*@*/.../lib/tsc
```

Set `TSC_QUEUE_GLOBS` for an unusual layout, such as a monorepo that keeps a
copy of TypeScript inside every package.

## Staying installed

A package install writes the original compiler back over the shim. Two launchd
jobs restore it.

- **The path watcher** watches each compiler's directory and fires within about
  a second of the binary being replaced.
- **The repair sweep** runs every 30 seconds. It is the net that also catches a
  brand-new checkout whose paths the watcher does not know about yet.

Both call `tsc-queue install --quiet`, which is idempotent. It rewrites a shim
only when the text differs, and it writes through a temp file and a rename,
because bash reads a script as it runs and overwriting a shim in place would
corrupt a compile already executing it.

A git hook cannot do this job. `post-merge` fires when the merge lands, which
is before the session runs its package install, so the hook would re-wrap and
then be overwritten seconds later.

Logs go to `~/.cache/tsc-queue/repair.log`.

## What is never queued

These finish in milliseconds and must not wait behind a 90 second build:

`--version`, `--help`, `--init`, `--showConfig`, `--listFilesOnly`, and a call
with no arguments.

`--watch` is also never queued, because a watch would hold a slot for hours.

Some repositories fire hundreds of `tsc --showConfig -p <dir>` probes in one
task. Counting those as compiles makes a correct queue look broken.

A nested call is also never queued. The JavaScript entry point spawns the
native compiler, and both are wrapped, so one invocation would take two slots.
The outer shim exports `TSC_QUEUE_HELD=1` and a shim that sees it runs straight
through. Without that guard the queue deadlocks at the slot ceiling: every slot
is held by an outer shim waiting on an inner one that can never be admitted.

## It also fixes orphans

The shim runs the real compiler as a child and traps `INT`, `TERM` and `HUP` to
kill it. A slot whose owner died is reclaimed by a liveness check, so Ctrl-C
never wedges the queue.

A task runner on its own does not do this. Killing turbo has been observed to
leave five `tsc` processes running unsupervised.

## Limits

- **macOS only.** The memory readings use `vm_stat`, `sysctl` and
  `/usr/bin/footprint`, and the background jobs use launchd.
- **It does not make anything faster.** It makes a machine that would have
  thrashed finish instead.
- **A project larger than your RAM still cannot run.** The queue admits it when
  nothing else is running, and it then fails the same way it always did. The
  history tells you which project that is: `tsc-queue history`.
- **Yarn Plug'n'Play is not supported.** There is no compiler file to wrap.

## License

MIT. See [LICENSE](LICENSE).
