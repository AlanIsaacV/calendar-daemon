//! iCalendar serialization.
//!
//! Occurrences are written as concrete instances with no `RRULE`: the provider
//! already expanded the recurrence, and re-encoding it as a rule would make the
//! events a reader sees depend on that reader's own expansion.

use chrono::{DateTime, Utc};

use crate::model::{EventStatus, EventTime, Occurrence};

/// Identifies this producer in the exported file.
pub const PRODID: &str = "-//calendar-daemon//calendar-daemon//EN";

/// A fixed timestamp for events the provider gave no `updated` for, so an
/// unchanged calendar serializes to identical bytes every cycle.
const FALLBACK_DTSTAMP: &str = "19700101T000000Z";

/// Serialize a whole calendar, including the case where it has no events.
///
/// An empty calendar still produces a complete `VCALENDAR`. Removing the file
/// instead would leave the collection with no `.ics` at all, and a vdir reader
/// that finds no collections treats the whole refresh as failed and keeps
/// showing the events it already had — so the last deleted event would never
/// disappear from the display.
pub fn serialize_calendar(
    display_name: &str,
    time_zone: Option<&str>,
    color: Option<&str>,
    occurrences: &[&Occurrence],
) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(occurrences.len() * 10 + 8);

    lines.push("BEGIN:VCALENDAR".into());
    lines.push("VERSION:2.0".into());
    lines.push(format!("PRODID:{PRODID}"));
    lines.push("CALSCALE:GREGORIAN".into());
    lines.push("METHOD:PUBLISH".into());
    // Kept near the top on purpose: a vdir reader looking for a display name
    // reads only the first few kilobytes of the file.
    lines.push(format!("X-WR-CALNAME:{}", escape_text(display_name)));
    if let Some(zone) = time_zone {
        lines.push(format!("X-WR-TIMEZONE:{}", escape_text(zone)));
    }
    if let Some(color) = color {
        lines.push(format!("X-APPLE-CALENDAR-COLOR:{}", escape_text(color)));
    }

    for occurrence in occurrences {
        push_event(&mut lines, occurrence);
    }

    lines.push("END:VCALENDAR".into());

    let mut out = String::new();
    for line in lines {
        for folded in fold_line(&line) {
            out.push_str(&folded);
            out.push_str("\r\n");
        }
    }
    out
}

fn push_event(lines: &mut Vec<String>, occurrence: &Occurrence) {
    lines.push("BEGIN:VEVENT".into());
    lines.push(format!("UID:{}", escape_text(&occurrence.id)));
    lines.push(format!(
        "DTSTAMP:{}",
        occurrence
            .updated
            .map(format_utc)
            .unwrap_or_else(|| FALLBACK_DTSTAMP.to_string())
    ));
    lines.push(time_property("DTSTART", &occurrence.start));
    lines.push(time_property("DTEND", &occurrence.end));
    lines.push(format!("SUMMARY:{}", escape_text(&occurrence.summary)));

    if let Some(location) = &occurrence.location {
        lines.push(format!("LOCATION:{}", escape_text(location)));
    }
    // A URI value, not TEXT: it is written verbatim, which is safe because the
    // provider mapping already normalized and validated it.
    if let Some(url) = &occurrence.meeting_url {
        lines.push(format!("URL:{url}"));
    }
    lines.push(format!(
        "STATUS:{}",
        match occurrence.status {
            EventStatus::Confirmed => "CONFIRMED",
            EventStatus::Tentative => "TENTATIVE",
            EventStatus::Cancelled => "CANCELLED",
        }
    ));
    if occurrence.start.is_all_day() {
        lines.push("X-MICROSOFT-CDO-ALLDAYEVENT:TRUE".into());
    }
    lines.push("END:VEVENT".into());
}

/// Render a start or end, keeping an all-day event a date.
///
/// The `VALUE=DATE` form is what makes a reader treat the event as all-day; a
/// midnight timestamp would show up as a zero-length appointment instead.
fn time_property(name: &str, time: &EventTime) -> String {
    match time {
        EventTime::AllDay { date } => {
            format!("{name};VALUE=DATE:{}", date.format("%Y%m%d"))
        }
        EventTime::Timed { at, .. } => format!("{name}:{}", format_utc(*at)),
    }
}

fn format_utc(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Escape a TEXT value per RFC 5545 §3.3.11.
pub fn escape_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

/// Fold a content line to 75 octets per RFC 5545 §3.1.
///
/// The split is placed on a character boundary: a multi-byte character broken
/// across two lines would be reassembled as mojibake by a conforming reader.
pub fn fold_line(line: &str) -> Vec<String> {
    const LIMIT: usize = 75;
    if line.len() <= LIMIT {
        return vec![line.to_string()];
    }

    let mut out = Vec::new();
    let mut remaining = line;
    let mut budget = LIMIT;

    while remaining.len() > budget {
        let mut split = budget;
        while split > 0 && !remaining.is_char_boundary(split) {
            split -= 1;
        }
        if split == 0 {
            // One character wider than the budget: emit it anyway rather than
            // loop forever.
            split = remaining
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len());
        }
        let (head, tail) = remaining.split_at(split);
        out.push(if out.is_empty() {
            head.to_string()
        } else {
            format!(" {head}")
        });
        remaining = tail;
        // Continuation lines spend one octet on the leading space.
        budget = LIMIT - 1;
    }
    out.push(if out.is_empty() {
        remaining.to_string()
    } else {
        format!(" {remaining}")
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReminderSpec, ResponseStatus};

    fn occurrence(id: &str, summary: &str) -> Occurrence {
        Occurrence {
            calendar_id: "work".into(),
            id: id.into(),
            summary: summary.into(),
            location: None,
            meeting_url: None,
            html_link: None,
            start: EventTime::Timed {
                at: "2026-09-11T14:00:00Z".parse().unwrap(),
                zone: None,
            },
            end: EventTime::Timed {
                at: "2026-09-11T14:30:00Z".parse().unwrap(),
                zone: None,
            },
            status: EventStatus::Confirmed,
            response: ResponseStatus::Accepted,
            reminders: ReminderSpec::UseDefault,
            updated: Some("2026-09-01T08:00:00Z".parse().unwrap()),
        }
    }

    #[test]
    fn an_empty_calendar_is_still_a_complete_vcalendar() {
        let ics = serialize_calendar("Work", Some("America/Bogota"), None, &[]);
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(ics.trim_end().ends_with("END:VCALENDAR"));
        assert!(ics.contains("VERSION:2.0"));
        assert!(!ics.contains("BEGIN:VEVENT"));
    }

    #[test]
    fn the_display_name_sits_inside_the_first_kilobytes_a_reader_scans() {
        let many: Vec<Occurrence> = (0..200)
            .map(|i| occurrence(&format!("e{i}"), "Long meeting title"))
            .collect();
        let refs: Vec<&Occurrence> = many.iter().collect();
        let ics = serialize_calendar("Trabajo", None, None, &refs);

        let header = &ics[..4096.min(ics.len())];
        assert!(
            header.contains("X-WR-CALNAME:Trabajo"),
            "display name outside the scanned header"
        );
    }

    #[test]
    fn every_line_ends_with_crlf() {
        let event = occurrence("e1", "Standup");
        let ics = serialize_calendar("Work", None, None, &[&event]);
        for line in ics.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(!line.contains('\n'), "bare newline in {line:?}");
            assert!(!line.contains('\r'), "stray carriage return in {line:?}");
        }
    }

    #[test]
    fn an_all_day_event_is_written_as_a_date_not_as_midnight() {
        let mut event = occurrence("e1", "Holiday");
        event.start = EventTime::AllDay {
            date: "2026-09-12".parse().unwrap(),
        };
        event.end = EventTime::AllDay {
            date: "2026-09-13".parse().unwrap(),
        };

        let ics = serialize_calendar("Work", None, None, &[&event]);
        assert!(ics.contains("DTSTART;VALUE=DATE:20260912"), "{ics}");
        assert!(ics.contains("DTEND;VALUE=DATE:20260913"), "{ics}");
        assert!(!ics.contains("DTSTART:20260912T000000Z"));
    }

    #[test]
    fn a_timed_event_keeps_its_instant() {
        let event = occurrence("e1", "Standup");
        let ics = serialize_calendar("Work", None, None, &[&event]);
        assert!(ics.contains("DTSTART:20260911T140000Z"), "{ics}");
        assert!(ics.contains("DTEND:20260911T143000Z"), "{ics}");
    }

    #[test]
    fn text_values_are_escaped_and_uris_are_not() {
        let mut event = occurrence("e1", "Plan; review, and sign off\\now");
        event.meeting_url = Some("https://meet.example/x?a=1;b=2".into());
        let ics = serialize_calendar("Work", None, None, &[&event]);

        assert!(
            ics.contains("SUMMARY:Plan\\; review\\, and sign off\\\\now"),
            "{ics}"
        );
        assert!(ics.contains("URL:https://meet.example/x?a=1;b=2"), "{ics}");
    }

    #[test]
    fn a_newline_in_a_summary_cannot_forge_a_property() {
        let event = occurrence("e1", "Hi\nSUMMARY:injected\nX:y");
        let ics = serialize_calendar("Work", None, None, &[&event]);

        // The newline survives as the two-character escape RFC 5545 defines,
        // so it stays inside the value instead of starting a content line.
        let property_lines = ics
            .split("\r\n")
            .filter(|line| line.starts_with("SUMMARY:") || line.starts_with("X:"))
            .count();
        assert_eq!(
            property_lines, 1,
            "an event title injected a property line: {ics}"
        );
        assert!(ics.contains("SUMMARY:Hi\\nSUMMARY:injected\\nX:y"), "{ics}");
    }

    #[test]
    fn long_lines_are_folded_at_seventy_five_octets() {
        let event = occurrence("e1", &"a".repeat(300));
        let ics = serialize_calendar("Work", None, None, &[&event]);
        for line in ics.split("\r\n") {
            assert!(
                line.len() <= 75,
                "unfolded line of {} octets: {line:?}",
                line.len()
            );
        }
    }

    #[test]
    fn folding_never_splits_a_multi_byte_character() {
        let line = format!("SUMMARY:{}", "ñ".repeat(120));
        let folded = fold_line(&line);
        let rejoined: String = folded
            .iter()
            .map(|l| l.strip_prefix(' ').unwrap_or(l))
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(rejoined, line, "unfolding did not reproduce the original");
        for part in &folded {
            assert!(part.len() <= 75, "folded part too long: {}", part.len());
            assert!(std::str::from_utf8(part.as_bytes()).is_ok());
        }
    }

    #[test]
    fn a_short_line_is_left_alone() {
        assert_eq!(fold_line("SUMMARY:hi"), vec!["SUMMARY:hi".to_string()]);
    }

    #[test]
    fn an_unchanged_calendar_serializes_to_identical_bytes() {
        let event = occurrence("e1", "Standup");
        let first = serialize_calendar("Work", None, None, &[&event]);
        let second = serialize_calendar("Work", None, None, &[&event]);
        assert_eq!(
            first, second,
            "serialization is not stable, so every sync would rewrite"
        );
    }

    #[test]
    fn an_event_the_provider_never_updated_still_gets_a_stable_dtstamp() {
        let mut event = occurrence("e1", "Standup");
        event.updated = None;
        let ics = serialize_calendar("Work", None, None, &[&event]);
        assert!(
            ics.contains(&format!("DTSTAMP:{FALLBACK_DTSTAMP}")),
            "{ics}"
        );
    }

    #[test]
    fn a_cancelled_event_is_written_with_its_status() {
        let mut event = occurrence("e1", "Called off");
        event.status = EventStatus::Cancelled;
        let ics = serialize_calendar("Work", None, None, &[&event]);
        assert!(ics.contains("STATUS:CANCELLED"), "{ics}");
    }

    #[test]
    fn the_calendar_colour_is_carried_for_readers_that_look_for_it() {
        let ics = serialize_calendar("Work", None, Some("#9fe1e7"), &[]);
        assert!(ics.contains("X-APPLE-CALENDAR-COLOR:#9fe1e7"), "{ics}");
    }
}
