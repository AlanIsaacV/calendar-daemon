//! Configuration file, its validation, and the directories the daemon owns.
//!
//! Every path can be overridden from the environment so a test run — or a
//! second instance used to try a change — never touches the state or the export
//! directory of the installed daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::util;

/// Environment override for the configuration file.
pub const ENV_CONFIG: &str = "CALENDAR_DAEMON_CONFIG";
/// Environment override for the state directory.
pub const ENV_STATE_DIR: &str = "CALENDAR_DAEMON_STATE_DIR";
/// Environment override for the control socket directory.
pub const ENV_RUNTIME_DIR: &str = "CALENDAR_DAEMON_RUNTIME_DIR";

/// Name used for the config, state and runtime directories.
pub const APP_NAME: &str = "calendar-daemon";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file {path} could not be read: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("configuration file {path} is not valid TOML: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// The whole configuration file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub auth: AuthConfig,
    pub sync: SyncConfig,
    pub reminders: ReminderConfig,
    pub export: ExportConfig,
    pub notifications: NotificationConfig,
}

/// The Google OAuth desktop client this installation authenticates with.
///
/// A desktop client's id and secret are not a security boundary — Google treats
/// installed applications as public clients — but they are still installation
/// data, so they live here and never in the repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Label the stored refresh token is filed under, so two accounts can
    /// coexist on one machine.
    pub account: String,
    /// Seconds of headroom before an access token's stated expiry at which it
    /// is treated as already expired.
    pub refresh_skew_secs: u64,
    /// How long the browser consent step may take before the loopback listener
    /// gives up and closes.
    #[serde(deserialize_with = "de_duration")]
    pub consent_timeout: Duration,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: String::new(),
            account: "default".into(),
            refresh_skew_secs: 120,
            consent_timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SyncConfig {
    /// Synchronize once when the daemon starts.
    pub on_start: bool,
    /// Synchronize again on `interval`.
    pub periodic: bool,
    #[serde(deserialize_with = "de_duration")]
    pub interval: Duration,
    /// How far back the fetched and exported window reaches.
    #[serde(deserialize_with = "de_duration")]
    pub window_past: Duration,
    /// How far forward the fetched and exported window reaches.
    #[serde(deserialize_with = "de_duration")]
    pub window_future: Duration,
    /// Ignore events this account has declined.
    pub skip_declined: bool,
    /// Calendar ids to synchronize. Empty means every calendar the account has
    /// selected in Google.
    pub calendars: Vec<String>,
    /// Upper bound on events fetched per calendar per cycle, so a runaway
    /// calendar cannot grow the daemon without limit.
    pub max_events_per_calendar: usize,
    /// Backoff applied after a failed cycle, doubling up to `retry_max`.
    #[serde(deserialize_with = "de_duration")]
    pub retry_base: Duration,
    #[serde(deserialize_with = "de_duration")]
    pub retry_max: Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            on_start: true,
            periodic: true,
            interval: Duration::from_secs(6 * 3600),
            window_past: Duration::from_secs(7 * 86_400),
            window_future: Duration::from_secs(60 * 86_400),
            skip_declined: true,
            calendars: Vec::new(),
            max_events_per_calendar: 2_000,
            retry_base: Duration::from_secs(60),
            retry_max: Duration::from_secs(30 * 60),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReminderConfig {
    pub enabled: bool,
    /// How late a reminder may be and still be delivered.
    ///
    /// Reminders that came due while the machine was suspended or the daemon
    /// was stopped are delivered only inside this window; older ones are
    /// recorded as delivered without notifying, so resuming after a weekend
    /// does not empty a backlog onto the screen.
    #[serde(deserialize_with = "de_duration")]
    pub catch_up_window: Duration,
    /// Hard cap on how many late reminders a single catch-up may deliver.
    pub max_catch_up: usize,
    /// Longest the scheduler sleeps before re-reading the wall clock.
    ///
    /// The sleep timer does not advance while the machine is suspended, so a
    /// deadline further away than this is re-evaluated rather than waited on in
    /// one go. This is what bounds post-resume lateness; it is not polling of
    /// the provider, which happens only on the sync interval.
    #[serde(deserialize_with = "de_duration")]
    pub max_sleep: Duration,
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            catch_up_window: Duration::from_secs(15 * 60),
            max_catch_up: 5,
            max_sleep: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExportConfig {
    pub enabled: bool,
    /// Root of the vdir tree. One subdirectory per calendar.
    pub directory: Option<PathBuf>,
    /// Write a `color` metadata file from the calendar's Google color.
    pub write_color: bool,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: None,
            write_color: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotificationConfig {
    pub enabled: bool,
    /// Program used to open a meeting link. Arguments are passed directly, with
    /// no shell involved.
    pub open_command: Vec<String>,
    /// Seconds a reminder notification stays on screen. Zero leaves it to the
    /// notification server.
    pub timeout_secs: u32,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            open_command: vec!["xdg-open".into()],
            timeout_secs: 0,
        }
    }
}

impl Config {
    /// Read and validate the configuration file, falling back to defaults when
    /// the file does not exist.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        config.validate()?;
        Ok(config)
    }

    /// Reject combinations that would otherwise fail later, at a point where
    /// the cause is no longer obvious.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| Err(ConfigError::Invalid(msg));

        if self.sync.periodic && self.sync.interval < Duration::from_secs(60) {
            return invalid(format!(
                "sync.interval is {}, which would hit the provider far more often than a \
                 calendar changes; use at least 1m",
                util::format_duration(self.sync.interval)
            ));
        }
        if self.sync.window_future < Duration::from_secs(86_400) {
            return invalid(
                "sync.window_future must reach at least 1d or tomorrow's events never load".into(),
            );
        }
        if self.sync.max_events_per_calendar == 0 {
            return invalid("sync.max_events_per_calendar must be greater than zero".into());
        }
        if self.sync.retry_base > self.sync.retry_max {
            return invalid("sync.retry_base cannot exceed sync.retry_max".into());
        }
        if self.reminders.enabled && self.reminders.max_sleep < Duration::from_secs(5) {
            return invalid(
                "reminders.max_sleep below 5s would wake the daemon constantly for no gain".into(),
            );
        }
        if self.reminders.enabled && self.reminders.max_sleep > Duration::from_secs(900) {
            return invalid(
                "reminders.max_sleep above 15m would let a reminder arrive that late after \
                 the machine resumes"
                    .into(),
            );
        }
        if self.notifications.enabled && self.notifications.open_command.is_empty() {
            return invalid(
                "notifications.open_command must name a program, for example [\"xdg-open\"]".into(),
            );
        }
        if !self.sync.on_start && !self.sync.periodic {
            // Legal, but only if the user intends to drive it by hand.
            tracing::warn!(
                "sync.on_start and sync.periodic are both false: the calendar will only \
                 update when `calendar-daemon sync` is run"
            );
        }
        Ok(())
    }

    /// Whether a Google client has been configured at all.
    pub fn has_oauth_client(&self) -> bool {
        !self.auth.client_id.trim().is_empty() && !self.auth.client_secret.trim().is_empty()
    }
}

/// Where the daemon reads and writes, resolved once at startup.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub export_dir: PathBuf,
}

impl Paths {
    /// Resolve every path, honouring the environment overrides and an explicit
    /// `--config` before any default.
    pub fn resolve(config_override: Option<&Path>, config: &Config) -> Self {
        let config_file = config_override
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os(ENV_CONFIG).map(PathBuf::from))
            .unwrap_or_else(|| default_config_dir().join("config.toml"));

        let state_dir = std::env::var_os(ENV_STATE_DIR)
            .map(PathBuf::from)
            .unwrap_or_else(default_state_dir);

        let runtime_dir = std::env::var_os(ENV_RUNTIME_DIR)
            .map(PathBuf::from)
            .unwrap_or_else(default_runtime_dir);

        let export_dir = config
            .export
            .directory
            .clone()
            .map(|p| expand_tilde(&p))
            .unwrap_or_else(|| state_dir.join("calendars"));

        Self {
            config_file,
            state_dir,
            runtime_dir,
            export_dir,
        }
    }

    /// The same resolution, before a configuration file has been read.
    pub fn bootstrap(config_override: Option<&Path>) -> Self {
        Self::resolve(config_override, &Config::default())
    }

    pub fn control_socket(&self) -> PathBuf {
        self.runtime_dir.join("control.sock")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("daemon.lock")
    }

    pub fn snapshot_file(&self) -> PathBuf {
        self.state_dir.join("snapshot.json")
    }

    pub fn ledger_file(&self) -> PathBuf {
        self.state_dir.join("reminders.json")
    }

    pub fn status_file(&self) -> PathBuf {
        self.state_dir.join("status.json")
    }

    /// Create the directories the daemon writes into, owner-only.
    pub fn ensure(&self) -> std::io::Result<()> {
        util::ensure_private_dir(&self.state_dir)?;
        util::ensure_private_dir(&self.runtime_dir)?;
        Ok(())
    }
}

fn default_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home().join(".config"))
        .join(APP_NAME)
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home().join(".local/state"))
        .join(APP_NAME)
}

fn default_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_NAME)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/") {
        home().join(rest)
    } else if text == "~" {
        home()
    } else {
        path.to_path_buf()
    }
}

/// Parse the duration spelling the configuration file uses: a sequence of
/// `<number><unit>` groups, units `d`, `h`, `m`, `s`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("duration is empty".into());
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut saw_group = false;

    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        let unit_secs = match ch {
            'd' => 86_400,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            other => {
                return Err(format!(
                    "unknown unit '{other}' in duration '{text}'; use d, h, m or s"
                ));
            }
        };
        if digits.is_empty() {
            return Err(format!(
                "unit '{ch}' has no number before it in duration '{text}'"
            ));
        }
        let value: u64 = digits
            .parse()
            .map_err(|_| format!("number '{digits}' in duration '{text}' is too large"))?;
        total = total
            .checked_add(value.saturating_mul(unit_secs))
            .ok_or_else(|| format!("duration '{text}' overflows"))?;
        digits.clear();
        saw_group = true;
    }

    if !digits.is_empty() {
        return Err(format!(
            "duration '{text}' ends in a number with no unit; write '{digits}s' for seconds"
        ));
    }
    if !saw_group {
        return Err(format!("duration '{text}' has no value"));
    }
    Ok(Duration::from_secs(total))
}

fn de_duration<'de, D: Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(de)?;
    parse_duration(&text).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_accept_the_documented_spellings() {
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21_600));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(
            parse_duration("1d12h").unwrap(),
            Duration::from_secs(129_600)
        );
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn an_invalid_duration_says_what_to_write_instead() {
        let err = parse_duration("6").unwrap_err();
        assert!(err.contains("6s"), "unhelpful message: {err}");

        let err = parse_duration("6y").unwrap_err();
        assert!(err.contains("use d, h, m or s"), "unhelpful message: {err}");

        assert!(parse_duration("").is_err());
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn the_default_configuration_is_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn a_too_short_interval_is_rejected_with_an_actionable_message() {
        let mut config = Config::default();
        config.sync.interval = Duration::from_secs(5);
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("at least 1m"), "unhelpful message: {err}");
    }

    #[test]
    fn a_short_interval_is_allowed_when_periodic_sync_is_off() {
        let mut config = Config::default();
        config.sync.periodic = false;
        config.sync.interval = Duration::from_secs(5);
        config.validate().unwrap();
    }

    #[test]
    fn startup_and_periodic_sync_are_independently_switchable() {
        let text = "[sync]\non_start = false\nperiodic = true\ninterval = \"2h\"\n";
        let config: Config = toml::from_str(text).unwrap();
        assert!(!config.sync.on_start);
        assert!(config.sync.periodic);
        assert_eq!(config.sync.interval, Duration::from_secs(7_200));
        config.validate().unwrap();
    }

    #[test]
    fn an_unknown_key_is_reported_rather_than_silently_ignored() {
        let err = toml::from_str::<Config>("[sync]\nintervul = \"6h\"\n").unwrap_err();
        assert!(
            err.to_string().contains("intervul"),
            "unhelpful message: {err}"
        );
    }

    #[test]
    fn an_empty_open_command_is_rejected_while_notifications_are_on() {
        let mut config = Config::default();
        config.notifications.open_command.clear();
        assert!(config.validate().is_err());

        config.notifications.enabled = false;
        config.validate().unwrap();
    }

    #[test]
    fn a_missing_configuration_file_yields_defaults() {
        let path = std::env::temp_dir().join("calendar-daemon-absent-config.toml");
        let _ = std::fs::remove_file(&path);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.sync.interval, Duration::from_secs(21_600));
    }

    #[test]
    fn a_client_is_only_configured_when_both_halves_are_present() {
        let mut config = Config::default();
        assert!(!config.has_oauth_client());
        config.auth.client_id = "id".into();
        assert!(!config.has_oauth_client());
        config.auth.client_secret = "secret".into();
        assert!(config.has_oauth_client());
    }
}
