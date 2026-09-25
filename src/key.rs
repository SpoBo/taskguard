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
/// of the directory that holds `worktrees/`.
pub fn namespace(cwd: &Path) -> String {
    let root = checkout_root(cwd);
    let dotgit = root.join(".git");
    let main = if dotgit.is_file() {
        std::fs::read_to_string(&dotgit)
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("gitdir:")).map(|p| PathBuf::from(p.trim())))
            .map(|gitdir| {
                let gitdir = if gitdir.is_absolute() { gitdir } else { root.join(gitdir) };
                // <main>/.git/worktrees/<name> -> <main>
                gitdir
                    .ancestors()
                    .find(|a| a.file_name().is_some_and(|n| n == ".git"))
                    .and_then(|g| g.parent())
                    .map(Path::to_path_buf)
                    .unwrap_or(root.clone())
            })
            .unwrap_or(root.clone())
    } else {
        root.clone()
    };
    main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into())
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
    fn long_keys_are_hashed() {
        let args: Vec<String> = std::iter::once("vitest".to_string()).chain((0..40).map(|i| format!("--flag{i}"))).collect();
        let k = run_key(Path::new("/nonexistent/pkg"), &args);
        assert!(k.chars().count() <= 64 + 9);
        assert!(k.contains('~'));
        assert_ne!(k, run_key(Path::new("/nonexistent/pkg"), &args[..39]));
    }
}
