//! Storage for the long-lived refresh token.
//!
//! The store is behind a trait so the token lifecycle — rotation, retention on
//! a failed refresh — can be tested without a session bus or an unlocked
//! keyring, and so a locked keyring can be simulated rather than merely
//! reasoned about.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::error::AuthFailure;

/// Service name the credential is filed under in the secret store.
pub const KEYRING_SERVICE: &str = "calendar-daemon";

/// A place to keep one secret per account.
pub trait SecretStore: Send + Sync {
    /// The stored secret, or `None` when the account has never been authorized.
    fn get(&self, account: &str) -> Result<Option<String>, AuthFailure>;
    fn set(&self, account: &str, secret: &str) -> Result<(), AuthFailure>;
    /// Remove the secret. Succeeds when there was nothing to remove.
    fn delete(&self, account: &str) -> Result<(), AuthFailure>;
    /// A one-line description for `status`, naming the backend in use.
    fn describe(&self) -> String;
}

/// The desktop secret store, reached over the Secret Service D-Bus interface.
///
/// Connecting is deferred and retried rather than done once at startup. A
/// session whose keyring is still locked, or whose secret service has not come
/// up yet, must not stop the daemon: it starts, reports the condition through
/// `status`, and succeeds on the first operation after the keyring unlocks —
/// with no new consent, because the grant was never in question.
#[derive(Default)]
pub struct KeyringStore {
    connected: std::sync::Mutex<bool>,
}

impl KeyringStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the Secret Service backend as the process-wide default,
    /// retrying on every call until it succeeds once.
    fn ensure_connected(&self) -> Result<(), AuthFailure> {
        let mut connected = self.connected.lock().expect("keyring store mutex");
        if *connected {
            return Ok(());
        }
        let store = zbus_secret_service_keyring_store::Store::new().map_err(map_keyring_error)?;
        keyring_core::set_default_store(store);
        *connected = true;
        Ok(())
    }

    /// Whether a connection has been established at least once.
    pub fn is_connected(&self) -> bool {
        *self.connected.lock().expect("keyring store mutex")
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>, AuthFailure> {
        self.ensure_connected()?;
        let entry = entry_for(account)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(err) => Err(map_keyring_error(err)),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), AuthFailure> {
        self.ensure_connected()?;
        entry_for(account)?
            .set_password(secret)
            .map_err(map_keyring_error)
    }

    fn delete(&self, account: &str) -> Result<(), AuthFailure> {
        self.ensure_connected()?;
        match entry_for(account)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(err) => Err(map_keyring_error(err)),
        }
    }

    fn describe(&self) -> String {
        let state = if self.is_connected() {
            "connected"
        } else {
            "not yet reached"
        };
        format!("secret service (service={KEYRING_SERVICE}, {state})")
    }
}

fn entry_for(account: &str) -> Result<keyring_core::Entry, AuthFailure> {
    keyring_core::Entry::new(KEYRING_SERVICE, account).map_err(map_keyring_error)
}

/// Map a secret-store error onto this daemon's failure taxonomy.
///
/// `NoStorageAccess` is what the Secret Service backend reports for a locked
/// collection, and it is the case that must never be mistaken for a revoked
/// grant: the token is there, the session simply cannot read it yet.
fn map_keyring_error(err: keyring_core::Error) -> AuthFailure {
    use keyring_core::Error as E;
    match err {
        E::NoEntry => AuthFailure::NotAuthorized,
        E::NoStorageAccess(inner) => AuthFailure::KeyringUnavailable {
            detail: format!("storage locked or inaccessible: {inner}"),
        },
        E::NoDefaultStore => AuthFailure::KeyringUnavailable {
            detail: "no secret store is available in this session".into(),
        },
        E::PlatformFailure(inner) => AuthFailure::KeyringUnavailable {
            detail: format!("secret store failure: {inner}"),
        },
        E::Ambiguous(_) => AuthFailure::KeyringUnavailable {
            detail: "more than one stored credential matches this account".into(),
        },
        other => AuthFailure::KeyringUnavailable {
            detail: other.to_string(),
        },
    }
}

/// An in-process store used by tests, able to pretend it is locked.
#[derive(Default, Clone)]
pub struct MemoryStore {
    inner: Arc<Mutex<MemoryState>>,
}

#[derive(Default)]
struct MemoryState {
    secrets: HashMap<String, String>,
    locked: bool,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every operation fail the way a locked keyring does.
    pub fn set_locked(&self, locked: bool) {
        self.inner.lock().expect("memory store mutex").locked = locked;
    }

    pub fn peek(&self, account: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("memory store mutex")
            .secrets
            .get(account)
            .cloned()
    }

    fn guard(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, AuthFailure> {
        let guard = self.inner.lock().expect("memory store mutex");
        if guard.locked {
            return Err(AuthFailure::KeyringUnavailable {
                detail: "test store is locked".into(),
            });
        }
        Ok(guard)
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> Result<Option<String>, AuthFailure> {
        Ok(self.guard()?.secrets.get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), AuthFailure> {
        self.guard()?
            .secrets
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), AuthFailure> {
        self.guard()?.secrets.remove(account);
        Ok(())
    }

    fn describe(&self) -> String {
        "in-memory (tests only)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_account_reads_as_absent_not_as_an_error() {
        let store = MemoryStore::new();
        assert_eq!(store.get("nobody").unwrap(), None);
    }

    #[test]
    fn a_locked_store_reports_unavailability_and_keeps_the_secret() {
        let store = MemoryStore::new();
        store.set("work", "refresh-token").unwrap();
        store.set_locked(true);

        let err = store.get("work").unwrap_err();
        assert!(err.preserves_credentials());
        assert!(!err.needs_user_authorization());

        store.set_locked(false);
        assert_eq!(store.get("work").unwrap().as_deref(), Some("refresh-token"));
    }

    #[test]
    fn a_keyring_that_cannot_be_reached_reports_unavailability_not_a_lost_grant() {
        // This runs wherever the test suite does, with or without a session
        // bus. Either outcome is acceptable; what must never happen is a
        // failure that reads as a revoked authorization.
        let store = KeyringStore::new();
        if let Err(failure) = store.get("nonexistent-test-account") {
            assert!(failure.preserves_credentials(), "{failure:?}");
            assert!(!failure.needs_user_authorization(), "{failure:?}");
            assert!(failure.is_retryable(), "{failure:?}");
        }
    }

    #[test]
    fn deleting_an_absent_account_is_not_an_error() {
        MemoryStore::new().delete("nobody").unwrap();
    }
}
