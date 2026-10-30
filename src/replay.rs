use crate::strategy::last_at;
use crate::*;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn replay(raw: &Value, start: i64, end: i64, rules: &Rules) -> Result<Value> {
    ensure!(start < end, "Replay start must precede end");
    let markets = raw["markets"].as_array().context("Markets array missing")?;
    let mut trades = Vec::new();
    let mut skips: BTreeMap<String, usize> = BTreeMap::new();
    for market in markets {
        let cid = text(&market["conditionId"]);
        if raw["coverage"][&cid]["complete"] != true {
            *skips.entry("incomplete_history".into()).or_default() += 1;
            continue;
        }
        let tokens = array(&market["clobTokenIds"])?;
        let outcomes = array(&market["outcomes"])?;
        if tokens.len() != 2 || outcomes.len() != 2 {
            *skips.entry("nonbinary".into()).or_default() += 1;
            continue;
        }
        let parsed = (|| -> Result<_> {
            Ok((
                epoch(&market["endDate"])?,
                normalize(
                    raw["trades"][&cid]
                        .as_array()
                        .context("Trade array missing")?,
                )?,
            ))
        })();
        let (deadline, rows) = match parsed {
            Ok(value) => value,
            Err(_) => {
                *skips.entry("invalid_market_or_trades".into()).or_default() += 1;
                continue;
            }
        };
        let first = start.max(deadline - rules.final_minutes * 60);
        let mut t = first.div_euclid(60) * 60;
        if t < first {
            t += 60;
        }
        while t <= end.min(deadline - 1) {
            if let Some(signal) = signal_for(market, &rows, t, rules)? {
                let Some(fill) = first_print(&rows, &signal, rules, end) else {
                    *skips.entry("signal_without_later_fill".into()).or_default() += 1;
                    break;
                };
                let Some(proxy) = entry_proxy(fill, rules) else {
                    *skips.entry("fill_price_or_size".into()).or_default() += 1;
                    break;
                };
                let fees = rules.fee_bps / 10000.;
                let shares = rules.notional / (proxy * (1. + fees));
                let chosen = tokens
                    .iter()
                    .position(|v| text(v) == signal.token_id)
                    .context("Token mismatch")?;
                let mut payout = None;
                if market["umaResolutionStatus"] == "resolved"
                    && !market["closedTime"].is_null()
                    && epoch(&market["closedTime"])? <= end
                {
                    let prices = array(&market["outcomePrices"])?
                        .iter()
                        .map(number)
                        .collect::<Result<Vec<_>>>()?;
                    if prices.len() == 2
                        && ((prices[0] == 0. && prices[1] == 1.)
                            || (prices[0] == 1. && prices[1] == 0.))
                    {
                        payout = Some(prices[chosen]);
                    }
                }
                let mark = last_at(&rows, &signal.token_id, end, rules.max_stale_seconds);
                let value = payout.map(|p| shares * p).or_else(|| {
                    mark.map(|r| shares * r.p * (1. - rules.slippage_bps / 10000.) * (1. - fees))
                });
                let status = if payout.is_some() {
                    "settled"
                } else if mark.is_some() {
                    "marked"
                } else {
                    "unpriced"
                };
                trades.push(
                    json!({"conditionId":cid,"title":signal.title,"outcome":signal.outcome,
                    "signalAt":iso(t),"entryAt":iso(fill.t),"scheduledClose":iso(deadline),
                    "changePp":signal.features.change_pp,"volumeUsd":signal.features.volume_usd,
                    "volumeRatio":signal.features.volume_ratio,"wallets":signal.features.wallets,
                    "entryProxy":proxy,"shares":shares,"costUsd":rules.notional,"status":status,
                    "valueUsd":value,"pnlUsd":value.map(|v| v-rules.notional)}),
                );
                break;
            }
            t += 60;
        }
    }
    let valued: Vec<_> = trades.iter().filter(|t| t["pnlUsd"].is_number()).collect();
    let settled: Vec<_> = trades.iter().filter(|t| t["status"] == "settled").collect();
    let pnl = sum_f64(valued.iter().map(|t| t["pnlUsd"].as_f64().unwrap()));
    let errors = raw.get("errors").cloned().unwrap_or_else(|| json!([]));
    let complete = ["closed=true", "closed=false"]
        .iter()
        .all(|k| raw["coverage"][*k]["complete"] == true)
        && markets
            .iter()
            .all(|m| raw["coverage"][text(&m["conditionId"])]["complete"] == true);
    let status = if markets.is_empty() && errors.as_array().is_some_and(|x| !x.is_empty()) {
        "unavailable"
    } else if !complete || errors.as_array().is_some_and(|x| !x.is_empty()) {
        "partial"
    } else {
        "ok"
    };
    Ok(
        json!({"status":status,"window":{"start":iso(start),"end":iso(end)},"config":rules,
        "summary":{"marketsDiscovered":markets.len(),"entries":trades.len(),"settled":settled.len(),
        "marked":trades.iter().filter(|t| t["status"]=="marked").count(),
        "unpriced":trades.iter().filter(|t| t["status"]=="unpriced").count(),
        "investedUsd":trades.len() as f64*rules.notional,
        "valuedPnlUsd":if valued.is_empty(){None}else{Some(pnl)},
        "valuedRoiPct":if valued.is_empty(){None}else{Some(100.*pnl/(valued.len() as f64*rules.notional))},
        "settledWinRate":if settled.is_empty(){None}else{Some(settled.iter().filter(|t| t["pnlUsd"].as_f64().unwrap()>0.).count() as f64/settled.len() as f64)}},
        "skips":skips,"trades":trades,"errors":errors,"coverage":raw["coverage"]}),
    )
}
