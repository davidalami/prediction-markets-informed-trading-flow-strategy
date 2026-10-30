use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use informed_trader_flow::{api::HttpApi, monitor::Monitor, storage, *};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn strategy_default() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/strategy.json")
}
fn project() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
#[derive(Parser)]
#[command(
    version,
    about = "Native read-only late-flow strategy monitor and historical replay"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Poll for live eligible trades; submits no orders.
    Monitor {
        #[arg(long,default_value_os_t=strategy_default())]
        strategy: PathBuf,
        #[arg(long,default_value_os_t=project().join("data/flow-monitor-state.json"))]
        state: PathBuf,
        #[arg(long,default_value_os_t=project().join("data/flow-alerts.jsonl"))]
        alerts: PathBuf,
        #[arg(long, default_value_t = 5.)]
        interval: f64,
        #[arg(long, default_value_t = 200)]
        max_pages: usize,
        #[arg(long, default_value_t = 30)]
        max_age: i64,
        #[arg(long)]
        count: Option<u64>,
        #[arg(long)]
        notify: bool,
    },
    /// Replay a captured raw.json using the same strategy engine.
    Replay {
        #[arg(long)]
        input: PathBuf,
        #[arg(long,default_value_os_t=strategy_default())]
        strategy: PathBuf,
        #[arg(long, requires = "end")]
        start: Option<String>,
        #[arg(long, requires = "start")]
        end: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}
fn emit(value: &impl serde::Serialize) -> Result<()> {
    let out = io::stdout();
    let mut handle = out.lock();
    serde_json::to_writer(&mut handle, value)?;
    handle.write_all(b"\n")?;
    handle.flush()?;
    Ok(())
}
fn notify(alert: &Value) -> Result<()> {
    let title = format!(
        "Flow: {} ${} ≤{}",
        text(&alert["outcome"]),
        alert["budgetUsd"],
        alert["limitPrice"]
    );
    let message = format!(
        "{} — closes {}",
        text(&alert["title"]),
        text(&alert["scheduledClose"])
    );
    let mut child = Command::new("osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            &title,
            &message,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "macOS notification failed");
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(10) {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("Notification timed out");
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn run() -> Result<i32> {
    match Cli::parse().command {
        Commands::Replay {
            input,
            strategy,
            start,
            end,
            output,
        } => {
            let (rules, _) = load_strategy(&strategy)?;
            let raw: Value = serde_json::from_slice(
                &fs::read(&input).with_context(|| format!("Read {}", input.display()))?,
            )?;
            let sibling = input
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("results.json");
            let (start, end) = match (start, end) {
                (Some(start), Some(end)) => (epoch(&json!(start))?, epoch(&json!(end))?),
                (None, None) => {
                    let saved: Value = serde_json::from_slice(&fs::read(sibling).context(
                        "Use --start/--end or supply a captured run with results.json",
                    )?)?;
                    (
                        epoch(&saved["window"]["start"])?,
                        epoch(&saved["window"]["end"])?,
                    )
                }
                _ => unreachable!(),
            };
            let result = replay(&raw, start, end, &rules)?;
            if let Some(path) = output {
                storage::atomic_save(&path, &result)?;
            }
            emit(
                &json!({"type":"flow-replay","status":result["status"],"window":result["window"],
                "summary":result["summary"],"skips":result["skips"]}),
            )?;
            Ok(if result["status"] == "ok" { 0 } else { 2 })
        }
        Commands::Monitor {
            strategy,
            state,
            alerts,
            interval,
            max_pages,
            max_age,
            count,
            notify: desktop,
        } => {
            ensure!(
                interval.is_finite() && (1. ..=60.).contains(&interval),
                "Interval must be 1–60 seconds"
            );
            ensure!(
                (1..=10000).contains(&max_pages) && (5..=60).contains(&max_age) && count != Some(0),
                "Invalid page/count/age bounds"
            );
            ensure!(
                !desktop || cfg!(target_os = "macos"),
                "--notify requires macOS"
            );
            let _locks = storage::lock_paths(&state, &alerts)?;
            let (rules, fingerprint) = load_strategy(&strategy)?;
            let current = storage::load_state(&state, &alerts, &fingerprint)?;
            let stopped = Arc::new(AtomicBool::new(false));
            let flag = stopped.clone();
            ctrlc::set_handler(move || {
                flag.store(true, Ordering::Relaxed);
            })?;
            let mut api = HttpApi::new(stopped.clone())?;
            let mut engine = Monitor::new(
                rules.clone(),
                fingerprint.clone(),
                current,
                max_pages,
                max_age,
            );
            emit(
                &json!({"schemaVersion":1,"type":"flow-monitor-start","implementation":"rust",
                "strategy":strategy,"strategyFingerprint":fingerprint,"rules":rules,
                "intervalSeconds":interval,"alerts":alerts,"state":state,
                "transport":"public-rest-polling","ordersEnabled":false}),
            )?;
            let (mut polls, mut code) = (0, 0);
            while !stopped.load(Ordering::Relaxed) && count.is_none_or(|limit| polls < limit) {
                let started = Instant::now();
                let result = engine.poll(&mut api);
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                storage::append_alerts(&alerts, &result.signals)?;
                storage::atomic_save(&state, &engine.state)?;
                emit(&result)?;
                for alert in &result.signals {
                    eprintln!(
                        "\x07ELIGIBLE {} ${} at ≤{} | {} | {}",
                        text(&alert["outcome"]),
                        alert["budgetUsd"],
                        alert["limitPrice"],
                        text(&alert["title"]),
                        text(&alert["url"])
                    );
                    if desktop {
                        if let Err(err) = notify(alert) {
                            eprintln!(
                                "{}",
                                json!({"type":"notification-error","id":alert["id"],"error":err.to_string()})
                            );
                        }
                    }
                }
                if result.status == "error" {
                    code = 1;
                } else if result.status == "partial" && code != 1 {
                    code = 2;
                }
                polls += 1;
                if count.is_some_and(|limit| polls >= limit) {
                    break;
                }
                while started.elapsed() < Duration::from_secs_f64(interval)
                    && !stopped.load(Ordering::Relaxed)
                {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            Ok(if stopped.load(Ordering::Relaxed) {
                130
            } else {
                code
            })
        }
    }
}
fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            if err
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                return;
            }
            eprintln!(
                "{}",
                json!({"type":"flow-monitor-error","status":"error","error":format!("{err:#}")})
            );
            std::process::exit(1);
        }
    }
}
