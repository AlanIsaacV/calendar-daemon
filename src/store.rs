//! Local state: the last good calendar snapshot, the ledger of reminders
//! already delivered, and the status other commands read.
//!
//! Two properties matter here. A failed sync must never destroy the previous
//! snapshot — an offline laptop should keep showing yesterday's calendar rather
//! than an empty one. And a restart must not re-fire reminders that already
//! went out, which is what the ledger is for.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::AuthHealth;
use crate::model::{CalendarRef, Occurrence};
use crate::util;

/// How long a delivered-reminder record is kept after its event started.
///
/// Long enough that a machine off for a week does not replay reminders on
/// return, short enough that the ledger cannot grow without bound.
const LEDGER_RETENTION_DAYS: i64 = 30;

/// Format version, so a future change can migrate rather than misread.
const STATE_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is not valid state: {source}")]
    Decode {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("another calendar-daemon already owns {path}")]
    AlreadyRunning { path: PathBuf },
}

/// The last successful read of the provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default = "default_version")]
    pub version: u32,
    pub fetched_at: DateTime<Utc>,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub calendars: Vec<CalendarRef>,
    pub occurrences: Vec<Occurrence>,
    /// Collection directory names the export wrote for this snapshot.
    #[serde(default)]
    pub collections: BTreeSet<String>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

impl Snapshot {
    pub fn empty(now: DateTime<Utc>) -> Self {
        Self {
            version: STATE_VERSION,
            fetched_at: now,
            window_start: now,
            window_end: now,
            calendars: Vec::new(),
            occurrences: Vec::new(),
            collections: BTreeSet::new(),
        }
    }

    /// How stale this data is, which `status` reports so offline operation is
    /// visible rather than silent.
    pub fn age(&self, now: DateTime<Utc>) -> Duration {
        now - self.fetched_at
    }

    pub fn calendar(&self, id: &str) -> Option<&CalendarRef> {
        self.calendars.iter().find(|c| c.id == id)
    }
}

/// One reminder that has already been dealt with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub at: DateTime<Utc>,
    /// The occurrence start, used to expire the record.
    pub event_start: DateTime<Utc>,
    /// True when the reminder was recorded without notifying because it was
    /// already too late to be useful.
    #[serde(default)]
    pub suppressed: bool,
}

/// Reminders already delivered or deliberately skipped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: BTreeMap<String, Delivery>,
}

impl Ledger {
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub fn record(
        &mut self,
        key: &str,
        now: DateTime<Utc>,
        event_start: DateTime<Utc>,
        suppressed: bool,
    ) {
        self.entries.insert(
            key.to_string(),
            Delivery {
                at: now,
                event_start,
                suppressed,
            },
        );
    }

    /// Drop records for events far enough in the past that they cannot recur.
    pub fn prune(&mut self, now: DateTime<Utc>) -> usize {
        let horizon = now - Duration::days(LEDGER_RETENTION_DAYS);
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.event_start > horizon);
        before - self.entries.len()
    }
}

/// What `status` reports, persisted so a one-shot command can read it without
/// the daemon running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_sync: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_reminder: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_reminder_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthHealth>,
    /// Category of the last failure, never its provider-supplied text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_summary: Option<String>,
    #[serde(default)]
    pub calendars: usize,
    #[serde(default)]
    pub events: usize,
    #[serde(default)]
    pub scheduled_reminders: usize,
    #[serde(default)]
    pub consecutive_failures: u32,
}

impl Default for StatusRecord {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            last_success: None,
            last_attempt: None,
            next_sync: None,
            next_reminder: None,
            next_reminder_summary: None,
            auth: None,
            last_error_category: None,
            last_error_summary: None,
            calendars: 0,
            events: 0,
            scheduled_reminders: 0,
            consecutive_failures: 0,
        }
    }
}

/// Reads and writes the daemon's state directory.
pub struct Store {
    snapshot_path: PathBuf,
    ledger_path: PathBuf,
    status_path: PathBuf,
}

impl Store {
    pub fn new(snapshot_path: PathBuf, ledger_path: PathBuf, status_path: PathBuf) -> Self {
        Self {
            snapshot_path,
            ledger_path,
            status_path,
        }
    }

    pub fn from_paths(paths: &crate::config::Paths) -> Self {
        Self::new(
            paths.snapshot_file(),
            paths.ledger_file(),
            paths.status_file(),
        )
    }

    /// The last good snapshot, or `None` when there has never been one.
    ///
    /// A snapshot that cannot be parsed is reported rather than silently
    /// replaced: overwriting it would throw away the only copy of the
    /// calendar the daemon has while offline.
    pub fn load_snapshot(&self) -> Result<Option<Snapshot>, StoreError> {
        read_json(&self.snapshot_path)
    }

    pub fn save_snapshot(&self, snapshot: &Snapshot) -> Result<(), StoreError> {
        write_json(&self.snapshot_path, snapshot)
    }

    /// The delivery ledger. A damaged ledger degrades to empty, because the
    /// cost of losing it is one duplicate notification, not a lost calendar.
    pub fn load_ledger(&self) -> Ledger {
        match read_json::<Ledger>(&self.ledger_path) {
            Ok(Some(ledger)) => ledger,
            Ok(None) => Ledger {
                version: STATE_VERSION,
                entries: BTreeMap::new(),
            },
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "reminder ledger could not be read; starting a new one, which may \
                     repeat at most one round of notifications"
                );
                Ledger {
                    version: STATE_VERSION,
                    entries: BTreeMap::new(),
                }
            }
        }
    }

    pub fn save_ledger(&self, ledger: &Ledger) -> Result<(), StoreError> {
        write_json(&self.ledger_path, ledger)
    }

    pub fn load_status(&self) -> StatusRecord {
        read_json::<StatusRecord>(&self.status_path)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    pub fn save_status(&self, status: &StatusRecord) -> Result<(), StoreError> {
        write_json(&self.status_path, status)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| StoreError::Decode {
            path: path.to_path_buf(),
            source,
        })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let text = serde_json::to_vec_pretty(value).map_err(|source| StoreError::Decode {
        path: path.to_path_buf(),
        source,
    })?;
    util::write_atomic(path, &text, util::FILE_MODE).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// An exclusive claim on the state directory, held for the life of a process.
///
/// One process owns writes and reminder scheduling. A second `run` fails fast
/// instead of producing two schedulers that each notify for the same event.
#[derive(Debug)]
pub struct DaemonLock {
    file: fs::File,
    path: PathBuf,
}

impl DaemonLock {
    pub fn acquire(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            util::ensure_private_dir(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;

        // `File::try_lock` is an advisory flock: it is released when this
        // process exits for any reason, including a kill, so a crash never
        // leaves the state directory permanently claimed.
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(StoreError::AlreadyRunning {
                    path: path.to_path_buf(),
                });
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(StoreError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }

        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Replace a snapshot's occurrences while keeping calendars whose fetch failed.
///
/// A cycle in which one calendar errored must not silently drop that calendar's
/// events from the export; the previous ones are carried forward and the caller
/// reports the partial result.
pub fn merge_partial(
    previous: Option<&Snapshot>,
    fetched: &BTreeMap<String, Vec<Occurrence>>,
    calendars: &[CalendarRef],
    failed: &BTreeSet<String>,
    now: DateTime<Utc>,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Snapshot {
    let mut occurrences: Vec<Occurrence> = Vec::new();

    for calendar in calendars {
        if let Some(fresh) = fetched.get(&calendar.id) {
            occurrences.extend(fresh.iter().cloned());
            continue;
        }
        if failed.contains(&calendar.id)
            && let Some(previous) = previous
        {
            occurrences.extend(
                previous
                    .occurrences
                    .iter()
                    .filter(|o| o.calendar_id == calendar.id)
                    .cloned(),
            );
        }
    }

    Snapshot {
        version: STATE_VERSION,
        // A partial cycle does not claim to be as fresh as a complete one.
        fetched_at: if failed.is_empty() {
            now
        } else {
            previous.map(|p| p.fetched_at).unwrap_or(now)
        },
        window_start,
        window_end,
        calendars: calendars.to_vec(),
        occurrences,
        collections: previous.map(|p| p.collections.clone()).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventStatus, EventTime, ReminderSpec, ResponseStatus};

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "calendar-daemon-store-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        util::ensure_private_dir(&dir).unwrap();
        dir
    }

    fn store(dir: &Path) -> Store {
        Store::new(
            dir.join("snapshot.json"),
            dir.join("reminders.json"),
            dir.join("status.json"),
        )
    }

    fn now() -> DateTime<Utc> {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn calendar(id: &str) -> CalendarRef {
        CalendarRef {
            id: id.into(),
            summary: id.into(),
            time_zone: Some("America/Bogota".into()),
            color: None,
            primary: false,
            selected: true,
            default_reminders: vec![],
        }
    }

    fn event(calendar_id: &str, id: &str) -> Occurrence {
        Occurrence {
            calendar_id: calendar_id.into(),
            id: id.into(),
            summary: "Standup".into(),
            location: None,
            meeting_url: None,
            html_link: None,
            start: EventTime::Timed {
                at: now(),
                zone: None,
            },
            end: EventTime::Timed {
                at: now(),
                zone: None,
            },
            status: EventStatus::Confirmed,
            response: ResponseStatus::Accepted,
            reminders: ReminderSpec::UseDefault,
            updated: None,
        }
    }

    #[test]
    fn an_absent_snapshot_is_reported_as_absent_not_as_an_error() {
        let dir = workspace("absent");
        assert!(store(&dir).load_snapshot().unwrap().is_none());
    }

    #[test]
    fn a_snapshot_round_trips_with_its_reminder_semantics_intact() {
        let dir = workspace("roundtrip");
        let store = store(&dir);

        let mut snapshot = Snapshot::empty(now());
        snapshot.calendars = vec![calendar("work")];
        let mut occurrence = event("work", "e1");
        occurrence.reminders = ReminderSpec::Disabled;
        snapshot.occurrences = vec![occurrence];

        store.save_snapshot(&snapshot).unwrap();
        let loaded = store.load_snapshot().unwrap().unwrap();

        assert_eq!(loaded.occurrences[0].reminders, ReminderSpec::Disabled);
        assert_eq!(loaded.calendars[0].id, "work");
    }

    #[test]
    fn a_damaged_snapshot_is_reported_rather_than_silently_discarded() {
        let dir = workspace("damaged");
        fs::write(dir.join("snapshot.json"), "{not json").unwrap();
        assert!(store(&dir).load_snapshot().is_err());
    }

    #[test]
    fn a_damaged_ledger_degrades_to_empty_instead_of_stopping_the_daemon() {
        let dir = workspace("ledger-damaged");
        fs::write(dir.join("reminders.json"), "{not json").unwrap();
        assert!(store(&dir).load_ledger().entries.is_empty());
    }

    #[test]
    fn the_ledger_survives_a_restart_so_reminders_are_not_repeated() {
        let dir = workspace("ledger");
        let store = store(&dir);

        let mut ledger = store.load_ledger();
        ledger.record("cal|evt|123|10", now(), now(), false);
        store.save_ledger(&ledger).unwrap();

        let reloaded = store.load_ledger();
        assert!(reloaded.contains("cal|evt|123|10"));
    }

    #[test]
    fn old_ledger_entries_are_pruned_and_recent_ones_kept() {
        let mut ledger = Ledger::default();
        ledger.record("old", now(), now() - Duration::days(40), false);
        ledger.record("recent", now(), now() - Duration::days(2), false);

        assert_eq!(ledger.prune(now()), 1);
        assert!(!ledger.contains("old"));
        assert!(ledger.contains("recent"));
    }

    #[test]
    fn state_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workspace("perms");
        let store = store(&dir);
        store.save_snapshot(&Snapshot::empty(now())).unwrap();

        let mode = fs::metadata(dir.join("snapshot.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn only_one_process_can_own_the_state_directory() {
        let dir = workspace("lock");
        let path = dir.join("daemon.lock");

        let first = DaemonLock::acquire(&path).unwrap();
        let second = DaemonLock::acquire(&path);
        assert!(
            matches!(second, Err(StoreError::AlreadyRunning { .. })),
            "a second daemon acquired the lock: {second:?}"
        );

        drop(first);
        assert!(
            DaemonLock::acquire(&path).is_ok(),
            "the lock was not released"
        );
    }

    #[test]
    fn a_failed_calendar_keeps_its_previous_events_rather_than_losing_them() {
        let mut previous = Snapshot::empty(now() - Duration::hours(6));
        previous.calendars = vec![calendar("work"), calendar("personal")];
        previous.occurrences = vec![event("work", "w1"), event("personal", "p1")];

        let mut fetched = BTreeMap::new();
        fetched.insert("work".to_string(), vec![event("work", "w2")]);
        let failed: BTreeSet<String> = ["personal".to_string()].into_iter().collect();

        let merged = merge_partial(
            Some(&previous),
            &fetched,
            &[calendar("work"), calendar("personal")],
            &failed,
            now(),
            now(),
            now(),
        );

        let ids: Vec<&str> = merged.occurrences.iter().map(|o| o.id.as_str()).collect();
        assert!(ids.contains(&"w2"), "fresh events missing");
        assert!(
            ids.contains(&"p1"),
            "a failed calendar lost its previous events"
        );
        assert!(
            !ids.contains(&"w1"),
            "stale events kept for a calendar that succeeded"
        );
    }

    #[test]
    fn a_partial_cycle_does_not_claim_to_be_fresh() {
        let previous = Snapshot::empty(now() - Duration::hours(6));
        let failed: BTreeSet<String> = ["work".to_string()].into_iter().collect();

        let merged = merge_partial(
            Some(&previous),
            &BTreeMap::new(),
            &[calendar("work")],
            &failed,
            now(),
            now(),
            now(),
        );

        assert_eq!(
            merged.fetched_at, previous.fetched_at,
            "a partial sync reported itself as a complete refresh"
        );
    }

    #[test]
    fn a_complete_cycle_marks_itself_fresh() {
        let previous = Snapshot::empty(now() - Duration::hours(6));
        let mut fetched = BTreeMap::new();
        fetched.insert("work".to_string(), vec![event("work", "w1")]);

        let merged = merge_partial(
            Some(&previous),
            &fetched,
            &[calendar("work")],
            &BTreeSet::new(),
            now(),
            now(),
            now(),
        );

        assert_eq!(merged.fetched_at, now());
        assert_eq!(merged.age(now()), Duration::zero());
    }

    #[test]
    fn status_round_trips_and_survives_an_absent_file() {
        let dir = workspace("status");
        let store = store(&dir);
        assert_eq!(store.load_status().consecutive_failures, 0);

        let status = StatusRecord {
            consecutive_failures: 3,
            last_error_category: Some("transient".into()),
            auth: Some(AuthHealth::NotAuthorized),
            ..Default::default()
        };
        store.save_status(&status).unwrap();

        let loaded = store.load_status();
        assert_eq!(loaded.consecutive_failures, 3);
        assert_eq!(loaded.auth, Some(AuthHealth::NotAuthorized));
    }
}
