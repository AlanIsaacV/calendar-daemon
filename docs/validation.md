# Validation

Two lists. The first runs anywhere — no network, no keyring, no graphical
session — and is what CI runs. The second needs your own logged-in session, and
some of it needs a real Google account and your consent. Nothing in the second
list has been run yet.

---

## Part 1 — Checks that run anywhere

```sh
cargo test                    # 182 tests
cargo clippy --all-targets    # no warnings
cargo fmt --check
```

### What the automated tests actually pin down

| Behaviour | Where |
| --- | --- |
| Calendar defaults are inherited; overrides replace rather than extend | `src/model.rs` |
| `useDefault: false` with no overrides means **silenced**, not inherited | `src/google/types.rs` |
| Email reminders are left to Google; only popups are delivered | `src/model.rs` |
| All-day lead times are measured from local midnight, not UTC | `src/model.rs`, `src/reminder.rs` |
| A rescheduled event earns a new reminder; an unchanged one does not | `src/model.rs`, `tests/sync_cycle.rs` |
| A restart does not repeat a delivered reminder | `tests/sync_cycle.rs` |
| A weekend of backlog produces no notifications, and never returns | `src/reminder.rs`, `tests/sync_cycle.rs` |
| A reminder missed by five minutes is delivered, marked delayed | `tests/sync_cycle.rs` |
| A burst of recent reminders is capped, keeping the most recent | `src/reminder.rs` |
| Deleting the last event leaves a valid empty calendar, not a missing file | `src/export/`, `tests/sync_cycle.rs` |
| An unchanged calendar is not rewritten | `src/export/`, `tests/sync_cycle.rs` |
| A removed calendar is cleaned up — unless it is the last one | `src/export/` |
| Directories the export did not create are never deleted | `src/export/` |
| A transient failure keeps the stored refresh token | `src/auth/` |
| Throttling (429) never looks like a dead grant | `src/auth/error.rs` |
| Only `invalid_grant` invalidates the stored token | `src/auth/error.rs` |
| A locked keyring is never mistaken for a revoked authorization | `src/auth/`, `src/auth/secrets.rs` |
| A bad client id does not send the user to a browser | `src/auth/error.rs` |
| Provider error descriptions never reach a log line | `src/auth/error.rs` |
| A rotated refresh token is persisted before it is relied on | `src/auth/` |
| Concurrent syncs renew the access token once, not several times | `src/auth/` |
| A forged callback state yields no authorization code | `src/auth/oauth.rs` |
| The callback listener closes when consent ends or times out | `src/auth/oauth.rs` |
| Only read-only calendar scopes are requested | `src/auth/oauth.rs` |
| A partial scope grant is refused rather than stored | `src/auth/oauth.rs` |
| Non-http(s) links are never opened or exported | `src/google/types.rs`, `src/notify.rs` |
| An event title cannot inject an iCalendar property | `src/export/ics.rs` |
| A failed sync leaves the previous calendar intact | `tests/sync_cycle.rs` |
| A rejected grant stops the cycle instead of failing every calendar | `tests/sync_cycle.rs` |
| Only one process can own the state directory | `src/store.rs` |
| State and export files are owner-only | `src/store.rs`, `src/export/` |
| Reminders fire independently of the sync interval | `tests/sync_cycle.rs` |

### Checking the export against a real iCalendar parser

The exported files are validated with libical — the library calendar clients
generally parse `.ics` with — rather than only against this project's own
serializer.

```sh
cc -o /tmp/validate-ics tools/validate-ics.c $(pkg-config --cflags --libs libical)
cargo run --example sample_export -- /tmp/calendar-daemon-sample
/tmp/validate-ics /tmp/calendar-daemon-sample/work_example.com/calendar.ics
```

Expected: `unparseable lines: 0`, `X-WR-CALNAME` extracted, the all-day event
reported as `allday=1`, escaped titles round-tripped intact, `STATUS:CANCELLED`
carried, and the `URL` property present. An empty calendar must parse as a
`VCALENDAR` with `events: 0` — not as a parse failure, which is what would make
a reader keep showing stale events.

**Result on 2026-09-11** (libical 4.0.5): passed, including the empty-calendar
case.

### Checking the daemon end to end without Google

Every path is overridable, so this touches nothing installed:

```sh
D=$(mktemp -d); mkdir -p $D/{config,state,run}
cat > $D/config/config.toml <<EOF
[auth]
client_id = "placeholder"
client_secret = "placeholder"
EOF
export CALENDAR_DAEMON_CONFIG=$D/config/config.toml \
       CALENDAR_DAEMON_STATE_DIR=$D/state \
       CALENDAR_DAEMON_RUNTIME_DIR=$D/run

calendar-daemon check
calendar-daemon paths
calendar-daemon run &          # starts even with no keyring
calendar-daemon status         # reports the degraded state over the socket
calendar-daemon run            # must refuse: state directory already claimed
calendar-daemon stop           # exits over the socket
calendar-daemon stop           # says no daemon is running
rm -rf $D
```

**Result on 2026-09-11:** the daemon started with no session bus available,
reported `keyring unavailable — unlock the login keyring; the authorization is
intact`, answered `status`, `sync` and `stop` over the control socket, refused a
second instance, and removed its socket on exit.

Not reachable without a working provider, and therefore **not yet verified**:
the coalescing of a repeated `sync`, which only applies after a cycle succeeds.
It is checked in Part 2, item 17.

---

## Part 2 — Checks that need your session

These require a logged-in graphical session. The ones marked **consent** also
require a real Google account and a browser sign-in. None of them has been run.

### Needs your session only (no Google)

1. **The keyring is reachable.** With the session unlocked:
   `secret-tool store --label='probe' service calendar-daemon-probe key probe`
   then `secret-tool clear service calendar-daemon-probe key probe`. If this
   fails, `auth` will too, and for the same reason.

2. **Notifications appear.** `notify-send -a calendar-daemon 'Test' 'Body'`.
   Confirms the notification server is running and shows unactioned
   notifications.

3. **A notification action works.** The daemon's action button depends on the
   server supporting actions, which not all do. If the "Open meeting" button
   never appears, that is the server, not the daemon.

4. **The calendar client reads the sample export.**
   `cargo run --example sample_export -- /tmp/calendar-daemon-sample`, add a
   local (vdir) account pointing at it, and confirm:
   - both sample calendars appear, with their names and colours;
   - the all-day event renders as all-day, not as a midnight appointment;
   - the title `Plan; review, and sign off` shows its punctuation intact;
   - clicking the standup opens `https://meet.example/sample-standup`.

   Do this **before** authorizing anything. It separates "the daemon is wrong"
   from "the client is not reading this directory", and it costs a minute.

5. **Emptying a calendar clears the display.** With the sample account added,
   replace one `calendar.ics` with an empty calendar and confirm the client
   stops showing its events rather than keeping them:

   ```sh
   printf 'BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//x//x//EN\r\nX-WR-CALNAME:Work (sample)\r\nEND:VCALENDAR\r\n' \
     > /tmp/calendar-daemon-sample/work_example.com/calendar.ics
   ```

   This is the specific behaviour the empty-calendar rule exists for.

6. **Rollback works.** Follow [install.md](install.md) rollback on the sample
   account and confirm the client returns to its previous state.

### Needs Google and your consent

7. **`calendar-daemon auth` completes.** The browser opens, the consent screen
   lists exactly the two read-only calendar scopes and nothing else, and the
   command reports success. Confirm the token landed:
   `secret-tool search --all service calendar-daemon`.

8. **`calendar-daemon sync` fetches real calendars.** Then `status` shows a
   recent success, a sensible event count, and `auth ready`.

9. **Reminder settings are honoured.** This is the check worth doing carefully,
   because it is the one automated tests can only assert about *modelled* data.
   In Google, create three events in the next hour or two:
   - one inheriting the calendar default;
   - one with a custom reminder (say 5 minutes);
   - one with reminders removed entirely.

   `calendar-daemon sync`, then `calendar-daemon status` and confirm
   `next reminder` names the right event at the right time. Then wait, and
   confirm the third event **never** notifies.

10. **An all-day reminder lands at the configured hour.** Create an all-day
    event for tomorrow with a reminder "1 day before at 09:00" and confirm
    `status` reports the reminder for 09:00 *local*, not 09:00 UTC.

11. **A meeting link opens.** An event with a Meet link: confirm the
    notification's action opens the right URL in your browser, and that the
    event's link in the calendar client does too.

12. **A cancellation withdraws its reminder.** Schedule an event with a reminder
    a few minutes out, cancel it in Google, `calendar-daemon sync`, and confirm
    nothing notifies.

13. **Reminders survive a restart.** With a reminder pending,
    `systemctl --user restart calendar-daemon`, and confirm it still fires once
    — and only once.

14. **Suspend and resume.** Suspend the machine across a reminder due less than
    15 minutes ago and confirm it arrives marked as delayed. Suspend across one
    due hours ago and confirm the screen stays quiet.

15. **A locked keyring recovers without re-consenting.** Lock the keyring
    (`secret-tool lock` or your keyring UI), `calendar-daemon sync` and confirm
    `status` says `keyring unavailable` and **not** `reauthorization required`.
    Unlock, sync again, and confirm it works with no browser.

16. **Offline behaviour.** Disable networking, `calendar-daemon sync`, and
    confirm: the command reports a transient failure, the previous calendar is
    still in the export, `status` reports the data age, and pending reminders
    still fire.

17. **Repeated `sync` coalesces.** With the daemon running and authorized, run
    `calendar-daemon sync; calendar-daemon sync` back to back. The second should
    return immediately without a second round trip to Google. Confirm in the
    journal that only one sync cycle is logged.

### Measurements, not yet taken

The design declines to claim resource figures before measuring, and none have
been measured. When the daemon has run for a day:

```sh
systemctl --user status calendar-daemon    # RSS while idle
pidstat -p $(pgrep -f 'calendar-daemon run') 60 10
```

Worth recording: idle RSS, CPU during a sync cycle, requests per day, and the
size of the export. Report them as measurements, not as expectations.

---

## Known limitations

- Recurrence is whatever Google's expansion says it is. The daemon does not
  interpret `RRULE`, so a series is only as correct as `singleEvents=true`.
- Reminders only fire while the daemon is running and you are logged in. It does
  not wake the machine.
- The notification action requires a notification server that supports actions.
- Only the primary account's `self` response status is read; delegated calendars
  where you are not an attendee are treated as not-invited.
- A refresh token in the keyring removes one reason for re-prompting. It cannot
  prevent Google from expiring or revoking a grant.
