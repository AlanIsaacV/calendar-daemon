//! Filesystem and duration helpers shared by the state store and the exporter.

use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Mode for files that may contain event details or credentials-adjacent data.
pub const FILE_MODE: u32 = 0o600;
/// Mode for directories this daemon owns.
pub const DIR_MODE: u32 = 0o700;

/// Create `dir` and every missing parent, then restrict it to the owner.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut perms = fs::metadata(dir)?.permissions();
    if perms.mode() & 0o777 != DIR_MODE {
        perms.set_mode(DIR_MODE);
        fs::set_permissions(dir, perms)?;
    }
    Ok(())
}

/// Write `contents` to `path` so a reader never observes a partial file.
///
/// The temporary file carries a `.tmp` suffix on purpose: the vdir readers that
/// consume the export directory skip `.tmp` entries, so a sync in progress is
/// never mistaken for a calendar.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    fs::create_dir_all(parent)?;

    let tmp = temp_path_for(path);
    {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    // An existing file keeps its own mode through a rename, so set it explicitly.
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    match fs::rename(&tmp, path) {
        Ok(()) => {}
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
    }
    // Durability of the rename itself, so a crash cannot leave the directory
    // entry pointing at nothing.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_string());
    let unique = std::process::id();
    path.with_file_name(format!(".{name}.{unique}.tmp"))
}

/// Render a duration the way the configuration file spells it.
pub fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return "0s".to_string();
    }
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hours, rem) = (rem / 3_600, rem % 3_600);
    let (minutes, seconds) = (rem / 60, rem % 60);
    let mut out = String::new();
    for (value, suffix) in [(days, 'd'), (hours, 'h'), (minutes, 'm'), (seconds, 's')] {
        if value > 0 {
            out.push_str(&format!("{value}{suffix}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn atomic_write_leaves_no_temporary_behind() {
        let dir = tempdir();
        let target = dir.join("calendar.ics");
        write_atomic(&target, b"BEGIN:VCALENDAR\r\n", FILE_MODE).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"BEGIN:VCALENDAR\r\n");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    }

    #[test]
    fn atomic_write_restricts_mode_on_overwrite() {
        let dir = tempdir();
        let target = dir.join("state.json");
        write_atomic(&target, b"{}", FILE_MODE).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();

        write_atomic(&target, b"{\"a\":1}", FILE_MODE).unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, FILE_MODE);
    }

    #[test]
    fn durations_round_trip_through_the_configuration_spelling() {
        assert_eq!(format_duration(Duration::from_secs(6 * 3600)), "6h");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
    }

    pub(crate) fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "calendar-daemon-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        ensure_private_dir(&base).unwrap();
        base
    }
}
