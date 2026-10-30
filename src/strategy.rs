use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::Path,
    str::FromStr,
};

pub const GAMMA: &str = "https://gamma-api.polymarket.com";
pub const DATA: &str = "https://data-api.polymarket.com";
pub const CLOB: &str = "https://clob.polymarket.com";
pub const RULE_KEYS: &[&str] = &[
    "lookback_minutes",
    "final_minutes",
    "move_pp",
    "min_volume_usd",
    "min_baseline_usd",
    "volume_multiple",
    "min_trades",
    "min_wallets",
    "notional",
    "slippage_bps",
    "fee_bps",
    "latency_seconds",
    "max_stale_seconds",
    "max_fill_wait_seconds",
    "max_participation",
    "direction",
    "max_entry_price",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rules {
    pub lookback_minutes: i64,
    pub final_minutes: i64,
    pub move_pp: f64,
    pub min_volume_usd: f64,
    pub min_baseline_usd: f64,
    pub volume_multiple: f64,
    pub min_trades: usize,
    pub min_wallets: usize,
    pub notional: f64,
    pub slippage_bps: f64,
    pub fee_bps: f64,
    pub latency_seconds: i64,
    pub max_stale_seconds: i64,
    pub max_fill_wait_seconds: i64,
    pub max_participation: f64,
    pub direction: String,
    pub max_entry_price: f64,
}

impl Rules {
    pub fn validate(&self) -> Result<()> {
        for value in [
            self.move_pp,
            self.min_volume_usd,
            self.min_baseline_usd,
            self.volume_multiple,
            self.notional,
            self.slippage_bps,
            self.fee_bps,
            self.max_participation,
            self.max_entry_price,
        ] {
            ensure!(
                value.is_finite() && value > 0.,
                "Rule values must be finite and positive"
            );
        }
        ensure!(
            (1..=525600).contains(&self.lookback_minutes)
                && (1..=525600).contains(&self.final_minutes),
            "Invalid minute window"
        );
        ensure!(
            [
                self.latency_seconds,
                self.max_stale_seconds,
                self.max_fill_wait_seconds
            ]
            .iter()
            .all(|x| (1..=31536000).contains(x)),
            "Invalid timing rule"
        );
        ensure!(
            self.min_trades > 0 && self.min_wallets > 0,
            "Invalid activity counts"
        );
        ensure!(
            self.max_entry_price <= 1.
                && self.max_participation <= 1.
                && self.slippage_bps < 10000.
                && self.fee_bps < 10000.,
            "Invalid cost/price bound"
        );
        ensure!(
            matches!(self.direction.as_str(), "momentum" | "reversal"),
            "Invalid direction"
        );
        Ok(())
    }
}

pub fn load_strategy(path: &Path) -> Result<(Rules, String)> {
    let source: Value = serde_json::from_slice(&fs::read(path)?)?;
    let params = source["parameters"]
        .as_object()
        .context("Strategy parameters missing")?;
    let mut canonical = BTreeMap::new();
    for key in RULE_KEYS {
        canonical.insert(
            *key,
            params
                .get(*key)
                .with_context(|| format!("Missing rule {key}"))?
                .clone(),
        );
    }
    let rules: Rules = serde_json::from_value(serde_json::to_value(&canonical)?)?;
    rules.validate()?;
    // Python json.dumps(sort_keys=True) uses these separators. Retain the JSON
    // number types from strategy.json so the pinned Python fingerprint is shared.
    let body = canonical
        .iter()
        .map(|(k, v)| format!("{}: {}", json!(k), v))
        .collect::<Vec<_>>()
        .join(", ");
    let fingerprint = format!("{:x}", Sha256::digest(format!("{{{body}}}").as_bytes()));
    Ok((rules, fingerprint))
}

pub fn number(value: &Value) -> Result<f64> {
    let n = match value {
        Value::Number(n) => n.as_f64().context("Number out of range")?,
        Value::String(s) => s.parse().context("Invalid numeric string")?,
        _ => bail!("Expected a number"),
    };
    ensure!(n.is_finite(), "Nonfinite number");
    Ok(n)
}

pub fn epoch(value: &Value) -> Result<i64> {
    if value.is_number() {
        let n = number(value)?;
        ensure!(
            n >= i64::MIN as f64 && n < i64::MAX as f64,
            "Timestamp out of range"
        );
        ensure!(
            DateTime::<Utc>::from_timestamp(n as i64, 0).is_some(),
            "Timestamp out of calendar range"
        );
        return Ok(n as i64);
    }
    let s = value.as_str().context("Timestamp must be ISO or numeric")?;
    let mut s = s.replacen(' ', "T", 1);
    if s.len() >= 3 {
        let tail = &s.as_bytes()[s.len() - 3..];
        if (tail[0] == b'+' || tail[0] == b'-') && tail[1..].iter().all(u8::is_ascii_digit) {
            s.push_str(":00");
        }
    }
    Ok(DateTime::parse_from_rfc3339(&s)
        .context("Timezone-aware ISO timestamp required")?
        .timestamp())
}

pub fn iso(t: i64) -> String {
    DateTime::<Utc>::from_timestamp(t, 0)
        .expect("validated timestamp")
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn array(value: &Value) -> Result<Vec<Value>> {
    match value {
        Value::Array(xs) => Ok(xs.clone()),
        Value::String(s) => serde_json::from_str::<Vec<Value>>(s).context("Invalid encoded array"),
        Value::Null => Ok(vec![]),
        _ => bail!("Expected array or encoded array"),
    }
}

pub fn text(v: &Value) -> String {
    v.as_str().map(str::to_owned).unwrap_or_else(|| {
        if v.is_null() {
            String::new()
        } else {
            v.to_string()
        }
    })
}

#[derive(Debug, Clone)]
pub struct Trade {
    pub cid: String,
    pub token: String,
    pub wallet: String,
    pub t: i64,
    pub p: f64,
    pub size: f64,
    pub usd: f64,
    pub key: String,
    pub seq: u64,
}

impl Trade {
    pub fn parse(v: &Value, seq: u64) -> Result<Option<Self>> {
        let t = epoch(&v["timestamp"])?;
        let p = number(&v["price"])?;
        let size = number(&v["size"])?;
        ensure!(
            (0. ..=1.).contains(&p) && size > 0.,
            "Invalid trade economics"
        );
        let token = text(
            v.get("token_id")
                .or_else(|| v.get("asset"))
                .unwrap_or(&Value::Null),
        );
        if p == 0. || p == 1. || token.is_empty() {
            return Ok(None);
        }
        let cid = text(&v["condition_id"]);
        let wallet = text(
            v.get("proxy_wallet")
                .or_else(|| v.get("proxyWallet"))
                .unwrap_or(&Value::Null),
        );
        let hash = v
            .get("transaction_hash")
            .or_else(|| v.get("transactionHash"))
            .unwrap_or(&Value::Null);
        let key = json!([cid, hash, token, t, v["side"], p, size, wallet]).to_string();
        Ok(Some(Self {
            cid,
            token,
            wallet,
            t,
            p,
            size,
            usd: p * size,
            key,
            seq,
        }))
    }
}

pub fn normalize(rows: &[Value]) -> Result<Vec<Trade>> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for (seq, row) in rows.iter().enumerate() {
        if let Some(trade) = Trade::parse(row, seq as u64)? {
            if seen.insert(trade.key.clone()) {
                result.push(trade);
            }
        }
    }
    result.sort_by_key(|r| (r.t, r.seq));
    Ok(result)
}

pub(crate) fn last_at<'a>(rows: &'a [Trade], token: &str, t: i64, stale: i64) -> Option<&'a Trade> {
    rows.iter()
        .rev()
        .find(|r| r.token == token && r.t <= t && t - r.t <= stale)
}

/// Neumaier compensated summation keeps threshold calculations stable.
pub fn sum_f64(values: impl IntoIterator<Item = f64>) -> f64 {
    let (mut total, mut correction) = (0.0_f64, 0.0_f64);
    for value in values {
        let next = total + value;
        correction += if total.abs() >= value.abs() {
            (total - next) + value
        } else {
            (value - next) + total
        };
        total = next;
    }
    if correction != 0. && correction.is_finite() {
        total + correction
    } else {
        total
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Features {
    pub change_pp: f64,
    pub volume_usd: f64,
    pub baseline_usd: f64,
    pub volume_ratio: f64,
    pub fills: usize,
    pub wallets: usize,
}

pub fn features(rows: &[Trade], token: &str, t: i64, rules: &Rules) -> Option<Features> {
    let window = rules.lookback_minutes * 60;
    let p0 = last_at(rows, token, t - window, rules.max_stale_seconds)?;
    let p1 = last_at(rows, token, t, rules.max_stale_seconds)?;
    let change = p1.p - p0.p;
    if change.abs() <= rules.move_pp / 100. + 1e-12 {
        return None;
    }
    let recent: Vec<_> = rows
        .iter()
        .filter(|r| t - window < r.t && r.t <= t)
        .collect();
    let previous = rows
        .iter()
        .filter(|r| t - 2 * window < r.t && r.t <= t - window);
    let volume = sum_f64(recent.iter().map(|r| r.usd));
    let baseline = sum_f64(previous.map(|r| r.usd));
    let wallets = recent
        .iter()
        .filter(|r| !r.wallet.is_empty())
        .map(|r| &r.wallet)
        .collect::<HashSet<_>>()
        .len();
    if volume < rules.min_volume_usd
        || baseline < rules.min_baseline_usd
        || volume < rules.volume_multiple * baseline
        || recent.len() < rules.min_trades
        || wallets < rules.min_wallets
    {
        return None;
    }
    Some(Features {
        change_pp: change * 100.,
        volume_usd: volume,
        baseline_usd: baseline,
        volume_ratio: volume / baseline,
        fills: recent.len(),
        wallets,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Signal {
    pub condition_id: String,
    pub title: String,
    pub token_id: String,
    pub outcome: String,
    pub signal_at: i64,
    pub deadline: i64,
    #[serde(flatten)]
    pub features: Features,
}

pub fn signal_for(market: &Value, rows: &[Trade], t: i64, rules: &Rules) -> Result<Option<Signal>> {
    let tokens = array(&market["clobTokenIds"])?;
    let outcomes = array(&market["outcomes"])?;
    if tokens.len() != 2 || outcomes.len() != 2 {
        return Ok(None);
    }
    let deadline = epoch(&market["endDate"])?;
    if !(deadline - rules.final_minutes * 60 <= t && t < deadline) {
        return Ok(None);
    }
    let base = outcomes
        .iter()
        .position(|v| text(v).to_lowercase() == "yes")
        .unwrap_or(0);
    let Some(stats) = features(rows, &text(&tokens[base]), t, rules) else {
        return Ok(None);
    };
    let mut rising = stats.change_pp > 0.;
    if rules.direction == "reversal" {
        rising = !rising;
    }
    let chosen = if rising { base } else { 1 - base };
    Ok(Some(Signal {
        condition_id: text(&market["conditionId"]),
        title: text(&market["question"]),
        token_id: text(&tokens[chosen]),
        outcome: text(&outcomes[chosen]),
        signal_at: t,
        deadline,
        features: stats,
    }))
}

pub fn first_print<'a>(
    rows: &'a [Trade],
    signal: &Signal,
    rules: &Rules,
    end: i64,
) -> Option<&'a Trade> {
    let expires = end
        .min(signal.deadline - 1)
        .min(signal.signal_at + rules.max_fill_wait_seconds);
    rows.iter().find(|r| {
        r.token == signal.token_id
            && r.t >= signal.signal_at + rules.latency_seconds
            && r.t <= expires
    })
}

pub fn entry_proxy(fill: &Trade, rules: &Rules) -> Option<f64> {
    let proxy = fill.p * (1. + rules.slippage_bps / 10000.);
    (proxy < 1.
        && proxy <= rules.max_entry_price
        && rules.notional <= fill.usd * rules.max_participation)
        .then_some(proxy)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveEntry {
    pub limit_price: f64,
    pub best_ask: f64,
    pub estimated_shares: f64,
    pub estimated_average_price: f64,
    pub budget_usd: f64,
    pub assumed_fee_bps: f64,
}

#[derive(Debug)]
pub enum BookDecision {
    Eligible(LiveEntry),
    Reject(&'static str),
}

pub fn book_check(book: &Value, market: &Value, proxy: f64, rules: &Rules) -> Result<BookDecision> {
    let tick_value = market
        .get("orderPriceMinTickSize")
        .filter(|v| !v.is_null() && v != &&json!(0) && v != &&json!(0.0));
    let tick_text = tick_value.map(text).unwrap_or_else(|| "0.01".into());
    let tick = Decimal::from_str(&tick_text).context("Invalid tick")?;
    ensure!(tick > Decimal::ZERO, "Invalid tick");
    let cap = Decimal::from_str(&proxy.min(rules.max_entry_price).to_string())?;
    let rounded = cap
        .checked_div(tick)
        .context("Tick division overflow")?
        .floor()
        .checked_mul(tick)
        .context("Tick multiplication overflow")?;
    let limit = rounded.to_f64().context("Invalid rounded limit")?;
    let mut asks = Vec::new();
    for row in book["asks"].as_array().context("Book asks array missing")? {
        let p = number(&row["price"])?;
        let size = number(&row["size"])?;
        ensure!(p > 0. && p < 1. && size > 0., "Invalid book level");
        asks.push((p, size));
    }
    asks.sort_by(|a, b| a.0.total_cmp(&b.0));
    let principal = rules.notional / (1. + rules.fee_bps / 10000.);
    let (mut remaining, mut shares, mut cost) = (principal, 0., 0.);
    for &(p, size) in &asks {
        if p > limit + 1e-12 {
            break;
        }
        let usd = remaining.min(p * size);
        shares += usd / p;
        cost += usd;
        remaining -= usd;
        if remaining <= 1e-9 {
            break;
        }
    }
    if remaining > 1e-9 {
        return Ok(BookDecision::Reject("insufficient-live-depth"));
    }
    let minimum = match market.get("orderMinSize") {
        None | Some(Value::Null) => 5.,
        Some(Value::Number(n)) if n.as_f64() == Some(0.) => 5.,
        Some(value) => number(value)?,
    };
    ensure!(minimum > 0., "Invalid minimum shares");
    if shares + 1e-9 < minimum {
        return Ok(BookDecision::Reject("below-market-minimum"));
    }
    Ok(BookDecision::Eligible(LiveEntry {
        limit_price: limit,
        best_ask: asks[0].0,
        estimated_shares: shares,
        estimated_average_price: cost / shares,
        budget_usd: rules.notional,
        assumed_fee_bps: rules.fee_bps,
    }))
}
