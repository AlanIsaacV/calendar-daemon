//! Classification of authorization failures.
//!
//! The distinction this module exists to make is between a failure that should
//! cost nothing — a flaky network, a throttled request, a keyring that is
//! merely locked — and one that genuinely invalidated the stored grant. Getting
//! that wrong in the cheap direction means deleting a working refresh token
//! because Wi-Fi dropped; getting it wrong in the expensive direction means
//! retrying a revoked grant forever while the user sees nothing.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Why an authorization attempt did not produce a usable access token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthFailure {
    /// The request never reached a verdict: DNS, TLS, connection, timeout.
    /// Stored credentials remain valid as far as anyone knows.
    Transient { detail: String },

    /// The provider answered, but with a status that says "later", not "no":
    /// 429 and 5xx. Credentials are untouched.
    Throttled {
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u64>,
    },

    /// The secret store could not be reached or is locked. This is emphatically
    /// not a revoked grant: the refresh token may be perfectly good and simply
    /// unreadable until the session unlocks the keyring.
    KeyringUnavailable { detail: String },

    /// Nothing has ever been stored for this account, or it was deleted. The
    /// first authorization has not happened yet.
    NotAuthorized,

    /// The provider rejected the refresh token itself. This is the one case
    /// that requires the user to authorize again.
    GrantInvalid {
        /// Provider error code, allowlisted to known values.
        code: String,
        /// Provider error subtype when it supplied one, allowlisted. Google
        /// uses this to distinguish an expired grant from a revoked one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtype: Option<String>,
    },

    /// The client registration is wrong: a bad id or secret, or a client that
    /// is not permitted to use this flow. Reauthorizing cannot fix this, so the
    /// daemon must not send the user to a browser for it.
    ClientMisconfigured { code: String },

    /// A response that does not fit any of the above. Treated as transient for
    /// credential-retention purposes, because destroying a grant on an
    /// unrecognized answer is the worse mistake.
    Unexpected { status: u16, detail: String },
}

impl AuthFailure {
    /// Whether the stored refresh token should survive this failure.
    ///
    /// Only an explicit rejection of the grant itself clears it.
    pub fn preserves_credentials(&self) -> bool {
        !matches!(self, AuthFailure::GrantInvalid { .. })
    }

    /// Whether the daemon should ask the user to authorize in a browser.
    ///
    /// Deliberately false for a locked keyring and for a misconfigured client:
    /// neither is fixed by consenting again, and opening a browser for them
    /// trains the user to re-consent on every hiccup.
    pub fn needs_user_authorization(&self) -> bool {
        matches!(
            self,
            AuthFailure::GrantInvalid { .. } | AuthFailure::NotAuthorized
        )
    }

    /// Whether retrying the same request later could succeed on its own.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AuthFailure::Transient { .. }
                | AuthFailure::Throttled { .. }
                | AuthFailure::Unexpected { .. }
                | AuthFailure::KeyringUnavailable { .. }
        )
    }

    /// A provider-suggested wait, when one was given.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            AuthFailure::Throttled {
                retry_after_secs: Some(secs),
                ..
            } => Some(Duration::from_secs(*secs)),
            _ => None,
        }
    }

    /// A short category name, safe to log and to show in `status`.
    pub fn category(&self) -> &'static str {
        match self {
            AuthFailure::Transient { .. } => "transient",
            AuthFailure::Throttled { .. } => "throttled",
            AuthFailure::KeyringUnavailable { .. } => "keyring-unavailable",
            AuthFailure::NotAuthorized => "not-authorized",
            AuthFailure::GrantInvalid { .. } => "grant-invalid",
            AuthFailure::ClientMisconfigured { .. } => "client-misconfigured",
            AuthFailure::Unexpected { .. } => "unexpected",
        }
    }

    /// What the user should do about it, in one sentence.
    pub fn remedy(&self) -> String {
        match self {
            AuthFailure::Transient { .. } => "No action needed; the next sync will retry.".into(),
            AuthFailure::Throttled { .. } => {
                "No action needed; requests are being backed off.".into()
            }
            AuthFailure::KeyringUnavailable { .. } => {
                "Unlock the login keyring. The stored authorization is intact and no new \
                 consent is needed."
                    .into()
            }
            AuthFailure::NotAuthorized => {
                "Run `calendar-daemon auth` to authorize this account for the first time.".into()
            }
            AuthFailure::GrantInvalid { subtype, .. } => {
                let detail = subtype
                    .as_deref()
                    .map(|s| format!(" (provider subtype: {s})"))
                    .unwrap_or_default();
                format!(
                    "Google rejected the stored authorization{detail}. Run \
                     `calendar-daemon auth` to grant access again."
                )
            }
            AuthFailure::ClientMisconfigured { code } => format!(
                "Google rejected the OAuth client itself ({code}). Check auth.client_id and \
                 auth.client_secret; authorizing again will not help."
            ),
            AuthFailure::Unexpected { .. } => {
                "Unrecognized provider response; credentials were kept. Retry, and report \
                 this if it persists."
                    .into()
            }
        }
    }
}

impl std::fmt::Display for AuthFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthFailure::Transient { detail } => write!(f, "transient failure: {detail}"),
            AuthFailure::Throttled {
                status,
                retry_after_secs,
            } => match retry_after_secs {
                Some(secs) => write!(f, "throttled by provider (HTTP {status}, retry in {secs}s)"),
                None => write!(f, "throttled by provider (HTTP {status})"),
            },
            AuthFailure::KeyringUnavailable { detail } => {
                write!(f, "secret store unavailable: {detail}")
            }
            AuthFailure::NotAuthorized => write!(f, "no stored authorization for this account"),
            AuthFailure::GrantInvalid { code, subtype } => match subtype {
                Some(s) => write!(f, "authorization rejected ({code}/{s})"),
                None => write!(f, "authorization rejected ({code})"),
            },
            AuthFailure::ClientMisconfigured { code } => {
                write!(f, "OAuth client rejected ({code})")
            }
            AuthFailure::Unexpected { status, detail } => {
                write!(f, "unexpected provider response (HTTP {status}): {detail}")
            }
        }
    }
}

impl std::error::Error for AuthFailure {}

/// OAuth error codes that are safe to record verbatim.
///
/// Anything outside this list is reduced to `other`, so a provider that starts
/// returning detail inside the code field cannot leak it into a log line.
const KNOWN_ERROR_CODES: &[&str] = &[
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
    "access_denied",
    "admin_policy_enforced",
    "disallowed_useragent",
    "org_internal",
    "redirect_uri_mismatch",
    "rate_limit_exceeded",
];

/// Google's `error_subtype` values that are safe to record verbatim.
const KNOWN_ERROR_SUBTYPES: &[&str] = &["invalid_rapt", "client_not_authorized", "rapt_required"];

/// Reduce a provider-supplied code to an allowlisted value.
pub fn sanitize_code(raw: &str) -> String {
    let trimmed = raw.trim();
    if KNOWN_ERROR_CODES.contains(&trimmed) {
        trimmed.to_string()
    } else {
        "other".to_string()
    }
}

/// Reduce a provider-supplied subtype to an allowlisted value, dropping it when
/// it is not one this daemon recognizes.
pub fn sanitize_subtype(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    KNOWN_ERROR_SUBTYPES
        .contains(&trimmed)
        .then(|| trimmed.to_string())
}

/// The provider's error body, as far as this daemon is willing to read it.
///
/// `error_description` is parsed but never retained: it is free text under the
/// provider's control and has no place in a log file that may be shared.
#[derive(Debug, Default, Deserialize)]
pub struct OAuthErrorBody {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_subtype: Option<String>,
}

/// Turn an HTTP status plus a token-endpoint body into a classified failure.
///
/// The rule that matters: a 4xx is not by itself proof that the grant is dead.
/// Only `invalid_grant` says that. A 429 arrives as 4xx and must keep the
/// credentials, which is exactly the case a blanket "4xx means unusable"
/// classification gets wrong.
pub fn classify_token_error(status: u16, body: &str, retry_after_secs: Option<u64>) -> AuthFailure {
    if status == 429 {
        return AuthFailure::Throttled {
            status,
            retry_after_secs,
        };
    }
    if (500..600).contains(&status) {
        return AuthFailure::Throttled {
            status,
            retry_after_secs,
        };
    }

    let parsed: OAuthErrorBody = serde_json::from_str(body).unwrap_or_default();
    let Some(raw_code) = parsed.error.as_deref() else {
        return AuthFailure::Unexpected {
            status,
            detail: format!("no error code in a {status} response"),
        };
    };
    let code = sanitize_code(raw_code);
    let subtype = parsed.error_subtype.as_deref().and_then(sanitize_subtype);

    match code.as_str() {
        "invalid_grant" => AuthFailure::GrantInvalid { code, subtype },
        "invalid_client"
        | "unauthorized_client"
        | "redirect_uri_mismatch"
        | "unsupported_grant_type"
        | "invalid_scope" => AuthFailure::ClientMisconfigured { code },
        "rate_limit_exceeded" => AuthFailure::Throttled {
            status,
            retry_after_secs,
        },
        // `invalid_request` is this daemon's own bug, and access_denied and the
        // policy codes describe a consent decision rather than a dead token.
        // None of them justify discarding a stored grant.
        _ => AuthFailure::Unexpected {
            status,
            detail: format!("{code} (HTTP {status})"),
        },
    }
}

/// Classify a failure from an ordinary API call rather than the token endpoint.
pub fn classify_api_status(status: u16, retry_after_secs: Option<u64>) -> Option<AuthFailure> {
    match status {
        401 => Some(AuthFailure::GrantInvalid {
            code: "invalid_grant".into(),
            subtype: None,
        }),
        403 => None, // a scope or quota problem, handled by the caller
        429 => Some(AuthFailure::Throttled {
            status,
            retry_after_secs,
        }),
        500..=599 => Some(AuthFailure::Throttled {
            status,
            retry_after_secs,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttling_never_looks_like_a_dead_grant() {
        let failure = classify_token_error(429, r#"{"error":"rate_limit_exceeded"}"#, Some(30));
        assert!(failure.preserves_credentials());
        assert!(!failure.needs_user_authorization());
        assert_eq!(failure.retry_after(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn a_server_error_preserves_credentials() {
        let failure = classify_token_error(503, "<html>upstream</html>", None);
        assert!(failure.preserves_credentials());
        assert!(failure.is_retryable());
    }

    #[test]
    fn invalid_grant_is_the_only_body_that_invalidates_the_stored_token() {
        let failure = classify_token_error(400, r#"{"error":"invalid_grant"}"#, None);
        assert!(!failure.preserves_credentials());
        assert!(failure.needs_user_authorization());
    }

    #[test]
    fn a_recognized_subtype_survives_and_reaches_the_remedy_text() {
        let failure = classify_token_error(
            400,
            r#"{"error":"invalid_grant","error_subtype":"invalid_rapt"}"#,
            None,
        );
        assert_eq!(
            failure,
            AuthFailure::GrantInvalid {
                code: "invalid_grant".into(),
                subtype: Some("invalid_rapt".into()),
            }
        );
        assert!(failure.remedy().contains("invalid_rapt"));
    }

    #[test]
    fn an_unrecognized_subtype_is_dropped_rather_than_logged() {
        let failure = classify_token_error(
            400,
            r#"{"error":"invalid_grant","error_subtype":"user bob@corp had 3 sessions"}"#,
            None,
        );
        match failure {
            AuthFailure::GrantInvalid { subtype, .. } => assert_eq!(subtype, None),
            other => panic!("expected GrantInvalid, got {other:?}"),
        }
    }

    #[test]
    fn a_provider_description_never_reaches_the_rendered_failure() {
        let failure = classify_token_error(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked for bob@corp"}"#,
            None,
        );
        let rendered = format!("{failure} {} {}", failure.category(), failure.remedy());
        assert!(
            !rendered.contains("bob@corp"),
            "leaked description: {rendered}"
        );
    }

    #[test]
    fn an_unknown_error_code_is_reduced_to_other_and_kept_retryable() {
        let failure =
            classify_token_error(400, r#"{"error":"quota_for_user_bob@corp_exceeded"}"#, None);
        assert!(failure.preserves_credentials());
        let rendered = format!("{failure}");
        assert!(!rendered.contains("bob@corp"), "leaked code: {rendered}");
    }

    #[test]
    fn a_bad_client_does_not_send_the_user_to_a_browser() {
        let failure = classify_token_error(401, r#"{"error":"invalid_client"}"#, None);
        assert!(!failure.needs_user_authorization());
        assert!(failure.preserves_credentials());
        assert!(failure.remedy().contains("client_id"));
    }

    #[test]
    fn a_locked_keyring_is_not_a_revoked_authorization() {
        let failure = AuthFailure::KeyringUnavailable {
            detail: "collection is locked".into(),
        };
        assert!(failure.preserves_credentials());
        assert!(!failure.needs_user_authorization());
        assert!(failure.is_retryable());
        assert!(failure.remedy().contains("Unlock"));
    }

    #[test]
    fn a_malformed_error_body_is_unexpected_rather_than_fatal() {
        let failure = classify_token_error(400, "not json at all", None);
        assert!(failure.preserves_credentials());
        assert!(!failure.needs_user_authorization());
    }

    #[test]
    fn an_api_401_reports_the_grant_while_a_403_is_left_to_the_caller() {
        assert!(matches!(
            classify_api_status(401, None),
            Some(AuthFailure::GrantInvalid { .. })
        ));
        assert!(classify_api_status(403, None).is_none());
        assert!(matches!(
            classify_api_status(429, Some(5)),
            Some(AuthFailure::Throttled { .. })
        ));
    }
}
