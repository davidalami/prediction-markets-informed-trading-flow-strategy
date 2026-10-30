use crate::*;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
pub enum Condition {
    Pending {
        #[serde(flatten)]
        signal: Signal,
    },
    Done {
        deadline: i64,
        #[serde(rename = "signalAt")]
        signal_at: i64,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
}
impl Condition {
    pub fn deadline(&self) -> i64 {
        match self {
            Self::Pending { signal } => signal.deadline,
            Self::Done { deadline, .. } => *deadline,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema_version: u32,
    pub strategy_fingerprint: String,
    pub conditions: BTreeMap<String, Condition>,
}
impl State {
    pub fn new(fingerprint: &str) -> Self {
        Self {
            schema_version: 1,
            strategy_fingerprint: fingerprint.into(),
            conditions: BTreeMap::new(),
        }
    }
}

pub fn load_state(path: &Path, ledger: &Path, fingerprint: &str) -> Result<State> {
    let mut state: State = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Corrupt monitor state")?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::new(fingerprint),
        Err(err) => return Err(err.into()),
    };
    ensure!(
        state.schema_version == 1 && state.strategy_fingerprint == fingerprint,
        "State does not match strategy; use separate state and alert paths"
    );
    for (cid, item) in &state.conditions {
        ensure!(
            !cid.is_empty() && DateTime::<Utc>::from_timestamp(item.deadline(), 0).is_some(),
            "Invalid condition state"
        );
        if let Condition::Pending { signal } = item {
            ensure!(
                signal.condition_id == *cid
                    && !signal.token_id.is_empty()
                    && !signal.outcome.is_empty(),
                "Invalid pending signal"
            );
        }
    }
    match File::open(ledger) {
        Ok(file) => {
            for line in BufReader::new(file).lines() {
                let record: Value = serde_json::from_str(&line?).context("Corrupt alert ledger")?;
                ensure!(
                    record["strategyFingerprint"] == fingerprint,
                    "Alert ledger belongs to another strategy"
                );
                ensure!(
                    record["type"] == "flow-alert"
                        && !text(&record["id"]).is_empty()
                        && !text(&record["conditionId"]).is_empty(),
                    "Invalid alert record"
                );
                state.conditions.insert(
                    text(&record["conditionId"]),
                    Condition::Done {
                        deadline: epoch(&record["scheduledClose"])?,
                        signal_at: epoch(&record["signalAt"])?,
                        reason: "alerted".into(),
                        id: Some(text(&record["id"])),
                    },
                );
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(state)
}

pub fn suffix(path: &Path, ending: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(ending);
    PathBuf::from(value)
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
pub fn atomic_save(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::create_dir_all(parent(path))?;
    let temporary = suffix(path, ".tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    #[cfg(unix)]
    File::open(parent(path))?.sync_all()?;
    Ok(())
}
pub fn append_alerts(path: &Path, alerts: &[Value]) -> Result<()> {
    if alerts.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(parent(path))?;
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    for alert in alerts {
        serde_json::to_writer(&mut file, alert)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}
pub fn lock_paths(state: &Path, ledger: &Path) -> Result<Vec<File>> {
    let resolve = |path: &Path| -> Result<PathBuf> {
        fs::create_dir_all(parent(path))?;
        if path.exists() {
            return Ok(path.canonicalize()?);
        }
        Ok(parent(path)
            .canonicalize()?
            .join(path.file_name().context("Missing filename")?))
    };
    let mut paths = vec![resolve(state)?, resolve(ledger)?];
    ensure!(
        paths[0] != paths[1],
        "State and alerts must be different files"
    );
    paths.sort();
    let mut locks = Vec::new();
    for path in paths {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(suffix(&path, ".lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .with_context(|| format!("A monitor is already using {}", path.display()))?;
        locks.push(file);
    }
    Ok(locks)
}
