# Decisions and assumptions

The design left several behaviours explicitly open. This records what was
decided, what it costs, and how to change it. Anything here is a judgement call,
not a requirement anyone stated — each is cheap to revisit.

## All-day events

**Decided.** An all-day event's reminder lead time is measured from **local
midnight in the calendar's own time zone**, not from UTC midnight and not from
the machine's time zone.

Google states an all-day reminder as minutes before the event start, and its
start is a bare date. Configuring "the day before at 09:00" in the Google UI
arrives at the API as `900` minutes. Only midnight-local makes that land at
09:00; reading the date as UTC midnight would fire it at 09:00 UTC, which in
Bogotá is 04:00. `model::EventTime::instant` does the conversion and
`model.rs` tests it against exactly that case.

A zone with no midnight on a given date — a DST transition that skips it, which
has happened in Chile — resolves forward to the first minute of the day that
exists, rather than failing the whole sync for one event.

In the export, an all-day event is written `DTSTART;VALUE=DATE:20260912`. A
midnight timestamp would render as a zero-length appointment instead. Verified
against libical: `icaltime_is_date` returns true.

## Reminders that came due while the machine was asleep

**Decided.** A past-due reminder is delivered if it is less than
`reminders.catch_up_window` old (default 15 minutes) and its event has not
already ended, up to `reminders.max_catch_up` per pass (default 5). Everything
older is written to the ledger as handled, with the reason, and never shown.

Three failure modes were on the table and this policy picks its poison
deliberately:

- Firing everything: resuming a laptop after a weekend empties forty
  notifications onto the screen, burying the one that matters.
- Firing nothing: a meeting that started four minutes ago passes in silence,
  which is the case a reminder exists for.
- Firing everything recent without a cap: twelve simultaneous reminders after a
  short suspend is the first failure mode in miniature.

Within a capped catch-up the **most recently due** reminders win, not the
oldest — the ones closest to now are the ones still worth acting on.

Suppressed reminders are recorded, not dropped: they never come back on the next
pass or after a restart, and `reminder.rs` tests that.

To change the trade-off: raise `catch_up_window` to be told about older misses,
lower it to be told about fewer.

## Sleeping across a suspend

**Decided.** The scheduler never sleeps longer than `reminders.max_sleep`
(default 2 minutes) before re-reading the wall clock, even when the next
reminder is eight hours away.

Tokio's timer is monotonic and, on Linux, does not advance while the machine is
suspended. A single long sleep would therefore fire late by however long the
machine was asleep. Capping the sleep bounds post-resume lateness to
`max_sleep`.

This is a local timer. It is not polling: no request reaches Google on these
wakeups, which happen only on the sync interval. Configuration validation
rejects values below 5 seconds (pointless wakeups) and above 15 minutes (a
reminder that late after a resume is not a reminder).

## Reminder identity and deduplication

**Decided.** A reminder's key is
`calendar_id | event_instance_id | event_start_unix | minutes`.

Including the start instant is what makes a rescheduled meeting earn a new
reminder instead of being silenced by the ledger entry for the time it used to
be at. Including `minutes` gives an event with several lead times one
independent reminder each.

Recurring series are never expanded here: `singleEvents=true` makes Google
expand them, and each instance arrives with its own stable id. Reminder timing
therefore never depends on this daemon reimplementing RRULE, and never diverges
from what Google itself shows.

Ledger entries are pruned 30 days after their event started.

## What the export contains

**Decided.** One collection directory per calendar, one complete `calendar.ics`
per collection, concrete instances with no `RRULE`, plus `displayname` and
`color`.

Concrete instances mean the reader's own recurrence expansion cannot disagree
with Google's. The cost is a larger file; at a 67-day window that is not a real
cost.

An unchanged calendar is **not rewritten**. The serialization is byte-stable —
`DTSTAMP` comes from the event's `updated`, not from the clock — so a cycle that
changed nothing does not wake every filesystem watcher pointed at the directory.

The temporary file used for the atomic rename is named `.<file>.<pid>.tmp`: both
the leading dot and the `.tmp` suffix are skipped by vdir readers, so a sync in
progress is never mistaken for a calendar.

## The last event in a calendar

**Decided.** When a calendar empties, its `.ics` is rewritten as a valid empty
`VCALENDAR`. It is never deleted.

A vdir reader that discovers no collections treats the refresh as *failed* and
keeps showing the events it already had — so deleting the last file would leave
stale events on screen permanently. A well-formed `VCALENDAR` with no `VEVENT`
parses as a complete, empty calendar instead. Verified with libical
(`tools/validate-ics.c`): zero unparseable lines, zero events.

A calendar removed from the account has its directory deleted — but only while
at least one other collection remains. Removing the last one would reproduce the
no-collections case, so that one is emptied instead.

Directories the export did not create are never touched: cleanup is driven by
the collection list recorded in the previous snapshot.

## Which meeting link to open

**Decided, in order:** `hangoutLink`, then a `video` conference entry point,
then any conference entry point, then the first URL embedded in the location,
then the first URL in the description.

Provider-supplied conference data beats anything scraped out of free text. The
location beats the description because a location holding a URL is usually the
room's own link.

Only `http` and `https` are ever accepted, anywhere: a `file:`, `tel:` or
`javascript:` value is dropped rather than carried. Links are normalized at the
boundary, so what reaches the `.ics`, the notification and the opener is always
a percent-encoded ASCII URI. The opener is executed directly with the URL as one
argument — no shell is involved, so nothing in an event's text can become a
command.

Note for vdir consumers: some readers prefer a URL embedded in `LOCATION` over
the `URL` property. Where Google supplies both and they differ, such a reader
will use the location's. Both come from the same event, so this is a difference
of preference rather than a wrong link.

## Which calendars are synchronized

**Decided.** `sync.calendars = []` means every calendar the account has ticked
as selected in Google — the set the user already curates there. An explicit list
overrides that selection in both directions, so a calendar hidden in Google can
still be synchronized by naming it.

## Declined and cancelled events

**Decided.** Cancelled events produce no reminders and, with
`sync.skip_declined = true` (the default), neither do events this account
declined. Both are still fetched, because a cancellation has to be *seen* to
withdraw the reminder that was already scheduled.

A cancelled event remains in the export with `STATUS:CANCELLED`, which lets a
reader show it struck through rather than having it silently vanish.

## Full snapshots rather than incremental sync

**Decided, following the design.** Each cycle fetches the whole bounded window.

Google's `syncToken` cannot be combined with a moving `timeMin`/`timeMax`, and
this daemon's window moves with the clock. Incremental sync would mean a fixed
window plus separate recovery paths for token invalidation — worth it only if
measurement shows the full fetch costs something. At a 67-day window and a
six-hour interval, it is four requests a day.

## What is kept on disk

**Decided.** The snapshot stores what display and reminders need: summary,
location, resolved meeting URL, start, end, status, response, reminder
specification, `updated`.

Event **descriptions are fetched but never stored.** They are scanned once, at
sync time, for a meeting link, and then discarded. They are the field most
likely to contain something private, and nothing downstream needs them.

Exports necessarily contain readable event fields — that is what the calendar
client displays. They are not encrypted; they are owner-only.

## A partial sync

**Decided.** A calendar that fails keeps the events the last good cycle had for
it, the cycle reports itself as partial, and `fetched_at` is *not* advanced — a
partial refresh does not get to claim it is as fresh as a complete one.

The exception is a failure that does not preserve credentials: a rejected grant
aborts the whole cycle rather than being retried per calendar, because every
remaining calendar would fail identically and the user should be told once.

## Repeated `sync` requests

**Decided.** A `sync` request that arrives within 3 seconds of a *successful*
cycle returns that cycle's result marked `coalesced`, instead of running an
identical one.

Requests that arrive while a cycle is running queue in the socket backlog and
are accepted the instant it finishes, so without this each would start another
full fetch over data a fraction of a second old. The window is deliberately
short: a person cannot edit an event in Google and re-run `sync` inside three
seconds, so a coalesced answer is never a stale one in practice.

Only successful cycles coalesce. A failed one is always retried, because the
condition that caused it may already have cleared — an unlocked keyring, a
network that came back.

## Provisional

- **The name.** `calendar-daemon` is the working name, in the binary, the
  keyring service name, the socket path and the unit file. Renaming touches all
  four.
- **`sync.interval = "6h"`.** A starting value, not a measurement.
- **The license.** Not chosen; nothing has been published.
- **`window_past = 7d`, `window_future = 60d`.** Wide enough for a calendar
  widget's month view with history, narrow enough to stay small.
