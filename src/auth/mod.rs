//! Authorization state: what is stored, when it is renewed, and what each kind
//! of failure means for the user.

pub mod error;
pub mod oauth;
pub mod secrets;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

pub use error::AuthFailure;
pub use oauth::TokenResponse;
pub use secrets::SecretStore;

/// The token endpoint, behind a trait so renewal behaviour can be tested
/// without reaching Google.
pub trait TokenEndpoint: Send + Sync {
    fn refresh<'a>(
        &'a self,
        refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, AuthFailure>> + Send + 'a>>;
}

/// The live token endpoint.
pub struct GoogleTokenEndpoint {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
}

impl GoogleTokenEndpoint {
    pub fn new(http: reqwest::Client, client_id: String, client_secret: String) -> Self {
        Self {
            http,
            client_id,
            client_secret,
        }
    }
}

impl TokenEndpoint for GoogleTokenEndpoint {
    fn refresh<'a>(
        &'a self,
        refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, AuthFailure>> + Send + 'a>> {
        Box::pin(async move {
            oauth::refresh_access_token(
                &self.http,
                &self.client_id,
                &self.client_secret,
                refresh_token,
            )
            .await
        })
    }
}

/// What `status` reports about authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum AuthHealth {
    /// No OAuth client has been configured yet.
    Unconfigured,
    /// A client is configured but this account has never authorized.
    NotAuthorized,
    /// A usable access token is held, or can be obtained without the user.
    Ready {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access_expires_at: Option<DateTime<Utc>>,
    },
    /// The stored grant was rejected. Only this state should send the user to a
    /// browser.
    ReauthorizationRequired { failure: AuthFailure },
    /// The secret store could not be read. The grant is presumed intact.
    KeyringUnavailable { detail: String },
    /// Something failed that is expected to recover on its own.
    Degraded { failure: AuthFailure },
}

impl AuthHealth {
    /// One line for `status`, naming the state and what to do about it.
    pub fn summary(&self) -> String {
        match self {
            AuthHealth::Unconfigured => {
                "not configured — set auth.client_id and auth.client_secret".into()
            }
            AuthHealth::NotAuthorized => "not authorized — run `calendar-daemon auth`".into(),
            AuthHealth::Ready {
                access_expires_at: Some(at),
            } => {
                format!(
                    "ready — access token valid until {}",
                    at.format("%Y-%m-%d %H:%M:%SZ")
                )
            }
            AuthHealth::Ready {
                access_expires_at: None,
            } => "ready".into(),
            AuthHealth::ReauthorizationRequired { failure } => {
                format!("reauthorization required — {}", failure.remedy())
            }
            AuthHealth::KeyringUnavailable { .. } => {
                "keyring unavailable — unlock the login keyring; the authorization is intact".into()
            }
            AuthHealth::Degraded { failure } => {
                format!("degraded ({}) — {}", failure.category(), failure.remedy())
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, AuthHealth::Ready { .. })
    }
}

#[derive(Debug, Default)]
struct TokenState {
    access_token: Option<String>,
    access_expires_at: Option<DateTime<Utc>>,
    /// Set once the provider has explicitly rejected the stored grant, so the
    /// daemon stops re-asking on every sync until the user authorizes again or
    /// asks for a sync.
    grant_rejected: Option<AuthFailure>,
    last_failure: Option<AuthFailure>,
}

/// Holds the refresh token, hands out access tokens, and decides what a failure
/// means.
pub struct TokenManager<S: SecretStore, E: TokenEndpoint> {
    account: String,
    store: S,
    endpoint: E,
    /// Headroom before the stated expiry at which a token is renewed anyway.
    skew: chrono::Duration,
    /// Held across a renewal so two concurrent syncs cannot both refresh, which
    /// is what makes one of them lose a rotated refresh token.
    state: Mutex<TokenState>,
}

impl<S: SecretStore, E: TokenEndpoint> TokenManager<S, E> {
    pub fn new(account: impl Into<String>, store: S, endpoint: E, skew: Duration) -> Self {
        Self {
            account: account.into(),
            store,
            endpoint,
            skew: chrono::Duration::from_std(skew)
                .unwrap_or_else(|_| chrono::Duration::seconds(120)),
            state: Mutex::new(TokenState::default()),
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// An access token good for a request made now.
    ///
    /// Renews only when there is no cached token or it is inside the skew
    /// window. Sleeping through an access token's lifetime costs nothing: the
    /// next call simply renews, which is why there is no keep-alive timer.
    pub async fn access_token(&self, now: DateTime<Utc>) -> Result<String, AuthFailure> {
        let mut state = self.state.lock().await;

        if let Some(failure) = &state.grant_rejected {
            return Err(failure.clone());
        }

        if let Some(token) = &state.access_token {
            let still_good = match state.access_expires_at {
                Some(expiry) => now + self.skew < expiry,
                None => true,
            };
            if still_good {
                return Ok(token.clone());
            }
        }

        self.renew_locked(&mut state, now).await
    }

    /// Drop the cached access token so the next request renews.
    ///
    /// Used after a single 401, which is retried once and never in a loop.
    pub async fn invalidate_access_token(&self) {
        let mut state = self.state.lock().await;
        state.access_token = None;
        state.access_expires_at = None;
    }

    pub async fn clear_grant_rejection(&self) -> bool {
        let mut state = self.state.lock().await;
        if state.grant_rejected.take().is_none() {
            return false;
        }
        state.access_token = None;
        state.access_expires_at = None;
        true
    }

    async fn renew_locked(
        &self,
        state: &mut TokenState,
        now: DateTime<Utc>,
    ) -> Result<String, AuthFailure> {
        let stored = match self.store.get(&self.account) {
            Ok(Some(token)) => token,
            Ok(None) => {
                let failure = AuthFailure::NotAuthorized;
                state.last_failure = Some(failure.clone());
                return Err(failure);
            }
            Err(failure) => {
                // A locked keyring is recorded but never allowed to look like a
                // rejected grant, so no reauthorization is triggered by it.
                state.last_failure = Some(failure.clone());
                return Err(failure);
            }
        };

        let response = match self.endpoint.refresh(&stored).await {
            Ok(response) => response,
            Err(failure) => {
                state.last_failure = Some(failure.clone());
                if !failure.preserves_credentials() {
                    state.grant_rejected = Some(failure.clone());
                    state.access_token = None;
                    state.access_expires_at = None;
                }
                return Err(failure);
            }
        };

        // Rotation: persist a replacement before anything depends on it, and
        // keep the existing one when the provider omits a replacement, which it
        // does on most refreshes.
        if let Some(rotated) = response.refresh_token.as_deref()
            && rotated != stored
            && let Err(failure) = self.store.set(&self.account, rotated)
        {
            // The new access token is usable, but a rotation that could
            // not be written down would be lost on restart. Report the
            // storage failure rather than proceed on a token whose
            // renewal path is now unknown.
            state.last_failure = Some(failure.clone());
            return Err(failure);
        }

        state.access_token = Some(response.access_token.clone());
        state.access_expires_at = response.expires_at(now);
        state.last_failure = None;
        state.grant_rejected = None;
        Ok(response.access_token)
    }

    /// Record the result of a first authorization.
    ///
    /// Refuses a response with no refresh token: without one the daemon would
    /// work for an hour and then need the browser again, which is precisely the
    /// behaviour it exists to avoid.
    pub async fn adopt_initial_grant(
        &self,
        response: &TokenResponse,
        now: DateTime<Utc>,
    ) -> Result<(), AuthFailure> {
        let Some(refresh) = response.refresh_token.as_deref() else {
            return Err(AuthFailure::Unexpected {
                status: 0,
                detail: "authorization returned no refresh token; offline access was not granted"
                    .into(),
            });
        };
        self.store.set(&self.account, refresh)?;

        let mut state = self.state.lock().await;
        state.access_token = Some(response.access_token.clone());
        state.access_expires_at = response.expires_at(now);
        state.grant_rejected = None;
        state.last_failure = None;
        Ok(())
    }

    /// Forget the stored grant. Only ever called because the user asked.
    pub async fn forget(&self) -> Result<(), AuthFailure> {
        self.store.delete(&self.account)?;
        let mut state = self.state.lock().await;
        *state = TokenState::default();
        Ok(())
    }

    /// Current health, without making a network request.
    pub async fn health(&self, configured: bool) -> AuthHealth {
        if !configured {
            return AuthHealth::Unconfigured;
        }
        let state = self.state.lock().await;

        if let Some(failure) = &state.grant_rejected {
            return AuthHealth::ReauthorizationRequired {
                failure: failure.clone(),
            };
        }
        match &state.last_failure {
            Some(AuthFailure::KeyringUnavailable { detail }) => {
                return AuthHealth::KeyringUnavailable {
                    detail: detail.clone(),
                };
            }
            Some(AuthFailure::NotAuthorized) => return AuthHealth::NotAuthorized,
            Some(failure) if state.access_token.is_none() => {
                return AuthHealth::Degraded {
                    failure: failure.clone(),
                };
            }
            _ => {}
        }
        if state.access_token.is_some() {
            return AuthHealth::Ready {
                access_expires_at: state.access_expires_at,
            };
        }
        match self.store.get(&self.account) {
            Ok(Some(_)) => AuthHealth::Ready {
                access_expires_at: None,
            },
            Ok(None) => AuthHealth::NotAuthorized,
            Err(AuthFailure::KeyringUnavailable { detail }) => {
                AuthHealth::KeyringUnavailable { detail }
            }
            Err(failure) => AuthHealth::Degraded { failure },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::secrets::MemoryStore;
    use std::sync::Mutex as StdMutex;

    /// A scripted token endpoint: each call pops the next queued outcome.
    struct ScriptedEndpoint {
        outcomes: StdMutex<Vec<Result<TokenResponse, AuthFailure>>>,
        seen: StdMutex<Vec<String>>,
    }

    impl ScriptedEndpoint {
        fn new(outcomes: Vec<Result<TokenResponse, AuthFailure>>) -> Self {
            let mut reversed = outcomes;
            reversed.reverse();
            Self {
                outcomes: StdMutex::new(reversed),
                seen: StdMutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl TokenEndpoint for ScriptedEndpoint {
        fn refresh<'a>(
            &'a self,
            refresh_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, AuthFailure>> + Send + 'a>> {
            self.seen.lock().unwrap().push(refresh_token.to_string());
            let next = self
                .outcomes
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(AuthFailure::Transient {
                    detail: "script exhausted".into(),
                }));
            Box::pin(async move { next })
        }
    }

    fn token(access: &str, refresh: Option<&str>, expires_in: i64) -> TokenResponse {
        TokenResponse {
            access_token: access.into(),
            refresh_token: refresh.map(str::to_string),
            expires_in: Some(expires_in),
            scope: None,
            token_type: Some("Bearer".into()),
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn manager(
        store: MemoryStore,
        endpoint: ScriptedEndpoint,
    ) -> TokenManager<MemoryStore, ScriptedEndpoint> {
        TokenManager::new("work", store, endpoint, Duration::from_secs(120))
    }

    #[tokio::test]
    async fn a_cached_access_token_is_reused_rather_than_renewed() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store,
            ScriptedEndpoint::new(vec![Ok(token("access-1", None, 3600))]),
        );

        assert_eq!(manager.access_token(now()).await.unwrap(), "access-1");
        assert_eq!(manager.access_token(now()).await.unwrap(), "access-1");
        assert_eq!(
            manager.endpoint.calls().len(),
            1,
            "renewed despite a valid cached token"
        );
    }

    #[tokio::test]
    async fn a_token_inside_the_skew_window_is_renewed_early() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store,
            ScriptedEndpoint::new(vec![
                Ok(token("access-1", None, 60)),
                Ok(token("access-2", None, 3600)),
            ]),
        );

        assert_eq!(manager.access_token(now()).await.unwrap(), "access-1");
        // 60s of life left against a 120s skew: renew rather than risk it.
        assert_eq!(manager.access_token(now()).await.unwrap(), "access-2");
    }

    #[tokio::test]
    async fn a_refresh_without_a_replacement_keeps_the_stored_token() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store.clone(),
            ScriptedEndpoint::new(vec![Ok(token("access-1", None, 3600))]),
        );

        manager.access_token(now()).await.unwrap();
        assert_eq!(store.peek("work").as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_is_persisted_before_it_is_relied_on() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store.clone(),
            ScriptedEndpoint::new(vec![
                Ok(token("access-1", Some("refresh-2"), 1)),
                Ok(token("access-2", None, 3600)),
            ]),
        );

        manager.access_token(now()).await.unwrap();
        assert_eq!(store.peek("work").as_deref(), Some("refresh-2"));

        // The next renewal must use the rotated token, not the original.
        manager.access_token(now()).await.unwrap();
        assert_eq!(manager.endpoint.calls(), vec!["refresh-1", "refresh-2"]);
    }

    #[tokio::test]
    async fn a_transient_failure_keeps_the_stored_token_and_stays_retryable() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store.clone(),
            ScriptedEndpoint::new(vec![
                Err(AuthFailure::Transient {
                    detail: "offline".into(),
                }),
                Ok(token("access-1", None, 3600)),
            ]),
        );

        assert!(manager.access_token(now()).await.is_err());
        assert_eq!(store.peek("work").as_deref(), Some("refresh-1"));
        assert_eq!(manager.access_token(now()).await.unwrap(), "access-1");
    }

    #[tokio::test]
    async fn throttling_keeps_the_stored_token_and_never_asks_for_consent() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store.clone(),
            ScriptedEndpoint::new(vec![Err(AuthFailure::Throttled {
                status: 429,
                retry_after_secs: Some(20),
            })]),
        );

        manager.access_token(now()).await.unwrap_err();
        assert_eq!(store.peek("work").as_deref(), Some("refresh-1"));
        let health = manager.health(true).await;
        assert!(
            !matches!(health, AuthHealth::ReauthorizationRequired { .. }),
            "throttling asked for reauthorization: {health:?}"
        );
    }

    #[tokio::test]
    async fn a_rejected_grant_reports_reauthorization_and_stops_re_asking() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store,
            ScriptedEndpoint::new(vec![Err(AuthFailure::GrantInvalid {
                code: "invalid_grant".into(),
                subtype: None,
            })]),
        );

        manager.access_token(now()).await.unwrap_err();
        assert!(matches!(
            manager.health(true).await,
            AuthHealth::ReauthorizationRequired { .. }
        ));

        // A second call must not produce a second request to the provider.
        manager.access_token(now()).await.unwrap_err();
        assert_eq!(
            manager.endpoint.calls().len(),
            1,
            "kept retrying a rejected grant"
        );
    }

    #[tokio::test]
    async fn a_locked_keyring_reports_itself_and_never_reauthorization() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        store.set_locked(true);
        let manager = manager(store.clone(), ScriptedEndpoint::new(vec![]));

        let failure = manager.access_token(now()).await.unwrap_err();
        assert!(failure.preserves_credentials());
        assert!(matches!(
            manager.health(true).await,
            AuthHealth::KeyringUnavailable { .. }
        ));
        assert!(
            manager.endpoint.calls().is_empty(),
            "contacted the provider without a token"
        );

        // Unlocking restores service with no new consent.
        store.set_locked(false);
        let manager = TokenManager::new(
            "work",
            store,
            ScriptedEndpoint::new(vec![Ok(token("access-1", None, 3600))]),
            Duration::from_secs(120),
        );
        assert_eq!(manager.access_token(now()).await.unwrap(), "access-1");
    }

    #[tokio::test]
    async fn an_account_that_never_authorized_is_distinguished_from_a_broken_one() {
        let manager = manager(MemoryStore::new(), ScriptedEndpoint::new(vec![]));
        assert_eq!(
            manager.access_token(now()).await.unwrap_err(),
            AuthFailure::NotAuthorized
        );
        assert_eq!(manager.health(true).await, AuthHealth::NotAuthorized);
    }

    #[tokio::test]
    async fn an_unconfigured_client_is_reported_before_anything_is_read() {
        let manager = manager(MemoryStore::new(), ScriptedEndpoint::new(vec![]));
        assert_eq!(manager.health(false).await, AuthHealth::Unconfigured);
    }

    #[tokio::test]
    async fn an_initial_grant_without_a_refresh_token_is_refused() {
        let store = MemoryStore::new();
        let manager = manager(store.clone(), ScriptedEndpoint::new(vec![]));
        let response = token("access-1", None, 3600);

        assert!(manager.adopt_initial_grant(&response, now()).await.is_err());
        assert_eq!(
            store.peek("work"),
            None,
            "stored a grant that cannot be renewed"
        );
    }

    #[tokio::test]
    async fn an_initial_grant_is_stored_and_immediately_usable() {
        let store = MemoryStore::new();
        let manager = manager(store.clone(), ScriptedEndpoint::new(vec![]));
        let response = token("access-1", Some("refresh-1"), 3600);

        manager.adopt_initial_grant(&response, now()).await.unwrap();
        assert_eq!(store.peek("work").as_deref(), Some("refresh-1"));
        assert_eq!(manager.access_token(now()).await.unwrap(), "access-1");
    }

    #[tokio::test]
    async fn reauthorizing_clears_a_previously_rejected_grant() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store,
            ScriptedEndpoint::new(vec![Err(AuthFailure::GrantInvalid {
                code: "invalid_grant".into(),
                subtype: None,
            })]),
        );
        manager.access_token(now()).await.unwrap_err();

        manager
            .adopt_initial_grant(&token("access-9", Some("refresh-9"), 3600), now())
            .await
            .unwrap();

        assert!(manager.health(true).await.is_ready());
        assert_eq!(manager.access_token(now()).await.unwrap(), "access-9");
    }

    #[tokio::test]
    async fn a_new_grant_is_used_after_a_rejection() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store.clone(),
            ScriptedEndpoint::new(vec![
                Err(AuthFailure::GrantInvalid {
                    code: "invalid_grant".into(),
                    subtype: None,
                }),
                Ok(token("access-2", None, 3600)),
            ]),
        );
        manager.access_token(now()).await.unwrap_err();

        store.set("work", "refresh-2").unwrap();
        manager.clear_grant_rejection().await;

        assert_eq!(manager.access_token(now()).await.unwrap(), "access-2");
        assert_eq!(manager.endpoint.calls(), vec!["refresh-1", "refresh-2"]);
    }

    #[tokio::test]
    async fn a_stale_token_does_not_loop() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = manager(
            store,
            ScriptedEndpoint::new(vec![
                Err(AuthFailure::GrantInvalid {
                    code: "invalid_grant".into(),
                    subtype: None,
                }),
                Err(AuthFailure::GrantInvalid {
                    code: "invalid_grant".into(),
                    subtype: None,
                }),
            ]),
        );
        manager.access_token(now()).await.unwrap_err();

        manager.clear_grant_rejection().await;
        assert!(matches!(
            manager.access_token(now()).await.unwrap_err(),
            AuthFailure::GrantInvalid { .. }
        ));
        assert_eq!(manager.endpoint.calls().len(), 2);

        manager.access_token(now()).await.unwrap_err();
        manager.access_token(now()).await.unwrap_err();
        assert_eq!(
            manager.endpoint.calls(),
            vec!["refresh-1", "refresh-1"],
            "kept retrying a rejected grant after recovery"
        );
    }

    #[tokio::test]
    async fn concurrent_callers_renew_only_once() {
        let store = MemoryStore::new();
        store.set("work", "refresh-1").unwrap();
        let manager = std::sync::Arc::new(manager(
            store,
            ScriptedEndpoint::new(vec![
                Ok(token("access-1", Some("refresh-2"), 3600)),
                Ok(token("access-2", Some("refresh-3"), 3600)),
            ]),
        ));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let manager = manager.clone();
            handles.push(tokio::spawn(
                async move { manager.access_token(now()).await },
            ));
        }
        for handle in handles {
            assert_eq!(handle.await.unwrap().unwrap(), "access-1");
        }
        assert_eq!(
            manager.endpoint.calls().len(),
            1,
            "concurrent syncs raced the refresh"
        );
    }
}
