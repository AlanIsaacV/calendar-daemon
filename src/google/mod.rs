//! The Google Calendar provider: what is fetched, and what a failed fetch means.

pub mod client;
pub mod types;

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};

use crate::auth::AuthFailure;
use crate::model::{CalendarRef, Occurrence};

pub use client::GoogleCalendarClient;

/// A future returned by a provider method.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ApiError>> + Send + 'a>>;

/// Why a calendar read did not produce data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Authorization could not be obtained or was rejected. Carries the full
    /// classification so the daemon knows whether to retry, wait for a keyring,
    /// or ask the user.
    Auth(AuthFailure),
    /// The account is authorized but not for this calendar or this data.
    Forbidden { detail: String },
    /// The calendar is gone.
    NotFound,
    /// A status that should be retried later.
    Retryable {
        status: u16,
        retry_after_secs: Option<u64>,
    },
    /// The request never got an answer.
    Transport(String),
    /// The answer was not the shape this daemon expects.
    Decode(String),
}

impl ApiError {
    pub fn is_retryable(&self) -> bool {
        match self {
            ApiError::Auth(failure) => failure.is_retryable(),
            ApiError::Retryable { .. } | ApiError::Transport(_) => true,
            ApiError::Forbidden { .. } | ApiError::NotFound | ApiError::Decode(_) => false,
        }
    }

    /// Whether the stored authorization survives this error.
    pub fn preserves_credentials(&self) -> bool {
        match self {
            ApiError::Auth(failure) => failure.preserves_credentials(),
            _ => true,
        }
    }

    pub fn category(&self) -> &'static str {
        match self {
            ApiError::Auth(failure) => failure.category(),
            ApiError::Forbidden { .. } => "forbidden",
            ApiError::NotFound => "not-found",
            ApiError::Retryable { .. } => "retryable",
            ApiError::Transport(_) => "transport",
            ApiError::Decode(_) => "decode",
        }
    }

    pub fn auth_failure(&self) -> Option<&AuthFailure> {
        match self {
            ApiError::Auth(failure) => Some(failure),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Auth(failure) => write!(f, "{failure}"),
            ApiError::Forbidden { detail } => write!(f, "access refused: {detail}"),
            ApiError::NotFound => write!(f, "calendar not found"),
            ApiError::Retryable { status, .. } => write!(f, "provider returned HTTP {status}"),
            ApiError::Transport(detail) => write!(f, "{detail}"),
            ApiError::Decode(detail) => write!(f, "unexpected response: {detail}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<AuthFailure> for ApiError {
    fn from(value: AuthFailure) -> Self {
        ApiError::Auth(value)
    }
}

/// The calendar source the sync engine reads from.
///
/// Behind a trait so the whole sync cycle — windowing, deduplication, export,
/// reminder scheduling — can be exercised against a scripted provider with no
/// network and no Google account.
pub trait CalendarProvider: Send + Sync {
    fn list_calendars(&self) -> ProviderFuture<'_, Vec<CalendarRef>>;

    fn list_events<'a>(
        &'a self,
        calendar_id: &'a str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
        max_events: usize,
    ) -> ProviderFuture<'a, Vec<Occurrence>>;
}
