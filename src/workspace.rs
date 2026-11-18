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
}

impl Workspace {
    pub fn read(cwd: &Path) -> Self {
        let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&canonical)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
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
        Self {
            root,
            project,
            head,
        }
    }
}
