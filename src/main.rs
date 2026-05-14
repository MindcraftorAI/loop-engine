//! `loop-engine` binary entry.

use std::fs;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use loop_engine::cli::{Cli, Command};
use loop_engine::config;
use loop_engine::lifecycle::{self, pre_detach_checks, read_pid_file, run_body};
use loop_engine::observability;
use loop_engine::paths;
use loop_engine::pid::pid_is_alive;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(err) => {
            // Tracing may not be initialized if early-init failed; print to stderr.
            eprintln!("loop-engine: error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Run { foreground } => {
            if foreground {
                run_foreground()
            } else {
                run_detached()
            }
        }
        Command::Status => status(),
        Command::Stop => stop(),
        Command::Serve => serve(),
    }
}

fn serve() -> Result<ExitCode> {
    // Init logging to stderr so JSON-RPC stdout stays clean.
    observability::init_foreground()?;
    paths::ensure_loop_dirs()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(loop_engine::serve::run())?;
    Ok(ExitCode::SUCCESS)
}

fn run_foreground() -> Result<ExitCode> {
    observability::init_foreground()?;
    paths::ensure_loop_dirs()?;
    pre_detach_checks()?;
    let cfg = config::load()?;
    info!("starting daemon in foreground mode");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async { run_body(&cfg).await })?;
    Ok(ExitCode::SUCCESS)
}

fn run_detached() -> Result<ExitCode> {
    // All filesystem prep must happen BEFORE fork — the child loses
    // useful error visibility.
    paths::ensure_loop_dirs()?;
    pre_detach_checks()?;
    let cfg = config::load()?;
    let log_path = paths::daemon_log_path()?;

    // PID file ownership is entirely in run_body's write_pid_file/
    // remove_pid_file pair. daemonize is NOT asked to manage one,
    // so there's a single source of truth (audit Day 10 finding #3).
    let daemonize = daemonize::Daemonize::new()
        .working_directory(paths::loop_home()?)
        .stdout(fs::File::create(&log_path).context("creating daemon stdout log")?)
        .stderr(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .context("opening daemon stderr log")?,
        );

    match daemonize.start() {
        Ok(_) => {
            // Now in the child. Initialize logging to the file and run.
            observability::init_detached(&log_path)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            if let Err(err) = runtime.block_on(async { run_body(&cfg).await }) {
                error!(?err, "daemon body returned error");
                return Ok(ExitCode::from(1));
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => Err(anyhow::anyhow!("daemonize failed: {err}")),
    }
}

fn status() -> Result<ExitCode> {
    observability::init_foreground()?;
    let pid_path = paths::daemon_pid_path()?;
    match read_pid_file(&pid_path)? {
        None => {
            println!(
                "loop-engine: not running (no PID file at {})",
                pid_path.display()
            );
            Ok(ExitCode::from(1))
        }
        Some(pid) => {
            if pid_is_alive(pid) {
                println!("loop-engine: running (pid={pid})");
                Ok(ExitCode::SUCCESS)
            } else {
                println!(
                    "loop-engine: not running (stale PID file at {}, pid={pid})",
                    pid_path.display()
                );
                Ok(ExitCode::from(1))
            }
        }
    }
}

fn stop() -> Result<ExitCode> {
    observability::init_foreground()?;
    let pid_path = paths::daemon_pid_path()?;
    let pid = match read_pid_file(&pid_path)? {
        None => {
            println!("loop-engine: not running (no PID file)");
            return Ok(ExitCode::SUCCESS);
        }
        Some(pid) => pid,
    };
    if !pid_is_alive(pid) {
        println!("loop-engine: PID file present (pid={pid}) but process not alive; clearing");
        lifecycle::remove_pid_file()?;
        return Ok(ExitCode::SUCCESS);
    }
    // SAFETY: SIGTERM is a non-destructive request; daemon handles it.
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("kill(pid={pid}, SIGTERM) failed: {err}");
    }
    println!("loop-engine: sent SIGTERM to pid={pid}");
    Ok(ExitCode::SUCCESS)
}
