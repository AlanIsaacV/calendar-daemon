//! The subset of the Google Calendar API this daemon reads, and the mapping
//! from it onto [`crate::model`].
//!
//! The mapping is where provider quirks are absorbed: the three-way reminder
//! encoding, the date-versus-dateTime split for all-day events, and the several
//! places a meeting link can hide.

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;

use crate::model::{
    CalendarRef, EventStatus, EventTime, Occurrence, Reminder, ReminderMethod, ReminderSpec,
    ResponseStatus,
};

/// Longest link this daemon will carry, matching what calendar clients accept.
const MAX_LINK_LEN: usize = 2048;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarListResponse {
    #[serde(default)]
    pub items: Vec<CalendarListEntry>,
    #[serde(default)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarListEntry {
    pub id: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub summary_override: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub background_color: Option<String>,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub access_role: Option<String>,
    #[serde(default)]
    pub default_reminders: Vec<ApiReminder>,
}

impl CalendarListEntry {
    pub fn into_calendar(self) -> CalendarRef {
        let summary = self
            .summary_override
            .filter(|s| !s.trim().is_empty())
            .or(self.summary)
            .unwrap_or_else(|| self.id.clone());
        CalendarRef {
            id: self.id,
            summary,
            time_zone: self.time_zone,
            color: self.background_color.filter(|c| is_hex_color(c)),
            primary: self.primary,
            selected: self.selected,
            default_reminders: self.default_reminders.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiReminder {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub minutes: Option<i64>,
}

impl From<ApiReminder> for Reminder {
    fn from(value: ApiReminder) -> Self {
        let method = match value.method.as_deref() {
            Some("popup") => ReminderMethod::Popup,
            Some("email") => ReminderMethod::Email,
            _ => ReminderMethod::Other,
        };
        Reminder {
            method,
            minutes: value.minutes.unwrap_or(0),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsResponse {
    #[serde(default)]
    pub items: Vec<ApiEvent>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiEvent {
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub start: Option<ApiEventTime>,
    #[serde(default)]
    pub end: Option<ApiEventTime>,
    #[serde(default)]
    pub reminders: Option<ApiReminders>,
    #[serde(default)]
    pub attendees: Vec<ApiAttendee>,
    #[serde(default)]
    pub hangout_link: Option<String>,
    #[serde(default)]
    pub conference_data: Option<ApiConferenceData>,
    #[serde(default)]
    pub html_link: Option<String>,
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiEventTime {
    /// Present for timed events.
    #[serde(default)]
    pub date_time: Option<DateTime<Utc>>,
    /// Present for all-day events, as a bare civil date.
    #[serde(default)]
    pub date: Option<NaiveDate>,
    #[serde(default)]
    pub time_zone: Option<String>,
}

impl ApiEventTime {
    fn into_event_time(self) -> Option<EventTime> {
        if let Some(date) = self.date {
            return Some(EventTime::AllDay { date });
        }
        self.date_time.map(|at| EventTime::Timed {
            at,
            zone: self.time_zone,
        })
    }
}

/// The provider's reminder block.
///
/// `use_default` false with no overrides is the encoding for "this event has no
/// reminders", and is deliberately not the same thing as an absent block.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiReminders {
    #[serde(default)]
    pub use_default: bool,
    #[serde(default)]
    pub overrides: Vec<ApiReminder>,
}

impl ApiReminders {
    fn into_spec(self) -> ReminderSpec {
        if self.use_default {
            return ReminderSpec::UseDefault;
        }
        if self.overrides.is_empty() {
            return ReminderSpec::Disabled;
        }
        ReminderSpec::Overrides(self.overrides.into_iter().map(Into::into).collect())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiAttendee {
    #[serde(rename = "self", default)]
    pub is_self: bool,
    #[serde(default)]
    pub response_status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiConferenceData {
    #[serde(default)]
    pub entry_points: Vec<ApiEntryPoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiEntryPoint {
    #[serde(default)]
    pub entry_point_type: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
}

impl ApiEvent {
    /// Map one API event onto an occurrence.
    ///
    /// Returns `None` for an event with no usable start, which the API can
    /// produce for a cancelled instance of a series the account never had.
    pub fn into_occurrence(self, calendar_id: &str) -> Option<Occurrence> {
        let status = match self.status.as_deref() {
            Some("cancelled") => EventStatus::Cancelled,
            Some("tentative") => EventStatus::Tentative,
            _ => EventStatus::Confirmed,
        };

        let start = self.start.and_then(ApiEventTime::into_event_time)?;
        // An event with a start but no end is treated as ending when it starts,
        // which keeps every downstream duration non-negative.
        let end = self
            .end
            .and_then(ApiEventTime::into_event_time)
            .unwrap_or_else(|| start.clone());

        let response = self
            .attendees
            .iter()
            .find(|a| a.is_self)
            .map(|a| match a.response_status.as_deref() {
                Some("declined") => ResponseStatus::Declined,
                Some("tentative") => ResponseStatus::Tentative,
                Some("accepted") => ResponseStatus::Accepted,
                _ => ResponseStatus::NeedsAction,
            })
            .unwrap_or(ResponseStatus::NotInvited);

        let meeting_url = resolve_meeting_url(
            self.hangout_link.as_deref(),
            self.conference_data.as_ref(),
            self.location.as_deref(),
            self.description.as_deref(),
        );

        Some(Occurrence {
            calendar_id: calendar_id.to_string(),
            id: self.id,
            summary: self
                .summary
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "(no title)".to_string()),
            location: self.location.filter(|s| !s.trim().is_empty()),
            meeting_url,
            html_link: self.html_link.as_deref().and_then(normalize_http_url),
            start,
            end,
            status,
            response,
            reminders: self
                .reminders
                .map(ApiReminders::into_spec)
                .unwrap_or(ReminderSpec::UseDefault),
            updated: self.updated,
        })
    }
}

/// Pick the link a "join" action should open.
///
/// Order is deliberate: the provider's own conference link beats anything
/// scraped out of free text, and the location beats the description because a
/// location that holds a URL is usually the meeting room's own link.
pub fn resolve_meeting_url(
    hangout_link: Option<&str>,
    conference: Option<&ApiConferenceData>,
    location: Option<&str>,
    description: Option<&str>,
) -> Option<String> {
    if let Some(link) = hangout_link.and_then(normalize_http_url) {
        return Some(link);
    }
    if let Some(conference) = conference {
        let video = conference
            .entry_points
            .iter()
            .find(|e| e.entry_point_type.as_deref() == Some("video"))
            .and_then(|e| e.uri.as_deref())
            .and_then(normalize_http_url);
        if let Some(uri) = video {
            return Some(uri);
        }
        let any = conference
            .entry_points
            .iter()
            .filter_map(|e| e.uri.as_deref())
            .find_map(normalize_http_url);
        if let Some(uri) = any {
            return Some(uri);
        }
    }
    if let Some(found) = location.and_then(first_http_url) {
        return Some(found);
    }
    description.and_then(first_http_url)
}

/// The first http(s) URL embedded in free text, trimmed of trailing prose.
///
/// Scans by character, not by byte offset: event text is routinely not ASCII,
/// and slicing at a byte that lands inside a multi-byte character would panic
/// on an ordinary Spanish meeting title.
pub fn first_http_url(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for (index, _) in text.char_indices() {
        let rest = &bytes[index..];
        let is_scheme = starts_with_ignore_ascii_case(rest, b"https://")
            || starts_with_ignore_ascii_case(rest, b"http://");
        if !is_scheme {
            continue;
        }
        // Only at a token boundary, so "myhttp://x" is not read as a link.
        if index > 0 && bytes[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let tail = &text[index..];
        let candidate = tail.split_whitespace().next().unwrap_or(tail);
        let candidate = trim_trailing_prose(candidate);
        if let Some(normalized) = normalize_http_url(candidate) {
            return Some(normalized);
        }
    }
    None
}

fn starts_with_ignore_ascii_case(haystack: &[u8], prefix: &[u8]) -> bool {
    haystack.len() > prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Drop punctuation and unbalanced wrappers a URL picks up inside a sentence.
fn trim_trailing_prose(mut candidate: &str) -> &str {
    loop {
        let Some(last) = candidate.chars().last() else {
            return candidate;
        };
        let sentence = matches!(last, '.' | ',' | ';' | ':' | '!' | '?');
        let unbalanced = (last == ')' && !candidate.contains('('))
            || (last == ']' && !candidate.contains('['))
            || (last == '}' && !candidate.contains('{'))
            || matches!(last, '>' | '"' | '\'');
        if !sentence && !unbalanced {
            return candidate;
        }
        candidate = &candidate[..candidate.len() - last.len_utf8()];
    }
}

/// Parse a link and return its normalized serialization, or `None` when it is
/// not one this daemon is willing to carry.
///
/// Normalizing at the boundary means everything downstream — the ICS `URL`
/// property, the notification action, the argument handed to the opener — sees
/// a percent-encoded ASCII URI rather than whatever the event text contained.
pub fn normalize_http_url(candidate: &str) -> Option<String> {
    is_valid_http_url(candidate)
        .then(|| {
            url::Url::parse(candidate)
                .ok()
                .map(|parsed| parsed.to_string())
        })
        .flatten()
}

/// Whether a string is a link this daemon is willing to hand to a browser.
///
/// Only http and https: a meeting link is opened by an external program, and
/// anything else — `file:`, a custom scheme — is not something an event from
/// the network gets to choose.
pub fn is_valid_http_url(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.len() > MAX_LINK_LEN {
        return false;
    }
    if candidate.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(candidate) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    parsed.host_str().is_some_and(|h| !h.is_empty())
}

fn is_hex_color(text: &str) -> bool {
    let text = text.trim();
    (text.len() == 7 || text.len() == 9)
        && text.starts_with('#')
        && text[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_json(body: &str) -> ApiEvent {
        serde_json::from_str(body).expect("test fixture must parse")
    }

    #[test]
    fn use_default_maps_to_inheritance() {
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "reminders":{"useDefault":true}}"#,
        );
        let occurrence = event.into_occurrence("cal").unwrap();
        assert_eq!(occurrence.reminders, ReminderSpec::UseDefault);
    }

    #[test]
    fn use_default_false_with_no_overrides_means_switched_off_not_inherited() {
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "reminders":{"useDefault":false}}"#,
        );
        let occurrence = event.into_occurrence("cal").unwrap();
        assert_eq!(
            occurrence.reminders,
            ReminderSpec::Disabled,
            "an event with reminders switched off was read as inheriting the defaults"
        );
    }

    #[test]
    fn overrides_are_carried_with_their_methods() {
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "reminders":{"useDefault":false,
                "overrides":[{"method":"popup","minutes":15},{"method":"email","minutes":60}]}}"#,
        );
        let occurrence = event.into_occurrence("cal").unwrap();
        assert_eq!(
            occurrence.reminders,
            ReminderSpec::Overrides(vec![
                Reminder::popup(15),
                Reminder {
                    method: ReminderMethod::Email,
                    minutes: 60
                },
            ])
        );
    }

    #[test]
    fn an_absent_reminder_block_inherits_rather_than_silencing() {
        let event = event_json(r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"}}"#);
        assert_eq!(
            event.into_occurrence("cal").unwrap().reminders,
            ReminderSpec::UseDefault
        );
    }

    #[test]
    fn an_all_day_event_keeps_its_date_rather_than_becoming_an_instant() {
        let event =
            event_json(r#"{"id":"a","start":{"date":"2026-09-12"},"end":{"date":"2026-09-13"}}"#);
        let occurrence = event.into_occurrence("cal").unwrap();
        assert_eq!(
            occurrence.start,
            EventTime::AllDay {
                date: "2026-09-12".parse().unwrap()
            }
        );
        assert!(occurrence.start.is_all_day());
    }

    #[test]
    fn this_accounts_own_response_is_read_and_other_attendees_are_ignored() {
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "attendees":[{"responseStatus":"accepted"},
                             {"self":true,"responseStatus":"declined"}]}"#,
        );
        assert_eq!(
            event.into_occurrence("cal").unwrap().response,
            ResponseStatus::Declined
        );
    }

    #[test]
    fn an_event_with_no_attendee_list_is_not_treated_as_declined() {
        let event = event_json(r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"}}"#);
        assert_eq!(
            event.into_occurrence("cal").unwrap().response,
            ResponseStatus::NotInvited
        );
    }

    #[test]
    fn a_cancelled_instance_is_carried_through_as_cancelled() {
        let event = event_json(
            r#"{"id":"a","status":"cancelled","start":{"dateTime":"2026-09-11T14:00:00Z"}}"#,
        );
        let occurrence = event.into_occurrence("cal").unwrap();
        assert_eq!(occurrence.status, EventStatus::Cancelled);
        assert!(!occurrence.is_active(true));
    }

    #[test]
    fn an_event_without_a_start_is_dropped_rather_than_guessed_at() {
        let event = event_json(r#"{"id":"a","status":"cancelled"}"#);
        assert!(event.into_occurrence("cal").is_none());
    }

    #[test]
    fn the_conference_link_beats_a_url_in_the_description() {
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "hangoutLink":"https://meet.google.com/abc-defg-hij",
                "description":"Notes at https://wiki.example/page"}"#,
        );
        assert_eq!(
            event.into_occurrence("cal").unwrap().meeting_url.as_deref(),
            Some("https://meet.google.com/abc-defg-hij")
        );
    }

    #[test]
    fn a_video_entry_point_is_preferred_over_a_phone_one() {
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "conferenceData":{"entryPoints":[
                  {"entryPointType":"phone","uri":"tel:+15551234"},
                  {"entryPointType":"video","uri":"https://zoom.example/j/1"}]}}"#,
        );
        assert_eq!(
            event.into_occurrence("cal").unwrap().meeting_url.as_deref(),
            Some("https://zoom.example/j/1")
        );
    }

    #[test]
    fn a_tel_entry_point_alone_is_not_offered_as_a_meeting_link() {
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "conferenceData":{"entryPoints":[{"entryPointType":"phone","uri":"tel:+15551234"}]}}"#,
        );
        assert_eq!(event.into_occurrence("cal").unwrap().meeting_url, None);
    }

    #[test]
    fn a_link_in_the_location_is_used_when_there_is_no_conference_data() {
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "location":"Join at https://meet.example/x, room 4"}"#,
        );
        assert_eq!(
            event.into_occurrence("cal").unwrap().meeting_url.as_deref(),
            Some("https://meet.example/x")
        );
    }

    #[test]
    fn a_non_http_scheme_is_never_accepted_as_a_meeting_link() {
        for hostile in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>",
            "ssh://host/x",
        ] {
            assert!(!is_valid_http_url(hostile), "accepted {hostile}");
        }
        let event = event_json(
            r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"},
                "hangoutLink":"file:///etc/passwd"}"#,
        );
        assert_eq!(event.into_occurrence("cal").unwrap().meeting_url, None);
    }

    #[test]
    fn a_url_with_whitespace_or_control_characters_is_refused() {
        assert!(!is_valid_http_url("https://example.com/a b"));
        assert!(!is_valid_http_url("https://example.com/a\nb"));
        assert!(!is_valid_http_url("https://example.com/a\u{7f}"));
    }

    #[test]
    fn embedded_urls_shed_the_punctuation_around_them() {
        assert_eq!(
            first_http_url("Join <https://meet.example/x>.").as_deref(),
            Some("https://meet.example/x")
        );
        assert_eq!(
            first_http_url("(https://meet.example/y)").as_deref(),
            Some("https://meet.example/y")
        );
        assert_eq!(
            first_http_url("see https://meet.example/z?a=1#b for details").as_deref(),
            Some("https://meet.example/z?a=1#b")
        );
        assert_eq!(first_http_url("myhttp://not-a-link"), None);
        assert_eq!(first_http_url("no link here"), None);
    }

    #[test]
    fn url_scanning_handles_multi_byte_text_without_panicking() {
        assert_eq!(
            first_http_url("Reunión — https://meet.example/ñ").as_deref(),
            Some("https://meet.example/%C3%B1")
        );
        assert_eq!(first_http_url("日本語のテキスト"), None);
    }

    #[test]
    fn a_calendar_entry_carries_its_defaults_and_display_name() {
        let entry: CalendarListEntry = serde_json::from_str(
            r##"{"id":"work@example.com","summary":"Work","summaryOverride":"Trabajo",
                "timeZone":"America/Bogota","backgroundColor":"#9fe1e7","selected":true,
                "defaultReminders":[{"method":"popup","minutes":10}]}"##,
        )
        .unwrap();
        let calendar = entry.into_calendar();
        assert_eq!(
            calendar.summary, "Trabajo",
            "the user's own rename should win"
        );
        assert_eq!(calendar.color.as_deref(), Some("#9fe1e7"));
        assert_eq!(calendar.default_reminders, vec![Reminder::popup(10)]);
        assert_eq!(calendar.zone(), chrono_tz::America::Bogota);
    }

    #[test]
    fn a_malformed_colour_is_dropped_rather_than_written_to_the_export() {
        let entry: CalendarListEntry =
            serde_json::from_str(r#"{"id":"a","backgroundColor":"teal"}"#).unwrap();
        assert_eq!(entry.into_calendar().color, None);
    }

    #[test]
    fn an_event_with_no_end_is_treated_as_zero_length() {
        let event = event_json(r#"{"id":"a","start":{"dateTime":"2026-09-11T14:00:00Z"}}"#);
        let occurrence = event.into_occurrence("cal").unwrap();
        assert_eq!(occurrence.start, occurrence.end);
    }

    #[test]
    fn an_untitled_event_gets_a_placeholder_rather_than_an_empty_notification() {
        let event =
            event_json(r#"{"id":"a","summary":"  ","start":{"dateTime":"2026-09-11T14:00:00Z"}}"#);
        assert_eq!(event.into_occurrence("cal").unwrap().summary, "(no title)");
    }
}
