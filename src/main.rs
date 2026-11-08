use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use vanity_gozcu::checker::{Checker, DEFAULT_BASE_URL};
use vanity_gozcu::config::{Config, Loaded, normalize_code};
use vanity_gozcu::model::CheckResult;
use vanity_gozcu::ratelimit::RateLimiter;
use vanity_gozcu::scheduler::{self, SchedulerSettings};

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
    /// Start monitoring headless (for systemd)
    Daemon,
    /// Check a single vanity code once and print the classified result
    Check { code: String },
    /// Send a test notification
    TestNotify,
}

fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
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
        Cmd::Run | Cmd::Daemon => {
            monitor().await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Check { code } => check_once(&code).await,
        Cmd::TestNotify => {
            eprintln!("test-notify: not implemented until stage 2");
            Ok(ExitCode::FAILURE)
        }
    }
}

async fn check_once(raw_code: &str) -> Result<ExitCode> {
    let code = normalize_code(raw_code)?;
    let loaded = load_config()?;
    init_logging(&loaded.config.general.log_level);
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

async fn monitor() -> Result<()> {
    let loaded = load_config()?;
    let config = loaded.config;
    init_logging(&config.general.log_level);
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

    let checker = Arc::new(Checker::new(DEFAULT_BASE_URL, config.request_timeout())?);
    let limiter = Arc::new(RateLimiter::new(config.ratelimit.max_requests_per_minute));
    let settings = SchedulerSettings {
        default_interval: config.default_interval(),
        jitter_percent: config.general.jitter_percent,
        backoff_max: std::time::Duration::from_secs(config.ratelimit.backoff_max_secs),
    };
    let mut scheduler = scheduler::spawn(settings, checker, limiter, config.targets);

    loop {
        tokio::select! {
            event = scheduler.events.recv() => {
                let Some(event) = event else { bail!("scheduler stopped unexpectedly") };
                match &event.result {
                    CheckResult::Free => info!(code = %event.code, "FREE"),
                    CheckResult::Taken { guild_name, .. } => {
                        info!(code = %event.code, guild = guild_name.as_deref().unwrap_or("?"), "taken");
                    }
                    other => warn!(code = %event.code, "{other}"),
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to listen for ctrl-c")?;
                info!("shutting down");
                return Ok(());
            }
        }
    }
}
