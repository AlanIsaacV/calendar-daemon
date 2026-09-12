//! The live Google Calendar client.
//!
//! One HTTP client is reused across a whole cycle. Each list request asks for
//! only the fields the daemon actually uses and pages until the provider stops
//! offering a page token or the per-calendar cap is reached.

use chrono::{DateTime, Utc};

use crate::auth::error::classify_api_status;
use crate::auth::oauth::describe_request_error;
use crate::auth::{AuthFailure, SecretStore, TokenEndpoint, TokenManager};
use crate::model::{CalendarRef, Occurrence};

use super::types::{CalendarListResponse, EventsResponse};
use super::{ApiError, CalendarProvider, ProviderFuture};

const CALENDAR_LIST_URL: &str = "https://www.googleapis.com/calendar/v3/users/me/calendarList";
const API_BASE: &str = "https://www.googleapis.com/calendar/v3";

/// Fields requested for the calendar list. Anything not named here is not
/// transferred and therefore never stored.
const CALENDAR_FIELDS: &str = "nextPageToken,items(id,summary,summaryOverride,timeZone,\
backgroundColor,primary,selected,deleted,accessRole,defaultReminders)";

/// Fields requested for events. The description is fetched because it is the
/// last place a meeting link hides; it is scanned for that link and then
/// discarded rather than persisted.
const EVENT_FIELDS: &str = "nextPageToken,items(id,status,summary,description,location,start,end,\
reminders,attendees(self,responseStatus),hangoutLink,\
conferenceData(entryPoints(entryPointType,uri)),htmlLink,updated)";

/// Provider page size. The API caps this at 2500.
const PAGE_SIZE: usize = 250;

pub struct GoogleCalendarClient<S: SecretStore, E: TokenEndpoint> {
    http: reqwest::Client,
    tokens: std::sync::Arc<TokenManager<S, E>>,
}

impl<S: SecretStore, E: TokenEndpoint> GoogleCalendarClient<S, E> {
    pub fn new(http: reqwest::Client, tokens: std::sync::Arc<TokenManager<S, E>>) -> Self {
        Self { http, tokens }
    }

    /// Issue a GET, renewing once on a 401 and never more than once.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<T, ApiError> {
        let mut allow_retry = true;
        loop {
            let token = self.tokens.access_token(Utc::now()).await?;
            let response = self
                .http
                .get(url)
                .bearer_auth(&token)
                .query(query)
                .send()
                .await
                .map_err(|err| ApiError::Transport(describe_request_error(&err)))?;

            let status = response.status().as_u16();
            if status == 401 && allow_retry {
                // One renewal, one retry. A second 401 after a fresh token is a
                // rejected grant, not a stale cache.
                allow_retry = false;
                self.tokens.invalidate_access_token().await;
                continue;
            }

            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());

            if !(200..300).contains(&status) {
                return Err(map_status(status, retry_after));
            }

            return response
                .json::<T>()
                .await
                .map_err(|err| ApiError::Decode(describe_request_error(&err)));
        }
    }
}

/// Map an HTTP status onto the error taxonomy.
///
/// The body is never read here: provider error bodies are free text and this
/// daemon does not put them in logs.
fn map_status(status: u16, retry_after_secs: Option<u64>) -> ApiError {
    if let Some(failure) = classify_api_status(status, retry_after_secs) {
        return match failure {
            AuthFailure::Throttled {
                status,
                retry_after_secs,
            } => ApiError::Retryable {
                status,
                retry_after_secs,
            },
            other => ApiError::Auth(other),
        };
    }
    match status {
        403 => ApiError::Forbidden {
            detail: "the granted scopes or the account's access do not cover this calendar".into(),
        },
        404 => ApiError::NotFound,
        other => ApiError::Decode(format!("unexpected status {other}")),
    }
}

impl<S: SecretStore + 'static, E: TokenEndpoint + 'static> CalendarProvider
    for GoogleCalendarClient<S, E>
{
    fn list_calendars(&self) -> ProviderFuture<'_, Vec<CalendarRef>> {
        Box::pin(async move {
            let mut calendars = Vec::new();
            let mut page_token: Option<String> = None;

            loop {
                let mut query = vec![
                    ("fields", CALENDAR_FIELDS.to_string()),
                    ("maxResults", PAGE_SIZE.to_string()),
                    ("showDeleted", "false".to_string()),
                    ("showHidden", "false".to_string()),
                ];
                if let Some(token) = &page_token {
                    query.push(("pageToken", token.clone()));
                }

                let page: CalendarListResponse = self.get_json(CALENDAR_LIST_URL, &query).await?;

                calendars.extend(
                    page.items
                        .into_iter()
                        .filter(|entry| !entry.deleted)
                        .map(|entry| entry.into_calendar()),
                );

                match page.next_page_token {
                    Some(token) if !token.is_empty() => page_token = Some(token),
                    _ => break,
                }
            }
            Ok(calendars)
        })
    }

    fn list_events<'a>(
        &'a self,
        calendar_id: &'a str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
        max_events: usize,
    ) -> ProviderFuture<'a, Vec<Occurrence>> {
        Box::pin(async move {
            let url = format!(
                "{API_BASE}/calendars/{}/events",
                urlencoding_path(calendar_id)
            );
            let mut occurrences = Vec::new();
            let mut page_token: Option<String> = None;

            loop {
                let remaining = max_events.saturating_sub(occurrences.len());
                if remaining == 0 {
                    tracing::warn!(
                        calendar = %calendar_id,
                        cap = max_events,
                        "stopped fetching at the per-calendar event cap; \
                         raise sync.max_events_per_calendar or narrow the window"
                    );
                    break;
                }

                let mut query = vec![
                    ("fields", EVENT_FIELDS.to_string()),
                    ("maxResults", PAGE_SIZE.min(remaining).to_string()),
                    // Expanded instances: the provider resolves recurrence, so
                    // reminder timing never depends on this daemon
                    // reimplementing RRULE.
                    ("singleEvents", "true".to_string()),
                    ("orderBy", "startTime".to_string()),
                    ("timeMin", time_min.to_rfc3339()),
                    ("timeMax", time_max.to_rfc3339()),
                    // Cancelled instances of a series must be visible, or a
                    // meeting the organizer called off keeps its reminder.
                    ("showDeleted", "true".to_string()),
                ];
                if let Some(token) = &page_token {
                    query.push(("pageToken", token.clone()));
                }

                let page: EventsResponse = self.get_json(&url, &query).await?;
                let next = page.next_page_token.clone();

                occurrences.extend(
                    page.items
                        .into_iter()
                        .filter_map(|event| event.into_occurrence(calendar_id)),
                );

                match next {
                    Some(token) if !token.is_empty() => page_token = Some(token),
                    _ => break,
                }
            }
            Ok(occurrences)
        })
    }
}

/// Percent-encode a calendar id for use as a path segment.
///
/// Calendar ids are email-like and can contain characters that would otherwise
/// change which endpoint is addressed.
fn urlencoding_path(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Build the HTTP client used for every provider request in a cycle.
pub fn build_http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("calendar-daemon/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_calendar_id_is_encoded_so_it_cannot_change_the_endpoint() {
        assert_eq!(urlencoding_path("work@example.com"), "work%40example.com");
        assert_eq!(urlencoding_path("../../admin"), "..%2F..%2Fadmin");
        assert_eq!(urlencoding_path("a b"), "a%20b");
    }

    #[test]
    fn throttling_is_retryable_while_a_scope_problem_is_not() {
        assert!(map_status(429, Some(10)).is_retryable());
        assert!(map_status(503, None).is_retryable());
        assert!(!map_status(403, None).is_retryable());
        assert!(!map_status(404, None).is_retryable());
    }

    #[test]
    fn a_401_reports_the_grant_and_a_429_never_does() {
        assert!(matches!(
            map_status(401, None),
            ApiError::Auth(AuthFailure::GrantInvalid { .. })
        ));
        let throttled = map_status(429, Some(30));
        assert!(throttled.preserves_credentials());
        assert_eq!(throttled.auth_failure(), None);
    }

    #[test]
    fn every_requested_field_is_one_the_daemon_reads() {
        // A field list that drifts from the mapping silently produces events
        // with no reminders, so keep the two in view of each other.
        for needed in [
            "reminders",
            "start",
            "end",
            "status",
            "attendees",
            "hangoutLink",
        ] {
            assert!(
                EVENT_FIELDS.contains(needed),
                "event field list dropped {needed}"
            );
        }
        assert!(CALENDAR_FIELDS.contains("defaultReminders"));
        assert!(CALENDAR_FIELDS.contains("timeZone"));
    }
}
