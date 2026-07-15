//! Best-effort desktop notifications (cross-platform via notify-rust).

/// Fire a desktop notification. Never fails the caller — logs and moves on.
pub fn send(body: &str) {
    if let Err(e) = notify_rust::Notification::new()
        .summary("AI Nudge")
        .body(body)
        .show()
    {
        tracing::warn!("nudge: notification failed: {e}");
    }
}
