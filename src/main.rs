//! Command-line entry point.
//!
//! `run` is the long-lived process a user service starts. Every other command
//! talks to it over the private control socket, and falls back to acting
//! directly — under the same exclusive lock — when it is not running.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use chrono::Utc;
use clap::{Parser, Subcommand};

use calendar_daemon::auth::secrets::{KeyringStore, SecretStore};
use calendar_daemon::auth::{AuthFailure, AuthHealth, GoogleTokenEndpoint, TokenManager, oauth};
use calendar_daemon::config::{Config, Paths};
use calendar_daemon::control::{self, ControlError, Request, Response};
use calendar_daemon::daemon::{self, Daemon};
use calendar_daemon::google::client::{
    GoogleCalendarClient, PAGE_SIZE, REQUEST_TIMEOUT, build_http_client,
};
use calendar_daemon::notify::DesktopNotifier;
use calendar_daemon::store::{DaemonLock, Snapshot, Store};
use calendar_daemon::util;

#[derive(Parser)]
#[command(
    name = "calendar-daemon",
    version,
    about = "Local calendar synchronization, reminders and vdir export",
    long_about = None
)]
struct Cli {
    /// Configuration file. Defaults to $XDG_CONFIG_HOME/calendar-daemon/config.toml
    #[arg(long, short, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Log more. Repeat for debug output.
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run in the foreground. This is what a user service starts.
    Run,
    /// Synchronize now, through the running daemon when there is one.
    Sync,
    /// Report last success, next sync, next reminder and authorization health.
    Status {
        /// Print the underlying record as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Authorize this account with Google in a browser.
    Auth {
        /// Remove the stored authorization instead of creating one.
        #[arg(long)]
        forget: bool,
    },
    /// Ask a running daemon to stop.
    Stop,
    /// Print the paths this installation uses, for setting up a vdir account.
    Paths,
    /// Check the configuration file and exit.
    Check,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;
    runtime.block_on(dispatch(cli))
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "calendar_daemon=info,warn",
        1 => "calendar_daemon=debug,info",
        _ => "calendar_daemon=trace,debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let bootstrap = Paths::bootstrap(cli.config.as_deref());
    let config = Config::load(&bootstrap.config_file)
        .with_context(|| format!("reading {}", bootstrap.config_file.display()))?;
    let paths = Paths::resolve(cli.config.as_deref(), &config);

    match cli.command {
        Command::Check => cmd_check(&config, &paths),
        Command::Paths => cmd_paths(&paths),
        Command::Run => cmd_run(config, paths).await,
        Command::Sync => cmd_sync(config, paths).await,
        Command::Stop => cmd_stop(paths).await,
        Command::Status { json } => cmd_status(config, paths, json).await,
        Command::Auth { forget } => cmd_auth(config, paths, forget).await,
    }
}

fn cmd_check(config: &Config, paths: &Paths) -> anyhow::Result<()> {
    config.validate()?;
    println!("Configuration at {} is valid.", paths.config_file.display());
    if !config.has_oauth_client() {
        println!(
            "No OAuth client is configured yet: set auth.client_id and auth.client_secret, \
             then run `calendar-daemon auth`."
        );
    }
    Ok(())
}

fn cmd_paths(paths: &Paths) -> anyhow::Result<()> {
    println!("config    {}", paths.config_file.display());
    println!("state     {}", paths.state_dir.display());
    println!("export    {}", paths.export_dir.display());
    println!("socket    {}", paths.control_socket().display());
    println!();
    println!(
        "Point a local (vdir) calendar account at the export directory above.\n\
         Create it first, so the reader has something to watch:\n  \
         mkdir -p {}",
        paths.export_dir.display()
    );
    Ok(())
}

async fn cmd_run(config: Config, paths: Paths) -> anyhow::Result<()> {
    paths.ensure()?;
    let lock = DaemonLock::acquire(&paths.lock_file())
        .context("another calendar-daemon is already running")?;
    tracing::debug!(lock = %lock.path().display(), "state directory claimed");

    if !config.has_oauth_client() {
        return Err(anyhow!(
            "no OAuth client is configured. Set auth.client_id and auth.client_secret in {}, \
             then run `calendar-daemon auth`.",
            paths.config_file.display()
        ));
    }

    util::ensure_private_dir(&paths.export_dir).context("creating the export directory")?;

    let (tokens, _store) = build_tokens(&config)?;
    let http = build_http_client().context("building the HTTP client")?;
    let provider = Arc::new(GoogleCalendarClient::new(http, tokens.clone()));
    let notifier = Arc::new(DesktopNotifier::new(config.notifications.clone()));

    let health = tokens.health(config.has_oauth_client()).await;
    if let AuthHealth::NotAuthorized = health {
        tracing::warn!("no stored authorization; run `calendar-daemon auth` to grant access");
    }

    let listener = control::bind(&paths.control_socket()).await?;
    tracing::info!(
        socket = %paths.control_socket().display(),
        export = %paths.export_dir.display(),
        "calendar-daemon started"
    );

    let daemon = Daemon {
        store: Store::from_paths(&paths),
        config,
        paths: paths.clone(),
        provider,
        notifier,
        tokens,
        auth_health: Arc::new(tokio::sync::Mutex::new(health)),
    };

    let result = daemon.run(listener).await;
    let _ = std::fs::remove_file(paths.control_socket());
    tracing::info!("calendar-daemon stopped");
    result
}

async fn cmd_sync(config: Config, paths: Paths) -> anyhow::Result<()> {
    match control::request(&paths.control_socket(), &Request::Sync).await {
        Ok(Response::Synced {
            calendars,
            events,
            partial,
            ..
        }) => {
            println!(
                "Synced {calendars} calendar(s), {events} event(s){}.",
                if partial {
                    " (partial: some calendars kept their previous events)"
                } else {
                    ""
                }
            );
            return Ok(());
        }
        Ok(Response::SyncFailed { category, message }) => {
            return Err(anyhow!("sync failed ({category}): {message}"));
        }
        Ok(other) => return Err(anyhow!("unexpected answer from the daemon: {other:?}")),
        Err(ControlError::NotRunning { .. }) => {
            tracing::info!("no daemon is running; synchronizing once in this process");
        }
        Err(err @ ControlError::PathTooLong { .. }) => {
            // A running daemon could not have bound this path either, so there
            // is nothing to talk to; say why and do the work here.
            tracing::warn!("{err}");
        }
        Err(err) => return Err(err.into()),
    }

    // One-shot: the same exclusive lock the daemon would hold, so this can
    // never run beside it. No reminder scheduler is started, so a one-shot sync
    // updates state and the export without notifying.
    paths.ensure()?;
    let _lock = DaemonLock::acquire(&paths.lock_file())
        .context("the state directory is claimed by another process")?;

    if !config.has_oauth_client() {
        return Err(anyhow!(
            "no OAuth client is configured; see `calendar-daemon check`"
        ));
    }
    util::ensure_private_dir(&paths.export_dir)?;

    let (tokens, _store) = build_tokens(&config)?;
    let http = build_http_client()?;
    let provider = GoogleCalendarClient::new(http, tokens);
    let store = Store::from_paths(&paths);

    let outcome = daemon::run_sync_cycle(&provider, &store, &config, &paths.export_dir, Utc::now())
        .await
        .map_err(|err| anyhow!("{}: {err}", err.category()))?;

    println!(
        "Synced {} calendar(s), {} event(s){}.",
        outcome.calendars,
        outcome.events,
        if outcome.partial { " (partial)" } else { "" }
    );
    println!("Reminders are only delivered while `calendar-daemon run` is active.");
    Ok(())
}

async fn cmd_stop(paths: Paths) -> anyhow::Result<()> {
    match control::request(&paths.control_socket(), &Request::Shutdown).await {
        Ok(Response::Ack) => {
            println!("Asked the daemon to stop.");
            Ok(())
        }
        Ok(other) => Err(anyhow!("unexpected answer from the daemon: {other:?}")),
        Err(ControlError::NotRunning { .. }) => {
            println!("No daemon is running.");
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

async fn cmd_status(config: Config, paths: Paths, json: bool) -> anyhow::Result<()> {
    let store = Store::from_paths(&paths);

    let (record, running) = match control::request(&paths.control_socket(), &Request::Status).await
    {
        Ok(Response::Status(record)) => (*record, true),
        Ok(other) => return Err(anyhow!("unexpected answer from the daemon: {other:?}")),
        Err(ControlError::NotRunning { .. }) => (store.load_status(), false),
        Err(err @ ControlError::PathTooLong { .. }) => {
            tracing::warn!("{err}");
            (store.load_status(), false)
        }
        Err(err) => return Err(err.into()),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&record)?);
        return Ok(());
    }

    let now = Utc::now();
    let snapshot = store.load_snapshot().ok().flatten();

    println!(
        "daemon        {}",
        if running { "running" } else { "not running" }
    );

    match &record.last_success {
        Some(at) => {
            let age = now - *at;
            println!(
                "last sync     {} ({} ago)",
                at.format("%Y-%m-%d %H:%M:%SZ"),
                util::format_duration(age.to_std().unwrap_or_default())
            );
        }
        None => println!("last sync     never"),
    }

    if let Some(snapshot) = &snapshot {
        let age = snapshot.age(now);
        println!(
            "data age      {}{}",
            util::format_duration(age.to_std().unwrap_or_default()),
            if age > chrono::Duration::hours(24) {
                "  (stale)"
            } else {
                ""
            }
        );
    }

    let awaiting_reauthorization = matches!(
        record.auth,
        Some(AuthHealth::ReauthorizationRequired { .. })
    );
    match record.next_sync {
        Some(at) if running => println!("next sync     {}", at.format("%Y-%m-%d %H:%M:%SZ")),
        Some(_) | None if !running => println!("next sync     — (daemon not running)"),
        None if awaiting_reauthorization => {
            println!("next sync     — (sync paused until reauthorization)")
        }
        _ => println!("next sync     — (periodic sync is off)"),
    }

    match (&record.next_reminder, &record.next_reminder_summary) {
        (Some(at), summary) => {
            let zone = snapshot
                .as_ref()
                .map(daemon::display_zone)
                .unwrap_or(chrono_tz::UTC);
            println!(
                "next reminder {}  {}",
                at.with_timezone(&zone).format("%Y-%m-%d %H:%M %Z"),
                summary.as_deref().unwrap_or("")
            );
        }
        _ => println!("next reminder —"),
    }

    println!(
        "calendars     {}   events {}   pending reminders {}",
        record.calendars, record.events, record.scheduled_reminders
    );

    // Reaching the keyring can fail on its own — a locked collection, no
    // session bus — and that is information `status` exists to report, not a
    // reason for it to stop.
    let auth = match record.auth {
        Some(auth) => auth,
        None if !config.has_oauth_client() => AuthHealth::Unconfigured,
        None => match build_tokens(&config) {
            Ok((tokens, _store)) => tokens.health(true).await,
            Err(err) => AuthHealth::KeyringUnavailable {
                detail: err.to_string(),
            },
        },
    };
    println!("auth          {}", auth.summary());

    if let (Some(category), Some(summary)) =
        (&record.last_error_category, &record.last_error_summary)
    {
        println!("last error    {category}: {summary}");
    }
    if record.consecutive_failures > 0 {
        println!("failures      {} in a row", record.consecutive_failures);
    }

    println!("export        {}", paths.export_dir.display());
    Ok(())
}

async fn cmd_auth(config: Config, paths: Paths, forget: bool) -> anyhow::Result<()> {
    let (tokens, _store) = build_tokens(&config)?;

    if forget {
        tokens.forget().await.map_err(|err| anyhow!("{err}"))?;
        println!(
            "Removed the stored authorization for account '{}'.",
            config.auth.account
        );
        println!("Google still lists this application until you revoke it at");
        println!("  https://myaccount.google.com/permissions");
        return Ok(());
    }

    if !config.has_oauth_client() {
        return Err(anyhow!(
            "no OAuth client is configured. Create a Desktop OAuth client in Google Cloud, \
             enable the Calendar API, and put its id and secret in {} under [auth].",
            paths.config_file.display()
        ));
    }

    let (listener, port) = oauth::bind_loopback()
        .await
        .context("binding a loopback port")?;
    let pending = oauth::begin_authorization(&config.auth.client_id, port);

    println!("Opening your browser to authorize calendar-daemon.");
    println!("Requested access (read-only):");
    for scope in oauth::requested_scopes() {
        println!("  {scope}");
    }
    println!();
    println!("If the browser does not open, visit this URL:");
    println!("{}", pending.authorization_url);
    println!();

    if let Err(err) = calendar_daemon::notify::open_link(
        &config.notifications.open_command,
        &pending.authorization_url,
    ) {
        tracing::warn!(error = %err, "could not open a browser automatically");
    }

    let callback = oauth::await_callback(listener, &pending, config.auth.consent_timeout)
        .await
        .map_err(|err: AuthFailure| anyhow!("{err}\n{}", err.remedy()))?;

    let http = build_http_client()?;
    let response = oauth::exchange_code(
        &http,
        &config.auth.client_id,
        &config.auth.client_secret,
        &callback.code,
        &pending,
    )
    .await
    .map_err(|err| anyhow!("{err}\n{}", err.remedy()))?;

    let missing = response.missing_scopes();
    if !missing.is_empty() {
        return Err(anyhow!(
            "the authorization granted less access than requested; missing:\n  {}\n\
             Run `calendar-daemon auth` again and leave every permission ticked.",
            missing.join("\n  ")
        ));
    }

    tokens
        .adopt_initial_grant(&response, Utc::now())
        .await
        .map_err(|err| anyhow!("{err}\n{}", err.remedy()))?;

    println!("Authorized. The refresh token is stored in the session keyring.");

    let deadline = daemon_sync_deadline(&config, Store::from_paths(&paths).load_status().calendars);
    let answer = tokio::time::timeout(
        deadline,
        control::request(&paths.control_socket(), &Request::Sync),
    )
    .await;
    match answer {
        Err(_) => {
            println!(
                "The authorization is stored, but the running daemon did not answer within {}.",
                util::format_duration(deadline)
            );
            print_restart_hint();
        }
        Ok(Ok(Response::Synced {
            calendars,
            events,
            partial,
            ..
        })) => {
            println!(
                "The running daemon picked up the new authorization: synced {calendars} \
                 calendar(s), {events} event(s){}.",
                partial_note(partial)
            );
        }
        Ok(Ok(Response::SyncFailed { category, message })) => {
            println!("The running daemon could not sync with it ({category}): {message}");
            print_restart_hint();
        }
        Ok(Ok(Response::Error { message })) => {
            println!("The running daemon refused the sync request: {message}");
            print_restart_hint();
        }
        Ok(Ok(other)) => {
            println!("The running daemon sent an unexpected answer: {other:?}");
            print_restart_hint();
        }
        Ok(Err(ControlError::NotRunning { .. })) => {
            println!("Run `calendar-daemon sync` to fetch your calendars.");
        }
        Ok(Err(err @ ControlError::PathTooLong { .. })) => {
            tracing::warn!("{err}");
            println!("Run `calendar-daemon sync` to fetch your calendars.");
        }
        Ok(Err(err)) => {
            println!("The running daemon could not be asked to sync: {err}");
            print_restart_hint();
        }
    }
    Ok(())
}

fn daemon_sync_deadline(config: &Config, last_known_calendars: usize) -> std::time::Duration {
    let calendars = if config.sync.calendars.is_empty() {
        last_known_calendars
    } else {
        config.sync.calendars.len()
    };
    let pages_per_calendar = config.sync.max_events_per_calendar.div_ceil(PAGE_SIZE);
    let calendar_list_requests: usize = 1;
    let token_refreshes: usize = 1;
    let one_retry_after_a_401: usize = 2;
    let requests = (calendar_list_requests + token_refreshes + one_retry_after_a_401)
        .saturating_add(calendars.saturating_mul(pages_per_calendar));
    REQUEST_TIMEOUT.saturating_mul(u32::try_from(requests).unwrap_or(u32::MAX))
}

fn partial_note(partial: bool) -> &'static str {
    if partial {
        " (partial: some calendars kept their previous events)"
    } else {
        ""
    }
}

fn print_restart_hint() {
    println!("Restart the service so it picks up the new authorization:");
    println!("  systemctl --user restart calendar-daemon.service");
}

/// Build the token manager over the desktop keyring.
fn build_tokens(
    config: &Config,
) -> anyhow::Result<(Arc<TokenManager<KeyringStore, GoogleTokenEndpoint>>, String)> {
    // Connecting to the secret store is deferred: a locked keyring must not
    // stop the daemon from starting and reporting that it is locked.
    let store = KeyringStore::new();
    let description = store.describe();
    let http = build_http_client().context("building the HTTP client")?;
    let endpoint = GoogleTokenEndpoint::new(
        http,
        config.auth.client_id.clone(),
        config.auth.client_secret.clone(),
    );
    let manager = TokenManager::new(
        config.auth.account.clone(),
        store,
        endpoint,
        std::time::Duration::from_secs(config.auth.refresh_skew_secs),
    );
    Ok((Arc::new(manager), description))
}

/// Referenced so the unused-import lint does not hide a real omission.
#[allow(dead_code)]
fn _assert_snapshot_type(_: &Snapshot) {}
