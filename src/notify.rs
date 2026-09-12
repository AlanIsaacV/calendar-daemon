//! Desktop notifications, and the action that opens a meeting.
//!
//! Notifications go out over the standard desktop notification interface, so
//! whatever the session's notification server is presents them. Waiting for a
//! user to press the action button takes as long as the user takes, so it never
//! happens on the scheduler's thread.

use std::process::Stdio;

use chrono_tz::Tz;

use crate::config::NotificationConfig;
use crate::model::{ScheduledReminder, describe_start};

/// Where a reminder is delivered.
pub trait Notifier: Send + Sync {
    /// Show one reminder. Returns once the notification has been handed to the
    /// session, not once the user has read it.
    fn notify(&self, reminder: &ScheduledReminder, late: bool, zone: Tz) -> Result<(), String>;
}

/// The session's notification server.
pub struct DesktopNotifier {
    config: NotificationConfig,
}

impl DesktopNotifier {
    pub fn new(config: NotificationConfig) -> Self {
        Self { config }
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, reminder: &ScheduledReminder, late: bool, zone: Tz) -> Result<(), String> {
        let summary = reminder.summary.clone();
        let body = build_body(reminder, late, zone);
        let url = reminder.meeting_url.clone();
        let open_command = self.config.open_command.clone();
        let timeout = self.config.timeout_secs;

        let mut notification = notify_rust::Notification::new();
        notification
            .appname("calendar-daemon")
            .summary(&summary)
            .body(&body)
            .icon("appointment-soon")
            .hint(notify_rust::Hint::Category("appointment".into()));

        if timeout > 0 {
            notification.timeout(notify_rust::Timeout::Milliseconds(timeout * 1000));
        }
        if url.is_some() {
            notification.action("open", "Open meeting");
        }

        let handle = notification.show().map_err(|err| err.to_string())?;

        // `wait_for_action` blocks until the user acts or the notification
        // closes. Detaching it is what keeps a notification nobody clicks from
        // holding up every later reminder.
        if let Some(url) = url {
            std::thread::Builder::new()
                .name("calendar-daemon-action".into())
                .spawn(move || {
                    handle.wait_for_action(|action| {
                        if action == "open"
                            && let Err(err) = open_link(&open_command, &url)
                        {
                            tracing::warn!(error = %err, "could not open the meeting link");
                        }
                    });
                })
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }
}

/// The notification body: when the event is, and where.
pub fn build_body(reminder: &ScheduledReminder, late: bool, zone: Tz) -> String {
    let mut parts = Vec::new();

    if reminder.minutes == 0 {
        parts.push("Starting now".to_string());
    } else if reminder.all_day {
        parts.push(describe_start(reminder.event_start, true, zone));
    } else {
        parts.push(format!(
            "{} — in {}",
            describe_start(reminder.event_start, false, zone),
            humanize_minutes(reminder.minutes)
        ));
    }

    if late {
        parts.push("(delayed reminder)".to_string());
    }
    if let Some(location) = &reminder.location {
        let trimmed = location.trim();
        if !trimmed.is_empty() && Some(trimmed) != reminder.meeting_url.as_deref() {
            parts.push(truncate(trimmed, 120));
        }
    }
    parts.join("\n")
}

fn humanize_minutes(minutes: i64) -> String {
    match minutes {
        m if m < 60 => format!("{m} min"),
        m if m % 1440 == 0 => {
            let days = m / 1440;
            if days == 1 {
                "1 day".into()
            } else {
                format!("{days} days")
            }
        }
        m if m % 60 == 0 => {
            let hours = m / 60;
            if hours == 1 {
                "1 hour".into()
            } else {
                format!("{hours} hours")
            }
        }
        m => format!("{} h {} min", m / 60, m % 60),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Hand a meeting link to the configured opener.
///
/// The URL is passed as one argument to an executable named in configuration.
/// No shell is involved, so nothing in an event's text can become a command.
pub fn open_link(command: &[String], url: &str) -> Result<(), String> {
    // The provider mapping already rejected anything that is not http(s), but
    // this is the last point before the link leaves the process.
    if !crate::google::types::is_valid_http_url(url) {
        return Err("refusing to open a link that is not http(s)".into());
    }
    let Some((program, args)) = command.split_first() else {
        return Err("no opener is configured".into());
    };

    std::process::Command::new(program)
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|err| format!("could not run {program}: {err}"))
}

/// A notifier that records instead of showing, for tests and `--dry-run`.
#[derive(Default)]
pub struct RecordingNotifier {
    pub shown: std::sync::Mutex<Vec<(String, bool)>>,
}

impl Notifier for RecordingNotifier {
    fn notify(&self, reminder: &ScheduledReminder, late: bool, _zone: Tz) -> Result<(), String> {
        self.shown
            .lock()
            .unwrap()
            .push((reminder.key.clone(), late));
        Ok(())
    }
}

impl RecordingNotifier {
    pub fn keys(&self) -> Vec<String> {
        self.shown
            .lock()
            .unwrap()
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn reminder(minutes: i64, all_day: bool) -> ScheduledReminder {
        let start: DateTime<Utc> = "2026-09-11T14:00:00Z".parse().unwrap();
        ScheduledReminder {
            key: "k".into(),
            calendar_id: "work".into(),
            event_id: "e1".into(),
            summary: "Standup".into(),
            minutes,
            fire_at: start - chrono::Duration::minutes(minutes),
            event_start: start,
            event_end: start + chrono::Duration::minutes(30),
            all_day,
            meeting_url: None,
            location: None,
        }
    }

    #[test]
    fn a_body_names_the_local_time_and_the_lead_time() {
        let body = build_body(&reminder(10, false), false, chrono_tz::America::Bogota);
        // 14:00Z is 09:00 in Bogota.
        assert!(body.contains("09:00"), "{body}");
        assert!(body.contains("10 min"), "{body}");
    }

    #[test]
    fn a_zero_minute_reminder_says_it_is_starting_now() {
        let body = build_body(&reminder(0, false), false, chrono_tz::UTC);
        assert!(body.starts_with("Starting now"), "{body}");
    }

    #[test]
    fn a_late_reminder_says_so_rather_than_pretending_to_be_on_time() {
        let body = build_body(&reminder(10, false), true, chrono_tz::UTC);
        assert!(body.contains("delayed"), "{body}");
    }

    #[test]
    fn an_all_day_reminder_names_the_day_and_not_a_time() {
        let body = build_body(&reminder(900, true), false, chrono_tz::America::Bogota);
        assert!(body.contains("September"), "{body}");
        assert!(
            !body.contains(':'),
            "an all-day body showed a clock time: {body}"
        );
    }

    #[test]
    fn lead_times_read_the_way_a_person_would_say_them() {
        assert_eq!(humanize_minutes(5), "5 min");
        assert_eq!(humanize_minutes(60), "1 hour");
        assert_eq!(humanize_minutes(120), "2 hours");
        assert_eq!(humanize_minutes(1440), "1 day");
        assert_eq!(humanize_minutes(2880), "2 days");
        assert_eq!(humanize_minutes(90), "1 h 30 min");
    }

    #[test]
    fn a_location_identical_to_the_link_is_not_repeated() {
        let mut r = reminder(10, false);
        r.meeting_url = Some("https://meet.example/x".into());
        r.location = Some("https://meet.example/x".into());
        let body = build_body(&r, false, chrono_tz::UTC);
        assert_eq!(body.matches("meet.example").count(), 0, "{body}");
    }

    #[test]
    fn a_long_location_is_truncated_rather_than_flooding_the_notification() {
        let mut r = reminder(10, false);
        r.location = Some("x".repeat(500));
        let body = build_body(&r, false, chrono_tz::UTC);
        assert!(
            body.chars().count() < 200,
            "body of {} chars",
            body.chars().count()
        );
    }

    #[test]
    fn opening_refuses_anything_that_is_not_an_http_link() {
        let command = vec!["true".to_string()];
        for hostile in ["file:///etc/passwd", "javascript:alert(1)", "; rm -rf /"] {
            assert!(open_link(&command, hostile).is_err(), "accepted {hostile}");
        }
    }

    #[test]
    fn opening_with_no_configured_program_is_an_error_not_a_shell() {
        assert!(open_link(&[], "https://meet.example/x").is_err());
    }

    #[test]
    fn a_url_is_passed_as_one_argument_to_the_named_program() {
        // `true` ignores its arguments and exits zero, so a successful spawn
        // means the URL reached argv rather than a shell.
        assert!(open_link(&["true".to_string()], "https://meet.example/x?a=1&b=2").is_ok());
    }

    #[test]
    fn text_is_truncated_on_a_character_boundary() {
        let truncated = truncate(&"ñ".repeat(200), 10);
        assert!(truncated.chars().count() <= 10);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }
}
