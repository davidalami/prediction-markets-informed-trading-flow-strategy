use anyhow::Result;
use informed_trader_flow::{
    api::{Api, read_feed},
    monitor::Monitor,
    storage::{self, Condition, State},
    *,
};
use serde_json::{Value, json};
use std::path::PathBuf;

const T: i64 = 10800;
fn rules() -> (Rules, String) {
    load_strategy(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/strategy.json")).unwrap()
}
fn fixture(falling: bool) -> (Vec<Value>, Value) {
    let mut rows = vec![
        json!({"condition_id":"m","token_id":"yes","timestamp":T-590,"price":0.5,"size":200,"proxy_wallet":"old","transaction_hash":"old1"}),
        json!({"condition_id":"m","token_id":"yes","timestamp":T-300,"price":0.5,"size":400,"proxy_wallet":"old","transaction_hash":"old2"}),
    ];
    for (i, t) in [T - 240, T - 180, T - 120, T - 60, T]
        .into_iter()
        .enumerate()
    {
        rows.push(json!({"condition_id":"m","token_id":"yes","timestamp":t,"price":if falling{0.4}else{0.6},
            "size":300,"proxy_wallet":format!("w{}",i%2),"transaction_hash":format!("recent{i}")}));
    }
    rows.push(
        json!({"condition_id":"m","token_id":if falling{"no"}else{"yes"},"timestamp":T+5,
        "price":0.6,"size":2000,"proxy_wallet":"fill","transaction_hash":"entry"}),
    );
    let market = json!({"conditionId":"m","question":"Example","outcomes":"[\"Yes\",\"No\"]",
        "clobTokenIds":"[\"yes\",\"no\"]","endDate":iso(T+600),"closed":false,"acceptingOrders":true,
        "orderPriceMinTickSize":0.01,"orderMinSize":5,"slug":"example","events":[{"slug":"event"}]});
    (rows, market)
}
struct Fake {
    now: i64,
    rows: Vec<Value>,
    market: Value,
    book: Value,
    fail_book: bool,
    fail_market: bool,
}
impl Fake {
    fn new() -> Self {
        let (rows, market) = fixture(false);
        Self {
            now: T - 5,
            rows,
            market,
            book: json!({"asks":[{"price":"0.6","size":"100"}]}),
            fail_book: false,
            fail_market: false,
        }
    }
}
impl Api for Fake {
    fn now(&self) -> i64 {
        self.now
    }
    fn get(&mut self, _base: &str, path: &str, _params: &[(&str, String)]) -> Result<Value> {
        match path {
            "/v2/trades" => {
                let mut rows: Vec<_> = self
                    .rows
                    .iter()
                    .filter(|r| r["timestamp"].as_i64().unwrap() <= self.now)
                    .cloned()
                    .collect();
                rows.push(json!({"condition_id":"heartbeat","token_id":"h","timestamp":self.now,
                    "price":0.5,"size":1,"proxy_wallet":"h","transaction_hash":self.now.to_string()}));
                rows.sort_by_key(|r| -r["timestamp"].as_i64().unwrap());
                Ok(json!({"data":rows,"pagination":{"next_cursor":null,"has_more":false}}))
            }
            "/markets" => {
                anyhow::ensure!(!self.fail_market, "metadata down");
                Ok(json!([self.market]))
            }
            "/book" => {
                anyhow::ensure!(!self.fail_book, "book down");
                let mut b = self.book.clone();
                b["timestamp"] = json!((self.now * 1000).to_string());
                Ok(b)
            }
            _ => anyhow::bail!("Unexpected path"),
        }
    }
}
fn engine() -> Monitor {
    let (rules, hash) = rules();
    Monitor::new(rules, hash.clone(), State::new(&hash), 200, 30)
}
fn raw(rows: Vec<Value>, market: Value) -> Value {
    json!({"markets":[market],"trades":{"m":rows},"errors":[],
        "coverage":{"m":{"complete":true},"closed=true":{"complete":true},"closed=false":{"complete":true}}})
}

#[test]
fn fingerprint_matches_python_pinned_rules() {
    assert_eq!(
        rules().1,
        "7e88e784b61d9fb406856a1945d5edc5440acfbd9893d86f2b2228c100ad37ac"
    );
}
#[test]
fn signal_is_chronological_and_uses_real_opposite_token() {
    let (rules, _) = rules();
    let (mut rows, market) = fixture(true);
    let a = signal_for(&market, &normalize(&rows).unwrap(), T, &rules)
        .unwrap()
        .unwrap();
    assert_eq!(a.outcome, "No");
    assert_eq!(a.token_id, "no");
    rows.last_mut().unwrap()["price"] = json!(0.99);
    let b = signal_for(&market, &normalize(&rows).unwrap(), T, &rules)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(a).unwrap(),
        serde_json::to_value(b).unwrap()
    );
}
#[test]
fn strict_move_threshold_stale_prices_and_reversal() {
    let (mut rules, _) = rules();
    let (rows, market) = fixture(false);
    rules.move_pp = 10.;
    assert!(
        signal_for(&market, &normalize(&rows).unwrap(), T, &rules)
            .unwrap()
            .is_none()
    );
    rules.move_pp = 5.;
    rules.direction = "reversal".into();
    assert_eq!(
        signal_for(&market, &normalize(&rows).unwrap(), T, &rules)
            .unwrap()
            .unwrap()
            .outcome,
        "No"
    );
    assert!(features(&normalize(&rows).unwrap(), "yes", T + 500, &rules).is_none());
}
#[test]
fn normalization_deduplicates_and_preserves_same_second_order() {
    let (rows, _) = fixture(false);
    let mut twice = rows.clone();
    twice.extend(rows.clone());
    assert_eq!(normalize(&twice).unwrap().len(), rows.len());
    let mut other = rows.last().unwrap().clone();
    other["price"] = json!(0.7);
    twice.push(other);
    let normalized = normalize(&twice).unwrap();
    assert_eq!(normalized.last().unwrap().p, 0.7);
}
#[test]
fn complete_budget_and_decimal_tick_limit() {
    let (rules, _) = rules();
    let (_, market) = fixture(false);
    let book = json!({"asks":[{"price":"0.6","size":"100"}]});
    match book_check(&book, &market, 0.606, &rules).unwrap() {
        BookDecision::Eligible(e) => {
            assert_eq!(e.limit_price, 0.6);
            assert!((e.estimated_shares * 0.6 * 1.005 - 5.).abs() < 1e-10);
        }
        _ => panic!("Expected entry"),
    }
    assert!(matches!(
        book_check(
            &json!({"asks":[{"price":0.61,"size":100}]}),
            &market,
            0.606,
            &rules
        )
        .unwrap(),
        BookDecision::Reject("insufficient-live-depth")
    ));
    let mut high_minimum = market.clone();
    high_minimum["orderMinSize"] = json!(10);
    assert!(matches!(
        book_check(&book, &high_minimum, 0.606, &rules).unwrap(),
        BookDecision::Reject("below-market-minimum")
    ));
}
#[test]
fn cold_start_then_exactly_one_live_alert() {
    let mut api = Fake::new();
    let mut engine = engine();
    let first = engine.poll(&mut api);
    assert!(first.baseline);
    assert!(first.signals.is_empty());
    api.now = T + 5;
    let result = engine.poll(&mut api);
    assert_eq!(result.status, "ok");
    assert_eq!(result.signals.len(), 1);
    let alert = &result.signals[0];
    assert_eq!(alert["outcome"], "Yes");
    assert_eq!(alert["limitPrice"], 0.6);
    assert_eq!(alert["url"], "https://polymarket.com/event/event/example");
    api.now += 5;
    assert!(engine.poll(&mut api).signals.is_empty());
}
#[test]
fn baseline_does_not_replay_an_existing_signal() {
    let mut api = Fake::new();
    let mut engine = engine();
    api.now = T + 10;
    assert!(engine.poll(&mut api).signals.is_empty());
    assert!(engine.state.conditions.is_empty());
}
#[test]
fn falling_live_signal_buys_no() {
    let mut api = Fake::new();
    (api.rows, api.market) = fixture(true);
    let mut engine = engine();
    engine.poll(&mut api);
    api.now = T + 5;
    assert_eq!(engine.poll(&mut api).signals[0]["tokenId"], "no");
}
#[test]
fn first_small_fill_is_not_replaced_by_a_later_large_fill() {
    let mut api = Fake::new();
    api.rows.last_mut().unwrap()["size"] = json!(1);
    let mut later = api.rows.last().unwrap().clone();
    later["timestamp"] = json!(T + 6);
    later["size"] = json!(2000);
    later["transaction_hash"] = json!("later");
    api.rows.push(later);
    let mut engine = engine();
    engine.poll(&mut api);
    api.now = T + 10;
    let result = engine.poll(&mut api);
    assert!(result.signals.is_empty());
    assert_eq!(result.skipped["fill-price-or-size"], 1);
}
#[test]
fn book_failure_preserves_pending_and_metadata_failure_retries_decision() {
    let mut api = Fake::new();
    let mut engine = engine();
    engine.poll(&mut api);
    api.now = T + 5;
    api.fail_market = true;
    assert_eq!(engine.poll(&mut api).status, "partial");
    assert_eq!(engine.next_minute, Some(T));
    api.fail_market = false;
    api.fail_book = true;
    api.now += 5;
    assert_eq!(engine.poll(&mut api).status, "partial");
    assert!(matches!(
        engine.state.conditions["m"],
        Condition::Pending { .. }
    ));
    api.fail_book = false;
    api.now += 5;
    assert_eq!(engine.poll(&mut api).signals.len(), 1);
}
#[test]
fn closed_and_deadline_edited_markets_do_not_alert() {
    for reason in ["market-not-open", "deadline-changed"] {
        let mut api = Fake::new();
        let mut engine = engine();
        engine.poll(&mut api);
        // Persist a pending signal first, then alter the current market.
        api.now = T;
        engine.poll(&mut api);
        if reason == "market-not-open" {
            api.market["closed"] = json!(true);
        } else {
            api.market["endDate"] = json!(iso(T + 700));
        }
        api.now = T + 5;
        let result = engine.poll(&mut api);
        assert!(result.signals.is_empty());
        assert_eq!(result.skipped[reason], 1);
    }
}
struct Feed {
    payload: Value,
    now: i64,
}
impl Api for Feed {
    fn now(&self) -> i64 {
        self.now
    }
    fn get(&mut self, _: &str, _: &str, _: &[(&str, String)]) -> Result<Value> {
        Ok(self.payload.clone())
    }
}
#[test]
fn repeated_cursor_missing_cursor_and_incomplete_feed_are_rejected() {
    let row = json!({"timestamp":T});
    let mut api = Feed {
        payload: json!({"data":[row],"pagination":{"next_cursor":"same"}}),
        now: T,
    };
    assert!(
        read_feed(&mut api, T - 600, 3, T)
            .unwrap_err()
            .to_string()
            .contains("Repeated")
    );
    let mut engine = engine();
    engine.max_pages = 1;
    let result = engine.poll(&mut api);
    assert_eq!(result.status, "partial");
    assert!(result.signals.is_empty());
    assert!(engine.next_minute.is_none());
    api.payload = json!({"data":[],"pagination":{"has_more":true}});
    assert!(read_feed(&mut api, T, 1, T).is_err());
}
#[test]
fn stale_and_future_feed_cannot_advance_monitor() {
    for timestamp in [T - 100, T + 1] {
        let mut api = Feed {
            payload: json!({"data":[{"timestamp":timestamp}],"pagination":{"next_cursor":null}}),
            now: T,
        };
        let mut engine = engine();
        let result = engine.poll(&mut api);
        assert_eq!(result.status, "partial");
        assert!(result.signals.is_empty());
        assert!(engine.anchor.is_none());
    }
}
#[test]
fn replay_costs_marks_and_settlement_cutoff_match_python() {
    let (rules, _) = rules();
    let (mut rows, mut market) = fixture(false);
    rows.push(
        json!({"condition_id":"m","token_id":"yes","timestamp":T+595,"price":0.7,"size":100}),
    );
    let marked = replay(&raw(rows.clone(), market.clone()), T, T + 600, &rules).unwrap();
    let shares = 5. / (0.606 * 1.005);
    assert!(
        (marked["trades"][0]["pnlUsd"].as_f64().unwrap() - (shares * 0.7 * 0.99 * 0.995 - 5.))
            .abs()
            < 1e-10
    );
    market["umaResolutionStatus"] = json!("resolved");
    market["outcomePrices"] = json!("[1,0]");
    market["closedTime"] = json!(iso(T + 700));
    assert_eq!(
        replay(&raw(rows.clone(), market.clone()), T, T + 600, &rules).unwrap()["trades"][0]["status"],
        "marked"
    );
    market["closedTime"] = json!(iso(T + 599));
    let settled = replay(&raw(rows, market), T, T + 600, &rules).unwrap();
    assert_eq!(settled["trades"][0]["status"], "settled");
    assert!((settled["summary"]["valuedPnlUsd"].as_f64().unwrap() - (shares - 5.)).abs() < 1e-10);
}
#[test]
fn incomplete_history_and_unpriced_returns_are_not_zero_roi() {
    let (rules, _) = rules();
    let (rows, market) = fixture(false);
    let mut saved = raw(rows, market);
    let r = replay(&saved, T, T + 600, &rules).unwrap();
    assert_eq!(r["summary"]["unpriced"], 1);
    assert!(r["summary"]["valuedRoiPct"].is_null());
    saved["coverage"]["m"]["complete"] = json!(false);
    let r = replay(&saved, T, T + 600, &rules).unwrap();
    assert_eq!(r["status"], "partial");
    assert_eq!(r["summary"]["entries"], 0);
}
#[test]
fn pending_and_ledger_recovery_are_compatible_with_restart() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state.json");
    let ledger = temp.path().join("alerts.jsonl");
    let mut api = Fake::new();
    let mut engine = engine();
    engine.poll(&mut api);
    api.now = T;
    engine.poll(&mut api);
    storage::atomic_save(&state, &engine.state).unwrap();
    let loaded = storage::load_state(&state, &ledger, &engine.fingerprint).unwrap();
    let mut restarted = Monitor::new(
        engine.rules.clone(),
        engine.fingerprint.clone(),
        loaded,
        200,
        30,
    );
    api.now = T + 5;
    let alerts = restarted.poll(&mut api).signals;
    assert_eq!(alerts.len(), 1);
    storage::append_alerts(&ledger, &alerts).unwrap();
    let recovered = storage::load_state(&state, &ledger, &engine.fingerprint).unwrap();
    assert!(matches!(recovered.conditions["m"], Condition::Done { .. }));
    assert!(storage::load_state(&state, &ledger, "wrong").is_err());
    std::fs::write(&ledger, b"{broken").unwrap();
    assert!(storage::load_state(&state, &ledger, &engine.fingerprint).is_err());
}
#[test]
fn state_and_ledger_locks_prevent_parallel_processes() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state.json");
    let ledger = temp.path().join("alerts.jsonl");
    let _locks = storage::lock_paths(&state, &ledger).unwrap();
    assert!(storage::lock_paths(&temp.path().join("other-state.json"), &ledger).is_err());
    assert!(storage::lock_paths(&state, &state).is_err());
}
#[test]
fn gamma_timestamps_and_malformed_rules_are_checked() {
    assert_eq!(
        epoch(&json!("2026-10-30 11:11:21.031998+00")).unwrap(),
        1793358681
    );
    assert!(epoch(&json!("2026-10-30T11:00:00")).is_err());
    let (mut rules, _) = rules();
    rules.notional = f64::NAN;
    assert!(rules.validate().is_err());
}

#[test]
fn historical_fixture_matches_all_twelve_reference_trades_exactly() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/historical");
    let raw: Value =
        serde_json::from_slice(&std::fs::read(root.join("raw.json")).unwrap()).unwrap();
    let expected: Value =
        serde_json::from_slice(&std::fs::read(root.join("results.json")).unwrap()).unwrap();
    let actual = replay(
        &raw,
        epoch(&expected["window"]["start"]).unwrap(),
        epoch(&expected["window"]["end"]).unwrap(),
        &rules().0,
    )
    .unwrap();
    assert_eq!(actual["summary"], expected["summary"]);
    assert_eq!(actual["trades"], expected["trades"]);
}
#[test]
fn compensated_summation_preserves_small_terms() {
    assert_eq!(sum_f64([1e16, 1., -1e16]), 1.);
}

#[test]
fn malformed_book_metadata_never_becomes_a_default_valid_entry() {
    let (rules, _) = rules();
    let (_, mut market) = fixture(false);
    let book = json!({"asks":[{"price":"0.6","size":"100"}]});
    market["orderPriceMinTickSize"] = json!("invalid");
    assert!(book_check(&book, &market, 0.606, &rules).is_err());
    market["orderPriceMinTickSize"] = json!(0.01);
    market["orderMinSize"] = json!("0");
    assert!(book_check(&book, &market, 0.606, &rules).is_err());
}
