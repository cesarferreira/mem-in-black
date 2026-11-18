use crate::{
    store::{RecentActivity, Store},
    workspace::Workspace,
};
use anyhow::Result;
use clap::ValueEnum;
use std::{env, path::Path};

pub const TERMINAL_LIMIT: u8 = 6;

#[derive(Clone, Copy, ValueEnum)]
pub enum Color {
    Auto,
    Always,
    Never,
}

impl Color {
    pub fn enabled(self, terminal: bool) -> bool {
        match self {
            Self::Auto => terminal && color_allowed(),
            Self::Always => true,
            Self::Never => false,
        }
    }
}

pub fn startup_enabled() -> bool {
    !matches!(
        env::var("MIB_STARTUP_PREVIEW").as_deref(),
        Ok("0" | "false" | "off")
    )
}

pub fn color_allowed() -> bool {
    env::var_os("NO_COLOR").is_none() && env::var("TERM").as_deref() != Ok("dumb")
}

pub fn render(
    store: &Store,
    workspace: &Workspace,
    limit: u8,
    color: bool,
) -> Result<Option<String>> {
    let mut rows = store.recent_activity(&workspace.root, limit)?;
    if rows.is_empty() {
        return Ok(None);
    }
    rows.sort_unstable_by(|a, b| (&b.date, &b.time, b.id).cmp(&(&a.date, &a.time, a.id)));

    let name = Path::new(&workspace.root)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&workspace.root);
    let mut message = format!(
        "{} · {}\n",
        paint("Mem in Black", "1;96", color),
        display_line(name, 48)
    );
    let mut date = "";
    for row in &rows {
        if row.date != date {
            date = &row.date;
            message.push_str(&format!("{}\n", paint(date, "1;94", color)));
        }
        let (source, source_style) = match row.source.as_str() {
            "claude" => ("Claude", "35"),
            "codex" => ("Codex", "36"),
            "manual" => ("Local", "37"),
            "claude-mem" => ("Import", "37"),
            _ => ("Agent", "37"),
        };
        let (kind, kind_style) = match row.kind.as_str() {
            "prompt" => ("Task", "93"),
            "assistant" => ("Update", "92"),
            "summary" => ("Summary", "92"),
            "tool" => ("Tool", "94"),
            "note" => ("Note", "93"),
            _ => ("Observation", "94"),
        };
        message.push_str(&format!(
            "  {}  {}  {} · {}  {}\n",
            paint(&row.time, "2", color),
            paint(&format!("#{}", row.id), "2", color),
            paint(source, source_style, color),
            paint(kind, kind_style, color),
            display_line(headline(row), 78),
        ));
    }
    message.push_str(&paint("Viewer · mib status", "2", color));
    Ok(Some(message))
}

fn headline(row: &RecentActivity) -> &str {
    let mut lines = row
        .body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default();
    match row.kind.as_str() {
        "summary" if first.starts_with("Evidence ") || first.starts_with("Model summary.") => {
            let detail = lines.next().unwrap_or(first);
            lines
                .find_map(|line| line.strip_prefix("Latest agent update: "))
                .filter(|line| !line.is_empty())
                .unwrap_or(detail)
        }
        "observation" => lines
            .find_map(|line| line.strip_prefix("title: "))
            .unwrap_or(first),
        _ => first,
    }
}

fn display_line(text: &str, width: usize) -> String {
    let mut characters = text.chars().filter(|c| {
        !c.is_control() && !matches!(c, '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    });
    let mut text: String = characters.by_ref().take(width).collect();
    if characters.next().is_some() {
        text.pop();
        text.push('…');
    }
    text
}

fn paint(text: &str, style: &str, color: bool) -> String {
    if color {
        format!("\x1b[{style}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}
