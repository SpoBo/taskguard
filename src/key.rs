//! Where a job runs and what it is called: the checkout root, the namespace,
//! and the history key.

use std::path::{Path, PathBuf};

/// The nearest directory holding `.git`. `.git` is a directory in a main
/// checkout and a file in a linked worktree, so both count. Walking up to
/// `.git` is deliberate: many packages own a task runner config file, and
/// stopping at the nearest one collapsed every key to "root" in tsc-queue.
pub fn checkout_root(cwd: &Path) -> PathBuf {
    let mut d = Some(cwd);
    while let Some(dir) = d {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        d = dir.parent();
    }
    cwd.to_path_buf()
}

/// The repo name, the same for every worktree of one repository, found
/// without running git. In a linked worktree `.git` is a file that says
/// `gitdir: <main>/.git/worktrees/<name>`, so the main checkout is the parent
/// of the directory that holds `worktrees/`. Tools such as no-mistakes check
/// out worktrees of a bare repo, `gitdir: <repo>.git/worktrees/<name>`, whose
/// folder name says nothing; there the name comes from the origin URL.
pub fn namespace(cwd: &Path) -> String {
    let root = checkout_root(cwd);
    let dotgit = root.join(".git");
    let gitdir = dotgit
        .is_file()
        .then(|| std::fs::read_to_string(&dotgit).ok())
        .flatten()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("gitdir:")).map(|p| PathBuf::from(p.trim())))
        .map(|g| if g.is_absolute() { g } else { root.join(g) });
    let Some(gitdir) = gitdir else {
        return dir_name(&root);
    };
    // <main>/.git/worktrees/<name> -> <main>
    if let Some(main) = gitdir.ancestors().find(|a| a.file_name().is_some_and(|n| n == ".git")).and_then(|g| g.parent()) {
        return dir_name(main);
    }
    // <repo>.git/worktrees/<name> -> <repo>.git
    if let Some(bare) = gitdir.ancestors().find(|a| a.file_name().is_some_and(|n| n == "worktrees")).and_then(|w| w.parent()) {
        if let Some(name) = std::fs::read_to_string(bare.join("config")).ok().as_deref().and_then(origin_repo_name) {
            return name;
        }
        return dir_name(bare).trim_end_matches(".git").to_string();
    }
    dir_name(&root)
}

fn dir_name(dir: &Path) -> String {
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into())
}

/// The last part of `remote.origin.url` in a git config, without `.git`:
/// `https://github.com/settlemint/dalp.git` and
/// `git@github.com:settlemint/dalp.git` both give `dalp`.
fn origin_repo_name(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line == r#"[remote "origin"]"#;
        } else if in_origin && let Some(url) = line.strip_prefix("url").map(str::trim_start).and_then(|r| r.strip_prefix('=')) {
            let name = url.trim().trim_end_matches('/').rsplit(['/', ':']).next()?.trim_end_matches(".git");
            return (!name.is_empty()).then(|| name.to_string());
        }
    }
    None
}

/// The right name for runs that older versions filed under `ns`, or None
/// when `ns` is right. Older versions named a worktree of a bare repo after
/// the worktree folder, so only a `ns` that is a folder on `cwd` can be
/// wrong. When that worktree still exists, it is asked again. When it is
/// gone, the no-mistakes layout still says which repo it was:
/// `<base>/worktrees/<id>/<name>` is a worktree of `<base>/repos/<id>.git`.
pub fn renamed_namespace(cwd: &Path, ns: &str) -> Option<String> {
    let root = cwd.ancestors().find(|a| a.file_name().is_some_and(|n| n == ns))?;
    let right = if root.join(".git").exists() {
        namespace(root)
    } else {
        let id = root.parent()?;
        let base = id.parent().filter(|w| w.file_name().is_some_and(|n| n == "worktrees"))?.parent()?;
        let bare = base.join("repos").join(format!("{}.git", id.file_name()?.to_string_lossy()));
        let config = std::fs::read_to_string(bare.join("config")).ok()?;
        origin_repo_name(&config).unwrap_or_else(|| dir_name(&bare).trim_end_matches(".git").to_string())
    };
    (right != ns).then_some(right)
}

/// The cwd relative to the checkout root, so the same package shares one
/// history across every worktree of the repository.
pub fn project_path(cwd: &Path) -> String {
    let root = checkout_root(cwd);
    match cwd.strip_prefix(&root) {
        Ok(rel) if rel.as_os_str().is_empty() => "root".into(),
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into()),
    }
}

fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// `<project>:<command>_<args>`. The checkout root is cut out of every
/// argument, so `node /wt/a/scripts/x.js` in one worktree and
/// `node /wt/b/scripts/x.js` in another share one history. The key holds no
/// tab or newline and stays short enough for a status column.
pub fn run_key(cwd: &Path, argv: &[String]) -> String {
    let root = checkout_root(cwd);
    let root_s = root.to_string_lossy();
    let mut words: Vec<String> = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        let a = if i == 0 {
            a.rsplit('/').next().unwrap_or(a).to_string()
        } else if a.as_str() == root_s {
            ".".into()
        } else if let Some(rest) = a.strip_prefix(&format!("{root_s}/")) {
            rest.to_string()
        } else {
            a.clone()
        };
        words.push(a);
    }
    let key = format!("{}:{}", project_path(cwd), words.join("_"));
    let key: String = key.chars().map(|c| if c == '\t' || c == '\n' || c == ' ' { '_' } else { c }).collect();
    if key.chars().count() > 80 {
        let head: String = key.chars().take(64).collect();
        format!("{head}~{:08x}", fnv1a(&key))
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_shared_across_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("dalp");
        let wt = tmp.path().join("worktrees/dalp-fix");
        std::fs::create_dir_all(main.join(".git/worktrees/dalp-fix")).unwrap();
        std::fs::create_dir_all(main.join("packages/api")).unwrap();
        std::fs::create_dir_all(wt.join("packages/api")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/dalp-fix").display())).unwrap();

        assert_eq!(namespace(&main.join("packages/api")), "dalp");
        assert_eq!(namespace(&wt.join("packages/api")), "dalp");

        let a = run_key(&main.join("packages/api"), &["tsc".into(), "-p".into(), ".".into()]);
        let b = run_key(&wt.join("packages/api"), &["/x/node_modules/.bin/tsc".into(), "-p".into(), ".".into()]);
        assert_eq!(a, "packages/api:tsc_-p_.");
        assert_eq!(a, b);

        let script = format!("{}/scripts/x.js", wt.display());
        assert_eq!(run_key(&wt, &["node".into(), script]), "root:node_scripts/x.js");
    }

    #[test]
    fn bare_repo_worktrees_are_named_after_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("repos/8bebdfba2234.git");
        let wt = tmp.path().join("worktrees/8bebdfba2234/01M45SG2APYH6K2QC1662S99PD");
        std::fs::create_dir_all(bare.join("worktrees/01M45SG2APYH6K2QC1662S99PD")).unwrap();
        std::fs::create_dir_all(wt.join("packages/api")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", bare.join("worktrees/01M45SG2APYH6K2QC1662S99PD").display())).unwrap();

        // No origin: the bare folder, not the worktree's random name.
        std::fs::write(bare.join("config"), "[core]\n\tbare = true\n").unwrap();
        assert_eq!(namespace(&wt.join("packages/api")), "8bebdfba2234");

        std::fs::write(
            bare.join("config"),
            "[core]\n\tbare = true\n[remote \"upstream\"]\n\turl = https://github.com/x/other.git\n[remote \"origin\"]\n\turl = https://github.com/settlemint/dalp.git\n",
        )
        .unwrap();
        assert_eq!(namespace(&wt.join("packages/api")), "dalp");
        assert_eq!(project_path(&wt.join("packages/api")), "packages/api");
    }

    #[test]
    fn old_bare_repo_names_are_renamed() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(".no-mistakes");
        let bare = base.join("repos/8bebdfba2234.git");
        std::fs::create_dir_all(bare.join("worktrees/LIVE")).unwrap();
        std::fs::write(bare.join("config"), "[remote \"origin\"]\n\turl = git@github.com:settlemint/dalp.git\n").unwrap();
        let live = base.join("worktrees/8bebdfba2234/LIVE");
        std::fs::create_dir_all(live.join("packages/api")).unwrap();
        std::fs::write(live.join(".git"), format!("gitdir: {}\n", bare.join("worktrees/LIVE").display())).unwrap();
        let gone = base.join("worktrees/8bebdfba2234/GONE/packages/api");

        assert_eq!(renamed_namespace(&live.join("packages/api"), "LIVE").as_deref(), Some("dalp"));
        assert_eq!(renamed_namespace(&gone, "GONE").as_deref(), Some("dalp"));
        // Already right, or not a folder on the path: left alone.
        assert_eq!(renamed_namespace(&live.join("packages/api"), "dalp"), None);
        assert_eq!(renamed_namespace(&gone, "shop"), None);

        // A normal checkout keeps its name, also once it is gone.
        let main = tmp.path().join("dalp");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        assert_eq!(renamed_namespace(&main.join("packages/api"), "dalp"), None);
        assert_eq!(renamed_namespace(&tmp.path().join("ws/dalp/fix/packages/api"), "dalp"), None);
    }

    #[test]
    fn origin_url_forms() {
        let cfg = |u: &str| format!("[remote \"origin\"]\n\turl = {u}\n");
        assert_eq!(origin_repo_name(&cfg("git@github.com:settlemint/dalp.git")).as_deref(), Some("dalp"));
        assert_eq!(origin_repo_name(&cfg("https://github.com/settlemint/dalp")).as_deref(), Some("dalp"));
        assert_eq!(origin_repo_name(&cfg("/srv/git/dalp.git/")).as_deref(), Some("dalp"));
        assert_eq!(origin_repo_name("[core]\n\tbare = true\n"), None);
    }

    #[test]
    fn long_keys_are_hashed() {
        let args: Vec<String> = std::iter::once("vitest".to_string()).chain((0..40).map(|i| format!("--flag{i}"))).collect();
        let k = run_key(Path::new("/nonexistent/pkg"), &args);
        assert!(k.chars().count() <= 64 + 9);
        assert!(k.contains('~'));
        // Pinned: every version must give a job the same key, or they stop
        // sharing what they learned about it.
        assert_eq!(k, "root:vitest_--flag0_--flag1_--flag2_--flag3_--flag4_--flag5_--fl~ae87be9a");
        assert_ne!(k, run_key(Path::new("/nonexistent/pkg"), &args[..39]));
    }
}
