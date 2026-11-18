use crate::{hooks::Host, store::prepare_dir};
use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstRootNode},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use toml_edit::{Array, DocumentMut, value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Target {
    Claude,
    Codex,
    #[value(name = "opencode")]
    OpenCode,
    Pi,
    Omp,
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
    let mut changes = Vec::new();
    let selected_or_detected =
        |target, detected| selected == Some(target) || (selected.is_none() && detected);

    if selected_or_detected(Target::Claude, true) {
        changes.push(hooks_file(
            claude_config.join("settings.json"),
            exe,
            data,
            Host::Claude,
        )?);
        let path = if claude_config == home.join(".claude") {
            home.join(".claude.json")
        } else {
            claude_config.join(".claude.json")
        };
        changes.push(stdio_mcp_file(path, exe, data)?);
    }
    if selected_or_detected(Target::Codex, true) {
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
    let opencode = opencode_path(&home, home_override.is_none());
    if selected_or_detected(
        Target::OpenCode,
        opencode.parent().is_some_and(Path::is_dir),
    ) {
        changes.push(opencode_mcp_file(opencode, exe, data)?);
    }

    let pi = match home_override
        .is_none()
        .then(|| std::env::var_os("PI_CODING_AGENT_DIR"))
        .flatten()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        Some(path) => match path.strip_prefix("~") {
            Ok(relative) => home.join(relative),
            Err(_) => path,
        },
        None => home.join(".pi/agent"),
    };
    if selected_or_detected(Target::Pi, pi.is_dir()) {
        changes.push(stdio_mcp_file(pi.join("mcp.json"), exe, data)?);
        if selected == Some(Target::Pi) {
            changes.push(pi_extension_file(&pi, exe, data)?);
        }
    }

    if selected_or_detected(Target::Omp, true) {
        let omp = omp_agent_dir(&home, home_override.is_none())?;
        if selected_or_detected(Target::Omp, omp.is_dir()) {
            changes.push(stdio_mcp_file(omp.join("mcp.json"), exe, data)?);
        }
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

fn stdio_mcp_file(path: PathBuf, exe: &str, data: Option<&str>) -> Result<(PathBuf, String)> {
    let mut doc = read_json(&path)?;
    let servers = doc
        .as_object_mut()
        .context("MCP config must be an object")?
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("mcpServers must be an object")?;
    let server = servers
        .entry("quietmem")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("QuietMem MCP entry must be an object in {}", path.display()))?;
    ensure!(
        !server.contains_key("url"),
        "QuietMem MCP entry already points to a URL in {}",
        path.display()
    );
    server.insert("type".into(), json!("stdio"));
    server.insert("command".into(), json!(exe));
    server.insert("args".into(), json!(mcp_args(data)));
    Ok((path, serde_json::to_string_pretty(&doc)? + "\n"))
}

fn opencode_path(home: &Path, read_environment: bool) -> PathBuf {
    if read_environment && let Some(path) = std::env::var_os("OPENCODE_CONFIG") {
        return PathBuf::from(path);
    }
    let config_home = read_environment
        .then(|| std::env::var_os("XDG_CONFIG_HOME"))
        .flatten()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let directory = config_home.join("opencode");
    let json = directory.join("opencode.json");
    let jsonc = directory.join("opencode.jsonc");
    if !json.is_file() && jsonc.is_file() {
        jsonc
    } else {
        json
    }
}

fn omp_agent_dir(home: &Path, read_environment: bool) -> Result<PathBuf> {
    if !read_environment {
        return Ok(home.join(".omp/agent"));
    }
    let config = std::env::var_os("PI_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| ".omp".into());
    let root = home.join(config.strip_prefix("/").unwrap_or(&config));
    let profile = std::env::var_os("OMP_PROFILE")
        .or_else(|| std::env::var_os("PI_PROFILE"))
        .unwrap_or_default();
    let profile = profile.to_str().context("OMP profile is not UTF-8")?.trim();
    if profile.is_empty() || profile == "default" {
        return Ok(std::env::var_os("PI_CODING_AGENT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("agent")));
    }
    let mut bytes = profile.bytes();
    ensure!(
        profile.len() <= 64
            && bytes.next().is_some_and(is_lowercase_alphanumeric)
            && bytes.all(|byte| is_lowercase_alphanumeric(byte) || b"._-".contains(&byte))
            && !profile.ends_with('.'),
        "invalid OMP profile name {profile:?}"
    );
    Ok(root.join("profiles").join(profile).join("agent"))
}

fn is_lowercase_alphanumeric(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn opencode_mcp_file(path: PathBuf, exe: &str, data: Option<&str>) -> Result<(PathBuf, String)> {
    let source = read(&path)?.unwrap_or_else(|| "{}\n".into());
    let doc = CstRootNode::parse(&source, &ParseOptions::default())
        .with_context(|| format!("parsing {}", path.display()))?;
    let root = doc
        .object_value()
        .context("OpenCode config must be an object")?;
    let mcp = root
        .object_value_or_create("mcp")
        .context("OpenCode mcp must be an object")?;
    let legacy = mcp.get("servers").is_none()
        && mcp.properties().iter().any(|property| {
            property
                .object_value()
                .is_some_and(|server| server.get("type").is_some())
        });
    let servers = if legacy {
        mcp
    } else {
        mcp.object_value_or_create("servers")
            .context("OpenCode mcp.servers must be an object")?
    };
    let server = servers
        .object_value_or_create("quietmem")
        .with_context(|| format!("QuietMem MCP entry must be an object in {}", path.display()))?;
    ensure!(
        server.get("url").is_none(),
        "QuietMem MCP entry already points to a URL in {}",
        path.display()
    );
    let command: Vec<_> = std::iter::once(exe).chain(mcp_args(data)).collect();
    for (key, value) in [
        ("type", CstInputValue::from("local")),
        ("command", command.into()),
    ] {
        if let Some(property) = server.get(key) {
            property.set_value(value);
        } else {
            server.append(key, value);
        }
    }
    Ok((path, doc.to_string()))
}

fn pi_extension_file(
    config_dir: &Path,
    binary: &str,
    data: Option<&str>,
) -> Result<(PathBuf, String)> {
    const MARKER: &str = "// Installed by QuietMem\n";
    let path = config_dir.join("extensions/quietmem-startup.js");
    let config = json!({"binary": binary, "dataDirectory": data});
    let extension = format!(
        "{MARKER}const quietmem = {config};\n{}",
        include_str!("pi-preview.js")
    );
    if let Some(existing) = read(&path)? {
        ensure!(
            existing.starts_with(MARKER),
            "another Pi extension exists at {}",
            path.display()
        );
    }
    Ok((path, extension))
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
