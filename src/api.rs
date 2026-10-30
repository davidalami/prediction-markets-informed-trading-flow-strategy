use crate::*;
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub trait Api {
    fn now(&self) -> i64;
    fn get(&mut self, base: &str, path: &str, params: &[(&str, String)]) -> Result<Value>;
}

pub struct HttpApi {
    client: Client,
    pub stopped: Arc<AtomicBool>,
}

impl HttpApi {
    pub fn new(stopped: Arc<AtomicBool>) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .https_only(true)
                .user_agent("MarketSentinel/research")
                .timeout(Duration::from_secs(15))
                .connect_timeout(Duration::from_secs(5))
                .build()?,
            stopped,
        })
    }
    fn pause(&self, seconds: u64) -> Result<()> {
        for _ in 0..seconds * 10 {
            ensure!(!self.stopped.load(Ordering::Relaxed), "Monitor stopped");
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

impl Api for HttpApi {
    fn now(&self) -> i64 {
        Utc::now().timestamp()
    }
    fn get(&mut self, base: &str, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let mut error = None;
        for attempt in 0..3 {
            ensure!(!self.stopped.load(Ordering::Relaxed), "Monitor stopped");
            let response = self
                .client
                .get(format!("{base}{path}"))
                .query(params)
                .header("Accept", "application/json")
                .header("Cache-Control", "no-cache")
                .send();
            match response {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp
                            .json()
                            .with_context(|| format!("Invalid JSON from {path}"));
                    }
                    let wait = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(attempt + 1)
                        .clamp(1, 5);
                    let body = resp.text().unwrap_or_default();
                    let detail: String = body.chars().take(180).collect();
                    error = Some(anyhow::anyhow!("HTTP {status} from {path}: {detail}"));
                    if !(status.is_server_error() || status.as_u16() == 429) {
                        break;
                    }
                    if attempt < 2 {
                        self.pause(wait)?;
                    }
                }
                Err(err) => {
                    error = Some(err.into());
                    if attempt < 2 {
                        self.pause(attempt + 1)?;
                    }
                }
            }
        }
        Err(error.context("Request failed")?)
    }
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub complete: bool,
    pub pages: usize,
    pub rows: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_trade_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed_lag_seconds: Option<i64>,
}

pub fn read_feed(
    api: &mut impl Api,
    floor: i64,
    max_pages: usize,
    as_of: i64,
) -> Result<(Vec<Value>, Coverage)> {
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for page in 0..max_pages {
        // end is ignored by the global shape, but refreshes the response cache key.
        let mut params = vec![
            ("limit", "1000".into()),
            ("taker_only", "true".into()),
            ("end", as_of.to_string()),
        ];
        if let Some(value) = &cursor {
            params.push(("cursor", value.clone()));
        }
        let payload = api.get(DATA, "/v2/trades", &params)?;
        let batch = payload["data"]
            .as_array()
            .context("Trade data array missing")?;
        let pagination = payload["pagination"]
            .as_object()
            .context("Trade pagination missing")?;
        let mut oldest = None;
        for row in batch {
            let t = epoch(&row["timestamp"])?;
            oldest = Some(oldest.map_or(t, |old: i64| old.min(t)));
        }
        cursor = match pagination.get("next_cursor") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Null) | None => None,
            _ => bail!("Invalid trade cursor"),
        };
        ensure!(
            cursor.is_some() || pagination.get("has_more") != Some(&Value::Bool(true)),
            "Feed claims more rows without a cursor"
        );
        rows.extend(batch.iter().cloned());
        if cursor.is_none() || oldest.is_some_and(|t| t <= floor) {
            let count = rows.len();
            return Ok((
                rows,
                Coverage {
                    complete: true,
                    pages: page + 1,
                    rows: count,
                    ..Default::default()
                },
            ));
        }
        ensure!(
            seen.insert(cursor.clone().unwrap()),
            "Repeated trade-feed cursor"
        );
    }
    let count = rows.len();
    Ok((
        rows,
        Coverage {
            complete: false,
            pages: max_pages,
            rows: count,
            ..Default::default()
        },
    ))
}
