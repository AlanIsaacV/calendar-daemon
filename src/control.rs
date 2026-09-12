//! The private control channel between a command and the running daemon.
//!
//! A Unix socket in the user's runtime directory, owner-only, carrying one
//! JSON request and one JSON response per connection. The background process
//! owns every write to state and every notification; `sync` and `status` ask it
//! to act rather than acting themselves.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Largest request this daemon will read, so a stray writer cannot grow it.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// Longest a Unix socket path may be.
///
/// The kernel's `sockaddr_un` is a fixed 108-byte buffer, so an otherwise
/// reasonable runtime directory can make the socket unbindable. Checking it
/// here turns an opaque kernel error into something a user can act on.
const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    /// Synchronize now. Concurrent requests coalesce into the cycle in flight.
    Sync,
    /// Report last success, next sync, next reminder and authorization health.
    Status,
    /// Stop the daemon.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// A sync finished. `coalesced` means this request joined one already in
    /// flight rather than starting another.
    Synced {
        calendars: usize,
        events: usize,
        coalesced: bool,
        partial: bool,
    },
    /// A sync was attempted and failed; the previous snapshot is intact.
    SyncFailed {
        category: String,
        message: String,
    },
    Status(Box<crate::store::StatusRecord>),
    Ack,
    Error {
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("no daemon is listening on {path}")]
    NotRunning { path: PathBuf },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the daemon sent something unreadable: {0}")]
    Protocol(String),
    #[error(
        "the control socket path is {length} bytes, over the {limit}-byte kernel limit: {path}\n\
         Set CALENDAR_DAEMON_RUNTIME_DIR to a shorter directory."
    )]
    PathTooLong {
        path: PathBuf,
        length: usize,
        limit: usize,
    },
}

/// Reject a socket path the kernel cannot represent, before it becomes an
/// unexplained bind failure.
pub fn check_socket_path(path: &Path) -> Result<(), ControlError> {
    let length = path.as_os_str().as_encoded_bytes().len();
    if length > MAX_SOCKET_PATH {
        return Err(ControlError::PathTooLong {
            path: path.to_path_buf(),
            length,
            limit: MAX_SOCKET_PATH,
        });
    }
    Ok(())
}

/// Bind the control socket, replacing a socket left behind by a crash.
///
/// Safe because the caller already holds the exclusive state lock: if this
/// process owns the state directory, no other daemon can be listening.
pub async fn bind(path: &Path) -> Result<UnixListener, ControlError> {
    check_socket_path(path)?;
    if let Some(parent) = path.parent() {
        crate::util::ensure_private_dir(parent).map_err(|source| ControlError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    if path.exists() {
        std::fs::remove_file(path).map_err(|source| ControlError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }

    let listener = UnixListener::bind(path).map_err(|source| ControlError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        ControlError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;

    Ok(listener)
}

/// Read one request from a connection.
pub async fn read_request(stream: &mut UnixStream) -> Result<Request, ControlError> {
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|err| ControlError::Protocol(err.to_string()))?;
    serde_json::from_str(line.trim())
        .map_err(|err| ControlError::Protocol(format!("unrecognized request: {err}")))
}

/// Write one response and close.
pub async fn write_response(
    stream: &mut UnixStream,
    response: &Response,
) -> Result<(), ControlError> {
    let mut text =
        serde_json::to_string(response).map_err(|err| ControlError::Protocol(err.to_string()))?;
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .await
        .map_err(|err| ControlError::Protocol(err.to_string()))?;
    stream
        .flush()
        .await
        .map_err(|err| ControlError::Protocol(err.to_string()))?;
    Ok(())
}

/// Send one request to a running daemon and read its answer.
pub async fn request(path: &Path, request: &Request) -> Result<Response, ControlError> {
    check_socket_path(path)?;
    let mut stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(ControlError::NotRunning {
                path: path.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(ControlError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let mut text =
        serde_json::to_string(request).map_err(|err| ControlError::Protocol(err.to_string()))?;
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .await
        .map_err(|source| ControlError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    stream.shutdown().await.map_err(|source| ControlError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut reader = BufReader::new(stream.take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|source| ControlError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if line.trim().is_empty() {
        return Err(ControlError::Protocol(
            "the daemon closed without answering".into(),
        ));
    }
    serde_json::from_str(line.trim())
        .map_err(|err| ControlError::Protocol(format!("unrecognized response: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("calendar-daemon-ctl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::util::ensure_private_dir(&dir).unwrap();
        dir.join("control.sock")
    }

    #[test]
    fn requests_and_responses_round_trip_as_json() {
        let request = Request::Sync;
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(encoded, r#"{"command":"sync"}"#);
        assert_eq!(serde_json::from_str::<Request>(&encoded).unwrap(), request);

        let response = Response::Synced {
            calendars: 2,
            events: 40,
            coalesced: true,
            partial: false,
        };
        let encoded = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<Response>(&encoded).unwrap(),
            response
        );
    }

    #[tokio::test]
    async fn a_request_reaches_the_daemon_and_the_answer_comes_back() {
        let path = socket_path("roundtrip");
        let listener = bind(&path).await.unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await.unwrap();
            assert_eq!(request, Request::Sync);
            write_response(
                &mut stream,
                &Response::Synced {
                    calendars: 1,
                    events: 3,
                    coalesced: false,
                    partial: false,
                },
            )
            .await
            .unwrap();
        });

        let response = request(&path, &Request::Sync).await.unwrap();
        assert_eq!(
            response,
            Response::Synced {
                calendars: 1,
                events: 3,
                coalesced: false,
                partial: false
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn talking_to_a_daemon_that_is_not_running_says_exactly_that() {
        let path = socket_path("absent");
        let err = request(&path, &Request::Status).await.unwrap_err();
        assert!(matches!(err, ControlError::NotRunning { .. }), "{err}");
    }

    #[tokio::test]
    async fn the_socket_is_owner_only() {
        let path = socket_path("perms");
        let _listener = bind(&path).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the control socket was world-reachable"
        );
    }

    #[tokio::test]
    async fn a_socket_left_by_a_crash_is_replaced_rather_than_blocking_startup() {
        let path = socket_path("stale");
        let first = bind(&path).await.unwrap();
        drop(first);
        assert!(path.exists(), "the test needs a leftover socket file");

        // Binding again must succeed: the caller holds the state lock, so no
        // other daemon can be listening on it.
        let _second = bind(&path).await.unwrap();
    }

    #[tokio::test]
    async fn a_socket_path_the_kernel_cannot_hold_is_named_rather_than_failing_opaquely() {
        let path = PathBuf::from(format!("/tmp/{}/control.sock", "x".repeat(200)));
        let err = check_socket_path(&path).unwrap_err();
        assert!(matches!(err, ControlError::PathTooLong { .. }));
        assert!(
            err.to_string().contains("CALENDAR_DAEMON_RUNTIME_DIR"),
            "unhelpful message: {err}"
        );

        let err = request(&path, &Request::Status).await.unwrap_err();
        assert!(matches!(err, ControlError::PathTooLong { .. }));
    }

    #[tokio::test]
    async fn a_malformed_request_is_rejected_without_killing_the_listener() {
        let path = socket_path("garbage");
        let listener = bind(&path).await.unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let result = read_request(&mut stream).await;
            assert!(result.is_err());
            write_response(
                &mut stream,
                &Response::Error {
                    message: "unrecognized request".into(),
                },
            )
            .await
            .unwrap();
        });

        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream.write_all(b"not json at all\n").await.unwrap();
        stream.shutdown().await.unwrap();

        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("error"), "{line}");
        server.await.unwrap();
    }
}
