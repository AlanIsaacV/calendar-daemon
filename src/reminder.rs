//! Deciding which reminders fire, which are too late to be useful, and how long
//! to sleep before looking again.
//!
//! The awkward case this module exists for is a machine that was asleep. The
//! sleep timer does not advance while suspended, so on resume a pile of
//! reminders is simultaneously past due. Firing all of them would bury the one
//! that still matters; firing none would silently drop a meeting that started
//! four minutes ago. The policy below fires the recent ones, up to a cap, and
//! records the rest as handled so they never come back.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};

use crate::config::ReminderConfig;
use crate::model::{CalendarRef, Occurrence, ScheduledReminder, reminders_for};
use crate::store::Ledger;

/// Why a past-due reminder was recorded without notifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    /// Older than `reminders.catch_up_window`.
    TooLate,
    /// The occurrence itself has already finished.
    EventOver,
    /// Inside the window, but beyond `reminders.max_catch_up` for this pass.
    CatchUpCapReached,
}

impl SuppressReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SuppressReason::TooLate => "older than the catch-up window",
            SuppressReason::EventOver => "the event had already ended",
            SuppressReason::CatchUpCapReached => "the catch-up cap for this pass was reached",
        }
    }
}

/// What the scheduler decided this pass.
#[derive(Debug, Default)]
pub struct Plan {
    /// Deliver now. Includes reminders that came due while suspended but are
    /// still recent enough to matter.
    pub due: Vec<ScheduledReminder>,
    /// Record as handled without notifying.
    pub suppressed: Vec<(ScheduledReminder, SuppressReason)>,
    /// When the next future reminder comes due, if there is one.
    pub next_deadline: Option<DateTime<Utc>>,
    /// The summary of that next reminder, for `status`.
    pub next_summary: Option<String>,
    /// Reminders known and still ahead, for `status`.
    pub pending: usize,
}

impl Plan {
    /// How long to sleep before the next pass.
    ///
    /// Capped at `max_sleep` because the timer this feeds is monotonic and does
    /// not advance across a suspend: a deadline eight hours out must be
    /// re-checked against the wall clock long before then, or a resume would
    /// arrive hours late. This is a local timer, not a provider request.
    pub fn sleep_for(&self, now: DateTime<Utc>, config: &ReminderConfig) -> std::time::Duration {
        let max = config.max_sleep;
        let Some(deadline) = self.next_deadline else {
            return max;
        };
        let remaining = deadline - now;
        if remaining <= Duration::zero() {
            return std::time::Duration::ZERO;
        }
        remaining.to_std().unwrap_or(max).min(max)
    }
}

/// Expand a snapshot into every reminder it implies, skipping inactive events.
pub fn collect(
    calendars: &[CalendarRef],
    occurrences: &[Occurrence],
    skip_declined: bool,
) -> Vec<ScheduledReminder> {
    let mut out = Vec::new();
    for occurrence in occurrences {
        if !occurrence.is_active(skip_declined) {
            continue;
        }
        let Some(calendar) = calendars.iter().find(|c| c.id == occurrence.calendar_id) else {
            // A calendar the account can no longer see: its defaults are
            // unknown, so inheriting anything would be a guess.
            continue;
        };
        out.extend(reminders_for(occurrence, calendar));
    }
    out.sort_by(|a, b| a.fire_at.cmp(&b.fire_at).then_with(|| a.key.cmp(&b.key)));
    out
}

/// Decide what to do with every reminder, given what has already been handled.
pub fn plan(
    reminders: &[ScheduledReminder],
    ledger: &Ledger,
    now: DateTime<Utc>,
    config: &ReminderConfig,
) -> Plan {
    let mut plan = Plan::default();
    if !config.enabled {
        return plan;
    }

    let catch_up = Duration::from_std(config.catch_up_window).unwrap_or(Duration::minutes(15));
    let mut late: Vec<&ScheduledReminder> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();

    for reminder in reminders {
        if ledger.contains(&reminder.key) || !seen.insert(reminder.key.as_str()) {
            continue;
        }
        if reminder.fire_at > now {
            plan.pending += 1;
            if plan
                .next_deadline
                .is_none_or(|current| reminder.fire_at < current)
            {
                plan.next_deadline = Some(reminder.fire_at);
                plan.next_summary = Some(reminder.summary.clone());
            }
            continue;
        }
        late.push(reminder);
    }

    // Most recently due first, so a capped catch-up keeps the reminders closest
    // to now rather than the oldest ones in the backlog.
    late.sort_by_key(|reminder| std::cmp::Reverse(reminder.fire_at));

    let mut delivered = 0usize;
    for reminder in late {
        let late_by = now - reminder.fire_at;

        if late_by > catch_up {
            plan.suppressed
                .push((reminder.clone(), SuppressReason::TooLate));
            continue;
        }
        // An all-day event is "over" only once the day is, and its end is
        // already the exclusive next midnight, so the same comparison works.
        if now >= reminder.event_end && reminder.event_end > reminder.event_start {
            plan.suppressed
                .push((reminder.clone(), SuppressReason::EventOver));
            continue;
        }
        if delivered >= config.max_catch_up && late_by > Duration::zero() {
            plan.suppressed
                .push((reminder.clone(), SuppressReason::CatchUpCapReached));
            continue;
        }
        plan.due.push(reminder.clone());
        delivered += 1;
    }

    // Deliver in the order the events happen, which is how a person reads them.
    plan.due.sort_by(|a, b| {
        a.event_start
            .cmp(&b.event_start)
            .then_with(|| a.key.cmp(&b.key))
    });
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventStatus, EventTime, Reminder, ReminderSpec, ResponseStatus};
    use std::time::Duration as StdDuration;

    fn now() -> DateTime<Utc> {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn config() -> ReminderConfig {
        ReminderConfig {
            enabled: true,
            catch_up_window: StdDuration::from_secs(15 * 60),
            max_catch_up: 3,
            max_sleep: StdDuration::from_secs(120),
        }
    }

    fn calendar(defaults: Vec<Reminder>) -> CalendarRef {
        CalendarRef {
            id: "work".into(),
            summary: "Work".into(),
            time_zone: Some("America/Bogota".into()),
            color: None,
            primary: true,
            selected: true,
            default_reminders: defaults,
        }
    }

    /// An event starting `offset_minutes` from now, lasting 30 minutes.
    fn event_at(id: &str, offset_minutes: i64, minutes_before: i64) -> Occurrence {
        let start = now() + Duration::minutes(offset_minutes);
        Occurrence {
            calendar_id: "work".into(),
            id: id.into(),
            summary: format!("Meeting {id}"),
            location: None,
            meeting_url: None,
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
            reminders: ReminderSpec::Overrides(vec![Reminder::popup(minutes_before)]),
            updated: None,
        }
    }

    fn reminders(events: &[Occurrence]) -> Vec<ScheduledReminder> {
        collect(&[calendar(vec![])], events, true)
    }

    #[test]
    fn a_future_reminder_waits_and_sets_the_next_deadline() {
        let events = vec![event_at("e1", 30, 10)];
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());

        assert!(plan.due.is_empty());
        assert_eq!(plan.next_deadline, Some(now() + Duration::minutes(20)));
        assert_eq!(plan.pending, 1);
    }

    #[test]
    fn a_reminder_due_exactly_now_fires() {
        let events = vec![event_at("e1", 10, 10)];
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());
        assert_eq!(plan.due.len(), 1);
    }

    #[test]
    fn a_reminder_already_delivered_is_not_repeated_after_a_restart() {
        let events = vec![event_at("e1", 10, 10)];
        let all = reminders(&events);

        let mut ledger = Ledger::default();
        ledger.record(&all[0].key, now(), all[0].event_start, false);

        let plan = plan(&all, &ledger, now(), &config());
        assert!(
            plan.due.is_empty(),
            "a restart re-fired a delivered reminder"
        );
        assert!(plan.suppressed.is_empty());
    }

    #[test]
    fn a_reminder_missed_during_a_short_suspend_still_fires_late() {
        // Due five minutes ago; the meeting starts in five minutes.
        let events = vec![event_at("e1", 5, 10)];
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());

        assert_eq!(
            plan.due.len(),
            1,
            "a reminder missed during a short suspend was dropped"
        );
    }

    #[test]
    fn a_reminder_older_than_the_catch_up_window_is_suppressed_not_fired() {
        // Due twenty minutes ago, window is fifteen.
        let events = vec![event_at("e1", 40, 60)];
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());

        assert!(plan.due.is_empty());
        assert_eq!(plan.suppressed.len(), 1);
        assert_eq!(plan.suppressed[0].1, SuppressReason::TooLate);
    }

    #[test]
    fn a_weekend_of_backlog_produces_no_notifications_at_all() {
        let events: Vec<Occurrence> = (0..50)
            .map(|i| event_at(&format!("e{i}"), -(2000 + i * 10), 10))
            .collect();
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());

        assert!(
            plan.due.is_empty(),
            "resuming after a weekend emptied a backlog on screen"
        );
        assert_eq!(
            plan.suppressed.len(),
            50,
            "the backlog must still be recorded as handled"
        );
    }

    #[test]
    fn a_burst_of_recent_reminders_is_capped_and_the_rest_recorded() {
        // Ten reminders, all due within the last few minutes.
        let events: Vec<Occurrence> = (0..10)
            .map(|i| event_at(&format!("e{i}"), 10 - i, 10))
            .collect();
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());

        assert_eq!(plan.due.len(), config().max_catch_up);
        assert!(
            plan.suppressed
                .iter()
                .all(|(_, reason)| *reason == SuppressReason::CatchUpCapReached),
            "expected the overflow to be capped, got {:?}",
            plan.suppressed.iter().map(|(_, r)| r).collect::<Vec<_>>()
        );
        assert_eq!(
            plan.due.len() + plan.suppressed.len(),
            10,
            "a reminder was lost entirely"
        );
    }

    #[test]
    fn the_capped_catch_up_keeps_the_most_recent_reminders() {
        let events: Vec<Occurrence> = (0..6)
            .map(|i| event_at(&format!("e{i}"), 10 - i * 2, 10))
            .collect();
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());

        // e0 is due now, e5 was due ten minutes ago. The cap keeps the newest.
        let kept: Vec<&str> = plan.due.iter().map(|r| r.event_id.as_str()).collect();
        assert!(
            kept.contains(&"e0"),
            "dropped the most recent reminder: {kept:?}"
        );
        assert!(
            !kept.contains(&"e5"),
            "kept the oldest reminder over a newer one: {kept:?}"
        );
    }

    #[test]
    fn a_reminder_for_a_meeting_that_already_ended_is_suppressed() {
        // Started 40 minutes ago, 30 minutes long, reminder 30 minutes before:
        // due 70 minutes ago, so it is also outside the window.
        let mut event = event_at("e1", -40, 0);
        event.reminders = ReminderSpec::Overrides(vec![Reminder::popup(0)]);
        // Bring the reminder inside the catch-up window while the meeting is
        // still over, so the reason under test is the one that applies.
        event.start = EventTime::Timed {
            at: now() - Duration::minutes(10),
            zone: None,
        };
        event.end = EventTime::Timed {
            at: now() - Duration::minutes(5),
            zone: None,
        };

        let plan = plan(&reminders(&[event]), &Ledger::default(), now(), &config());
        assert!(plan.due.is_empty());
        assert_eq!(plan.suppressed[0].1, SuppressReason::EventOver);
    }

    #[test]
    fn a_reminder_during_a_meeting_that_is_still_running_fires() {
        let mut event = event_at("e1", 0, 0);
        event.start = EventTime::Timed {
            at: now() - Duration::minutes(5),
            zone: None,
        };
        event.end = EventTime::Timed {
            at: now() + Duration::minutes(25),
            zone: None,
        };

        let plan = plan(&reminders(&[event]), &Ledger::default(), now(), &config());
        assert_eq!(
            plan.due.len(),
            1,
            "a reminder for a meeting in progress was dropped"
        );
    }

    #[test]
    fn an_event_with_reminders_switched_off_never_reaches_the_plan() {
        let mut event = event_at("e1", 10, 10);
        event.reminders = ReminderSpec::Disabled;
        let scheduled = collect(&[calendar(vec![Reminder::popup(10)])], &[event], true);
        assert!(scheduled.is_empty());
    }

    #[test]
    fn a_declined_event_produces_no_reminder() {
        let mut event = event_at("e1", 10, 10);
        event.response = ResponseStatus::Declined;
        assert!(collect(&[calendar(vec![])], &[event.clone()], true).is_empty());
        assert_eq!(collect(&[calendar(vec![])], &[event], false).len(), 1);
    }

    #[test]
    fn a_cancelled_event_produces_no_reminder() {
        let mut event = event_at("e1", 10, 10);
        event.status = EventStatus::Cancelled;
        assert!(collect(&[calendar(vec![])], &[event], true).is_empty());
    }

    #[test]
    fn an_event_whose_calendar_vanished_is_skipped_rather_than_guessed_at() {
        let event = event_at("e1", 10, 10);
        assert!(collect(&[], &[event], true).is_empty());
    }

    #[test]
    fn the_sleep_is_capped_so_a_resume_is_never_hours_late() {
        let events = vec![event_at("e1", 600, 10)];
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());

        let sleep = plan.sleep_for(now(), &config());
        assert_eq!(
            sleep,
            config().max_sleep,
            "slept past a suspend-unaware timer"
        );
    }

    #[test]
    fn a_near_deadline_is_slept_to_exactly() {
        let events = vec![event_at("e1", 11, 10)];
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config());
        assert_eq!(plan.sleep_for(now(), &config()), StdDuration::from_secs(60));
    }

    #[test]
    fn no_reminders_at_all_still_wakes_on_the_cap() {
        let plan = plan(&[], &Ledger::default(), now(), &config());
        assert_eq!(plan.sleep_for(now(), &config()), config().max_sleep);
    }

    #[test]
    fn disabling_reminders_produces_an_empty_plan() {
        let mut config = config();
        config.enabled = false;
        let events = vec![event_at("e1", 10, 10)];
        let plan = plan(&reminders(&events), &Ledger::default(), now(), &config);

        assert!(plan.due.is_empty());
        assert!(plan.suppressed.is_empty());
    }

    #[test]
    fn several_lead_times_on_one_event_each_get_their_own_reminder() {
        let mut event = event_at("e1", 30, 0);
        event.reminders = ReminderSpec::Overrides(vec![Reminder::popup(10), Reminder::popup(30)]);
        let scheduled = reminders(&[event]);
        assert_eq!(scheduled.len(), 2);

        // The 30-minute one is due now; the 10-minute one is still ahead.
        let plan = plan(&scheduled, &Ledger::default(), now(), &config());
        assert_eq!(plan.due.len(), 1);
        assert_eq!(plan.next_deadline, Some(now() + Duration::minutes(20)));
    }

    #[test]
    fn an_all_day_reminder_the_morning_before_fires_at_that_hour() {
        let cal = calendar(vec![Reminder::popup(900)]);
        let event = Occurrence {
            calendar_id: "work".into(),
            id: "holiday".into(),
            summary: "Company holiday".into(),
            location: None,
            meeting_url: None,
            html_link: None,
            // Bogota is UTC-5: local midnight on the 12th is 05:00Z.
            start: EventTime::AllDay {
                date: "2026-09-12".parse().unwrap(),
            },
            end: EventTime::AllDay {
                date: "2026-09-13".parse().unwrap(),
            },
            status: EventStatus::Confirmed,
            response: ResponseStatus::NotInvited,
            reminders: ReminderSpec::UseDefault,
            updated: None,
        };

        let scheduled = collect(&[cal], &[event], true);
        assert_eq!(scheduled.len(), 1);
        assert!(scheduled[0].all_day);
        assert_eq!(
            scheduled[0].fire_at,
            "2026-09-11T14:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );

        // At 12:00Z the reminder is still ahead, so nothing fires yet.
        let plan = plan(&scheduled, &Ledger::default(), now(), &config());
        assert!(plan.due.is_empty());
        assert_eq!(
            plan.next_deadline,
            Some("2026-09-11T14:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn an_all_day_reminder_is_not_suppressed_merely_because_the_day_started() {
        let cal = calendar(vec![Reminder::popup(0)]);
        let event = Occurrence {
            calendar_id: "work".into(),
            id: "holiday".into(),
            summary: "Company holiday".into(),
            location: None,
            meeting_url: None,
            html_link: None,
            // Local midnight was 05:00Z today; it is now 12:00Z, so the day is
            // under way but far from over.
            start: EventTime::AllDay {
                date: "2026-09-11".parse().unwrap(),
            },
            end: EventTime::AllDay {
                date: "2026-09-12".parse().unwrap(),
            },
            status: EventStatus::Confirmed,
            response: ResponseStatus::NotInvited,
            reminders: ReminderSpec::UseDefault,
            updated: None,
        };

        let scheduled = collect(&[cal], &[event], true);
        // Due at 05:00Z, seven hours ago: outside the catch-up window, so it is
        // recorded rather than shown. The point under test is that the reason
        // is lateness, not the day having begun.
        let plan = plan(&scheduled, &Ledger::default(), now(), &config());
        assert_eq!(plan.suppressed[0].1, SuppressReason::TooLate);
    }
}
