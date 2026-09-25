//! taskguard: a `sem` that learns what each job needs. It starts a command only
//! when the machine has room for the CPU and memory that the same command used
//! on its past runs.

mod builtin;
mod commands;
mod config;
mod dash;
mod db;
mod groups;
mod insight;
mod key;
mod machine;
mod matcher;
mod queue;
mod recorder;
mod report;
mod runner;
#[cfg(test)]
mod sim;
mod sys;
mod top;

use anyhow::{Result, bail};
use runner::Opts;

const USAGE: &str = "\
taskguard - a sem that learns what each job needs

usage:
  taskguard [options] [--] COMMAND [ARGS...]   run COMMAND when the machine has room for it
  taskguard --wait [--id NAME]                 wait until no job of the pool runs or waits
  taskguard top                                the dashboard
  taskguard status [--json]                    what runs, what waits, and why
  taskguard history                            learned needs per command
  taskguard doctor [--explain \"COMMAND\"]       configuration, readings, and how a command matches
  taskguard import-history                     load tsc-queue's history
  taskguard run [options] -- COMMAND           the same as the first form, for a command named like a subcommand

options (sem style; the default is to run in the foreground):
  -j N | +N | -N | N%     slot ceiling for this pool (as in sem); none by default
  --id NAME               pool name (sem's semaphore id)
  --ns NAME               namespace for the dashboard (default: the repo name)
  --st SECS               SECS > 0: run anyway after SECS; SECS < 0: give up (exit 124)
  --st-exit N             the exit code when --st gives up, instead of 124
  --key KEY               history key (default: project path + command)
  --min-cpu N             never start with fewer than N free cores
  --min-mem SIZE          never start with less than SIZE free (6G, 512M)
  --now                   skip the queue, but still measure and learn
  --bg                    wait for room, then return and let the job run on
  --fg                    run in the foreground (the default)
  --pipe                  with --bg: pass stdin to the command
  -q, --quiet             only print waits longer than the status interval
  --hints / --no-hints    agent hints on or off for this call

settings: ~/.config/taskguard/config.toml, [dir.\"<path>\"] sections in it, and a
repo .taskguard.toml. See taskguard.example.toml.
";

const SUBCOMMANDS: &[&str] = &["top", "status", "history", "doctor", "import-history", "version", "help", "__recorder"];

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    let a = &args[*i];
    if let Some((_, v)) = a.split_once('=')
        && a.starts_with("--")
    {
        return Ok(v.to_string());
    }
    *i += 1;
    match args.get(*i) {
        Some(v) => Ok(v.clone()),
        None => bail!("{flag} needs a value"),
    }
}

/// sem-style parsing: options first, then the command. Parsing stops at `--`
/// or at the first word that is not an option.
pub fn parse_opts(args: &[String]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let name = a.split_once('=').map(|(n, _)| n).filter(|_| a.starts_with("--")).unwrap_or(a);
        match name {
            "--" => {
                i += 1;
                break;
            }
            "-j" | "--jobs" | "-P" | "--max-procs" => o.jobs = Some(take_value(args, &mut i, name)?),
            _ if a.starts_with("-j") && a.len() > 2 => o.jobs = Some(a[2..].to_string()),
            "--id" | "--semaphore-name" | "--semaphorename" => o.id = Some(take_value(args, &mut i, name)?),
            "--ns" => o.ns = Some(take_value(args, &mut i, name)?),
            "--st" | "--semaphore-timeout" | "--semaphoretimeout" => {
                let v = take_value(args, &mut i, name)?;
                o.timeout = Some(v.parse().map_err(|_| anyhow::anyhow!("{name} needs a number of seconds"))?);
            }
            "--st-exit" => {
                let v = take_value(args, &mut i, name)?;
                o.timeout_exit = Some(v.parse().map_err(|_| anyhow::anyhow!("--st-exit needs an exit code"))?);
            }
            "--key" => o.key = Some(take_value(args, &mut i, name)?),
            "--min-cpu" => {
                let v = take_value(args, &mut i, name)?;
                o.min_cpu = Some(v.parse().map_err(|_| anyhow::anyhow!("--min-cpu needs a number of cores"))?);
            }
            "--min-mem" => o.min_mem_kb = Some(config::parse_size_kb(&take_value(args, &mut i, name)?)?),
            "--now" => o.now = true,
            "--bg" => o.bg = true,
            "--fg" => o.bg = false,
            "--pipe" => o.pipe = true,
            "-q" | "--quiet" => o.quiet = true,
            "--hints" => o.hints = Some(true),
            "--no-hints" => o.hints = Some(false),
            "--wait" => o.wait = true,
            _ if a.starts_with('-') && o.cmd.is_empty() => bail!("unknown option {a}\n\n{USAGE}"),
            _ => break,
        }
        i += 1;
    }
    o.cmd = args[i.min(args.len())..].to_vec();
    Ok(o)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match dispatch(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("taskguard: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn dispatch(args: &[String]) -> Result<i32> {
    let first = args.first().map(String::as_str).unwrap_or("");
    if SUBCOMMANDS.contains(&first) || matches!(first, "-h" | "--help" | "-V" | "--version" | "") {
        let rest = &args[1.min(args.len())..];
        return match first {
            "" | "help" | "-h" | "--help" => {
                print!("{USAGE}");
                Ok(if first.is_empty() { 2 } else { 0 })
            }
            "version" | "-V" | "--version" => {
                println!("taskguard {}", env!("CARGO_PKG_VERSION"));
                Ok(0)
            }
            "status" => commands::status(rest.iter().any(|a| a == "--json")),
            "history" => commands::history(),
            "doctor" => commands::doctor(rest),
            "import-history" => commands::import_history(),
            "top" => top::run(rest),
            "__recorder" => recorder::run().map(|_| 0),
            _ => unreachable!(),
        };
    }
    let rest = if first == "run" { &args[1..] } else { args };
    let o = parse_opts(rest)?;
    if o.wait && o.cmd.is_empty() {
        return runner::wait_pool(o.id.as_deref());
    }
    if o.cmd.is_empty() {
        bail!("no command given\n\n{USAGE}");
    }
    runner::run(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        matcher::split_shell(s)
    }

    #[test]
    fn sem_style_parsing() {
        let o = parse_opts(&v("-j4 --id e2e --st -30 bunx playwright test --workers 2")).unwrap();
        assert_eq!(o.jobs.as_deref(), Some("4"));
        assert_eq!(o.id.as_deref(), Some("e2e"));
        assert_eq!(o.timeout, Some(-30.0));
        assert_eq!(o.cmd, v("bunx playwright test --workers 2"));

        let o = parse_opts(&v("--min-cpu 4 --min-mem=6G --now -- tsc -p .")).unwrap();
        assert_eq!(o.min_cpu, Some(4.0));
        assert_eq!(o.min_mem_kb, Some(6 * 1024 * 1024));
        assert!(o.now);
        assert_eq!(o.cmd, v("tsc -p ."));

        let o = parse_opts(&v("-j +0 --no-hints tsc --watch")).unwrap();
        assert_eq!(o.jobs.as_deref(), Some("+0"));
        assert_eq!(o.hints, Some(false));
        assert_eq!(o.cmd, v("tsc --watch"), "options after the command belong to it");

        assert!(parse_opts(&v("--bogus tsc")).is_err());
        let o = parse_opts(&v("--wait --id build")).unwrap();
        assert!(o.wait && o.cmd.is_empty());
    }
}
