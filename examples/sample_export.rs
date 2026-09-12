//! Write a sample vdir export, with no Google account and no network.
//!
//! Use it to set up and check a local calendar account before authorizing
//! anything: point the reader at the directory this creates and confirm the
//! events appear and the meeting link opens.
//!
//!     cargo run --example sample_export -- /tmp/calendar-daemon-sample
//!
//! The sample deliberately covers the cases that are easy to get wrong: an
//! all-day event, an event with reminders switched off, a cancelled one, a
//! title containing characters that must be escaped, and a second calendar.

use std::collections::BTreeSet;
use std::path::PathBuf;

use calendar_daemon::export;
use calendar_daemon::model::{
    CalendarRef, EventStatus, EventTime, Occurrence, Reminder, ReminderSpec, ResponseStatus,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("calendar-daemon-sample"));

    let work = CalendarRef {
        id: "work@example.com".into(),
        summary: "Work (sample)".into(),
        time_zone: Some("America/Bogota".into()),
        color: Some("#9fe1e7".into()),
        primary: true,
        selected: true,
        default_reminders: vec![Reminder::popup(10)],
    };
    let personal = CalendarRef {
        id: "personal@example.com".into(),
        summary: "Personal (sample)".into(),
        time_zone: Some("America/Bogota".into()),
        color: Some("#f6c026".into()),
        primary: false,
        selected: true,
        default_reminders: vec![],
    };

    let base: chrono::DateTime<chrono::Utc> = chrono::Utc::now();
    let occurrences = vec![
        timed(
            &work,
            "sample-standup",
            "Daily standup",
            base + chrono::Duration::hours(1),
            Some("https://meet.example/sample-standup"),
            ReminderSpec::UseDefault,
        ),
        timed(
            &work,
            "sample-escapes",
            "Plan; review, and sign off",
            base + chrono::Duration::hours(3),
            None,
            ReminderSpec::Overrides(vec![Reminder::popup(30)]),
        ),
        timed(
            &work,
            "sample-quiet",
            "Focus block (no reminder)",
            base + chrono::Duration::hours(5),
            None,
            ReminderSpec::Disabled,
        ),
        Occurrence {
            status: EventStatus::Cancelled,
            ..timed(
                &work,
                "sample-cancelled",
                "Cancelled meeting",
                base + chrono::Duration::hours(7),
                None,
                ReminderSpec::UseDefault,
            )
        },
        Occurrence {
            calendar_id: personal.id.clone(),
            id: "sample-holiday".into(),
            summary: "Public holiday".into(),
            location: None,
            meeting_url: None,
            html_link: None,
            start: EventTime::AllDay {
                date: (base + chrono::Duration::days(2)).date_naive(),
            },
            end: EventTime::AllDay {
                date: (base + chrono::Duration::days(3)).date_naive(),
            },
            status: EventStatus::Confirmed,
            response: ResponseStatus::NotInvited,
            reminders: ReminderSpec::Overrides(vec![Reminder::popup(900)]),
            updated: Some(base),
        },
    ];

    let report = export::write_vdir(
        &root,
        &[work, personal],
        &occurrences,
        &BTreeSet::new(),
        true,
    )?;

    println!("Wrote a sample vdir tree to {}", root.display());
    for name in &report.written {
        println!("  {name}/calendar.ics");
    }
    println!();
    println!("Point a local (vdir) calendar account at that directory.");
    println!("Remove it with:  rm -rf {}", root.display());
    Ok(())
}

fn timed(
    calendar: &CalendarRef,
    id: &str,
    summary: &str,
    start: chrono::DateTime<chrono::Utc>,
    url: Option<&str>,
    reminders: ReminderSpec,
) -> Occurrence {
    Occurrence {
        calendar_id: calendar.id.clone(),
        id: id.into(),
        summary: summary.into(),
        location: Some("Meeting room 4".into()),
        meeting_url: url.map(str::to_string),
        html_link: None,
        start: EventTime::Timed {
            at: start,
            zone: calendar.time_zone.clone(),
        },
        end: EventTime::Timed {
            at: start + chrono::Duration::minutes(30),
            zone: calendar.time_zone.clone(),
        },
        status: EventStatus::Confirmed,
        response: ResponseStatus::Accepted,
        reminders,
        updated: Some(start),
    }
}
