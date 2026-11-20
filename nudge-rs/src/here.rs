//! Where nudge is running, and what a `-p` names.

use std::path::PathBuf;

use anyhow::bail;

/// Where nudge is running, as its environment says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Here {
    /// A tmux pane, with tmux's id for it (`$TMUX_PANE`).
    TmuxPane { pane: String },
    /// A tmux popup (`display-popup`).
    TmuxPopup,
    /// A herdr popup, over the pane `pane`, on the herdr server at `socket`.
    HerdrPopup {
        pane: String,
        socket: Option<PathBuf>,
    },
    /// A herdr pane split beside others, on the herdr server at `socket`.
    HerdrSplit { socket: Option<PathBuf> },
    /// Neither tmux nor herdr.
    Outside,
}

/// Where nudge is running, given a way to read its environment.
pub fn here(var: impl Fn(&str) -> Option<String>) -> Here {
    // An empty value is no value. `TMUX= command` is the usual way to run
    // something as if outside tmux, and it leaves `TMUX` set but empty.
    let set = |name: &str| var(name).filter(|v| !v.is_empty());
    // tmux before herdr. A tmux started in a herdr pane hands that pane's
    // `HERDR_*` on to its own panes, so a pane of that tmux has both; herdr
    // removes `TMUX` and `TMUX_PANE` from its panes, so a herdr pane inside
    // tmux has only its own (both seen 2026-11-19).
    if let Some(pane) = set("TMUX_PANE") {
        return Here::TmuxPane { pane };
    }
    // A tmux popup has `TMUX` but no `TMUX_PANE`: it is not a pane.
    if set("TMUX").is_some() {
        return Here::TmuxPopup;
    }
    let socket = set("HERDR_SOCKET_PATH").map(PathBuf::from);
    // A herdr popup has `HERDR_ACTIVE_PANE_ID`, naming the pane under it, and
    // no `HERDR_PANE_ID`. A split has only `HERDR_PANE_ID`, its own id.
    if let Some(pane) = set("HERDR_ACTIVE_PANE_ID") {
        return Here::HerdrPopup { pane, socket };
    }
    if set("HERDR_PANE_ID").is_some() {
        return Here::HerdrSplit { socket };
    }
    Here::Outside
}

impl Here {
    /// Whether nudge is in a popup, which closes as soon as nudge exits and
    /// takes whatever nudge printed with it.
    pub fn is_popup(&self) -> bool {
        matches!(self, Here::TmuxPopup | Here::HerdrPopup { .. })
    }

    /// Where this process is running.
    pub fn from_env() -> Here {
        here(|name| std::env::var(name).ok())
    }
}

/// The pane a `-p` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneArg {
    /// A tmux target, as tmux reads one: `main:1.0`, `%3`.
    Tmux(String),
    /// A herdr pane by herdr's id for it (`w7:pX`), in the named herdr
    /// session, or with none named, in the one nudge runs under or herdr's
    /// default.
    Herdr {
        session: Option<String>,
        pane: String,
    },
}

/// What `-p` names.
///
/// A herdr pane needs the `herdr:` prefix, since `w7:pX` is also a tmux
/// target (session `w7`, window `pX`). After it, a herdr id has exactly one
/// colon, so one more colon can only mean a session's name in front of it.
/// The prefix is taken whatever follows it: a tmux session that happens to be
/// called `herdr` is still reachable by its pane's id (`%3`).
pub fn parse_pane_arg(s: &str) -> anyhow::Result<PaneArg> {
    let Some(rest) = s.strip_prefix("herdr:") else {
        return Ok(PaneArg::Tmux(s.to_string()));
    };
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.iter().any(|p| p.is_empty()) {
        bail!(not_a_herdr_pane(s));
    }
    match parts.as_slice() {
        [workspace, pane] => Ok(PaneArg::Herdr {
            session: None,
            pane: format!("{workspace}:{pane}"),
        }),
        [session, workspace, pane] => Ok(PaneArg::Herdr {
            session: Some((*session).to_string()),
            pane: format!("{workspace}:{pane}"),
        }),
        _ => bail!(not_a_herdr_pane(s)),
    }
}

/// What to say of a `-p herdr:…` that names no herdr pane.
fn not_a_herdr_pane(s: &str) -> String {
    format!(
        "-p {s} is not a herdr pane: write herdr:<workspace>:<pane> (herdr:w7:pX), \
         or herdr:<session>:<workspace>:<pane> for a pane in a named herdr session"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment holding just `vars`.
    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    /// The variables each place sets, as seen on 2026-11-19 (tmux 3.8, herdr
    /// 0.9.5).
    #[test]
    fn where_nudge_runs_follows_the_env() {
        assert_eq!(
            here(env(&[("TMUX", "s"), ("TMUX_PANE", "%3")])),
            Here::TmuxPane { pane: "%3".into() }
        );
        // tmux inside herdr: the pane inherits herdr's variables.
        assert_eq!(
            here(env(&[
                ("TMUX", "s"),
                ("TMUX_PANE", "%3"),
                ("HERDR_PANE_ID", "w1:p1")
            ])),
            Here::TmuxPane { pane: "%3".into() }
        );
        assert_eq!(
            here(env(&[("TMUX", "s"), ("HERDR_ACTIVE_PANE_ID", "w1:p1")])),
            Here::TmuxPopup
        );
        assert_eq!(
            here(env(&[
                ("HERDR_ACTIVE_PANE_ID", "w1:p1"),
                ("HERDR_SOCKET_PATH", "/s")
            ])),
            Here::HerdrPopup {
                pane: "w1:p1".into(),
                socket: Some("/s".into())
            }
        );
        assert_eq!(
            here(env(&[
                ("HERDR_PANE_ID", "w1:p1"),
                ("HERDR_SOCKET_PATH", "/s")
            ])),
            Here::HerdrSplit {
                socket: Some("/s".into())
            }
        );
        assert_eq!(here(env(&[])), Here::Outside);
    }

    #[test]
    fn a_herdr_without_its_socket_variable_is_still_herdr() {
        assert_eq!(
            here(env(&[("HERDR_ACTIVE_PANE_ID", "w1:p1")])),
            Here::HerdrPopup {
                pane: "w1:p1".into(),
                socket: None
            }
        );
        assert_eq!(
            here(env(&[("HERDR_PANE_ID", "w1:p1")])),
            Here::HerdrSplit { socket: None }
        );
    }

    /// `TMUX= tmux` leaves `TMUX` set to nothing, which says no tmux.
    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(
            here(env(&[
                ("TMUX", ""),
                ("TMUX_PANE", ""),
                ("HERDR_PANE_ID", "w1:p1")
            ])),
            Here::HerdrSplit { socket: None }
        );
        assert_eq!(
            here(env(&[("HERDR_ACTIVE_PANE_ID", ""), ("HERDR_PANE_ID", "")])),
            Here::Outside
        );
    }

    #[test]
    fn only_a_popup_is_a_popup() {
        assert!(Here::TmuxPopup.is_popup() && !Here::Outside.is_popup());
        assert!(
            Here::HerdrPopup {
                pane: "w1:p1".into(),
                socket: None
            }
            .is_popup()
        );
        assert!(!Here::TmuxPane { pane: "%3".into() }.is_popup());
        assert!(!Here::HerdrSplit { socket: None }.is_popup());
    }

    #[test]
    fn a_pane_without_the_herdr_prefix_is_tmuxs() {
        for s in ["main:1.0", "%3", "w7:pX"] {
            assert_eq!(parse_pane_arg(s).unwrap(), PaneArg::Tmux(s.into()), "{s}");
        }
    }

    #[test]
    fn a_herdr_pane_is_named_with_or_without_its_session() {
        assert_eq!(
            parse_pane_arg("herdr:w7:pX").unwrap(),
            PaneArg::Herdr {
                session: None,
                pane: "w7:pX".into()
            }
        );
        assert_eq!(
            parse_pane_arg("herdr:beaver:w1:p2").unwrap(),
            PaneArg::Herdr {
                session: Some("beaver".into()),
                pane: "w1:p2".into()
            }
        );
    }

    #[test]
    fn a_herdr_pane_with_too_few_or_too_many_parts_is_refused() {
        for s in [
            "herdr:w7",
            "herdr:a:b:c:d",
            "herdr:",
            "herdr:w7:",
            "herdr::w7:pX",
        ] {
            let e = parse_pane_arg(s).unwrap_err().to_string();
            assert!(e.contains("herdr:<workspace>:<pane>"), "{s}: {e}");
        }
    }
}
