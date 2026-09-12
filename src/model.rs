//! Provider-independent calendar types and the reminder rules that act on them.
//!
//! Nothing here talks to Google or to the filesystem. The Google DTOs in
//! [`crate::google`] are mapped onto these types once, at sync time, so the
//! rules below can be tested directly.

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// How a provider was asked to deliver a reminder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReminderMethod {
    /// A desktop alert. The only method this daemon delivers.
    Popup,
    /// Delivered by the provider, not by us.
    Email,
    /// Anything the provider adds later.
    #[serde(other)]
    Other,
}

/// One reminder as the provider states it: a method and a lead time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reminder {
    pub method: ReminderMethod,
    /// Minutes before the event start. Google allows 0 and up to 40320 (4 weeks).
    pub minutes: i64,
}

impl Reminder {
    pub fn popup(minutes: i64) -> Self {
        Self {
            method: ReminderMethod::Popup,
            minutes,
        }
    }
}

/// What an event says about its own reminders.
///
/// The three cases are distinct on purpose. Google encodes "no reminders at
/// all" as `useDefault: false` with an empty override list, which is easy to
/// collapse into "inherit the calendar default" and thereby notify for an event
/// the user explicitly silenced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReminderSpec {
    /// Inherit whatever the calendar's defaults are.
    UseDefault,
    /// The user turned reminders off for this event.
    Disabled,
    /// The user replaced the defaults for this event.
    Overrides(Vec<Reminder>),
}

/// When an event happens, keeping all-day events distinguishable from timed
/// ones rather than flattening both to an instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventTime {
    /// An instant, plus the zone the provider stated it in when it gave one.
    Timed {
        at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        zone: Option<String>,
    },
    /// A calendar date with no time of day.
    AllDay { date: NaiveDate },
}

impl EventTime {
    /// The instant this time refers to.
    ///
    /// An all-day event resolves to midnight in `calendar_zone`, which is what
    /// Google's own reminder lead times are measured from: an all-day reminder
    /// configured as "1 day before at 09:00" arrives from the API as 900
    /// minutes, and only midnight-local makes that land at 09:00.
    pub fn instant(&self, calendar_zone: Tz) -> DateTime<Utc> {
        match self {
            EventTime::Timed { at, .. } => *at,
            EventTime::AllDay { date } => midnight_in(*date, calendar_zone),
        }
    }

    pub fn is_all_day(&self) -> bool {
        matches!(self, EventTime::AllDay { .. })
    }
}

/// Midnight on `date` in `zone`, resolving a daylight-saving gap forward.
///
/// A zone that skips midnight — America/Santiago has done this — has no 00:00
/// on that date at all. Falling back to the first instant of the day keeps the
/// day's reminders anchored to the day instead of failing the whole sync.
fn midnight_in(date: NaiveDate, zone: Tz) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).expect("midnight is a valid time");
    match zone.from_local_datetime(&naive) {
        chrono::offset::LocalResult::Single(dt) => dt.with_timezone(&Utc),
        chrono::offset::LocalResult::Ambiguous(earlier, _) => earlier.with_timezone(&Utc),
        chrono::offset::LocalResult::None => {
            // Walk forward to the first minute of the day that exists.
            for minute in 1..(24 * 60) {
                let candidate = naive + Duration::minutes(minute);
                if let chrono::offset::LocalResult::Single(dt) =
                    zone.from_local_datetime(&candidate)
                {
                    return dt.with_timezone(&Utc);
                }
            }
            Utc.from_utc_datetime(&naive)
        }
    }
}

/// Whether the provider still considers the event live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventStatus {
    Confirmed,
    Tentative,
    Cancelled,
}

/// This account's own answer to an invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResponseStatus {
    NeedsAction,
    Declined,
    Tentative,
    Accepted,
    /// No attendee entry for this account: an event the user owns outright.
    NotInvited,
}

/// A calendar as the provider describes it, including the defaults its events
/// inherit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarRef {
    pub id: String,
    pub summary: String,
    /// IANA zone name. Reminder lead times for all-day events are measured
    /// from midnight here.
    pub time_zone: Option<String>,
    pub color: Option<String>,
    pub primary: bool,
    pub selected: bool,
    pub default_reminders: Vec<Reminder>,
}

impl CalendarRef {
    /// The zone to resolve this calendar's all-day events in, falling back to
    /// UTC when the provider states none.
    pub fn zone(&self) -> Tz {
        self.time_zone
            .as_deref()
            .and_then(|name| name.parse::<Tz>().ok())
            .unwrap_or(chrono_tz::UTC)
    }

    /// A filesystem-safe collection directory name for this calendar.
    pub fn collection_name(&self) -> String {
        sanitize_component(&self.id)
    }
}

/// One concrete occurrence of an event, already expanded by the provider.
///
/// Recurring series are never stored as a rule: the provider expands them and
/// each instance arrives with its own stable id, so reminder timing and
/// deduplication never depend on this daemon reimplementing RRULE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub calendar_id: String,
    /// The provider's instance id. Unique per occurrence and stable across
    /// syncs, which is what makes the reminder ledger work across restarts.
    pub id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// The meeting link, already resolved and validated at sync time. The event
    /// description it may have come from is deliberately not persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_link: Option<String>,
    pub start: EventTime,
    pub end: EventTime,
    pub status: EventStatus,
    pub response: ResponseStatus,
    pub reminders: ReminderSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,
}

impl Occurrence {
    pub fn start_instant(&self, zone: Tz) -> DateTime<Utc> {
        self.start.instant(zone)
    }

    /// The instant the occurrence ends.
    ///
    /// An all-day event's `end` is the exclusive day after it, which is already
    /// the instant we want.
    pub fn end_instant(&self, zone: Tz) -> DateTime<Utc> {
        self.end.instant(zone)
    }

    /// Whether this occurrence should produce reminders and appear in the
    /// export at all.
    pub fn is_active(&self, skip_declined: bool) -> bool {
        if self.status == EventStatus::Cancelled {
            return false;
        }
        if skip_declined && self.response == ResponseStatus::Declined {
            return false;
        }
        true
    }

    /// The popup lead times that actually apply, after inheritance.
    ///
    /// Returns an empty vector both for an event with reminders switched off
    /// and for one inheriting a calendar that has no popup default — two
    /// different reasons for the same correct outcome of staying silent.
    pub fn popup_minutes(&self, calendar_defaults: &[Reminder]) -> Vec<i64> {
        let source: &[Reminder] = match &self.reminders {
            ReminderSpec::Disabled => &[],
            ReminderSpec::UseDefault => calendar_defaults,
            ReminderSpec::Overrides(overrides) => overrides,
        };
        let mut minutes: Vec<i64> = source
            .iter()
            .filter(|r| r.method == ReminderMethod::Popup)
            .map(|r| r.minutes)
            .collect();
        minutes.sort_unstable();
        minutes.dedup();
        minutes
    }
}

/// Replace anything that would escape a directory or confuse a reader.
pub fn sanitize_component(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            '@' => '_',
            _ => '-',
        })
        .collect();
    // A leading dot would make vdir readers skip the directory as hidden.
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() {
        out.push_str("calendar");
    }
    out.truncate(120);
    out
}

/// A due reminder, resolved to a wall-clock instant with a stable identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledReminder {
    /// Stable across restarts and across resyncs of an unchanged event.
    pub key: String,
    pub calendar_id: String,
    pub event_id: String,
    pub summary: String,
    pub minutes: i64,
    pub fire_at: DateTime<Utc>,
    pub event_start: DateTime<Utc>,
    pub event_end: DateTime<Utc>,
    pub all_day: bool,
    pub meeting_url: Option<String>,
    pub location: Option<String>,
}

impl ScheduledReminder {
    /// Identity of one reminder for deduplication.
    ///
    /// The start instant is part of the key so that a rescheduled event earns a
    /// fresh reminder rather than being silenced by the ledger entry of the
    /// time it used to be at.
    pub fn make_key(
        calendar_id: &str,
        event_id: &str,
        start: DateTime<Utc>,
        minutes: i64,
    ) -> String {
        format!("{calendar_id}|{event_id}|{}|{minutes}", start.timestamp())
    }
}

/// Expand one occurrence into every popup reminder it implies.
pub fn reminders_for(occurrence: &Occurrence, calendar: &CalendarRef) -> Vec<ScheduledReminder> {
    let zone = calendar.zone();
    let start = occurrence.start_instant(zone);
    let end = occurrence.end_instant(zone);
    occurrence
        .popup_minutes(&calendar.default_reminders)
        .into_iter()
        .map(|minutes| ScheduledReminder {
            key: ScheduledReminder::make_key(
                &occurrence.calendar_id,
                &occurrence.id,
                start,
                minutes,
            ),
            calendar_id: occurrence.calendar_id.clone(),
            event_id: occurrence.id.clone(),
            summary: occurrence.summary.clone(),
            minutes,
            fire_at: start - Duration::minutes(minutes),
            event_start: start,
            event_end: end,
            all_day: occurrence.start.is_all_day(),
            meeting_url: occurrence.meeting_url.clone(),
            location: occurrence.location.clone(),
        })
        .collect()
}

/// Format an occurrence start for a notification body.
pub fn describe_start(start: DateTime<Utc>, all_day: bool, zone: Tz) -> String {
    let local = start.with_timezone(&zone);
    if all_day {
        format!("{}", local.format("%A %-d %B"))
    } else {
        format!("{}", local.format("%A %-d %B, %H:%M"))
    }
}

/// Whether `date` falls inside the half-open window `[from, to)`.
pub fn date_in_window(date: NaiveDate, from: NaiveDate, to: NaiveDate) -> bool {
    date >= from && date < to
}

/// The civil date an instant falls on in `zone`.
pub fn local_date(at: DateTime<Utc>, zone: Tz) -> NaiveDate {
    let local = at.with_timezone(&zone);
    NaiveDate::from_ymd_opt(local.year(), local.month(), local.day())
        .expect("a real instant has a real date")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calendar(defaults: Vec<Reminder>, zone: &str) -> CalendarRef {
        CalendarRef {
            id: "primary@example.com".into(),
            summary: "Work".into(),
            time_zone: Some(zone.into()),
            color: None,
            primary: true,
            selected: true,
            default_reminders: defaults,
        }
    }

    fn timed_event(spec: ReminderSpec) -> Occurrence {
        Occurrence {
            calendar_id: "primary@example.com".into(),
            id: "evt_20260911T140000Z".into(),
            summary: "Standup".into(),
            location: None,
            meeting_url: None,
            html_link: None,
            start: EventTime::Timed {
                at: "2026-09-11T14:00:00Z".parse().unwrap(),
                zone: Some("America/Bogota".into()),
            },
            end: EventTime::Timed {
                at: "2026-09-11T14:30:00Z".parse().unwrap(),
                zone: Some("America/Bogota".into()),
            },
            status: EventStatus::Confirmed,
            response: ResponseStatus::Accepted,
            reminders: spec,
            updated: None,
        }
    }

    #[test]
    fn an_event_without_overrides_inherits_the_calendar_popup_defaults() {
        let cal = calendar(
            vec![Reminder::popup(10), Reminder::popup(30)],
            "America/Bogota",
        );
        let event = timed_event(ReminderSpec::UseDefault);
        assert_eq!(event.popup_minutes(&cal.default_reminders), vec![10, 30]);
    }

    #[test]
    fn explicitly_disabled_reminders_stay_silent_even_with_calendar_defaults() {
        let cal = calendar(vec![Reminder::popup(10)], "America/Bogota");
        let event = timed_event(ReminderSpec::Disabled);
        assert!(event.popup_minutes(&cal.default_reminders).is_empty());
        assert!(reminders_for(&event, &cal).is_empty());
    }

    #[test]
    fn overrides_replace_rather_than_extend_the_defaults() {
        let cal = calendar(vec![Reminder::popup(10)], "America/Bogota");
        let event = timed_event(ReminderSpec::Overrides(vec![Reminder::popup(5)]));
        assert_eq!(event.popup_minutes(&cal.default_reminders), vec![5]);
    }

    #[test]
    fn email_reminders_are_left_to_the_provider() {
        let cal = calendar(vec![], "America/Bogota");
        let event = timed_event(ReminderSpec::Overrides(vec![
            Reminder {
                method: ReminderMethod::Email,
                minutes: 60,
            },
            Reminder::popup(15),
        ]));
        assert_eq!(event.popup_minutes(&cal.default_reminders), vec![15]);
    }

    #[test]
    fn inheriting_a_calendar_whose_defaults_are_email_only_stays_silent() {
        let cal = calendar(
            vec![Reminder {
                method: ReminderMethod::Email,
                minutes: 1440,
            }],
            "America/Bogota",
        );
        let event = timed_event(ReminderSpec::UseDefault);
        assert!(event.popup_minutes(&cal.default_reminders).is_empty());
    }

    #[test]
    fn duplicate_lead_times_produce_one_reminder() {
        let cal = calendar(vec![], "America/Bogota");
        let event = timed_event(ReminderSpec::Overrides(vec![
            Reminder::popup(10),
            Reminder::popup(10),
        ]));
        assert_eq!(reminders_for(&event, &cal).len(), 1);
    }

    #[test]
    fn a_reminder_fires_its_lead_time_before_the_start() {
        let cal = calendar(vec![], "America/Bogota");
        let event = timed_event(ReminderSpec::Overrides(vec![Reminder::popup(10)]));
        let scheduled = reminders_for(&event, &cal);
        assert_eq!(
            scheduled[0].fire_at,
            "2026-09-11T13:50:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn an_all_day_reminder_is_measured_from_local_midnight_not_from_utc() {
        // Bogota is UTC-5, so local midnight on the 12th is 05:00Z.
        // Google states "the day before at 09:00" as 900 minutes.
        let cal = calendar(vec![Reminder::popup(900)], "America/Bogota");
        let mut event = timed_event(ReminderSpec::UseDefault);
        event.start = EventTime::AllDay {
            date: "2026-09-12".parse().unwrap(),
        };
        event.end = EventTime::AllDay {
            date: "2026-09-13".parse().unwrap(),
        };

        let scheduled = reminders_for(&event, &cal);
        assert_eq!(scheduled.len(), 1);
        assert!(scheduled[0].all_day);
        // 05:00Z minus 900 minutes = 14:00Z on the 11th = 09:00 local. If the
        // date had been read as UTC midnight this would be 09:00Z instead.
        assert_eq!(
            scheduled[0].fire_at,
            "2026-09-11T14:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn rescheduling_an_event_produces_a_different_reminder_identity() {
        let cal = calendar(vec![], "America/Bogota");
        let event = timed_event(ReminderSpec::Overrides(vec![Reminder::popup(10)]));
        let before = reminders_for(&event, &cal);

        let mut moved = event.clone();
        moved.start = EventTime::Timed {
            at: "2026-09-11T15:00:00Z".parse().unwrap(),
            zone: Some("America/Bogota".into()),
        };
        let after = reminders_for(&moved, &cal);

        assert_ne!(before[0].key, after[0].key);
    }

    #[test]
    fn an_unchanged_event_keeps_its_reminder_identity_across_syncs() {
        let cal = calendar(vec![], "America/Bogota");
        let event = timed_event(ReminderSpec::Overrides(vec![Reminder::popup(10)]));
        assert_eq!(
            reminders_for(&event, &cal)[0].key,
            reminders_for(&event, &cal)[0].key
        );
    }

    #[test]
    fn cancelled_and_declined_events_drop_out() {
        let mut cancelled = timed_event(ReminderSpec::UseDefault);
        cancelled.status = EventStatus::Cancelled;
        assert!(!cancelled.is_active(true));

        let mut declined = timed_event(ReminderSpec::UseDefault);
        declined.response = ResponseStatus::Declined;
        assert!(!declined.is_active(true));
        assert!(
            declined.is_active(false),
            "declined events stay when not skipping them"
        );
    }

    #[test]
    fn collection_names_never_escape_the_export_directory() {
        let cal = CalendarRef {
            id: "../../etc/passwd".into(),
            summary: "evil".into(),
            time_zone: None,
            color: None,
            primary: false,
            selected: true,
            default_reminders: vec![],
        };
        let name = cal.collection_name();
        assert!(!name.contains('/'));
        assert!(!name.starts_with('.'));
    }

    #[test]
    fn a_calendar_without_a_zone_falls_back_to_utc() {
        let cal = CalendarRef {
            id: "x".into(),
            summary: "x".into(),
            time_zone: None,
            color: None,
            primary: false,
            selected: true,
            default_reminders: vec![],
        };
        assert_eq!(cal.zone(), chrono_tz::UTC);
    }
}
