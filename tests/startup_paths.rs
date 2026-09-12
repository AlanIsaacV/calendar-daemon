use std::path::{Path, PathBuf};
use std::process::Command;

use calendar_daemon::{control, util};

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "calendar-daemon-startup-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    util::ensure_private_dir(&dir).unwrap();
    dir
}

fn a_file_not_a_directory(dir: &Path) -> PathBuf {
    let path = dir.join("a-file-not-a-directory");
    std::fs::write(&path, b"").unwrap();
    path
}

fn a_link_to_nothing(dir: &Path) -> PathBuf {
    let path = dir.join("a-link-to-nothing");
    std::os::unix::fs::symlink(dir.join("no-such-target"), &path).unwrap();
    path
}

fn assert_sync_takes_the_not_running_branch(runtime_dir: &Path) {
    let socket = runtime_dir.join("control.sock");
    assert!(
        control::check_socket_path(&socket).is_ok(),
        "socket path is {} bytes, over the kernel limit: this test would exercise \
         the PathTooLong branch of `sync`, not NotRunning",
        socket.as_os_str().len()
    );
}

fn failure_of(dir: &Path, command: &str, state_dir: &Path, runtime_dir: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_calendar-daemon"))
        .arg(command)
        .env("CALENDAR_DAEMON_CONFIG", dir.join("config.toml"))
        .env("CALENDAR_DAEMON_STATE_DIR", state_dir)
        .env("CALENDAR_DAEMON_RUNTIME_DIR", runtime_dir)
        .env("RUST_LOG", "error")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "`{command}` succeeded with an uncreatable directory: {stderr}"
    );
    stderr
}

#[test]
fn run_names_the_runtime_directory_when_that_is_the_one_it_could_not_create() {
    let dir = workspace("run-runtime");
    let state_dir = dir.join("state");
    let runtime_dir = a_file_not_a_directory(&dir).join("runtime");

    let stderr = failure_of(&dir, "run", &state_dir, &runtime_dir);

    assert!(
        stderr.contains("creating the runtime directory"),
        "misnamed: {stderr}"
    );
    assert!(
        stderr.contains(&runtime_dir.display().to_string()),
        "no path in: {stderr}"
    );
    assert!(!stderr.contains("state directory"), "misnamed: {stderr}");
    assert_eq!(
        stderr.matches("Not a directory").count(),
        1,
        "the io cause should appear once, under `Caused by:`: {stderr}"
    );
}

#[test]
fn run_names_the_state_directory_when_that_is_the_one_it_could_not_create() {
    let dir = workspace("run-state");
    let state_dir = a_file_not_a_directory(&dir).join("state");
    let runtime_dir = dir.join("runtime");

    let stderr = failure_of(&dir, "run", &state_dir, &runtime_dir);

    assert!(
        stderr.contains("creating the state directory"),
        "misnamed: {stderr}"
    );
    assert!(
        stderr.contains(&state_dir.display().to_string()),
        "no path in: {stderr}"
    );
    assert!(!stderr.contains("runtime directory"), "misnamed: {stderr}");
    assert_eq!(
        stderr.matches("Not a directory").count(),
        1,
        "the io cause should appear once, under `Caused by:`: {stderr}"
    );
}

#[test]
fn sync_names_the_runtime_directory_when_that_is_the_one_it_could_not_create() {
    let dir = workspace("sync-runtime");
    let state_dir = dir.join("state");
    let runtime_dir = a_link_to_nothing(&dir).join("runtime");
    assert_sync_takes_the_not_running_branch(&runtime_dir);

    let stderr = failure_of(&dir, "sync", &state_dir, &runtime_dir);

    assert!(
        stderr.contains("creating the runtime directory"),
        "misnamed: {stderr}"
    );
    assert!(
        stderr.contains(&runtime_dir.display().to_string()),
        "no path in: {stderr}"
    );
    assert!(!stderr.contains("state directory"), "misnamed: {stderr}");
    assert_eq!(
        stderr.matches("File exists").count(),
        1,
        "the io cause of a dangling symlink is EEXIST, and it should appear once, \
         under `Caused by:`: {stderr}"
    );
}

#[test]
fn sync_names_the_state_directory_when_that_is_the_one_it_could_not_create() {
    let dir = workspace("sync-state");
    let state_dir = a_file_not_a_directory(&dir).join("state");
    let runtime_dir = dir.join("runtime");
    assert_sync_takes_the_not_running_branch(&runtime_dir);

    let stderr = failure_of(&dir, "sync", &state_dir, &runtime_dir);

    assert!(
        stderr.contains("creating the state directory"),
        "misnamed: {stderr}"
    );
    assert!(
        stderr.contains(&state_dir.display().to_string()),
        "no path in: {stderr}"
    );
    assert!(!stderr.contains("runtime directory"), "misnamed: {stderr}");
    assert_eq!(
        stderr.matches("Not a directory").count(),
        1,
        "the io cause should appear once, under `Caused by:`: {stderr}"
    );
}
