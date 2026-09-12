//! The vdir export: one collection directory per calendar, each holding a
//! complete `.ics` snapshot plus the metadata files a vdir reader looks for.

pub mod ics;

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::model::{CalendarRef, Occurrence};
use crate::util;

/// Name of the snapshot file inside each collection directory.
const SNAPSHOT_FILE: &str = "calendar.ics";

/// What one export pass changed, for logging and for `status`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExportReport {
    /// Collections whose snapshot was rewritten.
    pub written: Vec<String>,
    /// Collections whose snapshot was already byte-identical.
    pub unchanged: Vec<String>,
    /// Collections emptied because their calendar went away but removing them
    /// would have left the root with none.
    pub emptied: Vec<String>,
    /// Collection directories removed.
    pub removed: Vec<String>,
}

impl ExportReport {
    pub fn touched(&self) -> usize {
        self.written.len() + self.emptied.len() + self.removed.len()
    }
}

/// Write the export for `calendars`, given every occurrence in the window.
///
/// `previous_collections` names the collection directories the last export
/// created, which is what lets a calendar the user removed be cleaned up
/// without this function ever deleting a directory it did not write.
pub fn write_vdir(
    root: &Path,
    calendars: &[CalendarRef],
    occurrences: &[Occurrence],
    previous_collections: &BTreeSet<String>,
    write_color: bool,
) -> io::Result<ExportReport> {
    util::ensure_private_dir(root)?;
    let mut report = ExportReport::default();

    let current: BTreeSet<String> = calendars.iter().map(CalendarRef::collection_name).collect();

    for calendar in calendars {
        let name = calendar.collection_name();
        let dir = root.join(&name);
        util::ensure_private_dir(&dir)?;

        let mine: Vec<&Occurrence> = occurrences
            .iter()
            .filter(|o| o.calendar_id == calendar.id)
            .collect();

        let contents = ics::serialize_calendar(
            &calendar.summary,
            calendar.time_zone.as_deref(),
            calendar.color.as_deref().filter(|_| write_color),
            &mine,
        );

        let target = dir.join(SNAPSHOT_FILE);
        if write_if_changed(&target, contents.as_bytes())? {
            report.written.push(name.clone());
        } else {
            report.unchanged.push(name.clone());
        }

        write_if_changed(&dir.join("displayname"), calendar.summary.as_bytes())?;
        let color_file = dir.join("color");
        match calendar.color.as_deref().filter(|_| write_color) {
            Some(color) => {
                write_if_changed(&color_file, color.as_bytes())?;
            }
            None if color_file.exists() => {
                fs::remove_file(&color_file)?;
            }
            None => {}
        }
    }

    // A calendar that disappeared from the account: its directory goes, but
    // only while at least one collection is left behind. A root with no
    // collections at all reads as a failed refresh to a vdir consumer, which
    // then keeps displaying the events it already had — so the safe fallback is
    // an empty calendar rather than an empty directory.
    let stale: Vec<&String> = previous_collections.difference(&current).collect();
    for name in stale {
        let dir = root.join(name);
        if !dir.is_dir() {
            continue;
        }
        if current.is_empty() {
            let empty = ics::serialize_calendar(name, None, None, &[]);
            write_if_changed(&dir.join(SNAPSHOT_FILE), empty.as_bytes())?;
            report.emptied.push(name.clone());
        } else {
            fs::remove_dir_all(&dir)?;
            report.removed.push(name.clone());
        }
    }

    Ok(report)
}

/// Write only when the bytes differ.
///
/// vdir consumers watch these directories, so rewriting an identical snapshot
/// every cycle would wake a reader for nothing. Returns whether it wrote.
fn write_if_changed(path: &Path, contents: &[u8]) -> io::Result<bool> {
    if let Ok(existing) = fs::read(path)
        && existing == contents
    {
        return Ok(false);
    }
    util::write_atomic(path, contents, util::FILE_MODE)?;
    Ok(true)
}

/// The collection directories currently present under `root`.
pub fn discover_collections(root: &Path) -> BTreeSet<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return BTreeSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter(|e| e.path().join(SNAPSHOT_FILE).is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// The snapshot path for one collection, for documentation and tests.
pub fn snapshot_path(root: &Path, calendar: &CalendarRef) -> PathBuf {
    root.join(calendar.collection_name()).join(SNAPSHOT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventStatus, EventTime, ReminderSpec, ResponseStatus};

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "calendar-daemon-export-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        util::ensure_private_dir(&dir).unwrap();
        dir
    }

    fn calendar(id: &str, summary: &str) -> CalendarRef {
        CalendarRef {
            id: id.into(),
            summary: summary.into(),
            time_zone: Some("America/Bogota".into()),
            color: Some("#9fe1e7".into()),
            primary: true,
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
            meeting_url: Some("https://meet.example/x".into()),
            html_link: None,
            start: EventTime::Timed {
                at: "2026-09-11T14:00:00Z".parse().unwrap(),
                zone: None,
            },
            end: EventTime::Timed {
                at: "2026-09-11T14:30:00Z".parse().unwrap(),
                zone: None,
            },
            status: EventStatus::Confirmed,
            response: ResponseStatus::Accepted,
            reminders: ReminderSpec::UseDefault,
            updated: Some("2026-09-01T08:00:00Z".parse().unwrap()),
        }
    }

    #[test]
    fn a_calendar_becomes_a_collection_with_its_metadata() {
        let root = workspace("basic");
        let cal = calendar("work@example.com", "Trabajo");
        let events = vec![event("work@example.com", "e1")];

        let report = write_vdir(
            &root,
            std::slice::from_ref(&cal),
            &events,
            &BTreeSet::new(),
            true,
        )
        .unwrap();
        assert_eq!(report.written, vec!["work_example.com".to_string()]);

        let dir = root.join("work_example.com");
        assert!(dir.join("calendar.ics").is_file());
        assert_eq!(
            fs::read_to_string(dir.join("displayname")).unwrap(),
            "Trabajo"
        );
        assert_eq!(fs::read_to_string(dir.join("color")).unwrap(), "#9fe1e7");

        let ics = fs::read_to_string(snapshot_path(&root, &cal)).unwrap();
        assert!(
            ics.contains("URL:https://meet.example/x"),
            "meeting link missing: {ics}"
        );
    }

    #[test]
    fn deleting_the_last_event_leaves_a_valid_empty_calendar_not_a_missing_file() {
        let root = workspace("empty");
        let cal = calendar("work@example.com", "Trabajo");

        write_vdir(
            &root,
            std::slice::from_ref(&cal),
            &[event("work@example.com", "e1")],
            &BTreeSet::new(),
            true,
        )
        .unwrap();
        let collections = discover_collections(&root);

        write_vdir(&root, std::slice::from_ref(&cal), &[], &collections, true).unwrap();

        let path = snapshot_path(&root, &cal);
        assert!(
            path.is_file(),
            "the snapshot was deleted with its last event"
        );
        let ics = fs::read_to_string(&path).unwrap();
        assert!(ics.contains("BEGIN:VCALENDAR"));
        assert!(ics.contains("END:VCALENDAR"));
        assert!(!ics.contains("BEGIN:VEVENT"));
        assert!(
            !discover_collections(&root).is_empty(),
            "the root lost every collection"
        );
    }

    #[test]
    fn an_unchanged_calendar_is_not_rewritten() {
        let root = workspace("stable");
        let cal = calendar("work@example.com", "Trabajo");
        let events = vec![event("work@example.com", "e1")];

        write_vdir(
            &root,
            std::slice::from_ref(&cal),
            &events,
            &BTreeSet::new(),
            true,
        )
        .unwrap();
        let before = fs::metadata(snapshot_path(&root, &cal))
            .unwrap()
            .modified()
            .unwrap();

        let report = write_vdir(
            &root,
            std::slice::from_ref(&cal),
            &events,
            &discover_collections(&root),
            true,
        )
        .unwrap();

        assert!(report.written.is_empty(), "rewrote an unchanged calendar");
        assert_eq!(report.unchanged.len(), 1);
        let after = fs::metadata(snapshot_path(&root, &cal))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            before, after,
            "an unchanged calendar woke the filesystem watcher"
        );
    }

    #[test]
    fn a_removed_calendar_is_cleaned_up_while_another_remains() {
        let root = workspace("removed");
        let work = calendar("work@example.com", "Trabajo");
        let personal = calendar("me@example.com", "Personal");

        write_vdir(
            &root,
            &[work.clone(), personal.clone()],
            &[],
            &BTreeSet::new(),
            true,
        )
        .unwrap();
        let previous = discover_collections(&root);
        assert_eq!(previous.len(), 2);

        let report = write_vdir(&root, std::slice::from_ref(&work), &[], &previous, true).unwrap();
        assert_eq!(report.removed, vec!["me_example.com".to_string()]);
        assert!(!root.join("me_example.com").exists());
        assert!(root.join("work_example.com").exists());
    }

    #[test]
    fn removing_the_only_calendar_empties_it_rather_than_leaving_no_collections() {
        let root = workspace("last");
        let work = calendar("work@example.com", "Trabajo");

        write_vdir(
            &root,
            &[work],
            &[event("work@example.com", "e1")],
            &BTreeSet::new(),
            true,
        )
        .unwrap();
        let previous = discover_collections(&root);

        let report = write_vdir(&root, &[], &[], &previous, true).unwrap();

        assert_eq!(report.emptied, vec!["work_example.com".to_string()]);
        assert!(report.removed.is_empty());
        let collections = discover_collections(&root);
        assert_eq!(
            collections.len(),
            1,
            "the root was left with no collection at all"
        );
        let ics = fs::read_to_string(root.join("work_example.com/calendar.ics")).unwrap();
        assert!(!ics.contains("BEGIN:VEVENT"));
    }

    #[test]
    fn a_directory_the_export_never_created_is_left_untouched() {
        let root = workspace("foreign");
        let foreign = root.join("someone-elses-calendar");
        util::ensure_private_dir(&foreign).unwrap();
        fs::write(
            foreign.join("calendar.ics"),
            "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
        )
        .unwrap();

        // `previous_collections` is empty, so the export knows of no directory
        // of its own to clean up.
        write_vdir(
            &root,
            &[calendar("work@example.com", "Trabajo")],
            &[],
            &BTreeSet::new(),
            true,
        )
        .unwrap();

        assert!(
            foreign.join("calendar.ics").is_file(),
            "removed a directory it did not create"
        );
    }

    #[test]
    fn only_this_calendars_events_reach_its_collection() {
        let root = workspace("split");
        let work = calendar("work@example.com", "Trabajo");
        let personal = calendar("me@example.com", "Personal");
        let events = vec![
            event("work@example.com", "w1"),
            event("me@example.com", "p1"),
        ];

        write_vdir(
            &root,
            &[work.clone(), personal.clone()],
            &events,
            &BTreeSet::new(),
            true,
        )
        .unwrap();

        let work_ics = fs::read_to_string(snapshot_path(&root, &work)).unwrap();
        assert!(work_ics.contains("UID:w1"));
        assert!(
            !work_ics.contains("UID:p1"),
            "events leaked across calendars"
        );
    }

    #[test]
    fn no_temporary_file_is_left_where_a_reader_would_find_it() {
        let root = workspace("tmp");
        let cal = calendar("work@example.com", "Trabajo");
        write_vdir(
            &root,
            &[cal],
            &[event("work@example.com", "e1")],
            &BTreeSet::new(),
            true,
        )
        .unwrap();

        let names: Vec<String> = fs::read_dir(root.join("work_example.com"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        // A vdir reader skips dotfiles and `.tmp`, so anything else must be a
        // file this export means to publish.
        for name in &names {
            assert!(
                ["calendar.ics", "displayname", "color"].contains(&name.as_str()),
                "unexpected file published: {name}"
            );
        }
    }

    #[test]
    fn the_export_root_and_its_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = workspace("perms");
        let cal = calendar("work@example.com", "Trabajo");
        write_vdir(
            &root,
            std::slice::from_ref(&cal),
            &[event("work@example.com", "e1")],
            &BTreeSet::new(),
            true,
        )
        .unwrap();

        let dir_mode = fs::metadata(root.join("work_example.com"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        let file_mode = fs::metadata(snapshot_path(&root, &cal))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600);
    }

    #[test]
    fn dropping_a_colour_removes_the_metadata_file() {
        let root = workspace("color");
        let mut cal = calendar("work@example.com", "Trabajo");
        write_vdir(&root, &[cal.clone()], &[], &BTreeSet::new(), true).unwrap();
        assert!(root.join("work_example.com/color").is_file());

        cal.color = None;
        write_vdir(&root, &[cal], &[], &discover_collections(&root), true).unwrap();
        assert!(!root.join("work_example.com/color").exists());
    }
}
