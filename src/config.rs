use anyhow::{Context, Result, ensure};
use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};
use toml_edit::DocumentMut;

/// Directory name for data and configuration.
pub const NAME: &str = "mem-in-black";

#[derive(Clone)]
pub struct Storage {
    pub directory: PathBuf,
    pub database: PathBuf,
    explicit_directory: bool,
}

impl Storage {
    pub fn resolve(override_dir: Option<PathBuf>) -> Result<Self> {
        let explicit_directory = override_dir.is_some();
        let directory = override_dir
            .or_else(|| env::var_os("XDG_DATA_HOME").map(|path| PathBuf::from(path).join(NAME)))
            .or_else(|| home().map(|path| path.join(".local/share").join(NAME)))
            .context("no data directory configured")?;
        let database = if explicit_directory {
            None
        } else {
            database_setting()?
        };
        Ok(Self {
            database: database.unwrap_or_else(|| directory.join("memory.sqlite3")),
            directory,
            explicit_directory,
        })
    }

    pub fn directory_override(&self) -> Option<&Path> {
        self.explicit_directory.then_some(&self.directory)
    }

    pub fn database_lock(&self, name: &str) -> PathBuf {
        if self
            .database
            .file_name()
            .is_some_and(|file| file == "memory.sqlite3")
        {
            self.database.with_file_name(name)
        } else {
            let mut path = self.database.as_os_str().to_owned();
            path.push(".");
            path.push(name);
            PathBuf::from(path)
        }
    }
}

fn home() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn database_setting() -> Result<Option<PathBuf>> {
    let Some(base) = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|path| path.join(".config")))
    else {
        return Ok(None);
    };
    let directory = base.join(NAME);
    let file = directory.join("config.toml");
    let source = match fs::read_to_string(&file) {
        Ok(source) => source,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
    };
    let parsed = source
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", file.display()))?;
    let Some(setting) = parsed.get("database_path") else {
        return Ok(None);
    };
    let path = setting.as_str().context("database_path must be a string")?;
    let path = match path.strip_prefix("~/") {
        Some(suffix) => home()
            .context("no home directory to expand database_path")?
            .join(suffix),
        None => PathBuf::from(path),
    };
    ensure!(
        path.is_absolute() && path.file_name().is_some(),
        "database_path must be an absolute database filename"
    );
    Ok(Some(path))
}
