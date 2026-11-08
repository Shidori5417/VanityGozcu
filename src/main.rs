use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing::{info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use vanity_gozcu::checker::{Checker, DEFAULT_BASE_URL};
use vanity_gozcu::config::{Config, Loaded, normalize_code};
use vanity_gozcu::model::CheckResult;
use vanity_gozcu::notify::{Message, Notifiers};
use vanity_gozcu::pipeline;
use vanity_gozcu::ratelimit::RateLimiter;
use vanity_gozcu::scheduler::{self, SchedulerSettings};
use vanity_gozcu::store::Store;

/// Max time to let queued notifications drain on shutdown.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(name = "vanity-gozcu", version, about = "Discord vanity URL watcher")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start monitoring (the TUI arrives in a later stage; logs results for now)
    Run,
    /// Start monitoring headless (for systemd); also logs to a daily rolling file
    Daemon,
    /// Check a single vanity code once and print the classified result
    Check { code: String },
    /// Send a test notification through every enabled notifier
    TestNotify,
}

/// Logs to stderr and, when `log_dir` is given, to a daily rolling file. The returned
/// guard must live until exit so buffered lines are flushed.
fn init_logging(level: &str, log_dir: Option<&Path>) -> Option<WorkerGuard> {
    let filter = || {
        EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(level))
            .unwrap_or_else(|_| EnvFilter::new("info"))
    };
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter());
    let (file, guard) = match log_dir {
        Some(dir) => {
            let appender = tracing_appender::rolling::daily(dir, "vanity-gozcu.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(filter());
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(stderr)
        .with(file)
        .init();
    guard
}

fn load_config() -> Result<Loaded> {
    Config::load().context("failed to load configuration")
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Run => {
            monitor(false).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Daemon => {
            monitor(true).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Check { code } => check_once(&code).await,
        Cmd::TestNotify => test_notify().await,
    }
}

async fn check_once(raw_code: &str) -> Result<ExitCode> {
    let code = normalize_code(raw_code)?;
    let loaded = load_config()?;
    let _guard = init_logging(&loaded.config.general.log_level, None);
    for w in &loaded.warnings {
        warn!("{w}");
    }
    let checker = Checker::new(DEFAULT_BASE_URL, loaded.config.request_timeout())?;
    let checked = checker.check(&code).await;
    println!("{code}: {}", checked.result);
    let conclusive = matches!(
        checked.result,
        CheckResult::Taken { .. } | CheckResult::Free
    );
    Ok(if conclusive {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn test_notify() -> Result<ExitCode> {
    let loaded = load_config()?;
    let _guard = init_logging(&loaded.config.general.log_level, None);
    let (notifiers, problems) = Notifiers::from_config(&loaded.config);
    let mut failed = false;
    for p in &problems {
        eprintln!("FAIL {p}");
        failed = true;
    }
    if notifiers.is_empty() && problems.is_empty() {
        eprintln!(
            "no notifier is enabled; set `enabled = true` under [notify.discord] and/or \
             [notify.telegram] in the config (secrets come from VG_DISCORD_WEBHOOK / \
             VG_TELEGRAM_TOKEN)"
        );
        return Ok(ExitCode::FAILURE);
    }
    let message = Message::test(SystemTime::now());
    for (name, result) in notifiers.send_all(&message).await {
        match result {
            Ok(()) => println!("OK   {name}"),
            Err(e) => {
                eprintln!("FAIL {e}");
                failed = true;
            }
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Resolves on SIGINT or SIGTERM.
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
    let mut int = signal(SignalKind::interrupt()).context("failed to listen for SIGINT")?;
    tokio::select! {
        _ = term.recv() => info!("received SIGTERM"),
        _ = int.recv() => info!("received SIGINT"),
    }
    Ok(())
}

async fn monitor(daemon: bool) -> Result<()> {
    let loaded = load_config()?;
    let config = loaded.config;
    if daemon {
        std::fs::create_dir_all(&config.general.log_dir).with_context(|| {
            format!("cannot create log dir {}", config.general.log_dir.display())
        })?;
    }
    let _guard = init_logging(
        &config.general.log_level,
        daemon.then_some(config.general.log_dir.as_path()),
    );
    match &loaded.source {
        Some(path) => info!("loaded config from {}", path.display()),
        None => info!("no config file found, using defaults"),
    }
    for w in &loaded.warnings {
        warn!("{w}");
    }
    if config.targets.is_empty() {
        bail!("no [[targets]] configured; add at least one to the config file");
    }
    for (code, interval) in config.effective_intervals() {
        info!(%code, interval_secs = interval.as_secs_f64(), "watching");
    }

    let store = Store::open(&config.general.database_path).with_context(|| {
        format!(
            "cannot open database {}",
            config.general.database_path.display()
        )
    })?;
    let mut engine = pipeline::restore_engine(&store, config.general.confirm_checks).await?;

    let (notifiers, problems) = Notifiers::from_config(&config);
    for p in &problems {
        warn!("notifier disabled: {p}");
    }
    if notifiers.is_empty() {
        warn!("no notifier active; events are only logged and stored");
    }
    let bus = pipeline::event_bus();
    let notifier_task = pipeline::spawn_notifier(notifiers, bus.subscribe());

    let checker = Arc::new(Checker::new(DEFAULT_BASE_URL, config.request_timeout())?);
    let limiter = Arc::new(RateLimiter::new(config.ratelimit.max_requests_per_minute));
    let settings = SchedulerSettings {
        default_interval: config.default_interval(),
        jitter_percent: config.general.jitter_percent,
        backoff_max: Duration::from_secs(config.ratelimit.backoff_max_secs),
    };
    let mut scheduler = scheduler::spawn(settings, checker, limiter, config.targets);

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let outcome = loop {
        tokio::select! {
            event = scheduler.events.recv() => {
                let Some(event) = event else { break Err(anyhow::anyhow!("scheduler stopped unexpectedly")) };
                pipeline::process(&mut engine, &store, &bus, &event).await;
            }
            signal = &mut shutdown => break signal,
        }
    };

    info!("shutting down");
    scheduler.task.abort();
    drop(scheduler);
    drop(bus);
    if tokio::time::timeout(SHUTDOWN_DRAIN, notifier_task)
        .await
        .is_err()
    {
        warn!("pending notifications not delivered within {SHUTDOWN_DRAIN:?}");
    }
    if let Err(e) = store.close().await {
        warn!("failed to close database cleanly: {e}");
    }
    outcome
}
