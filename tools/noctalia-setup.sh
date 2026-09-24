#!/usr/bin/env bash
#
# Wire Noctalia's calendar to this daemon's export, and report enough evidence
# to tell a working setup from a silent one.
#
# It writes exactly one new file — a drop-in in Noctalia's config directory —
# and never edits a file you already have. Undo it with `rm`; the path is
# printed at the end.
#
#   tools/noctalia-setup.sh             # set it up and report
#   tools/noctalia-setup.sh --dry-run   # report only, write nothing
#   tools/noctalia-setup.sh --force     # overwrite the drop-in
#   tools/noctalia-setup.sh --report    # also tee the report into the repo
#
# --report exists because the daemon's user and this repository's user need not
# be the same account: the report lands somewhere both can read, so the run can
# be checked rather than retold.
#
set -o errexit -o nounset -o pipefail

ACCOUNT_ID=calendar-daemon
DROP_IN_NAME=calendar-daemon.toml

dry_run=false
force=false
report=
repo_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
for arg in "$@"; do
    case $arg in
        --dry-run) dry_run=true ;;
        --force) force=true ;;
        --report) report=$repo_root/noctalia-setup-report.txt ;;
        --report=*) report=${arg#--report=} ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

if [ -n "$report" ]; then
    # Process substitution rather than a pipe, so the script's own exit status
    # survives. The copy on disk is stripped of colour, since it is meant to be
    # read by whoever did not run it.
    exec > >(tee >(sed 's/\x1b\[[0-9;]*m//g' > "$report")) 2>&1
    printf 'Report: %s\n' "$report"
fi

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
bad() { printf '  \033[31m✗\033[0m %s\n' "$*"; }
good() { printf '  \033[32m✓\033[0m %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }

# The [calendar] block of the merged config: everything from its header to the
# next *unindented* section. Sub-tables are indented and stay in the block.
#
# Read into a variable and filtered from a here-string rather than piped: an
# awk that stopped early would hand `noctalia config export` a SIGPIPE, which
# under `pipefail` and `errexit` kills this script with no output at all. That
# only happens when a section follows [calendar] — which is every real config,
# and no fixture that puts [calendar] last.
calendar_lines() {
    awk '
        /^\[calendar/ { inblock = 1 }
        inblock && /^\[[A-Za-z]/ && !/^\[calendar/ { inblock = 0 }
        inblock { print }
    '
}

calendar_block() {
    local merged
    merged=$(noctalia config export 2>/dev/null) || true
    calendar_lines <<<"$merged"
}

for tool in calendar-daemon noctalia; do
    command -v "$tool" >/dev/null 2>&1 || { bad "$tool is not on PATH"; exit 1; }
done

# ---------------------------------------------------------------- the export

say "1. The daemon's export"

# `paths` is authoritative: it resolves the config's own [export] directory,
# which need not be the default.
paths_output=$(calendar-daemon paths 2>/dev/null) || true
export_dir=$(awk '$1 == "export" { print $2 }' <<<"$paths_output")
[ -n "$export_dir" ] || { bad "could not read the export directory from \`calendar-daemon paths\`"; exit 1; }
note "$export_dir"

if [ ! -d "$export_dir" ]; then
    bad "that directory does not exist — the daemon has not exported yet"
    note "Run: calendar-daemon sync"
    exit 1
fi

collections=0
events=0
while IFS= read -r ics; do
    collections=$((collections + 1))
    n=$(grep -c '^BEGIN:VEVENT' "$ics" || true)
    events=$((events + n))
    printf '    %-40s %4s event(s)   %s\n' \
        "$(basename "$(dirname "$ics")")" "$n" \
        "$(date -r "$ics" '+%Y-%m-%d %H:%M')"
done < <(find "$export_dir" -mindepth 2 -maxdepth 2 -name '*.ics' -type f | sort)

if [ "$collections" -eq 0 ]; then
    bad "no collections found — Noctalia would find nothing to read either"
    note "Run: calendar-daemon sync"
    exit 1
fi
good "$collections collection(s), $events event(s) on disk"
[ "$events" -eq 0 ] && note "An export with no events displays as an empty calendar, correctly."

# ------------------------------------------------------- Noctalia, as it is

say "2. Noctalia's calendar settings, before"

config_dir=${NOCTALIA_CONFIG_HOME:-${XDG_CONFIG_HOME:-$HOME/.config}}/noctalia
state_dir=${NOCTALIA_STATE_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}}/noctalia
drop_in=$config_dir/$DROP_IN_NAME

before=$(calendar_block)
if [ -z "$before" ]; then
    bad "no [calendar] section in the merged config: the service is off and no account is configured"
else
    printf '%s\n' "$before" | sed 's/^/    /'
fi

# --------------------------------------------------------------- the drop-in

say "3. The drop-in"

if [ -e "$drop_in" ] && [ "$force" = false ]; then
    good "already present: $drop_in"
    note "Re-write it with --force."
elif [ "$dry_run" = true ]; then
    note "would write $drop_in"
else
    mkdir -p "$config_dir"
    cat > "$drop_in" <<EOF
# Written by calendar-daemon's tools/noctalia-setup.sh.
# Noctalia merges every *.toml in this directory, so this leaves your own
# config.toml untouched. Remove this file to undo.

[calendar]
enabled = true

[calendar.account.$ACCOUNT_ID]
type = "vdir"
name = "Google (calendar-daemon)"
path = "$export_dir"
calendars = []
EOF
    good "wrote $drop_in"
fi

if [ "$dry_run" = false ]; then
    noctalia config validate >/dev/null 2>&1 \
        && good "config validates" \
        || { bad "config does not validate:"; noctalia config validate 2>&1 | sed 's/^/    /'; exit 1; }
    noctalia msg config-reload >/dev/null 2>&1 \
        && good "reloaded the running shell" \
        || note "could not reach a running Noctalia; it will pick this up on next start"
fi

# ------------------------------------------------------------ what it became

if [ "$dry_run" = true ]; then
    say "Nothing was written."
    note "Re-run without --dry-run to apply it."
    exit 0
fi

say "4. Noctalia's calendar settings, after"

after=$(calendar_block)
printf '%s\n' "$after" | sed 's/^/    /'

enabled=$(printf '%s\n' "$after" | awk -F'= *' '$1 ~ /^enabled/ { print $2; exit }')
if [ "$enabled" = "true" ]; then
    good "the calendar service is on and pointed at the export"
    note "Open Noctalia's calendar. Events land on their dates within a few seconds:"
    note "it watches the tree with inotify rather than waiting for refresh_minutes."
else
    # The state directory's settings.toml is applied last and wins, so a
    # toggle left there defeats the drop-in with no error anywhere.
    bad "calendar.enabled is still not true — something later in the stack is pinning it"
    pinned=
    [ -f "$state_dir/settings.toml" ] && pinned=$(calendar_lines < "$state_dir/settings.toml") || true
    if [ -n "$pinned" ]; then
        note "$state_dir/settings.toml is applied after the config directory, and has:"
        printf '%s\n' "$pinned" | sed 's/^/      /'
        note "Set enabled = true there, or delete that [calendar] block."
    else
        note "It is not $state_dir/settings.toml. Look for another *.toml in"
        note "$config_dir that sets calendar.enabled."
    fi
    exit 1
fi
