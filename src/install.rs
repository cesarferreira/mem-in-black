use crate::{hooks::Host, store::prepare_dir};
use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use serde_json::{Value, json};
use std::{
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use toml_edit::{Array, DocumentMut, value};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Target {
    Claude,
    Codex,
    Pi,
}

pub fn install(
    data: Option<&Path>,
    home_override: Option<&Path>,
    selected: Option<Target>,
) -> Result<()> {
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .context("no user home available")?;
    let config_dir = |variable, name| {
        home_override
            .is_none()
            .then(|| std::env::var_os(variable))
            .flatten()
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(name))
    };
    let claude_config = config_dir("CLAUDE_CONFIG_DIR", ".claude");
    let codex_config = config_dir("CODEX_HOME", ".codex");
    let exe = std::env::current_exe()?;
    let exe = exe.to_str().context("binary path is not UTF-8")?;
    let data = data
        .map(|path| path.to_str().context("data path is not UTF-8"))
        .transpose()?;
    if matches!(selected, Some(Target::Pi)) {
        let pi = config_dir("PI_CODING_AGENT_DIR", ".pi/agent");
        return install_pi(&pi, exe, data);
    }
    let mut changes = Vec::new();
    if !matches!(selected, Some(Target::Codex)) {
        let path = claude_config.join("settings.json");
        changes.push(hooks_file(path, exe, data, Host::Claude)?);
        let path = if claude_config == home.join(".claude") {
            home.join(".claude.json")
        } else {
            claude_config.join(".claude.json")
        };
        let mut doc = read_json(&path)?;
        let servers = doc
            .as_object_mut()
            .context("Claude MCP config must be an object")?
            .entry("mcpServers")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("mcpServers must be an object")?;
        servers.insert(
            "quietmem".into(),
            json!({"type":"stdio","command":exe,"args":mcp_args(data)}),
        );
        changes.push((path, serde_json::to_string_pretty(&doc)? + "\n"));
    }
    if !matches!(selected, Some(Target::Claude)) {
        changes.push(hooks_file(
            codex_config.join("hooks.json"),
            exe,
            data,
            Host::Codex,
        )?);
        let path = codex_config.join("config.toml");
        let mut doc = read(&path)?.unwrap_or_default().parse::<DocumentMut>()?;
        if let Some(existing) = doc.get("mcp_servers") {
            ensure!(existing.is_table_like(), "mcp_servers must be a TOML table");
        }
        doc["mcp_servers"]["quietmem"]["command"] = value(exe);
        let mut args = Array::new();
        for part in mcp_args(data) {
            args.push(part);
        }
        doc["mcp_servers"]["quietmem"]["args"] = value(args);
        changes.push((path, doc.to_string()));
    }
    for (path, contents) in changes {
        if read(&path)?.as_deref() != Some(&contents) {
            atomic_write(&path, contents.as_bytes())?;
        }
    }
    Ok(())
}

fn mcp_args(data: Option<&str>) -> Vec<&str> {
    match data {
        Some(dir) => vec!["--data-dir", dir, "mcp"],
        None => vec!["mcp"],
    }
}

fn install_pi(config_dir: &Path, binary: &str, data: Option<&str>) -> Result<()> {
    const MARKER: &str = "// Installed by QuietMem\n";
    let path = config_dir.join("extensions/quietmem-startup.js");
    let config = json!({"binary": binary, "dataDirectory": data});
    let extension = format!(
        "{MARKER}const quietmem = {config};\n{}",
        include_str!("pi-preview.js")
    );
    if let Some(existing) = read(&path)? {
        if existing == extension {
            return Ok(());
        }
        ensure!(
            existing.starts_with(MARKER),
            "another Pi extension exists at {}",
            path.display()
        );
    }
    atomic_write(&path, extension.as_bytes())
}

fn hooks_file(
    path: PathBuf,
    exe: &str,
    data: Option<&str>,
    host: Host,
) -> Result<(PathBuf, String)> {
    let mut doc = read_json(&path)?;
    let hooks = doc
        .as_object_mut()
        .context("hook config must be an object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks must be an object")?;
    let command = format!(
        "{}{} hook {} # quietmem-managed",
        quote(exe),
        data.map(|dir| format!(" --data-dir {}", quote(dir)))
            .unwrap_or_default(),
        host.name()
    );
    let mut events = vec![
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "PreToolUse",
        "PostToolUse",
    ];
    if matches!(host, Host::Claude) {
        events.push("PostToolUseFailure");
    }
    for event in events {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("invalid {event} hook groups"))?;
        let existing = groups
            .iter_mut()
            .filter_map(|group| group.get_mut("hooks")?.as_array_mut())
            .flatten()
            .find(|hook| {
                hook["command"]
                    .as_str()
                    .is_some_and(|command| command.ends_with(" # quietmem-managed"))
            });
        if let Some(existing) = existing {
            existing["command"] = json!(command);
        } else {
            groups.push(json!({"hooks":[{"type":"command","command":command,"timeout":2}]}));
        }
    }
    Ok((path, serde_json::to_string_pretty(&doc)? + "\n"))
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn read_json(path: &Path) -> Result<Value> {
    match read(path)? {
        Some(contents) => {
            serde_json::from_str(&contents).with_context(|| format!("parsing {}", path.display()))
        }
        None => Ok(json!({})),
    }
}

pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let path = if path.is_symlink() {
        path.canonicalize()?
    } else {
        path.to_owned()
    };
    let parent = path.parent().context("file has no parent directory")?;
    prepare_dir(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    if let Ok(existing) = fs::metadata(&path) {
        temp.as_file().set_permissions(fs::Permissions::from_mode(
            existing.permissions().mode() & 0o777,
        ))?;
    }
    temp.write_all(content)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}
