//! The sync cycle and the process that runs it.
//!
//! Two timers, deliberately independent. The sync timer decides how often the
//! provider is asked what changed; the reminder timer decides when a local
//! notification fires. An event loaded at 06:00 with a reminder at 08:50 is
//! notified at 08:50 whether or not a sync happens in between.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;

use crate::auth::{AuthFailure, SecretStore, TokenEndpoint, TokenManager};
use crate::config::Config;
use crate::control::{self, Request, Response};
use crate::export;
use crate::google::{ApiError, CalendarProvider};
use crate::model::CalendarRef;
use crate::notify::Notifier;
use crate::reminder::{self, Plan};
use crate::store::{Ledger, Snapshot, StatusRecord, Store, merge_partial};

/// What one sync cycle achieved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub calendars: usize,
    pub events: usize,
    /// At least one calendar failed and kept its previous events.
    pub partial: bool,
    pub failed: BTreeSet<String>,
    pub written: usize,
    pub removed: usize,
}

/// Choose which calendars to synchronize.
///
/// An empty list in configuration means every calendar the account has ticked
/// in Google, which is the set the user already curates there.
pub fn select_calendars(all: Vec<CalendarRef>, wanted: &[String]) -> Vec<CalendarRef> {
    if wanted.is_empty() {
        return all.into_iter().filter(|c| c.selected).collect();
    }
    let wanted: BTreeSet<&str> = wanted.iter().map(String::as_str).collect();
    all.into_iter()
        .filter(|c| wanted.contains(c.id.as_str()))
        .collect()
}

/// Run one complete synchronization: fetch, merge, persist, export.
///
/// A calendar that fails keeps the events the last good cycle had for it, and
/// the outcome says the result was partial. Only a failure to list calendars at
/// all aborts the cycle, because without that list there is nothing to merge
/// against.
pub async fn run_sync_cycle<P: CalendarProvider + ?Sized>(
    provider: &P,
    store: &Store,
    config: &Config,
    export_dir: &Path,
    now: DateTime<Utc>,
) -> Result<SyncOutcome, ApiError> {
    let window_start = now - to_chrono(config.sync.window_past);
    let window_end = now + to_chrono(config.sync.window_future);

    let calendars = select_calendars(provider.list_calendars().await?, &config.sync.calendars);

    let previous = match store.load_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            // A snapshot that cannot be read is left on disk rather than
            // overwritten, but the cycle still proceeds with fresh data.
            tracing::warn!(error = %err, "previous snapshot unreadable; continuing without it");
            None
        }
    };

    let mut fetched: BTreeMap<String, Vec<crate::model::Occurrence>> = BTreeMap::new();
    let mut failed: BTreeSet<String> = BTreeSet::new();

    for calendar in &calendars {
        let result = provider
            .list_events(
                &calendar.id,
                window_start,
                window_end,
                config.sync.max_events_per_calendar,
            )
            .await;

        match result {
            Ok(events) => {
                fetched.insert(calendar.id.clone(), events);
            }
            Err(err) => {
                // A rejected grant is not a per-calendar problem: every later
                // calendar would fail the same way, and the daemon should
                // surface it once instead of once per calendar.
                if !err.preserves_credentials() {
                    return Err(err);
                }
                tracing::warn!(
                    calendar = %calendar.id,
                    category = err.category(),
                    "calendar could not be read; keeping its previous events"
                );
                failed.insert(calendar.id.clone());
            }
        }
    }

    let mut snapshot = merge_partial(
        previous.as_ref(),
        &fetched,
        &calendars,
        &failed,
        now,
        window_start,
        window_end,
    );

    let mut written = 0usize;
    let mut removed = 0usize;

    if config.export.enabled {
        let previous_collections = previous
            .as_ref()
            .map(|p| p.collections.clone())
            .unwrap_or_else(|| export::discover_collections(export_dir));

        match export::write_vdir(
            export_dir,
            &snapshot.calendars,
            &snapshot.occurrences,
            &previous_collections,
            config.export.write_color,
        ) {
            Ok(report) => {
                written = report.written.len();
                removed = report.removed.len();
                snapshot.collections = snapshot
                    .calendars
                    .iter()
                    .map(CalendarRef::collection_name)
                    .collect();
            }
            Err(err) => {
                // The calendar data is good; only publishing it failed. Keep
                // the snapshot so reminders still work.
                tracing::error!(error = %err, "calendar export failed");
                snapshot.collections = previous_collections;
            }
        }
    }

    let outcome = SyncOutcome {
        calendars: snapshot.calendars.len(),
        events: snapshot.occurrences.len(),
        partial: !failed.is_empty(),
        failed,
        written,
        removed,
    };

    store
        .save_snapshot(&snapshot)
        .map_err(|err| ApiError::Decode(format!("state could not be written: {err}")))?;

    Ok(outcome)
}

/// Deliver every reminder that is due, recording each one exactly once.
///
/// The ledger is written whether a reminder was shown or deliberately skipped,
/// so neither a restart nor the next pass revisits it.
pub fn deliver_due<N: Notifier + ?Sized>(
    snapshot: &Snapshot,
    ledger: &mut Ledger,
    notifier: &N,
    config: &Config,
    now: DateTime<Utc>,
) -> Plan {
    let scheduled = reminder::collect(
        &snapshot.calendars,
        &snapshot.occurrences,
        config.sync.skip_declined,
    );
    let plan = reminder::plan(&scheduled, ledger, now, &config.reminders);

    for due in &plan.due {
        let late = now - due.fire_at > Duration::seconds(30);
        let zone = snapshot
            .calendar(&due.calendar_id)
            .map(CalendarRef::zone)
            .unwrap_or(chrono_tz::UTC);

        match notifier.notify(due, late, zone) {
            Ok(()) => {
                ledger.record(&due.key, now, due.event_start, false);
            }
            Err(err) => {
                // Not recorded: a notification the session refused should be
                // retried on the next pass rather than silently lost.
                tracing::warn!(error = %err, summary = %due.summary, "notification failed");
            }
        }
    }

    for (skipped, reason) in &plan.suppressed {
        tracing::info!(
            summary = %skipped.summary,
            reason = reason.as_str(),
            "reminder recorded without notifying"
        );
        ledger.record(&skipped.key, now, skipped.event_start, true);
    }

    plan
}

/// Count of reminders suppressed for each reason, for logging.
pub fn suppression_summary(plan: &Plan) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for (_, reason) in &plan.suppressed {
        *counts.entry(reason.as_str()).or_insert(0) += 1;
    }
    counts
}

fn to_chrono(duration: StdDuration) -> Duration {
    Duration::from_std(duration).unwrap_or_else(|_| Duration::days(365))
}

/// How recently a finished sync still satisfies an incoming request.
///
/// Requests that arrive while a cycle is running queue in the listener backlog
/// and are accepted the moment it finishes. Without this they would each start
/// another full cycle over data that is already a fraction of a second old.
const COALESCE_WINDOW: Duration = Duration::milliseconds(3_000);

/// Everything the running process needs.
pub struct Daemon<P, N, S, E>
where
    P: CalendarProvider + ?Sized,
    N: Notifier + ?Sized,
    S: SecretStore,
    E: TokenEndpoint,
{
    pub config: Config,
    pub paths: crate::config::Paths,
    pub store: Store,
    pub provider: Arc<P>,
    pub notifier: Arc<N>,
    pub tokens: Arc<TokenManager<S, E>>,
    /// Reported in `status` without the daemon making a network request.
    pub auth_health: Arc<tokio::sync::Mutex<crate::auth::AuthHealth>>,
}

impl<P, N, S, E> Daemon<P, N, S, E>
where
    P: CalendarProvider + ?Sized,
    N: Notifier + ?Sized,
    S: SecretStore,
    E: TokenEndpoint,
{
    /// Run until asked to stop.
    ///
    /// Waits on whichever comes first: a control request, the next reminder
    /// deadline, the next sync deadline, or a termination signal.
    pub async fn run(&self, listener: tokio::net::UnixListener) -> anyhow::Result<()> {
        let mut ledger = self.store.load_ledger();
        ledger.prune(Utc::now());

        let mut status = self.store.load_status();
        let mut next_sync = if self.config.sync.on_start {
            Some(Utc::now())
        } else if self.config.sync.periodic {
            Some(Utc::now() + to_chrono(self.config.sync.interval))
        } else {
            None
        };
        let mut backoff: Option<StdDuration> = None;
        let mut last_complete_sync: Option<(DateTime<Utc>, SyncOutcome)> = None;

        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        loop {
            let now = Utc::now();

            // Sync first, so reminders are planned against the freshest data.
            if next_sync.is_some_and(|due| due <= now) {
                let outcome = self.sync_once(&mut status, now).await;
                match outcome {
                    Ok(outcome) => {
                        backoff = None;
                        last_complete_sync = Some((Utc::now(), outcome));
                        next_sync = self
                            .config
                            .sync
                            .periodic
                            .then(|| Utc::now() + to_chrono(self.config.sync.interval));
                    }
                    Err(err) => {
                        let wait = self.next_backoff(backoff, &err);
                        backoff = Some(wait);
                        next_sync = match &err {
                            // Nothing retried on a timer will fix a grant the
                            // provider rejected; wait for the user instead.
                            err if err
                                .auth_failure()
                                .is_some_and(|f| f.needs_user_authorization()) =>
                            {
                                None
                            }
                            _ => Some(Utc::now() + to_chrono(wait)),
                        };
                    }
                }
                status.next_sync = next_sync;
                let _ = self.store.save_status(&status);
            }

            let now = Utc::now();
            let snapshot = self
                .store
                .load_snapshot()
                .ok()
                .flatten()
                .unwrap_or_else(|| Snapshot::empty(now));

            let plan = deliver_due(&snapshot, &mut ledger, &*self.notifier, &self.config, now);
            if !plan.due.is_empty() || !plan.suppressed.is_empty() {
                if let Err(err) = self.store.save_ledger(&ledger) {
                    tracing::error!(error = %err, "reminder ledger could not be saved");
                }
                let suppressed = suppression_summary(&plan);
                if !suppressed.is_empty() {
                    tracing::info!(?suppressed, "past-due reminders recorded without notifying");
                }
            }

            status.next_reminder = plan.next_deadline;
            status.next_reminder_summary = plan.next_summary.clone();
            status.scheduled_reminders = plan.pending;
            status.auth = Some(self.auth_health.lock().await.clone());
            let _ = self.store.save_status(&status);

            let sleep = self.sleep_until_next(&plan, next_sync, Utc::now());

            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((mut stream, _)) => {
                            let request = control::read_request(&mut stream).await;
                            let response = match request {
                                Ok(Request::Sync) => {
                                    let now = Utc::now();
                                    // Join the cycle that just finished rather
                                    // than running an identical one.
                                    if let Some(recent) = last_complete_sync
                                        .as_ref()
                                        .filter(|(at, _)| now - *at < COALESCE_WINDOW)
                                    {
                                        let outcome = recent.1.clone();
                                        let _ = control::write_response(
                                            &mut stream,
                                            &Response::Synced {
                                                calendars: outcome.calendars,
                                                events: outcome.events,
                                                coalesced: true,
                                                partial: outcome.partial,
                                            },
                                        )
                                        .await;
                                        continue;
                                    }
                                    if self.tokens.clear_grant_rejection().await {
                                        tracing::info!(
                                            "sync requested after a rejected grant; \
                                             retrying with the stored authorization"
                                        );
                                    }
                                    match self.sync_once(&mut status, now).await {
                                        Ok(outcome) => {
                                            backoff = None;
                                            last_complete_sync =
                                                Some((Utc::now(), outcome.clone()));
                                            next_sync = self.config.sync.periodic.then(|| {
                                                Utc::now() + to_chrono(self.config.sync.interval)
                                            });
                                            status.next_sync = next_sync;
                                            Response::Synced {
                                                calendars: outcome.calendars,
                                                events: outcome.events,
                                                coalesced: false,
                                                partial: outcome.partial,
                                            }
                                        }
                                        Err(err) => {
                                            if err.auth_failure().is_none() {
                                                let health = self
                                                    .tokens
                                                    .health(self.config.has_oauth_client())
                                                    .await;
                                                *self.auth_health.lock().await = health;
                                            }
                                            let keyring_unavailable = matches!(
                                                err.auth_failure(),
                                                Some(AuthFailure::KeyringUnavailable { .. })
                                            );
                                            if !keyring_unavailable {
                                                let wait = self.next_backoff(backoff, &err);
                                                backoff = Some(wait);
                                                next_sync = if err
                                                    .auth_failure()
                                                    .is_some_and(|f| f.needs_user_authorization())
                                                {
                                                    None
                                                } else {
                                                    Some(Utc::now() + to_chrono(wait))
                                                };
                                                status.next_sync = next_sync;
                                            }
                                            Response::SyncFailed {
                                                category: err.category().to_string(),
                                                message: err.to_string(),
                                            }
                                        }
                                    }
                                }
                                Ok(Request::Status) => {
                                    status.auth = Some(self.auth_health.lock().await.clone());
                                    Response::Status(Box::new(status.clone()))
                                }
                                Ok(Request::Shutdown) => {
                                    let _ = control::write_response(
                                        &mut stream, &Response::Ack,
                                    ).await;
                                    break;
                                }
                                Err(err) => Response::Error { message: err.to_string() },
                            };
                            let _ = control::write_response(&mut stream, &response).await;
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "control connection failed");
                        }
                    }
                }
                _ = tokio::time::sleep(sleep) => {}
                _ = tokio::signal::ctrl_c() => break,
                _ = sigterm.recv() => break,
            }
        }

        let _ = self.store.save_ledger(&ledger);
        let _ = self.store.save_status(&status);
        Ok(())
    }

    async fn sync_once(
        &self,
        status: &mut StatusRecord,
        now: DateTime<Utc>,
    ) -> Result<SyncOutcome, ApiError> {
        status.last_attempt = Some(now);

        let result = run_sync_cycle(
            &*self.provider,
            &self.store,
            &self.config,
            &self.paths.export_dir,
            now,
        )
        .await;

        match &result {
            Ok(outcome) => {
                status.last_success = Some(now);
                status.calendars = outcome.calendars;
                status.events = outcome.events;
                status.consecutive_failures = 0;
                if outcome.partial {
                    status.last_error_category = Some("partial".into());
                    status.last_error_summary = Some(format!(
                        "{} calendar(s) kept their previous events",
                        outcome.failed.len()
                    ));
                } else {
                    status.last_error_category = None;
                    status.last_error_summary = None;
                }
                *self.auth_health.lock().await =
                    self.tokens.health(self.config.has_oauth_client()).await;
                tracing::info!(
                    calendars = outcome.calendars,
                    events = outcome.events,
                    written = outcome.written,
                    removed = outcome.removed,
                    partial = outcome.partial,
                    "sync complete"
                );
            }
            Err(err) => {
                status.consecutive_failures = status.consecutive_failures.saturating_add(1);
                status.last_error_category = Some(err.category().to_string());
                status.last_error_summary = Some(err.to_string());
                if let Some(failure) = err.auth_failure() {
                    *self.auth_health.lock().await = if failure.needs_user_authorization() {
                        crate::auth::AuthHealth::ReauthorizationRequired {
                            failure: failure.clone(),
                        }
                    } else if let crate::auth::AuthFailure::KeyringUnavailable { detail } = failure
                    {
                        crate::auth::AuthHealth::KeyringUnavailable {
                            detail: detail.clone(),
                        }
                    } else {
                        crate::auth::AuthHealth::Degraded {
                            failure: failure.clone(),
                        }
                    };
                }
                tracing::warn!(
                    category = err.category(),
                    failures = status.consecutive_failures,
                    "sync failed; the previous calendar is unchanged"
                );
            }
        }
        result
    }

    /// Exponential backoff, honouring a provider-supplied wait when there is
    /// one.
    fn next_backoff(&self, previous: Option<StdDuration>, err: &ApiError) -> StdDuration {
        if let Some(retry_after) = err.auth_failure().and_then(|f| f.retry_after()) {
            return retry_after.min(self.config.sync.retry_max);
        }
        let base = self.config.sync.retry_base;
        match previous {
            None => base,
            Some(previous) => (previous * 2).min(self.config.sync.retry_max),
        }
    }

    /// The shorter of the reminder deadline and the sync deadline, capped.
    fn sleep_until_next(
        &self,
        plan: &Plan,
        next_sync: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> StdDuration {
        let reminder_sleep = plan.sleep_for(now, &self.config.reminders);
        let Some(next_sync) = next_sync else {
            return reminder_sleep;
        };

        let until_sync = next_sync - now;
        let sync_sleep = if until_sync <= Duration::zero() {
            StdDuration::ZERO
        } else {
            until_sync.to_std().unwrap_or(reminder_sleep)
        };
        reminder_sleep.min(sync_sleep)
    }
}

/// The calendar time zone a `status` line should be rendered in.
pub fn display_zone(snapshot: &Snapshot) -> Tz {
    snapshot
        .calendars
        .iter()
        .find(|c| c.primary)
        .or_else(|| snapshot.calendars.first())
        .map(CalendarRef::zone)
        .unwrap_or(chrono_tz::UTC)
}
