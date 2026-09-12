//! The Google desktop OAuth flow: PKCE, a loopback callback that exists only
//! for the duration of consent, and the token endpoint.
//!
//! The daemon never keeps an HTTP listener running. The listener in
//! [`await_callback`] is bound when the user asks to authorize, is closed when
//! consent finishes or the timeout expires, and binds only to the numeric
//! loopback address on a kernel-chosen port.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rand::TryRng as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::error::{AuthFailure, classify_token_error};

/// Read access to events on the calendars this account can see.
pub const SCOPE_EVENTS: &str = "https://www.googleapis.com/auth/calendar.events.readonly";
/// Read access to the calendar list, which is where per-calendar reminder
/// defaults and colours come from.
pub const SCOPE_CALENDAR_LIST: &str =
    "https://www.googleapis.com/auth/calendar.calendarlist.readonly";

pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// The path the browser is redirected back to. Any other path on the temporary
/// listener is refused.
const CALLBACK_PATH: &str = "/oauth/callback";

/// The scopes this daemon requests, in request order.
pub fn requested_scopes() -> [&'static str; 2] {
    [SCOPE_EVENTS, SCOPE_CALENDAR_LIST]
}

/// A PKCE verifier and the challenge derived from it.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh pair. S256 only; the `plain` method is not offered.
    pub fn generate() -> Self {
        let verifier = random_token(64);
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
        }
    }
}

/// A URL-safe random token of `bytes` bytes of entropy.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::SysRng
        .try_fill_bytes(&mut buf)
        .expect("the operating system must provide randomness");
    URL_SAFE_NO_PAD.encode(buf)
}

/// Everything one consent attempt needs to be validated on the way back.
#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub state: String,
    pub pkce: Pkce,
    pub redirect_uri: String,
    pub authorization_url: String,
}

/// Build the authorization URL for a listener already bound to `port`.
pub fn begin_authorization(client_id: &str, port: u16) -> PendingAuthorization {
    let pkce = Pkce::generate();
    let state = random_token(32);
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

    let scopes = requested_scopes().join(" ");
    let mut url = url::Url::parse(AUTH_ENDPOINT).expect("the endpoint constant is a valid URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &scopes)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        // Offline access is what yields a refresh token at all.
        .append_pair("access_type", "offline")
        // Without this, a re-authorization of an account that already consented
        // returns no refresh token, and the daemon would silently end up with
        // an access token it cannot renew.
        .append_pair("prompt", "consent");

    PendingAuthorization {
        state,
        pkce,
        redirect_uri,
        authorization_url: url.to_string(),
    }
}

/// What the browser handed back, once validated.
#[derive(Debug, Clone)]
pub struct CallbackResult {
    pub code: String,
}

/// Bind a loopback listener on an available port.
pub async fn bind_loopback() -> std::io::Result<(TcpListener, u16)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Wait for the browser to come back, validating the path and the state.
///
/// Requests to any other path — a browser probing `/favicon.ico`, or anything
/// else that finds the port — are answered with 404 and do not end the wait.
/// The listener is dropped when this returns, either way.
pub async fn await_callback(
    listener: TcpListener,
    pending: &PendingAuthorization,
    timeout: Duration,
) -> Result<CallbackResult, AuthFailure> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let accepted = tokio::time::timeout_at(deadline, listener.accept()).await;
        let (mut stream, _peer) = match accepted {
            Ok(Ok(pair)) => pair,
            Ok(Err(err)) => {
                return Err(AuthFailure::Transient {
                    detail: format!("callback listener failed: {err}"),
                });
            }
            Err(_elapsed) => {
                return Err(AuthFailure::Transient {
                    detail: "browser authorization was not completed before the timeout".into(),
                });
            }
        };

        let request_line = match read_request_line(&mut stream, deadline).await {
            Ok(line) => line,
            Err(_) => continue,
        };

        let Some(target) = request_target(&request_line) else {
            respond(&mut stream, 400, "Bad request").await;
            continue;
        };

        // A relative target needs a base to parse against; the base itself is
        // never used for anything but parsing.
        let parsed = match url::Url::parse(&format!("http://127.0.0.1{target}")) {
            Ok(parsed) => parsed,
            Err(_) => {
                respond(&mut stream, 400, "Bad request").await;
                continue;
            }
        };

        if parsed.path() != CALLBACK_PATH {
            respond(&mut stream, 404, "Not found").await;
            continue;
        }

        let params: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        // Validate state before looking at anything else on the request, so a
        // forged callback cannot reach the token endpoint.
        let state_matches = params
            .get("state")
            .map(|s| constant_time_eq(s.as_bytes(), pending.state.as_bytes()))
            .unwrap_or(false);
        if !state_matches {
            respond(
                &mut stream,
                400,
                "Authorization state did not match. Nothing was changed.",
            )
            .await;
            return Err(AuthFailure::Unexpected {
                status: 0,
                detail: "callback state did not match the pending authorization".into(),
            });
        }

        if let Some(error) = params.get("error") {
            let code = super::error::sanitize_code(error);
            respond(
                &mut stream,
                200,
                "Authorization was declined. You can close this tab.",
            )
            .await;
            return Err(match code.as_str() {
                "access_denied" => AuthFailure::Unexpected {
                    status: 0,
                    detail: "authorization was declined in the browser".into(),
                },
                "admin_policy_enforced" | "org_internal" | "disallowed_useragent" => {
                    AuthFailure::ClientMisconfigured { code }
                }
                _ => AuthFailure::Unexpected {
                    status: 0,
                    detail: format!("authorization returned {code}"),
                },
            });
        }

        let Some(code) = params.get("code").filter(|c| !c.is_empty()).cloned() else {
            respond(&mut stream, 400, "No authorization code was returned.").await;
            return Err(AuthFailure::Unexpected {
                status: 0,
                detail: "callback carried neither an error nor a code".into(),
            });
        };

        respond(
            &mut stream,
            200,
            "calendar-daemon is authorized. You can close this tab and return to the terminal.",
        )
        .await;
        return Ok(CallbackResult { code });
    }
}

async fn read_request_line(
    stream: &mut tokio::net::TcpStream,
    deadline: tokio::time::Instant,
) -> Result<String, ()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        // A request line longer than this is not a browser redirect.
        if buf.len() > 8192 {
            return Err(());
        }
        let read = tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await;
        match read {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                    return String::from_utf8(buf[..pos].to_vec()).map_err(|_| ());
                }
            }
            _ => return Err(()),
        }
    }
    Err(())
}

/// Extract the request target from a `GET /path?query HTTP/1.1` line.
fn request_target(line: &str) -> Option<&str> {
    let mut parts = line.split(' ');
    let method = parts.next()?;
    if method != "GET" {
        return None;
    }
    let target = parts.next()?;
    target.starts_with('/').then_some(target)
}

async fn respond(stream: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    // The message is this program's own text, but escape it anyway rather than
    // relying on that staying true.
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>calendar-daemon</title>\
         <body style=\"font-family:system-ui;margin:3rem\"><p>{}</p></body>",
        html_escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn html_escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// Compare without an early return on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A token-endpoint success response.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

impl TokenResponse {
    /// When the access token stops being usable.
    pub fn expires_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.expires_in
            .map(|secs| now + chrono::Duration::seconds(secs))
    }

    /// Whether the response granted every scope this daemon needs.
    ///
    /// Google returns the granted scopes on the token response; a user who
    /// unticks one of the boxes on the consent screen produces a token that
    /// authorizes less than was asked for, and that has to be caught here
    /// rather than as a confusing 403 hours later.
    pub fn missing_scopes(&self) -> Vec<&'static str> {
        let Some(granted) = self.scope.as_deref() else {
            // No scope field at all: assume the request was honoured as sent,
            // which is what the specification permits.
            return Vec::new();
        };
        let granted: Vec<&str> = granted.split_whitespace().collect();
        requested_scopes()
            .into_iter()
            .filter(|needed| !granted.contains(needed))
            .collect()
    }
}

/// Exchange an authorization code for tokens.
pub async fn exchange_code(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
    pending: &PendingAuthorization,
) -> Result<TokenResponse, AuthFailure> {
    let form = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", code),
        ("code_verifier", pending.pkce.verifier.as_str()),
        ("grant_type", "authorization_code"),
        ("redirect_uri", pending.redirect_uri.as_str()),
    ];
    post_token(http, &form).await
}

/// Exchange a refresh token for a fresh access token.
pub async fn refresh_access_token(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse, AuthFailure> {
    let form = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];
    post_token(http, &form).await
}

async fn post_token(
    http: &reqwest::Client,
    form: &[(&str, &str)],
) -> Result<TokenResponse, AuthFailure> {
    let response = http
        .post(TOKEN_ENDPOINT)
        .form(form)
        .send()
        .await
        .map_err(|err| AuthFailure::Transient {
            detail: describe_request_error(&err),
        })?;

    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());

    let body = response
        .text()
        .await
        .map_err(|err| AuthFailure::Transient {
            detail: describe_request_error(&err),
        })?;

    if !(200..300).contains(&status) {
        return Err(classify_token_error(status, &body, retry_after));
    }

    serde_json::from_str::<TokenResponse>(&body).map_err(|_| AuthFailure::Unexpected {
        status,
        detail: "token response was not the expected shape".into(),
    })
}

/// Describe a transport failure without echoing a URL that may carry a token.
pub fn describe_request_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "request timed out".into()
    } else if err.is_connect() {
        "could not connect to the provider".into()
    } else if err.is_decode() {
        "response body could not be decoded".into()
    } else if err.is_body() {
        "request body failed".into()
    } else {
        "network request failed".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pkce_challenge_is_the_s256_of_its_verifier() {
        let pkce = Pkce::generate();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        // RFC 7636 requires 43..=128 characters for the verifier.
        assert!(
            (43..=128).contains(&pkce.verifier.len()),
            "len {}",
            pkce.verifier.len()
        );
    }

    #[test]
    fn every_authorization_gets_fresh_pkce_and_state() {
        let a = begin_authorization("client", 1234);
        let b = begin_authorization("client", 1234);
        assert_ne!(a.state, b.state);
        assert_ne!(a.pkce.verifier, b.pkce.verifier);
    }

    #[test]
    fn the_authorization_url_asks_for_offline_access_with_s256_and_read_only_scopes() {
        let pending = begin_authorization("client-id.apps.googleusercontent.com", 45_678);
        let url = url::Url::parse(&pending.authorization_url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["access_type"], "offline");
        assert_eq!(params["prompt"], "consent");
        assert_eq!(params["response_type"], "code");
        assert_eq!(
            params["redirect_uri"],
            "http://127.0.0.1:45678/oauth/callback"
        );
        assert_eq!(params["code_challenge"], pending.pkce.challenge);

        let scope = &params["scope"];
        assert!(scope.contains("calendar.events.readonly"));
        assert!(scope.contains("calendar.calendarlist.readonly"));
        assert!(
            !scope.contains("cloud-platform"),
            "over-broad scope requested: {scope}"
        );
        assert!(
            !scope.contains("gmail"),
            "over-broad scope requested: {scope}"
        );
        // A writable calendar scope would be a superset; make sure the
        // read-only suffix is on every calendar scope requested.
        for granted in scope.split(' ') {
            assert!(
                granted.ends_with(".readonly"),
                "writable scope requested: {granted}"
            );
        }
    }

    #[test]
    fn the_redirect_uri_is_numeric_loopback_never_a_hostname() {
        let pending = begin_authorization("client", 9999);
        assert!(pending.redirect_uri.starts_with("http://127.0.0.1:"));
        assert!(!pending.redirect_uri.contains("localhost"));
    }

    #[test]
    fn only_get_requests_with_an_absolute_path_are_accepted() {
        assert_eq!(
            request_target("GET /oauth/callback?code=x HTTP/1.1"),
            Some("/oauth/callback?code=x")
        );
        assert_eq!(request_target("POST /oauth/callback HTTP/1.1"), None);
        assert_eq!(request_target("GET http://evil/ HTTP/1.1"), None);
        assert_eq!(request_target("garbage"), None);
    }

    #[test]
    fn a_partial_scope_grant_is_detected() {
        let response = TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: Some(3600),
            scope: Some(SCOPE_EVENTS.into()),
            token_type: Some("Bearer".into()),
        };
        assert_eq!(response.missing_scopes(), vec![SCOPE_CALENDAR_LIST]);
    }

    #[test]
    fn a_full_scope_grant_reports_nothing_missing() {
        let response = TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: Some(3600),
            scope: Some(format!("{SCOPE_CALENDAR_LIST} {SCOPE_EVENTS} openid")),
            token_type: Some("Bearer".into()),
        };
        assert!(response.missing_scopes().is_empty());
    }

    #[test]
    fn constant_time_comparison_still_compares() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn html_in_a_message_is_escaped() {
        assert_eq!(html_escape("<b>&"), "&lt;b&gt;&amp;");
    }

    #[tokio::test]
    async fn a_mismatched_state_is_rejected_without_yielding_a_code() {
        let (listener, port) = bind_loopback().await.unwrap();
        let pending = begin_authorization("client", port);

        let url = format!("http://127.0.0.1:{port}/oauth/callback?code=secret&state=forged");
        tokio::spawn(async move {
            let _ = reqwest::Client::new().get(&url).send().await;
        });

        let result = await_callback(listener, &pending, Duration::from_secs(5)).await;
        assert!(result.is_err(), "a forged state must not produce a code");
    }

    #[tokio::test]
    async fn a_valid_callback_yields_the_code_and_ignores_unrelated_paths() {
        let (listener, port) = bind_loopback().await.unwrap();
        let pending = begin_authorization("client", port);
        let state = pending.state.clone();

        tokio::spawn(async move {
            let client = reqwest::Client::new();
            // A browser probing for a favicon must not end the wait.
            let _ = client
                .get(format!("http://127.0.0.1:{port}/favicon.ico"))
                .send()
                .await;
            let _ = client
                .get(format!(
                    "http://127.0.0.1:{port}/oauth/callback?code=the-code&state={state}"
                ))
                .send()
                .await;
        });

        let result = await_callback(listener, &pending, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(result.code, "the-code");
    }

    #[tokio::test]
    async fn the_listener_gives_up_and_closes_after_the_timeout() {
        let (listener, port) = bind_loopback().await.unwrap();
        let pending = begin_authorization("client", port);

        let result = await_callback(listener, &pending, Duration::from_millis(150)).await;
        assert!(matches!(result, Err(AuthFailure::Transient { .. })));

        // The listener was consumed, so the port is free again.
        let rebound = TcpListener::bind(("127.0.0.1", port)).await;
        assert!(
            rebound.is_ok(),
            "temporary listener outlived the authorization attempt"
        );
    }
}
