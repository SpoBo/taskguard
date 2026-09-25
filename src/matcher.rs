//! Turns a command line into the command that really runs, and matches it
//! against patterns.
//!
//! A pattern is a list of words. The first word matches the command (the
//! basename when the pattern has no `/`, the whole path otherwise). The other
//! words must appear among the arguments, in order, but not necessarily next to
//! each other. `*` matches inside one word, `**` also across `/`, a lone `*`
//! as the first word matches any command, and a final `$` means "no more
//! arguments after the last matched word".

/// Launchers that only start the real command. They are peeled off so a rule
/// for `vitest run` also matches `bun --bun vitest run` and `bunx vitest run`.
pub fn effective(argv: &[String]) -> Vec<String> {
    let mut v: Vec<String> = argv.to_vec();
    for _ in 0..8 {
        let before = v.clone();
        v = strip_once(v);
        if v == before {
            break;
        }
    }
    v
}

fn is_script(s: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".tsx", ".jsx"].iter().any(|e| s.ends_with(e))
}

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn strip_once(mut v: Vec<String>) -> Vec<String> {
    if v.is_empty() {
        return v;
    }
    // Leading VAR=value pairs.
    if v[0].contains('=') && !v[0].starts_with('-') && !v[0].contains('/') {
        v.remove(0);
        return v;
    }
    let first = base(&v[0]).to_string();
    let drop = |v: &mut Vec<String>, n: usize| {
        let n = n.min(v.len());
        v.drain(..n);
    };
    match first.as_str() {
        "time" | "exec" | "command" => drop(&mut v, 1),
        "nice" => {
            let n = if v.get(1).is_some_and(|a| a == "-n") { 3 } else { 1 };
            drop(&mut v, n);
        }
        "timeout" => {
            // timeout [flags] DURATION CMD
            let mut i = 1;
            while v.get(i).is_some_and(|a| a.starts_with('-')) {
                i += 1;
            }
            drop(&mut v, i + 1);
        }
        "env" => {
            let mut i = 1;
            while v.get(i).is_some_and(|a| a.starts_with('-') || a.contains('=')) {
                i += 1;
            }
            drop(&mut v, i);
        }
        "sh" | "bash" | "zsh" if v.get(1).is_some_and(|a| a == "-c") && v.len() >= 3 => {
            v = split_shell(&v[2]);
        }
        "devenv" if v.get(1).is_some_and(|a| a == "shell") => {
            let at = v.iter().position(|a| a == "--").map(|i| i + 1).unwrap_or(2);
            drop(&mut v, at);
        }
        "npx" | "bunx" | "pnpx" => {
            let mut i = 1;
            while v.get(i).is_some_and(|a| a.starts_with('-')) {
                i += 1;
            }
            drop(&mut v, i);
        }
        "pnpm" | "yarn" if v.get(1).is_some_and(|a| a == "exec" || a == "dlx") => drop(&mut v, 2),
        "npm" if v.get(1).is_some_and(|a| a == "exec") => {
            let at = v.iter().position(|a| a == "--").map(|i| i + 1).unwrap_or(2);
            drop(&mut v, at);
        }
        "bun" => {
            match v.get(1).map(String::as_str) {
                Some("--bun") | Some("x") => drop(&mut v, 2),
                Some("run") => {
                    // bun run [--cwd DIR] [flags] TARGET
                    let mut i = 2;
                    while let Some(a) = v.get(i) {
                        if a == "--cwd" {
                            i += 2;
                        } else if a.starts_with('-') {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    drop(&mut v, i);
                }
                Some(s) if is_script(s) => drop(&mut v, 1),
                _ => {}
            }
        }
        "node" | "deno" | "tsx" | "ts-node" => {
            // Skip runtime flags, then the script becomes the command.
            let mut i = 1;
            while v.get(i).is_some_and(|a| a.starts_with('-')) {
                i += 1;
            }
            if v.get(i).is_some() {
                drop(&mut v, i);
            }
        }
        _ => {}
    }
    v
}

/// A small shell word splitter for `sh -c "..."`: quotes and backslashes only.
/// Anything after a shell operator is dropped, because only the first command
/// decides what the job is.
pub fn split_shell(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                have = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    have = true;
                }
            }
            (None, ';' | '&' | '|') => break,
            (None, c) if c.is_whitespace() => {
                if have || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            (None, c) => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Glob match for one word. `*` stops at `/`, `**` does not.
pub fn glob(pat: &str, s: &str) -> bool {
    fn rec(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        if p.starts_with(b"**") {
            let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
            return (0..=s.len()).any(|i| rec(rest, &s[i..]) || rec(&p[2..], &s[i..]));
        }
        if p[0] == b'*' {
            return (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| rec(&p[1..], &s[i..]));
        }
        !s.is_empty() && p[0] == s[0] && rec(&p[1..], &s[1..])
    }
    rec(pat.as_bytes(), s.as_bytes())
}

fn command_word_matches(pat: &str, cmd: &str) -> bool {
    if pat == "*" {
        return true;
    }
    if pat.contains('/') { glob(pat, cmd) || glob(&format!("**/{pat}"), cmd) } else { glob(pat, base(cmd)) }
}

/// Does the pattern match this command line? Both the raw line and the
/// effective line are tried, so a pattern can also name a launcher
/// (`bun --watch`).
pub fn matches(pattern: &str, argv: &[String]) -> bool {
    let words: Vec<&str> = pattern.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    let eff = effective(argv);
    matches_words(&words, &eff) || matches_words(&words, argv)
}

fn matches_words(words: &[&str], argv: &[String]) -> bool {
    let Some(cmd) = argv.first() else {
        return false;
    };
    if !command_word_matches(words[0], cmd) {
        return false;
    }
    let anchored = words.last() == Some(&"$");
    let want = if anchored { &words[1..words.len() - 1] } else { &words[1..] };
    let mut i = 1;
    for w in want {
        match argv[i..].iter().position(|a| glob(w, a)) {
            Some(p) => i += p + 1,
            None => return false,
        }
    }
    !anchored || i == argv.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        split_shell(s)
    }

    #[test]
    fn strips_launchers() {
        assert_eq!(effective(&v("bun --bun vitest run --project unit")), v("vitest run --project unit"));
        assert_eq!(effective(&v("bunx --bun vite build")), v("vite build"));
        assert_eq!(effective(&v("bun x tsc -p .")), v("tsc -p ."));
        assert_eq!(effective(&v("npx -y tsc -p .")), v("tsc -p ."));
        assert_eq!(effective(&v("pnpm exec tsc")), v("tsc"));
        assert_eq!(effective(&v("NODE_ENV=test FOO=1 vitest run")), v("vitest run"));
        assert_eq!(effective(&v("devenv shell -- bun tools/x.ts")), v("tools/x.ts"));
        assert_eq!(
            effective(&v("node --max-old-space-size=8192 node_modules/typescript/bin/tsc -p .")),
            v("node_modules/typescript/bin/tsc -p .")
        );
        assert_eq!(effective(&v("bun run --cwd web/dapp e2e:ui")), v("e2e:ui"));
        assert_eq!(effective(&v("time nice -n 5 tsc")), v("tsc"));
        assert_eq!(effective(&v("sh -c 'tsc -p . && echo done'")), v("tsc -p ."));
        assert_eq!(effective(&v("bun build src/main.ts --compile")), v("bun build src/main.ts --compile"));
    }

    #[test]
    fn globs() {
        assert!(glob("check-*.ts", "check-foo.ts"));
        assert!(!glob("*.ts", "a/b.ts"));
        assert!(glob("**/run-integration.ts", "testkit/helpers/tools/run-integration.ts"));
        assert!(glob("**/x.ts", "x.ts"));
    }

    #[test]
    fn patterns() {
        assert!(matches(
            "vitest run --project integration",
            &v("bun --bun vitest run --config c.mjs --project integration tests/integration")
        ));
        assert!(!matches("vitest run --project integration", &v("vitest run --project unit")));
        assert!(matches("tsc", &v("node node_modules/typescript/bin/tsc -p .")));
        assert!(matches("* --watch", &v("tsc -p . --watch")));
        assert!(matches("vite $", &v("bunx vite")));
        assert!(!matches("vite $", &v("bunx vite build")));
        assert!(matches("**/run-integration.ts", &v("bun testkit/helpers/tools/run-integration.ts --task x")));
        assert!(matches("bun --watch", &v("bun --watch src/main.ts")));
    }
}
