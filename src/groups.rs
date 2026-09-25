//! Groups of well-known programs outside taskguard: agents, browsers,
//! editors, and so on. The Overview splits the load that taskguard did not
//! start into these groups, so it is clear what could be closed to make room.

use std::collections::HashMap;

pub struct Group {
    pub name: &'static str,
    /// Child processes count toward this group too, so an MCP server or a
    /// test run that an agent started counts as "agents".
    pub inherit: bool,
    /// Parts of the program's name or path, compared in lower case. A
    /// pattern that starts with "/" is only looked for in the path.
    pub patterns: &'static [&'static str],
}

pub const GROUPS: [Group; 7] = [
    Group {
        name: "agents",
        inherit: true,
        patterns: &[
            "/claude/versions/",
            "/bin/claude",
            "claude.app",
            "/codex",
            "codex.app",
            "cursor-agent",
            "opencode",
            "/bin/gemini",
            "/bin/aider",
            "/bin/goose",
            "/bin/crush",
            "/bin/droid",
            "/bin/amp",
            "/bin/qwen",
        ],
    },
    Group {
        name: "browsers",
        inherit: true,
        patterns: &[
            "google chrome",
            "chromium",
            "safari.app",
            "com.apple.webkit.webcontent",
            "firefox",
            "arc.app",
            "brave browser",
            "microsoft edge",
            "vivaldi",
            "opera.app",
            "zen.app",
            "dia.app",
            "comet.app",
        ],
    },
    Group {
        name: "editors",
        inherit: true,
        patterns: &[
            "visual studio code",
            "code helper",
            "cursor.app",
            "windsurf",
            "zed.app",
            "/zed",
            "intellij",
            "webstorm",
            "pycharm",
            "goland",
            "rustrover",
            "xcode.app",
            "sublime text",
            "nova.app",
            "orca.app",
        ],
    },
    Group {
        name: "dev services",
        inherit: true,
        patterns: &[
            "/postgres",
            "pgbouncer",
            "redis-server",
            "valkey-server",
            "mysqld",
            "mariadbd",
            "mongod",
            "/anvil",
            "hardhat",
            "ganache",
            "clickhouse",
            "minio",
            "temporal",
            "kafka",
            "elasticsearch",
            "opensearch",
        ],
    },
    Group {
        name: "containers",
        inherit: true,
        patterns: &[
            "orbstack",
            "com.docker",
            "docker.app",
            "/dockerd",
            "containerd",
            "qemu-system",
            "colima",
            "limactl",
            "podman",
            "virtualization.virtualmachine",
        ],
    },
    Group {
        name: "chat & apps",
        inherit: true,
        patterns: &[
            "slack",
            "discord",
            "microsoft teams",
            "zoom.us",
            "spotify",
            "notion",
            "figma",
            "linear.app",
            "obsidian",
            "granola",
            "whatsapp",
            "telegram",
            "signal.app",
        ],
    },
    // Only the terminal itself: what runs inside it is counted on its own.
    Group {
        name: "terminals",
        inherit: false,
        patterns: &["iterm", "ghostty", "terminal.app", "wezterm", "kitty", "alacritty", "warp.app"],
    },
];

/// The group a program belongs to by its own name or path.
pub fn own_group(name: &str, path: &str) -> Option<&'static Group> {
    let (name, path) = (name.to_lowercase(), path.to_lowercase());
    GROUPS.iter().find(|g| g.patterns.iter().any(|p| path.contains(p) || (!p.starts_with('/') && name.contains(p))))
}

/// The group of every process: its own, or else that of the nearest
/// ancestor whose group is inherited.
pub fn assign(parents: &HashMap<i32, i32>, own: &HashMap<i32, Option<&'static Group>>) -> HashMap<i32, &'static str> {
    let mut out = HashMap::new();
    for &pid in own.keys() {
        let mut cur = pid;
        let mut depth = 0;
        let found = loop {
            match own.get(&cur).copied().flatten() {
                Some(g) if cur == pid || g.inherit => break Some(g.name),
                _ => {}
            }
            match parents.get(&cur) {
                Some(&p) if p > 1 && p != cur && depth < 64 => {
                    cur = p;
                    depth += 1;
                }
                _ => break None,
            }
        };
        if let Some(g) = found {
            out.insert(pid, g);
        }
    }
    out
}

/// A short name for a program, to show in the legend: the app for anything
/// inside an app bundle, and no version numbers.
pub fn display_name(name: &str, path: &str) -> String {
    if let Some(i) = path.find(".app/") {
        let app = &path[..i];
        return app.rsplit('/').next().unwrap_or(name).to_string();
    }
    let versionish = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.');
    if versionish(name) {
        // ~/.local/share/claude/versions/2.1.282 -> claude
        if let Some(part) = path.rsplit('/').find(|p| !versionish(p) && *p != "versions" && !p.is_empty()) {
            return part.to_string();
        }
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_grouped_by_name_or_path() {
        let g = |name: &str, path: &str| own_group(name, path).map(|g| g.name);
        assert_eq!(g("2.1.282", "/Users/v/.local/share/claude/versions/2.1.282"), Some("agents"));
        assert_eq!(g("codex", "/opt/homebrew/bin/codex"), Some("agents"));
        assert_eq!(g("Google Chrome Helper (Rend", "/Applications/Google Chrome.app/Contents/Frameworks/x"), Some("browsers"));
        assert_eq!(g("postgres", "/nix/store/abc-postgresql-18.6/bin/postgres"), Some("dev services"));
        assert_eq!(g("Code Helper (Plugin)", "/Applications/Visual Studio Code.app/Contents/x"), Some("editors"));
        assert_eq!(g("OrbStack Helper", "/Applications/OrbStack.app/Contents/MacOS/OrbStack Helper"), Some("containers"));
        assert_eq!(g("zsh", "/bin/zsh"), None);
        assert_eq!(g("bun", "/nix/store/x-bun-1.4.2/bin/bun"), None);
        assert_eq!(g("timestamp-helper", "/usr/libexec/timestamp-helper"), None, "\"/bin/amp\" is no part of a name");
        assert_eq!(g("operationd", "/usr/libexec/operationd"), None);
    }

    #[test]
    fn legend_names_are_short() {
        assert_eq!(display_name("2.1.282", "/Users/v/.local/share/claude/versions/2.1.282"), "claude");
        assert_eq!(
            display_name("Google Chrome Helper (Rend", "/Applications/Google Chrome.app/Contents/Frameworks/H.app/x"),
            "Google Chrome"
        );
        assert_eq!(display_name("postgres", "/nix/store/a-postgresql/bin/postgres"), "postgres");
    }

    #[test]
    fn children_count_toward_the_group_that_started_them() {
        // terminal 10 -> zsh 11 -> claude 12 -> fff-mcp 13, bun 14 -> bun 15; chrome 20 -> helper 21
        let parents: HashMap<i32, i32> = [(11, 10), (12, 11), (13, 12), (14, 12), (15, 14), (21, 20), (10, 1), (20, 1)].into();
        let find = |n: &str| GROUPS.iter().find(|g| g.name == n);
        let own: HashMap<i32, Option<&'static Group>> = [
            (10, find("terminals")),
            (11, None),
            (12, find("agents")),
            (13, None),
            (14, None),
            (15, None),
            (20, find("browsers")),
            (21, None),
        ]
        .into();
        let got = assign(&parents, &own);
        assert_eq!(got.get(&15), Some(&"agents"), "a test run an agent started counts as the agent's");
        assert_eq!(got.get(&13), Some(&"agents"));
        assert_eq!(got.get(&21), Some(&"browsers"));
        assert_eq!(got.get(&10), Some(&"terminals"));
        assert_eq!(got.get(&11), None, "a shell in a terminal is not the terminal's");
    }
}
