use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub root: String,
    pub project: String,
    pub head: Option<String>,
    /// Full branch ref, or the commit SHA when HEAD is detached.
    pub git_ref: Option<String>,
}

impl Workspace {
    pub fn read(cwd: &Path) -> Self {
        let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let run_git = |args: &[&str]| {
            Command::new("git")
                .current_dir(&canonical)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
        };
        let git = |args: &[&str]| {
            let output = run_git(args)?;
            if !output.status.success() {
                return None;
            }
            String::from_utf8(output.stdout)
                .ok()
                .map(|value| value.trim_end().to_owned())
        };
        let paths = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ]);
        let mut paths = paths.as_deref().unwrap_or_default().lines();
        let root = paths
            .next()
            .map(str::to_owned)
            .unwrap_or_else(|| canonical.to_string_lossy().into_owned());
        let project = paths.next().unwrap_or(&root).to_owned();
        let head = git(&["rev-parse", "--verify", "HEAD"]);
        let git_ref =
            run_git(&["symbolic-ref", "--quiet", "HEAD"]).and_then(|mut output| {
                match output.status.code() {
                    Some(0) => {
                        if output.stdout.last() == Some(&b'\n') {
                            output.stdout.pop();
                        }
                        // Git refs can contain non-UTF-8 bytes. Preserve an exact
                        // identity in SQLite even when the name cannot be decoded.
                        Some(String::from_utf8(output.stdout).unwrap_or_else(|error| {
                            format!("non-UTF-8 ref {:02x?}", error.as_bytes())
                        }))
                    }
                    Some(1) => head.clone(),
                    _ => None,
                }
            });
        Self {
            root,
            project,
            head,
            git_ref,
        }
    }
}
