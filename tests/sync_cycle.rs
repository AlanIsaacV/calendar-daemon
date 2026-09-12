//! End-to-end checks of one sync cycle and the reminder pass that follows it,
//! against a scripted provider. No network, no keyring, no graphical session.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

use calendar_daemon::config::Config;
use calendar_daemon::daemon::{deliver_due, run_sync_cycle, select_calendars};
use calendar_daemon::export;
use calendar_daemon::google::{ApiError, CalendarProvider, ProviderFuture};
use calendar_daemon::model::{
    CalendarRef, EventStatus, EventTime, Occurrence, Reminder, ReminderSpec, ResponseStatus,
};
use calendar_daemon::notify::RecordingNotifier;
use calendar_daemon::store::Store;
use calendar_daemon::util;

/// A provider whose answers the test writes.
struct FakeProvider {
    calendars: Mutex<Result<Vec<CalendarRef>, ApiError>>,
    events: Mutex<std::collections::BTreeMap<String, Result<Vec<Occurrence>, ApiError>>>,
    event_calls: Mutex<Vec<String>>,
}

impl FakeProvider {
    fn with_calendars(calendars: Vec<CalendarRef>) -> Self {
        Self {
            calendars: Mutex::new(Ok(calendars)),
            events: Mutex::new(Default::default()),
            event_calls: Mutex::new(Vec::new()),
        }
    }

    fn set_events(&self, calendar_id: &str, events: Vec<Occurrence>) {
        self.events
            .lock()
            .unwrap()
            .insert(calendar_id.to_string(), Ok(events));
    }

    fn fail_events(&self, calendar_id: &str, error: ApiError) {
        self.events
            .lock()
            .unwrap()
            .insert(calendar_id.to_string(), Err(error));
    }

    fn calls(&self) -> Vec<String> {
        self.event_calls.lock().unwrap().clone()
    }
}

impl CalendarProvider for FakeProvider {
    fn list_calendars(&self) -> ProviderFuture<'_, Vec<CalendarRef>> {
        let answer = self.calendars.lock().unwrap().clone();
        Box::pin(async move { answer })
    }

    fn list_events<'a>(
        &'a self,
        calendar_id: &'a str,
        _time_min: DateTime<Utc>,
        _time_max: DateTime<Utc>,
        _max_events: usize,
    ) -> ProviderFuture<'a, Vec<Occurrence>> {
        self.event_calls
            .lock()
            .unwrap()
            .push(calendar_id.to_string());
        let answer = self
            .events
            .lock()
            .unwrap()
            .get(calendar_id)
            .cloned()
            .unwrap_or_else(|| Ok(Vec::new()));
        Box::pin(async move { answer })
    }
}

fn workspace(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("calendar-daemon-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    util::ensure_private_dir(&dir).unwrap();
    dir
}

fn store_in(dir: &Path) -> Store {
    Store::new(
        dir.join("snapshot.json"),
        dir.join("reminders.json"),
        dir.join("status.json"),
    )
}

fn now() -> DateTime<Utc> {
    "2026-09-11T12:00:00Z".parse().unwrap()
}

fn calendar(id: &str, defaults: Vec<Reminder>) -> CalendarRef {
    CalendarRef {
        id: id.into(),
        summary: id.into(),
        time_zone: Some("America/Bogota".into()),
        color: Some("#9fe1e7".into()),
        primary: id == "work",
        selected: true,
        default_reminders: defaults,
    }
}

/// An event starting `offset_minutes` from `now()`.
fn event(calendar_id: &str, id: &str, offset_minutes: i64, spec: ReminderSpec) -> Occurrence {
    let start = now() + Duration::minutes(offset_minutes);
    Occurrence {
        calendar_id: calendar_id.into(),
        id: id.into(),
        summary: format!("Meeting {id}"),
        location: None,
        meeting_url: Some("https://meet.example/x".into()),
        html_link: None,
        start: EventTime::Timed {
            at: start,
            zone: None,
        },
        end: EventTime::Timed {
            at: start + Duration::minutes(30),
            zone: None,
        },
        status: EventStatus::Confirmed,
        response: ResponseStatus::Accepted,
        reminders: spec,
        updated: Some("2026-09-01T08:00:00Z".parse().unwrap()),
    }
}

fn config_for(dir: &Path) -> Config {
    let mut config = Config::default();
    config.export.directory = Some(dir.join("calendars"));
    config
}

#[tokio::test]
async fn a_sync_writes_a_snapshot_and_an_export_a_vdir_reader_can_find() {
    let dir = workspace("basic");
    let store = store_in(&dir);
    let config = config_for(&dir);
    let export_dir = dir.join("calendars");

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![Reminder::popup(10)])]);
    provider.set_events(
        "work",
        vec![event("work", "e1", 60, ReminderSpec::UseDefault)],
    );

    let outcome = run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    assert_eq!(outcome.calendars, 1);
    assert_eq!(outcome.events, 1);
    assert!(!outcome.partial);

    let snapshot = store.load_snapshot().unwrap().unwrap();
    assert_eq!(snapshot.occurrences.len(), 1);

    let collections = export::discover_collections(&export_dir);
    assert_eq!(
        collections.len(),
        1,
        "no collection a vdir reader would discover"
    );

    let ics = std::fs::read_to_string(export_dir.join("work/calendar.ics")).unwrap();
    assert!(ics.contains("BEGIN:VEVENT"));
    assert!(
        ics.contains("URL:https://meet.example/x"),
        "the meeting link was lost: {ics}"
    );
}

#[tokio::test]
async fn deleting_the_last_event_leaves_a_valid_empty_calendar_behind() {
    let dir = workspace("empty");
    let store = store_in(&dir);
    let config = config_for(&dir);
    let export_dir = dir.join("calendars");

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![])]);
    provider.set_events(
        "work",
        vec![event("work", "e1", 60, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    provider.set_events("work", vec![]);
    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    let path = export_dir.join("work/calendar.ics");
    assert!(
        path.is_file(),
        "the snapshot file disappeared with its last event"
    );
    let ics = std::fs::read_to_string(&path).unwrap();
    assert!(ics.contains("BEGIN:VCALENDAR") && ics.contains("END:VCALENDAR"));
    assert!(!ics.contains("BEGIN:VEVENT"));
    assert_eq!(
        export::discover_collections(&export_dir).len(),
        1,
        "the export root was left with no collection, which reads as a failed refresh"
    );
}

#[tokio::test]
async fn a_failing_calendar_keeps_its_events_and_the_others_still_refresh() {
    let dir = workspace("partial");
    let store = store_in(&dir);
    let config = config_for(&dir);
    let export_dir = dir.join("calendars");

    let provider =
        FakeProvider::with_calendars(vec![calendar("work", vec![]), calendar("personal", vec![])]);
    provider.set_events(
        "work",
        vec![event("work", "w1", 60, ReminderSpec::UseDefault)],
    );
    provider.set_events(
        "personal",
        vec![event("personal", "p1", 90, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    provider.set_events(
        "work",
        vec![event("work", "w2", 120, ReminderSpec::UseDefault)],
    );
    provider.fail_events("personal", ApiError::Transport("offline".into()));

    let outcome = run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    assert!(outcome.partial);
    assert!(outcome.failed.contains("personal"));

    let snapshot = store.load_snapshot().unwrap().unwrap();
    let ids: Vec<&str> = snapshot.occurrences.iter().map(|o| o.id.as_str()).collect();
    assert!(ids.contains(&"w2"), "the working calendar did not refresh");
    assert!(
        ids.contains(&"p1"),
        "the failing calendar lost its events: {ids:?}"
    );

    let personal_ics = std::fs::read_to_string(export_dir.join("personal/calendar.ics")).unwrap();
    assert!(
        personal_ics.contains("UID:p1"),
        "a transient failure emptied a calendar"
    );
}

#[tokio::test]
async fn a_rejected_grant_stops_the_cycle_instead_of_failing_every_calendar_in_turn() {
    let dir = workspace("grant");
    let store = store_in(&dir);
    let config = config_for(&dir);

    let provider =
        FakeProvider::with_calendars(vec![calendar("work", vec![]), calendar("personal", vec![])]);
    provider.fail_events(
        "work",
        ApiError::Auth(calendar_daemon::auth::AuthFailure::GrantInvalid {
            code: "invalid_grant".into(),
            subtype: None,
        }),
    );

    let err = run_sync_cycle(&provider, &store, &config, &dir.join("calendars"), now())
        .await
        .unwrap_err();

    assert!(
        err.auth_failure()
            .is_some_and(|f| f.needs_user_authorization())
    );
    assert_eq!(
        provider.calls(),
        vec!["work".to_string()],
        "kept asking other calendars after the grant was rejected"
    );
}

#[tokio::test]
async fn a_failed_sync_leaves_the_previous_calendar_on_disk() {
    let dir = workspace("offline");
    let store = store_in(&dir);
    let config = config_for(&dir);
    let export_dir = dir.join("calendars");

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![])]);
    provider.set_events(
        "work",
        vec![event("work", "e1", 60, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    *provider.calendars.lock().unwrap() = Err(ApiError::Transport("offline".into()));
    let err = run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap_err();
    assert!(err.is_retryable());

    let snapshot = store.load_snapshot().unwrap().unwrap();
    assert_eq!(
        snapshot.occurrences.len(),
        1,
        "an offline cycle erased the calendar"
    );
    let ics = std::fs::read_to_string(export_dir.join("work/calendar.ics")).unwrap();
    assert!(ics.contains("UID:e1"));
}

#[tokio::test]
async fn a_restart_does_not_repeat_a_reminder_that_already_fired() {
    let dir = workspace("restart");
    let store = store_in(&dir);
    let config = config_for(&dir);

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![Reminder::popup(10)])]);
    // Due right now: starts in ten minutes with a ten-minute lead time.
    provider.set_events(
        "work",
        vec![event("work", "e1", 10, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &dir.join("calendars"), now())
        .await
        .unwrap();

    let snapshot = store.load_snapshot().unwrap().unwrap();

    let first_notifier = RecordingNotifier::default();
    let mut ledger = store.load_ledger();
    deliver_due(&snapshot, &mut ledger, &first_notifier, &config, now());
    store.save_ledger(&ledger).unwrap();
    assert_eq!(first_notifier.keys().len(), 1, "the reminder never fired");

    // Restart: a fresh ledger read, a fresh notifier, the same snapshot.
    let second_notifier = RecordingNotifier::default();
    let mut ledger = store.load_ledger();
    deliver_due(
        &snapshot,
        &mut ledger,
        &second_notifier,
        &config,
        now() + Duration::minutes(1),
    );

    assert!(
        second_notifier.keys().is_empty(),
        "restarting re-fired a reminder that had already been delivered"
    );
}

#[tokio::test]
async fn rescheduling_an_event_earns_a_new_reminder() {
    let dir = workspace("reschedule");
    let store = store_in(&dir);
    let config = config_for(&dir);

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![Reminder::popup(10)])]);
    provider.set_events(
        "work",
        vec![event("work", "e1", 10, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &dir.join("calendars"), now())
        .await
        .unwrap();

    let mut ledger = store.load_ledger();
    let notifier = RecordingNotifier::default();
    deliver_due(
        &store.load_snapshot().unwrap().unwrap(),
        &mut ledger,
        &notifier,
        &config,
        now(),
    );
    store.save_ledger(&ledger).unwrap();
    assert_eq!(notifier.keys().len(), 1);

    // The organizer moves it an hour later; its reminder is due again at the
    // new time.
    provider.set_events(
        "work",
        vec![event("work", "e1", 70, ReminderSpec::UseDefault)],
    );
    let later = now() + Duration::minutes(60);
    run_sync_cycle(&provider, &store, &config, &dir.join("calendars"), later)
        .await
        .unwrap();

    let notifier = RecordingNotifier::default();
    let mut ledger = store.load_ledger();
    deliver_due(
        &store.load_snapshot().unwrap().unwrap(),
        &mut ledger,
        &notifier,
        &config,
        later,
    );

    assert_eq!(
        notifier.keys().len(),
        1,
        "a rescheduled meeting gave no new reminder"
    );
}

#[tokio::test]
async fn cancelling_an_event_withdraws_its_pending_reminder_and_its_export_entry() {
    let dir = workspace("cancel");
    let store = store_in(&dir);
    let config = config_for(&dir);
    let export_dir = dir.join("calendars");

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![Reminder::popup(10)])]);
    provider.set_events(
        "work",
        vec![event("work", "e1", 60, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    let mut cancelled = event("work", "e1", 60, ReminderSpec::UseDefault);
    cancelled.status = EventStatus::Cancelled;
    provider.set_events("work", vec![cancelled]);
    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    let snapshot = store.load_snapshot().unwrap().unwrap();
    let notifier = RecordingNotifier::default();
    let mut ledger = store.load_ledger();
    // Move to the moment the reminder would have fired.
    let plan = deliver_due(
        &snapshot,
        &mut ledger,
        &notifier,
        &config,
        now() + Duration::minutes(50),
    );

    assert!(
        notifier.keys().is_empty(),
        "a cancelled meeting still notified"
    );
    assert_eq!(plan.pending, 0);
}

#[tokio::test]
async fn an_event_with_reminders_switched_off_is_exported_but_never_notified() {
    let dir = workspace("disabled");
    let store = store_in(&dir);
    let config = config_for(&dir);
    let export_dir = dir.join("calendars");

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![Reminder::popup(10)])]);
    provider.set_events(
        "work",
        vec![event("work", "quiet", 10, ReminderSpec::Disabled)],
    );
    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    let ics = std::fs::read_to_string(export_dir.join("work/calendar.ics")).unwrap();
    assert!(
        ics.contains("UID:quiet"),
        "a silenced event vanished from the calendar display"
    );

    let notifier = RecordingNotifier::default();
    let mut ledger = store.load_ledger();
    deliver_due(
        &store.load_snapshot().unwrap().unwrap(),
        &mut ledger,
        &notifier,
        &config,
        now(),
    );
    assert!(
        notifier.keys().is_empty(),
        "an event with reminders off produced a notification"
    );
}

#[tokio::test]
async fn a_backlog_after_a_long_suspend_is_recorded_without_notifying() {
    let dir = workspace("suspend");
    let store = store_in(&dir);
    let config = config_for(&dir);

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![Reminder::popup(10)])]);
    let events: Vec<Occurrence> = (1..=20)
        .map(|i| {
            event(
                "work",
                &format!("e{i}"),
                -(i * 30),
                ReminderSpec::UseDefault,
            )
        })
        .collect();
    provider.set_events("work", events);
    run_sync_cycle(&provider, &store, &config, &dir.join("calendars"), now())
        .await
        .unwrap();

    let notifier = RecordingNotifier::default();
    let mut ledger = store.load_ledger();
    let plan = deliver_due(
        &store.load_snapshot().unwrap().unwrap(),
        &mut ledger,
        &notifier,
        &config,
        now(),
    );

    assert!(
        notifier.keys().is_empty(),
        "resuming emptied a backlog of reminders on screen"
    );
    assert_eq!(
        plan.suppressed.len(),
        20,
        "the backlog was not recorded as handled"
    );

    // And the very next pass must not reconsider any of them.
    let second = RecordingNotifier::default();
    let plan = deliver_due(
        &store.load_snapshot().unwrap().unwrap(),
        &mut ledger,
        &second,
        &config,
        now() + Duration::minutes(1),
    );
    assert!(plan.suppressed.is_empty() && plan.due.is_empty());
}

#[tokio::test]
async fn a_reminder_missed_during_a_short_suspend_is_delivered_late_and_marked_as_such() {
    let dir = workspace("late");
    let store = store_in(&dir);
    let config = config_for(&dir);

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![Reminder::popup(10)])]);
    provider.set_events(
        "work",
        vec![event("work", "e1", 10, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &dir.join("calendars"), now())
        .await
        .unwrap();

    // Five minutes after the reminder came due, inside the catch-up window.
    let notifier = RecordingNotifier::default();
    let mut ledger = store.load_ledger();
    deliver_due(
        &store.load_snapshot().unwrap().unwrap(),
        &mut ledger,
        &notifier,
        &config,
        now() + Duration::minutes(5),
    );

    let shown = notifier.shown.lock().unwrap();
    assert_eq!(
        shown.len(),
        1,
        "a reminder missed by five minutes was dropped"
    );
    assert!(shown[0].1, "the late reminder did not say it was delayed");
}

#[tokio::test]
async fn only_calendars_the_user_selected_are_synchronized() {
    let mut hidden = calendar("hidden", vec![]);
    hidden.selected = false;

    let selected = select_calendars(vec![calendar("work", vec![]), hidden.clone()], &[]);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "work");

    // An explicit list overrides the provider's own selection.
    let explicit = select_calendars(
        vec![calendar("work", vec![]), hidden],
        &["hidden".to_string()],
    );
    assert_eq!(explicit.len(), 1);
    assert_eq!(explicit[0].id, "hidden");
}

#[tokio::test]
async fn a_calendar_removed_from_the_account_is_cleaned_out_of_the_export() {
    let dir = workspace("dropped");
    let store = store_in(&dir);
    let config = config_for(&dir);
    let export_dir = dir.join("calendars");

    let provider =
        FakeProvider::with_calendars(vec![calendar("work", vec![]), calendar("personal", vec![])]);
    provider.set_events(
        "work",
        vec![event("work", "w1", 60, ReminderSpec::UseDefault)],
    );
    provider.set_events(
        "personal",
        vec![event("personal", "p1", 60, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();
    assert_eq!(export::discover_collections(&export_dir).len(), 2);

    *provider.calendars.lock().unwrap() = Ok(vec![calendar("work", vec![])]);
    let outcome = run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    assert_eq!(outcome.removed, 1);
    let collections = export::discover_collections(&export_dir);
    assert_eq!(collections, BTreeSet::from(["work".to_string()]));
}

#[tokio::test]
async fn an_unchanged_calendar_is_not_rewritten_on_the_next_cycle() {
    let dir = workspace("stable");
    let store = store_in(&dir);
    let config = config_for(&dir);
    let export_dir = dir.join("calendars");

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![])]);
    provider.set_events(
        "work",
        vec![event("work", "e1", 60, ReminderSpec::UseDefault)],
    );

    run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();
    let outcome = run_sync_cycle(&provider, &store, &config, &export_dir, now())
        .await
        .unwrap();

    assert_eq!(
        outcome.written, 0,
        "an unchanged calendar was rewritten, waking every vdir watcher for nothing"
    );
}

#[tokio::test]
async fn reminders_do_not_depend_on_how_often_the_provider_is_asked() {
    let dir = workspace("independent");
    let store = store_in(&dir);
    let mut config = config_for(&dir);
    // A deliberately long sync interval: the reminder must still be on time.
    config.sync.interval = std::time::Duration::from_secs(24 * 3600);

    let provider = FakeProvider::with_calendars(vec![calendar("work", vec![Reminder::popup(10)])]);
    // Loaded now, due in three hours.
    provider.set_events(
        "work",
        vec![event("work", "e1", 190, ReminderSpec::UseDefault)],
    );
    run_sync_cycle(&provider, &store, &config, &dir.join("calendars"), now())
        .await
        .unwrap();

    let snapshot = store.load_snapshot().unwrap().unwrap();
    let mut ledger = store.load_ledger();
    let notifier = RecordingNotifier::default();

    // Nothing yet.
    let plan = deliver_due(&snapshot, &mut ledger, &notifier, &config, now());
    assert!(notifier.keys().is_empty());
    assert_eq!(plan.next_deadline, Some(now() + Duration::minutes(180)));

    // Three hours later, with no sync in between, it fires.
    let plan = deliver_due(
        &snapshot,
        &mut ledger,
        &notifier,
        &config,
        now() + Duration::minutes(180),
    );
    assert_eq!(
        notifier.keys().len(),
        1,
        "the reminder waited for the next sync"
    );
    assert!(
        plan.due[0].meeting_url.is_some(),
        "the notification lost its meeting link"
    );
}
