//! Best-effort desktop notifications (cross-platform via notify-rust).

/// Fire a desktop notification. Never fails the caller — logs and moves on.
pub fn send(body: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary("nudge")
        .body(&escaped(body, reads_markup()))
        .show()
    {
        tracing::warn!("nudge: notification failed: {e}");
    }
}

/// Whether the notification server reads markup in a body: on Linux, whether
/// its capabilities say `body-markup`. Nearly every Linux server does, so one
/// that cannot be asked (it may be starting) is taken to. macOS shows the text
/// as it is.
fn reads_markup() -> bool {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        notify_rust::get_capabilities().map_or(true, |caps| caps.iter().any(|c| c == "body-markup"))
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        false
    }
}

/// `body`, escaped for a server that reads markup in it (GNOME's, dunst,
/// mako), which takes `&` and `<` as markup, so a session named `R&D`
/// garbled or blanked the notification. A server that does not read markup
/// would show the escapes, so it gets the text as it is.
fn escaped(body: &str, markup: bool) -> std::borrow::Cow<'_, str> {
    if !markup || !body.contains(['&', '<', '>']) {
        return body.into();
    }
    body.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .into()
}

#[cfg(test)]
mod tests {
    use super::escaped;

    #[test]
    fn markup_in_a_body_is_escaped_for_a_server_that_reads_it() {
        let body = "nudge fired for R&D <fix> · main:5.0 (%13)";
        assert_eq!(
            escaped(body, true),
            "nudge fired for R&amp;D &lt;fix&gt; · main:5.0 (%13)"
        );
        assert_eq!(escaped(body, false), body);
    }
}
