//! Human-readable usage figures for the CLI and the viewer.

use crate::store::Stats;

pub struct Line {
    pub label: &'static str,
    pub value: String,
    pub unit: &'static str,
    pub detail: String,
}

pub fn lines(stats: &Stats) -> Vec<Line> {
    let tokens = |bytes: i64| compact(bytes / 4);
    vec![
        Line {
            label: "Memory",
            value: compact(stats.memories),
            unit: plural(stats.memories, "memory", "memories"),
            detail: format!(
                "{} total activity {} across {} {}",
                compact(stats.records),
                plural(stats.records, "record", "records"),
                stats.workspaces,
                plural(stats.workspaces, "workspace", "workspaces"),
            ),
        },
        Line {
            label: "Recall",
            value: compact(stats.sessions_helped),
            unit: plural(
                stats.sessions_helped,
                "session with automatic recall",
                "sessions with automatic recall",
            ),
            detail: format!(
                "{} injections, {} records of evidence, ≈{} tokens injected",
                compact(stats.context_injections),
                compact(stats.evidence_delivered),
                tokens(stats.context_bytes)
            ),
        },
        Line {
            label: "MCP lookups",
            value: compact(stats.mcp_searches),
            unit: plural(stats.mcp_searches, "search", "searches"),
            detail: format!(
                "{} {} fetched by agents, ≈{} tokens returned",
                compact(stats.mcp_records),
                plural(stats.mcp_records, "record", "records"),
                tokens(stats.mcp_bytes)
            ),
        },
        Line {
            label: "Model cost",
            value: compact(stats.observer_tokens),
            unit: "tokens",
            detail: if stats.observer_tokens == 0 {
                "Summaries are built locally".to_owned()
            } else {
                "Reported by the opt-in summary model".to_owned()
            },
        },
        Line {
            label: "Saved (est.)",
            value: format!("≈{}", compact(stats.estimated_tokens_saved())),
            unit: "tokens",
            detail: format!(
                "Upper bound: ≈{} tokens of original output delivered, minus what was injected",
                tokens(stats.evidence_source_bytes + stats.mcp_source_bytes)
            ),
        },
    ]
}

pub fn text(stats: &Stats) -> String {
    let scope = stats.workspace.as_deref().unwrap_or("all workspaces");
    let mut out = format!("Mem in Black stats · {scope}\n");
    for line in lines(stats) {
        let value = format!("{} {}", line.value, line.unit);
        out.push_str(&format!("{:<13} {value:<22} {}\n", line.label, line.detail));
    }
    out.push_str(
        "\nTokens are estimated at 4 bytes each. Savings assume each delivered record \
         replaced re-reading its original output; real savings are lower.",
    );
    out
}

fn plural<'a>(count: i64, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

pub fn compact(value: i64) -> String {
    match value.abs() {
        0..1_000 => value.to_string(),
        1_000..1_000_000 => trim(format!("{:.1}k", value as f64 / 1_000.0)),
        _ => trim(format!("{:.1}M", value as f64 / 1_000_000.0)),
    }
}

fn trim(text: String) -> String {
    text.replace(".0k", "k").replace(".0M", "M")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_numbers_read_naturally() {
        assert_eq!(compact(950), "950");
        assert_eq!(compact(1_000), "1k");
        assert_eq!(compact(38_420), "38.4k");
        assert_eq!(compact(2_500_000), "2.5M");
    }
}
