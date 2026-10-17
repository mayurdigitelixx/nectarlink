// SPDX-License-Identifier: GPL-3.0-or-later
//! PC Notifications → Phone (§3.2 P2): forwards selected PC notifications to
//! connected phones when `forward_pc_notifications_to_phone` is enabled in
//! Settings (default `false`, privacy-first).

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use nectarlink_core::Notification;

use crate::{core_host, settings};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Builds a sanitized [`Notification`] representing a Windows PC notification.
pub fn build_pc_notification(
    key: &str,
    app_name: &str,
    title: &str,
    text: &str,
    when_ms: i64,
) -> Option<Notification> {
    let clean_app = if app_name.trim().is_empty() { "PC".to_owned() } else { app_name.trim().to_owned() };
    Notification {
        key: key.to_owned(),
        app: "pc.windows".to_owned(),
        app_name: clean_app,
        title: (!title.trim().is_empty()).then(|| title.trim().to_owned()),
        text: (!text.trim().is_empty()).then(|| text.trim().to_owned()),
        sub: None,
        when: when_ms,
        actions: Vec::new(),
        silent: false,
        icon: None,
        image: None,
        live: None,
        conversation: None,
    }
    .sanitized()
}

/// Forwards a PC notification (`app_name`, `title`, `text`) to connected
/// phones when `forward_pc_notifications_to_phone` is enabled. Returns `true`
/// if the notification was queued for delivery.
pub fn forward_to_phones(app_name: &str, title: &str, text: &str) -> bool {
    if !settings::forward_pc_notifications_to_phone() {
        return false;
    }
    let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let when_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    let key = format!("pc:win:{when_ms}:{seq}");
    let Some(notification) = build_pc_notification(&key, app_name, title, text, when_ms) else {
        return false;
    };
    if let Some(node) = core_host::node() {
        core_host::spawn(async move {
            node.notification_posted(notification).await;
        });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_to_phones_respects_privacy_toggle() {
        // Default is false (privacy-first), so forward_to_phones is a no-op.
        assert!(!settings::forward_pc_notifications_to_phone());
        assert!(!forward_to_phones("Explorer", "Download complete", "report.pdf"));

        // Verify payload construction when enabled.
        let notif = build_pc_notification(
            "pc:win:1",
            "Task Watcher",
            "Build finished",
            "cargo test succeeded",
            1_700_000_000_000,
        )
        .expect("valid PC notification");
        assert_eq!(notif.app, "pc.windows");
        assert_eq!(notif.app_name, "Task Watcher");
        assert_eq!(notif.title.as_deref(), Some("Build finished"));
        assert_eq!(notif.text.as_deref(), Some("cargo test succeeded"));
    }
}
