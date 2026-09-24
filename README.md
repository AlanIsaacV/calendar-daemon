# calendar-daemon

A small Linux daemon that keeps a local copy of your Google calendars, fires
desktop reminders from it, and publishes the events as a plain vdir tree that
any local-calendar client can read.

It exists because a calendar widget that authenticates to Google itself has to
stay logged in, poll on its own schedule, and reimplement Google's reminder
rules. Moving all three into one background process means the widget only has
to read files.

> **Status: pre-release.** The logic is implemented and tested against a
> scripted provider; the exported calendars are validated against libical. It
> has not yet been run against a live Google account. `calendar-daemon` is a
> provisional name.

## What it does

- **Synchronizes** on start, on a configurable interval, and on demand
  (`calendar-daemon sync`). The three are independently switchable.
- **Fires reminders on a separate clock.** An event loaded at 06:00 with a
  reminder due at 08:50 is notified at 08:50, whether or not a sync happens in
  between. A long sync interval costs freshness, never punctuality.
- **Honours Google's reminder settings**: calendar defaults, per-event
  overrides, and — the case that is easy to get wrong — events where the user
  switched reminders off, which Google encodes the same way as "no overrides".
- **Authorizes directly** with a Desktop OAuth client: your browser, PKCE S256,
  a loopback callback that exists only during consent, a refresh token in the
  session keyring, and access tokens renewed only when a request needs one.
- **Exports a vdir tree** — one directory per calendar, a complete `.ics`
  snapshot, plus `displayname` and `color` — written atomically, and left as a
  valid empty calendar rather than deleted when the last event goes.
- **Notifies through the session's own notification server**, with an action
  that opens the meeting link.
- **Survives restarts and suspends** without repeating reminders or emptying a
  backlog onto your screen.

```
Google Calendar ──▶ sync engine ──▶ local snapshot ──┬──▶ vdir export ──▶ calendar client
                         ▲                           │
                    OAuth tokens                      └──▶ reminder scheduler ──▶ notification
                    (keyring)                                                       server
```

## Quick start

```sh
cargo build --release
install -Dm755 target/release/calendar-daemon ~/.local/bin/calendar-daemon

mkdir -p ~/.config/calendar-daemon
cp examples/config.toml ~/.config/calendar-daemon/config.toml
$EDITOR ~/.config/calendar-daemon/config.toml    # fill in [auth]

calendar-daemon check       # validate the configuration
calendar-daemon auth        # authorize in a browser, once
calendar-daemon sync        # fetch now
calendar-daemon status      # what it knows and how fresh it is
calendar-daemon paths       # where everything lives
```

Getting the OAuth client and wiring up a calendar client are in
[docs/install.md](docs/install.md).

### Trying the export without a Google account

```sh
cargo run --example sample_export -- /tmp/calendar-daemon-sample
```

Writes a sample vdir tree covering the awkward cases — an all-day event, an
event with reminders switched off, a cancelled one, a title full of characters
that need escaping. Point a local calendar account at it to check your reader
before authorizing anything real. Remove it with `rm -rf`.

## Configuration

`$XDG_CONFIG_HOME/calendar-daemon/config.toml`. Every key is documented in
[examples/config.toml](examples/config.toml); `calendar-daemon check` validates
it and says what to write instead when it is wrong.

Three settings carry the decisions most worth knowing about:

| Setting | Default | What it trades |
| --- | --- | --- |
| `sync.interval` | `6h` | How long an edit or cancellation can go unnoticed. `sync` is always immediate. |
| `reminders.catch_up_window` | `15m` | How late a reminder missed during a suspend may be and still be shown. |
| `reminders.max_sleep` | `2m` | Worst-case lateness after a resume. It is a local timer, not a request to Google. |

The reasoning behind these and the other judgement calls is in
[docs/decisions.md](docs/decisions.md).

## Scopes

Read-only, and nothing beyond calendars:

- `calendar.events.readonly`
- `calendar.calendarlist.readonly`

The daemon never writes to your calendar, never touches Gmail or Drive, and does
not share token storage with any other application.

## When something is wrong

`calendar-daemon status` names the state and what to do about it. The
distinction it works hardest to preserve is between failures that cost nothing
and the one that needs you:

| State | Means | What happens |
| --- | --- | --- |
| `degraded (transient)` | network, DNS, TLS | retried with backoff; credentials untouched |
| `degraded (throttled)` | HTTP 429 or 5xx | backed off; credentials untouched |
| `keyring unavailable` | locked or no secret service | the daemon keeps running and recovers when you unlock — **no new consent** |
| `not authorized` | nothing stored yet | run `calendar-daemon auth` |
| `reauthorization required` | Google rejected the grant | sync pauses; run `calendar-daemon auth`, which resumes a running daemon — **no restart** |
| `client misconfigured` | bad client id or secret | fix the config; consent will not help |

A failed sync never erases the last good calendar, and `status` reports how old
the data is.

**On Google asking for authorization again:** a stored refresh token removes
*one* reason a desktop application re-prompts. It is not a guarantee. Google can
expire or revoke a grant on its own — an OAuth client still in Testing, an
organization policy, a password change. This daemon's job is to make that
visible and to never confuse it with a locked keyring or a dropped connection,
not to promise it will not happen.

## Development

```sh
cargo test              # 189 tests: no network, no keyring, no graphical session
cargo clippy --all-targets
cargo fmt --check
```

Everything that encodes a rule lives in the library and is tested directly:
reminder inheritance, all-day timing, recurrence identity, restart
deduplication, catch-up policy, empty-calendar exports, and the failure
classification. The checks that need your own session and a real Google account
are listed separately in [docs/validation.md](docs/validation.md).

## Layout

| Path | What is in it |
| --- | --- |
| `src/model.rs` | Calendar types and the reminder rules |
| `src/auth/` | OAuth flow, keyring, failure classification |
| `src/google/` | The API subset and its mapping onto the model |
| `src/export/` | vdir tree and iCalendar serialization |
| `src/reminder.rs` | Which reminders fire, and when to look again |
| `src/daemon.rs` | The sync cycle and the run loop |
| `docs/architecture.md` | The design this implements |

## License

Not yet chosen. Nothing here has been published.
