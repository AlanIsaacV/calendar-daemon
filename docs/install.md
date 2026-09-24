# Installing, and undoing it

Everything this daemon writes lives in three directories it owns. Nothing is
installed system-wide, no existing configuration is edited, and the rollback at
the end of this file returns the machine to exactly where it started.

`calendar-daemon paths` prints the three directories for your machine.

| What | Default |
| --- | --- |
| Configuration | `~/.config/calendar-daemon/config.toml` |
| State (snapshot, reminder ledger, lock) | `~/.local/state/calendar-daemon/` |
| Calendar export | `~/.local/state/calendar-daemon/calendars/` |
| Control socket | `$XDG_RUNTIME_DIR/calendar-daemon/control.sock` |

All four are owner-only (`0700` directories, `0600` files).

## 1. A Google OAuth client

The daemon authenticates as its own installed application. This is a one-time
setup in the Google Cloud console.

1. Pick or create a project. An existing project is fine; a new one is not
   required.
2. Enable the **Google Calendar API** for it.
3. Configure the OAuth consent screen. **External** with your own account added
   as a test user is enough for personal use.
4. Create credentials → **OAuth client ID** → application type **Desktop app**.
5. Copy the client ID and client secret into `[auth]` in your config file.

A desktop client's id and secret are not a security boundary — Google documents
installed applications as public clients, which is why the flow uses PKCE. They
are still installation data: keep them out of any repository.

**Testing-mode caveat.** While the consent screen is in Testing, Google expires
refresh tokens after seven days for most scopes. Calendar scopes do not qualify
for the identity-only exception, so a Testing client will ask for consent again
roughly weekly. Publishing the consent screen removes that limit. The daemon
reports this as `reauthorization required`, distinct from every failure that
does not need you.

## 2. Build and install the binary

```sh
cargo build --release
install -Dm755 target/release/calendar-daemon ~/.local/bin/calendar-daemon
```

Make sure `~/.local/bin` is on your `PATH`.

## 3. Configure

```sh
mkdir -p ~/.config/calendar-daemon
cp examples/config.toml ~/.config/calendar-daemon/config.toml
chmod 600 ~/.config/calendar-daemon/config.toml
$EDITOR ~/.config/calendar-daemon/config.toml
calendar-daemon check
```

`check` validates the file and, when something is wrong, says what to write
instead rather than only that it is wrong.

## 4. Authorize

```sh
calendar-daemon auth
```

This binds a listener on a free loopback port, opens your browser, and waits.
The listener closes as soon as consent finishes or the timeout expires; the
daemon keeps no HTTP listener while it runs.

The refresh token goes into the session keyring under service
`calendar-daemon`, keyed by the `auth.account` value. You can see it with:

```sh
secret-tool search --all service calendar-daemon
```

If the grant comes back covering fewer scopes than were asked for — you unticked
a box on the consent screen — `auth` refuses it and says which are missing,
rather than storing a token that fails hours later.

Running `auth` again is also how a running daemon recovers from
`reauthorization required`. Once the new grant is stored, `auth` asks the daemon
to sync with it and prints the result; the daemon resumes its schedule without a
service restart. If that sync fails, `auth` prints why and suggests restarting
the service with `systemctl --user restart calendar-daemon.service`; the usual
reason a restart is needed is a daemon started from a build that predates this
recovery.

## 5. Fetch, and check

```sh
calendar-daemon sync
calendar-daemon status
ls ~/.local/state/calendar-daemon/calendars/
```

Each calendar becomes a directory holding `calendar.ics`, `displayname` and
`color`.

## 6. Run it as a user service

```sh
mkdir -p ~/.config/systemd/user
cp packaging/calendar-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now calendar-daemon.service
systemctl --user status calendar-daemon.service
journalctl --user -u calendar-daemon.service -f
```

The unit starts after `graphical-session.target`, because the keyring and the
notification server both live on the session bus. It does not *require* them:
if the keyring is still locked when the daemon starts, it starts anyway, reports
`keyring unavailable`, and succeeds on the first operation after you unlock —
with no new consent, because the authorization was never in question.

The unit runs sandboxed: `ProtectSystem=strict` and `ProtectHome=read-only`
leave the filesystem read-only apart from the two `ReadWritePaths=` entries.
`ProtectHome=read-only` covers `/run/user` as well as `/home`, so the daemon
cannot create its own runtime directory from inside the sandbox.
`RuntimeDirectory=calendar-daemon` has systemd create it at mode `0700` before
the daemon starts — exempt from both protections — and remove it again when the
service stops.

Only one process may own the state directory. A second `calendar-daemon run`
fails immediately rather than producing two schedulers that both notify for the
same meeting.

## 7. Point a calendar client at the export

The export is an ordinary [vdir](https://vdirsyncer.pimutils.org/en/stable/vdir.html)
tree, so any client that reads local calendar directories can consume it. For a
client that watches the directory for changes, no further configuration is
needed after adding the account.

1. Create the directory first, so the client has something to watch:
   `mkdir -p ~/.local/state/calendar-daemon/calendars`
2. Add a **Local (vdir)** calendar account pointing at that directory.
3. Confirm the events appear and a meeting link opens.

### Noctalia

Noctalia's calendar service is **off by default** — its built-in defaults carry
`calendar.enabled = false`, so an account on its own displays nothing and the
calendar shows bare dates. Both halves are needed. In
`~/.config/noctalia/config.toml`:

```toml
[calendar]
enabled = true

[calendar.account.calendar-daemon]
type = "vdir"
name = "Google (calendar-daemon)"
path = "~/.local/state/calendar-daemon/calendars"
calendars = []   # empty means every collection it discovers
```

`tools/noctalia-setup.sh` writes exactly that, as a drop-in beside your own
config — Noctalia merges every `*.toml` in the directory, so nothing you already
have is edited, and `rm` undoes it. It also reports what the daemon has on disk
and what Noctalia's merged configuration became, which is what tells a working
setup apart from a silent one:

```sh
tools/noctalia-setup.sh --dry-run   # report only
tools/noctalia-setup.sh             # write the drop-in, validate, reload
tools/noctalia-setup.sh --report    # and leave the report in the repository
```

`--report` is for the case where the account running the daemon and the account
working on this repository are not the same one: it leaves the run's evidence at
`noctalia-setup-report.txt`, which both can read, so the result can be checked
instead of described.

**The override that defeats all of this.** `~/.local/state/noctalia/settings.toml`
is applied *after* the config directory and wins. If Noctalia's settings UI has
ever written `calendar.enabled = false` there, the drop-in is overridden with no
error in any log, and the calendar stays empty exactly as if the export were
unreadable. `noctalia config export` shows the merged truth, and the script
reads it back and names the offending file when they disagree.

Nothing else is required: Noctalia watches the tree with inotify, so a sync
appears without waiting for `calendar.refresh_minutes`. It reads every `.ics`
file in a collection directory and every `VEVENT` inside each one, which is
what the daemon's one-snapshot-per-calendar layout depends on, and it takes the
`displayname` and `color` files the daemon writes. Snapshots are staged as
`.calendar.ics.<pid>.tmp`, which the reader skips, so a half-written file is
never displayed.

**Try it with the sample first.** `cargo run --example sample_export --
/tmp/calendar-daemon-sample` writes a tree with no Google account involved. Point
the client at that, confirm it renders, then switch to the real path. It costs
one minute and separates "the daemon is wrong" from "the client is not reading
this directory".

**Migrating from a client's own Google account.** Add the local account, confirm
it shows the same events, and only then remove the client's Google account —
otherwise every event appears twice. Keep the Google account's settings written
down until you are satisfied; re-adding it is the rollback.

## Rollback

Each step undoes one thing. Stop wherever you have undone enough.

**Stop the service.**

```sh
systemctl --user disable --now calendar-daemon.service
rm ~/.config/systemd/user/calendar-daemon.service
systemctl --user daemon-reload
```

**Restore the calendar client.** Remove the local (vdir) account you added. If
you removed a Google account from it, add it back.

**Remove the stored authorization.**

```sh
calendar-daemon auth --forget
```

This deletes the refresh token from the keyring. Google still lists the
application until you revoke it at
<https://myaccount.google.com/permissions>.

**Remove the local data.** The export and the state directory:

```sh
rm -rf ~/.local/state/calendar-daemon
```

**Remove the configuration and the binary.**

```sh
rm -rf ~/.config/calendar-daemon
rm ~/.local/bin/calendar-daemon
```

**Delete the OAuth client.** In the Google Cloud console, if you created one for
this and want it gone.

Nothing else on the machine was modified: the daemon writes only inside the
directories above and the one keyring entry.

## Running a second instance without disturbing the first

Every path is overridable, so an experimental build can run beside an installed
one:

```sh
export CALENDAR_DAEMON_CONFIG=/tmp/try/config.toml
export CALENDAR_DAEMON_STATE_DIR=/tmp/try/state
export CALENDAR_DAEMON_RUNTIME_DIR=/tmp/try/run
```

The runtime directory holds the control socket, which the kernel limits to about
100 bytes of path. A directory nested too deep is reported by name with that
limit, not as an unexplained bind failure.

These overrides are meant for a shell, not for the packaged unit. The daemon
resolves its runtime directory itself — `CALENDAR_DAEMON_RUNTIME_DIR`, else
`$XDG_RUNTIME_DIR/calendar-daemon` — and never reads what systemd exports about
the directory it provisioned; under the unit the two agree only because
`RuntimeDirectory=calendar-daemon` names the same path. Setting
`CALENDAR_DAEMON_RUNTIME_DIR` there points the daemon at a directory systemd has
not created, and what follows depends on the path. One the sandbox leaves
read-only — most of the filesystem, under `ProtectSystem=strict` and
`ProtectHome=read-only` — fails at startup, and says so in the journal. One
under `/tmp` does not fail: `PrivateTmp=true` alongside `ProtectSystem=strict`
leaves `/tmp` and `/var/tmp` writable (systemd.exec(5), under `ProtectSystem=`),
so `/tmp/try/run` above would start cleanly and bind the control socket inside
the unit's private `/tmp`, which nothing outside the unit can reach. The service
then looks healthy while every `calendar-daemon sync` and `status` from a shell
quietly takes the not-running branch — the worse of the two outcomes, because
nothing announces it. Override it in the unit only alongside a
`RuntimeDirectory=` naming the same directory — or, for a path outside
`$XDG_RUNTIME_DIR`, a `ReadWritePaths=` entry for it.
