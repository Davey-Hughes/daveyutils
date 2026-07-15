//! Enumerate tmux panes for the interactive picker.

use anyhow::{bail, Context};

/// One selectable tmux pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub target: String,
    pub title: String,
}

/// Parse `list-panes -F '<target>\t<title>'` output.
pub fn parse_list(output: &str) -> Vec<Pane> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (target, title) = l.split_once('\t').unwrap_or((l, ""));
            Pane {
                target: target.to_string(),
                title: title.to_string(),
            }
        })
        .collect()
}

/// List all tmux panes across sessions.
pub fn list() -> anyhow::Result<Vec<Pane>> {
    let out = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{window_index}.#{pane_index}\t#{pane_title}",
        ])
        .output()
        .context("running tmux list-panes")?;
    if !out.status.success() {
        bail!(
            "tmux list-panes failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_list(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tab_separated_panes() {
        let out = "bot:0.0\tclaude\nbot:0.1\tagy\nsolo:1.2\t\n";
        let panes = parse_list(out);
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].target, "bot:0.0");
        assert_eq!(panes[0].title, "claude");
        assert_eq!(panes[2].target, "solo:1.2");
        assert_eq!(panes[2].title, ""); // empty title tolerated
    }

    #[test]
    fn ignores_blank_lines() {
        assert_eq!(parse_list("\n\nbot:0.0\tx\n\n").len(), 1);
    }
}
