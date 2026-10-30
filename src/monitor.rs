use crate::{
    api::{Api, Coverage, read_feed},
    storage::{Condition, State},
    *,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Poll {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub status: &'static str,
    pub fetched_at: String,
    pub baseline: bool,
    pub coverage: Coverage,
    pub buffered_trades: usize,
    pub pending: usize,
    pub signals: Vec<Value>,
    pub skipped: BTreeMap<String, usize>,
    pub errors: Vec<Value>,
}

pub struct Monitor {
    pub rules: Rules,
    pub fingerprint: String,
    pub state: State,
    pub max_pages: usize,
    pub max_age: i64,
    pub anchor: Option<i64>,
    pub next_minute: Option<i64>,
    buffer: HashMap<String, Trade>,
    metadata: HashMap<String, (i64, Value)>,
    sequence: u64,
}

impl Monitor {
    pub fn new(
        rules: Rules,
        fingerprint: String,
        state: State,
        max_pages: usize,
        max_age: i64,
    ) -> Self {
        Self {
            rules,
            fingerprint,
            state,
            max_pages,
            max_age,
            anchor: None,
            next_minute: None,
            buffer: HashMap::new(),
            metadata: HashMap::new(),
            sequence: 0,
        }
    }
    fn market(&mut self, api: &mut impl Api, cid: &str, fresh: bool) -> Result<Value> {
        if !fresh {
            if let Some((at, value)) = self.metadata.get(cid) {
                if api.now() - at < 15 {
                    return Ok(value.clone());
                }
            }
        }
        let payload = api.get(
            GAMMA,
            "/markets",
            &[
                ("condition_ids", cid.into()),
                ("limit", "1".into()),
                ("_monitor_ts", api.now().to_string()),
            ],
        )?;
        let market = payload
            .as_array()
            .and_then(|xs| xs.iter().find(|m| m["conditionId"] == cid))
            .context("Market metadata unavailable")?
            .clone();
        self.metadata
            .insert(cid.into(), (api.now(), market.clone()));
        Ok(market)
    }
    fn finish(
        &mut self,
        cid: &str,
        signal: &Signal,
        reason: &str,
        skipped: &mut BTreeMap<String, usize>,
    ) {
        self.state.conditions.insert(
            cid.into(),
            Condition::Done {
                deadline: signal.deadline,
                signal_at: signal.signal_at,
                reason: reason.into(),
                id: None,
            },
        );
        *skipped.entry(reason.into()).or_default() += 1;
    }
    fn output(
        &self,
        status: &'static str,
        timing: (i64, bool),
        coverage: Coverage,
        signals: Vec<Value>,
        skipped: BTreeMap<String, usize>,
        errors: Vec<Value>,
    ) -> Poll {
        let (now, baseline) = timing;
        Poll {
            schema_version: 1,
            kind: "flow-monitor",
            status,
            fetched_at: iso(now),
            baseline,
            coverage,
            buffered_trades: self.buffer.len(),
            pending: self
                .state
                .conditions
                .values()
                .filter(|s| matches!(s, Condition::Pending { .. }))
                .count(),
            signals,
            skipped,
            errors,
        }
    }
    pub fn poll(&mut self, api: &mut impl Api) -> Poll {
        let started = api.now();
        let warmup = 2 * self.rules.lookback_minutes * 60 + self.rules.max_stale_seconds + 60;
        let floor = self.anchor.map(|t| t - 30).unwrap_or(started - warmup);
        let (incoming, mut coverage) = match read_feed(api, floor, self.max_pages, started) {
            Ok(result) => result,
            Err(err) => {
                return self.output(
                    "error",
                    (api.now(), false),
                    Coverage::default(),
                    vec![],
                    BTreeMap::new(),
                    vec![json!({"stage":"feed","error":err.to_string()})],
                );
            }
        };
        if !coverage.complete {
            return self.output(
                "partial",
                (api.now(), false),
                coverage,
                vec![],
                BTreeMap::new(),
                vec![json!({"stage":"feed","error":"pagination-cap"})],
            );
        }
        let now = api.now();
        let latest = incoming
            .iter()
            .filter_map(|r| epoch(&r["timestamp"]).ok())
            .max()
            .unwrap_or(0);
        coverage.latest_trade_at = (latest > 0).then(|| iso(latest));
        coverage.feed_lag_seconds = (latest > 0).then_some(now - latest);
        if latest == 0 || latest > now || now - latest > self.max_age {
            return self.output(
                "partial",
                (now, false),
                coverage,
                vec![],
                BTreeMap::new(),
                vec![json!({"stage":"feed","error":"stale-or-future-feed"})],
            );
        }
        let parsed = (|| -> Result<Vec<Trade>> {
            let mut trades = Vec::new();
            for row in &incoming {
                if text(&row["condition_id"]).is_empty() {
                    continue;
                }
                if let Some(trade) = Trade::parse(row, 0)? {
                    trades.push(trade);
                }
            }
            Ok(trades)
        })();
        let parsed = match parsed {
            Ok(rows) => rows,
            Err(err) => {
                return self.output(
                    "partial",
                    (now, false),
                    coverage,
                    vec![],
                    BTreeMap::new(),
                    vec![json!({"stage":"feed","error":err.to_string()})],
                );
            }
        };
        for mut row in parsed {
            if let Some(old) = self.buffer.get(&row.key) {
                row.seq = old.seq;
            } else {
                row.seq = self.sequence;
                self.sequence += 1;
            }
            self.buffer.insert(row.key.clone(), row);
        }
        self.buffer.retain(|_, r| r.t >= now - warmup);
        self.anchor = Some(latest);
        let mut groups: BTreeMap<String, Vec<Trade>> = BTreeMap::new();
        for row in self.buffer.values() {
            groups.entry(row.cid.clone()).or_default().push(row.clone());
        }
        for rows in groups.values_mut() {
            rows.sort_by_key(|r| (r.t, r.seq));
        }
        let baseline = self.next_minute.is_none();
        let (mut errors, mut skipped, mut alerts) = (vec![], BTreeMap::new(), vec![]);
        if baseline {
            self.next_minute = Some((now.div_euclid(60) + 1) * 60);
        } else {
            while self.next_minute.unwrap() <= now.min(latest) {
                let t = self.next_minute.unwrap();
                if now - t > self.max_age {
                    *skipped.entry("missed-stale-decision".into()).or_default() += 1;
                    self.next_minute = Some(t + 60);
                    continue;
                }
                let mut retry = false;
                for (cid, rows) in &groups {
                    if self.state.conditions.contains_key(cid) {
                        continue;
                    }
                    let tokens: BTreeSet<_> = rows.iter().map(|r| r.token.as_str()).collect();
                    if !tokens
                        .into_iter()
                        .any(|token| features(rows, token, t, &self.rules).is_some())
                    {
                        continue;
                    }
                    let result = (|| -> Result<Option<Signal>> {
                        let market = self.market(api, cid, false)?;
                        signal_for(&market, rows, t, &self.rules)
                    })();
                    match result {
                        Ok(Some(signal)) => {
                            self.state
                                .conditions
                                .insert(cid.clone(), Condition::Pending { signal });
                        }
                        Ok(None) => {}
                        Err(err) => {
                            errors.push(
                                json!({"stage":"market","conditionId":cid,"error":err.to_string()}),
                            );
                            retry = true;
                        }
                    }
                }
                if retry {
                    break;
                }
                self.next_minute = Some(t + 60);
            }
        }
        let pending: Vec<_> = self
            .state
            .conditions
            .iter()
            .filter_map(|(cid, item)| match item {
                Condition::Pending { signal } => Some((cid.clone(), signal.clone())),
                _ => None,
            })
            .collect();
        for (cid, signal) in pending {
            let rows = groups.get(&cid).map(Vec::as_slice).unwrap_or(&[]);
            let expires =
                (signal.deadline - 1).min(signal.signal_at + self.rules.max_fill_wait_seconds);
            let Some(fill) = first_print(rows, &signal, &self.rules, now) else {
                if now > expires {
                    self.finish(&cid, &signal, "signal-without-later-fill", &mut skipped);
                }
                continue;
            };
            let Some(proxy) = entry_proxy(fill, &self.rules) else {
                self.finish(&cid, &signal, "fill-price-or-size", &mut skipped);
                continue;
            };
            if now - fill.t > self.max_age || now >= signal.deadline {
                self.finish(&cid, &signal, "stale-entry", &mut skipped);
                continue;
            }
            let checked = (|| -> Result<std::result::Result<Value, &'static str>> {
                let market = self.market(api, &cid, true)?;
                if epoch(&market["endDate"])? != signal.deadline {
                    return Ok(Err("deadline-changed"));
                }
                if market["closed"] == true || market["acceptingOrders"] != true {
                    return Ok(Err("market-not-open"));
                }
                if array(&market["clobTokenIds"])?
                    .iter()
                    .filter(|v| text(v) == signal.token_id)
                    .count()
                    != 1
                {
                    return Ok(Err("token-mapping-changed"));
                }
                let book = api.get(
                    CLOB,
                    "/book",
                    &[
                        ("token_id", signal.token_id.clone()),
                        ("_monitor_ts", api.now().to_string()),
                    ],
                )?;
                let entry = match book_check(&book, &market, proxy, &self.rules)? {
                    BookDecision::Eligible(entry) => entry,
                    BookDecision::Reject(reason) => return Ok(Err(reason)),
                };
                let at = api.now();
                if at - fill.t > self.max_age || at >= signal.deadline {
                    return Ok(Err("stale-entry"));
                }
                if !book["timestamp"].is_null() {
                    let time = number(&book["timestamp"])? / 1000.;
                    ensure!(
                        at as f64 - time <= self.max_age as f64 && time <= at as f64 + 5.,
                        "Stale order book"
                    );
                }
                let id = format!(
                    "{:x}",
                    Sha256::digest(format!("{}:{cid}", self.fingerprint))
                )[..20]
                    .to_owned();
                let slug = text(&market["slug"]);
                let event_slug = market["events"]
                    .as_array()
                    .and_then(|xs| xs.iter().find_map(|e| e["slug"].as_str()))
                    .unwrap_or(&slug);
                let mut url = format!("https://polymarket.com/event/{event_slug}");
                if !slug.is_empty() && slug != event_slug {
                    url.push('/');
                    url.push_str(&slug);
                }
                let mut alert = serde_json::to_value(&signal)?;
                let map = alert.as_object_mut().unwrap();
                let extra = json!({"schemaVersion":1,"type":"flow-alert","status":"eligible","phase":"pending",
                    "id":id,"strategyFingerprint":self.fingerprint,"alertedAt":iso(at),
                    "signalAt":iso(signal.signal_at),"scheduledClose":iso(signal.deadline),
                    "entryPrintAt":iso(fill.t),"entryPrintPrice":fill.p,"entryPrintUsd":fill.usd,
                    "entryProxy":proxy,"feedLagSeconds":coverage.feed_lag_seconds,"url":url});
                map.extend(extra.as_object().unwrap().clone());
                map.extend(serde_json::to_value(entry)?.as_object().unwrap().clone());
                Ok(Ok(alert))
            })();
            match checked {
                Ok(Ok(alert)) => {
                    self.state.conditions.insert(
                        cid,
                        Condition::Done {
                            deadline: signal.deadline,
                            signal_at: signal.signal_at,
                            reason: "alerted".into(),
                            id: Some(text(&alert["id"])),
                        },
                    );
                    alerts.push(alert);
                }
                Ok(Err(reason)) => self.finish(&cid, &signal, reason, &mut skipped),
                Err(err) => {
                    errors.push(json!({"stage":"entry","conditionId":cid,"error":err.to_string()}))
                }
            }
        }
        self.state
            .conditions
            .retain(|_, s| s.deadline() > now - 7 * 86400);
        // Market metadata is transient too; do not grow without bound for long runs.
        self.metadata.retain(|_, (at, _)| now - *at < 3600);
        self.output(
            if errors.is_empty() { "ok" } else { "partial" },
            (api.now(), baseline),
            coverage,
            alerts,
            skipped,
            errors,
        )
    }
}
