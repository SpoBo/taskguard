//! taskguard: a `sem` that learns what each job needs. It starts a command only
//! when the machine has room for the CPU and memory that the same command used
//! on its past runs.

mod builtin;
mod commands;
mod config;
mod dash;
mod db;
mod groups;
mod help;
mod insight;
mod key;
mod machine;
mod matcher;
mod proof;
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

const USAGE_HEAD: &str = "\
taskguard - a sem that learns what each job needs

usage:
  taskguard [options] [--] COMMAND [ARGS...]   run COMMAND when the machine has room for it
  taskguard --wait [--id NAME]                 wait until no job of the pool runs or waits
  taskguard top                                the dashboard
  taskguard status [--json]                    what runs, what waits, and why
  taskguard pause JOB | resume JOB             pause or resume a running job (a pid, or its key or part of it)
  taskguard history                            learned needs per command
  taskguard outliers [PATTERN]                 memory peaks far above a job's other runs, and how much they count
  taskguard prune PATTERN | --outliers | --undo
                                               drop runs from the history, or put them back (a dry run without --apply)
  taskguard doctor [--explain \"COMMAND\"]       configuration, readings, and how a command matches
  taskguard import-history                     load tsc-queue's history
  taskguard --receipt ID [--partial WHY] [-- COMMAND]
                                               run a command for a proof receipt that CI can skip on
  taskguard proof publish|show|check|log|install|uninstall
                                               publish receipts as commit statuses, and read them back
  taskguard run [options] -- COMMAND           the same as the first form, for a command named like a subcommand
  taskguard help COMMAND                       more about one command; COMMAND --help shows the same
  taskguard help --all                         every command in full, in one text (for LLM agents)

";

const USAGE_TAIL: &str = "
settings: ~/.config/taskguard/config.toml, [dir.\"<path>\"] sections in it, and a
repo .taskguard.toml. See taskguard.example.toml.
";

fn usage() -> String {
    format!("{USAGE_HEAD}{}{USAGE_TAIL}", help::RUN_OPTIONS)
}

const SUBCOMMANDS: &[&str] = &[
    "top",
    "status",
    "pause",
    "resume",
    "history",
    "outliers",
    "prune",
    "doctor",
    "import-history",
    "proof",
    "version",
    "help",
    "__recorder",
];

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
            "--priority" => {
                let v = take_value(args, &mut i, name)?;
                o.priority = Some(v.parse().map_err(|_| anyhow::anyhow!("--priority needs a whole number"))?);
            }
            "--now" => o.now = true,
            "--bg" => o.bg = true,
            "--fg" => o.bg = false,
            "--pipe" => o.pipe = true,
            "-q" | "--quiet" => o.quiet = true,
            "--hints" => o.hints = Some(true),
            "--no-hints" => o.hints = Some(false),
            "--wait" => o.wait = true,
            "--receipt" => o.receipt = Some(take_value(args, &mut i, name)?),
            "--partial" => o.partial = Some(take_value(args, &mut i, name)?),
            "--publish" => {
                let v = take_value(args, &mut i, name)?;
                o.publish = Some(v.parse().map_err(|_| anyhow::anyhow!("--publish needs a number of seconds"))?);
            }
            "-h" | "--help" => o.help = true,
            _ if a.starts_with('-') && o.cmd.is_empty() => bail!("unknown option {a}\n\n{}", usage()),
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
        if first != "help" && first != "__recorder" && help::asked(rest) {
            print!("{}", help::text(first).unwrap_or_else(usage));
            return Ok(0);
        }
        return match first {
            "help" if matches!(rest.first().map(String::as_str), Some("--all" | "all")) => {
                print!("{}", help::all(&usage()));
                Ok(0)
            }
            "help" if !rest.is_empty() => match help::text(if help::asked(rest) { "help" } else { &rest[0] }) {
                Some(t) => {
                    print!("{t}");
                    Ok(0)
                }
                None => bail!("no command {:?}; taskguard help lists them", rest[0]),
            },
            "" | "help" | "-h" | "--help" => {
                print!("{}", usage());
                Ok(if first.is_empty() { 2 } else { 0 })
            }
            "version" | "-V" | "--version" => {
                println!("taskguard {}", env!("CARGO_PKG_VERSION"));
                Ok(0)
            }
            "status" => commands::status(rest.iter().any(|a| a == "--json")),
            "pause" => commands::pause(rest, true),
            "resume" => commands::pause(rest, false),
            "history" => commands::history(),
            "outliers" => commands::outliers(rest),
            "prune" => commands::prune(rest),
            "doctor" => commands::doctor(rest),
            "import-history" => commands::import_history(),
            "proof" => proof::dispatch(rest),
            "top" => top::run(rest),
            "__recorder" => recorder::run().map(|_| 0),
            _ => unreachable!(),
        };
    }
    let rest = if first == "run" { &args[1..] } else { args };
    let o = parse_opts(rest)?;
    if o.help {
        print!("{}", help::text(if o.wait { "wait" } else { "run" }).unwrap_or_default());
        return Ok(0);
    }
    if let Some(id) = &o.receipt {
        if o.jobs.is_some() || o.id.is_some() || o.bg || o.wait {
            bail!("--receipt runs the command outside the queue; put taskguard inside the command for that");
        }
        return proof::run_receipt(id, o.partial.as_deref(), o.publish, &o.cmd);
    }
    if o.partial.is_some() || o.publish.is_some() {
        bail!("--partial and --publish go with --receipt ID");
    }
    if o.wait && o.cmd.is_empty() {
        return runner::wait_pool(o.id.as_deref());
    }
    if o.cmd.is_empty() {
        bail!("no command given\n\n{}", usage());
    }
    runner::run(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        matcher::split_shell(s)
    }

    /// A release must say what changed: the release workflow takes its notes
    /// from this version's section, and fails without one.
    #[test]
    fn the_changelog_has_a_section_for_this_version() {
        let log = include_str!("../CHANGELOG.md");
        let version = env!("CARGO_PKG_VERSION");
        let head = format!("## [{version}] - ");
        assert!(log.lines().any(|l| l.starts_with(&head)), "CHANGELOG.md has no section \"{head}DATE\"");
        assert!(log.contains(&format!("\n[{version}]: https://github.com/SpoBo/taskguard/")), "CHANGELOG.md has no link for {version}");
        assert!(log.contains("## [Unreleased]"), "CHANGELOG.md keeps an Unreleased section for the next release");
    }

    #[test]
    fn sem_style_parsing() {
        let o = parse_opts(&v("-j4 --id e2e --st -30 bunx playwright test --workers 2")).unwrap();
        assert_eq!(o.jobs.as_deref(), Some("4"));
        assert_eq!(o.id.as_deref(), Some("e2e"));
        assert_eq!(o.timeout, Some(-30.0));
        assert_eq!(o.cmd, v("bunx playwright test --workers 2"));

        let o = parse_opts(&v("--min-cpu 4 --min-mem=6G --priority=-2 --now -- tsc -p .")).unwrap();
        assert_eq!(o.priority, Some(-2));
        assert_eq!(o.min_cpu, Some(4.0));
        assert_eq!(o.min_mem_kb, Some(6 * 1024 * 1024));
        assert!(o.now);
        assert_eq!(o.cmd, v("tsc -p ."));

        let o = parse_opts(&v("-j +0 --no-hints tsc --watch")).unwrap();
        assert_eq!(o.jobs.as_deref(), Some("+0"));
        assert_eq!(o.hints, Some(false));
        assert_eq!(o.cmd, v("tsc --watch"), "options after the command belong to it");

        assert!(parse_opts(&v("--bogus tsc")).is_err());
        let o = parse_opts(&v("--receipt e2e --partial 'transfer specs only' -- bunx playwright test transfer")).unwrap();
        assert_eq!((o.receipt.as_deref(), o.partial.as_deref()), (Some("e2e"), Some("transfer specs only")));
        assert_eq!(o.cmd, v("bunx playwright test transfer"));
        let o = parse_opts(&v("--receipt=unit")).unwrap();
        assert!(o.receipt.is_some() && o.cmd.is_empty(), "no command: the policy command");
        let o = parse_opts(&v("--wait --id build")).unwrap();
        assert!(o.wait && o.cmd.is_empty());
    }
}
