use super::*;
use proptest::prelude::*;
use serde_json::json;
use std::cmp::Ordering;

const NOW: f64 = 1_000_000.0;
const HOUR: f64 = 3600.0;
const HORIZON: f64 = 24.0 * HOUR;

fn entry() -> Value {
    json!({"provider": "codex", "windows": [
        {"label": "5h", "remaining_pct": 80, "resets_at": NOW + HOUR},
        {"label": "sem", "remaining_pct": 60, "resets_at": NOW + 100.0 * HOUR}
    ]})
}

fn with_credits(mut entry: Value, count: i64, expires: Vec<f64>) -> Value {
    entry["reset_credits"] = json!({"available": count, "expires": expires});
    entry
}

fn key(entry: &Value) -> PriorityKey {
    use_priority(entry, NOW, HORIZON)
}

#[test]
fn monthly_total_governs_opencode_go_even_when_weekly_resets_sooner() {
    let entry = json!({"provider": "opencode-go", "windows": [
        {"label": "5h", "remaining_pct": 100, "resets_at": NOW + HOUR},
        {"label": "sem", "remaining_pct": 90, "resets_at": NOW + 24.0 * HOUR},
        {"label": "mês", "remaining_pct": 10, "resets_at": NOW + 240.0 * HOUR}
    ]});
    let actual = key(&entry);
    assert_eq!(actual.0, 2);
    assert!((-actual.1 - 10.0 / 240.0).abs() < 1e-9);
    assert_eq!(actual.2, 240.0 * HOUR);
}

#[test]
fn monthly_label_aliases_take_precedence_over_weekly() {
    for label in ["mês", "mes", "mensal", "month", "Monthly"] {
        let windows = vec![
            json!({"label": "sem", "remaining_pct": 80, "resets_at": NOW + HOUR}),
            json!({"label": label, "remaining_pct": 20, "resets_at": NOW + 100.0 * HOUR}),
        ];
        assert_eq!(governing_window(&windows, NOW).unwrap()["label"], label);
    }
}

#[test]
fn auxiliary_monthly_mcp_limit_does_not_replace_the_weekly_total() {
    let windows = vec![
        json!({"label": "sem", "remaining_pct": 80, "resets_at": NOW + HOUR}),
        json!({"label": "mcp", "remaining_pct": 100, "resets_at": NOW + 720.0 * HOUR}),
    ];
    assert_eq!(governing_window(&windows, NOW).unwrap()["label"], "sem");
}

#[test]
fn expiring_extra_reset_advances_the_consumption_deadline() {
    let ordinary = entry();
    let extra = with_credits(entry(), 2, vec![NOW + 6.0 * HOUR, NOW + 80.0 * HOUR]);
    let (a, b) = (key(&ordinary), key(&extra));
    assert_eq!(b.0, 0);
    assert_eq!(b.2, 6.0 * HOUR);
    assert!((-b.1 - 60.0 / 6.0).abs() < 1e-9);
    assert_eq!(b.partial_cmp(&a), Some(Ordering::Less));
}

#[test]
fn extra_reset_cannot_postpone_a_sooner_natural_reset() {
    let a = key(&entry());
    let b = key(&with_credits(entry(), 2, vec![NOW + 200.0 * HOUR]));
    assert_eq!((a.0, a.1, a.2), (b.0, b.1, b.2));
}

#[test]
fn more_available_resets_break_otherwise_equal_priorities() {
    let one = key(&with_credits(entry(), 1, vec![]));
    let two = key(&with_credits(entry(), 2, vec![]));
    assert_eq!(two.partial_cmp(&one), Some(Ordering::Less));
    assert_eq!(
        (one.0, one.1, one.2),
        (two.0, two.1, two.2),
        "unknown expiry must not invent urgency or quota"
    );
}

#[test]
fn expired_or_unavailable_credits_do_not_improve_the_order() {
    let original = key(&entry());
    for extra in [
        with_credits(entry(), 2, vec![NOW - HOUR, NOW]),
        with_credits(entry(), 0, vec![NOW + HOUR]),
        with_credits(entry(), -2, vec![NOW + HOUR]),
    ] {
        assert_eq!(key(&extra), original);
    }
}

#[test]
fn auxiliary_limits_remain_hard_blocks_even_with_extra_resets() {
    let mut blocked = entry();
    blocked["windows"][0]["remaining_pct"] = json!(0);
    assert_eq!(key(&with_credits(blocked, 2, vec![NOW + HOUR])).0, 4);
}

#[test]
fn reset_credits_alone_do_not_invent_usable_quota() {
    let entry = with_credits(json!({}), 2, vec![NOW + HOUR]);
    assert_eq!(key(&entry).0, 5);
}

#[test]
fn failed_quota_query_cannot_recommend_last_good_values() {
    let mut stale = with_credits(entry(), 2, vec![NOW + HOUR]);
    stale["ok"] = json!(false);
    stale["error"] = json!("expired");
    assert_eq!(key(&stale).0, 5);
    assert!(priority_hint(&stale, NOW).unwrap().contains("consulta de cota"));
}

#[test]
fn hints_distinguish_allowance_expiry_and_manual_redemption() {
    assert_eq!(priority_hint(&entry(), NOW).as_deref(), Some("Ordem pela cota semanal"));
    let extra = with_credits(entry(), 1, vec![NOW + HOUR]);
    assert!(priority_hint(&extra, NOW).unwrap().contains("reset extra"));
    let mut blocked = extra;
    blocked["windows"][0]["remaining_pct"] = json!(0);
    assert!(priority_hint(&blocked, NOW).unwrap().contains("resgate manual"));
    blocked["reset_credits"]["available"] = json!(0);
    assert!(priority_hint(&blocked, NOW).is_none());
    assert!(priority_hint(&json!({}), NOW).is_none());
    let generic = json!({"windows": [{"label": "5h", "remaining_pct": 80}]});
    assert_eq!(priority_hint(&generic, NOW).as_deref(), Some("Ordem pela janela 5h"));
}

proptest! {
    #[test]
    fn an_earlier_credit_expiry_never_reduces_usable_priority(
        first in 0.1f64..90.0, extra in 0.0f64..300.0, remaining in 0.1f64..100.0,
    ) {
        let mut base = entry();
        base["windows"][1]["remaining_pct"] = json!(remaining);
        let sooner = key(&with_credits(base.clone(), 1, vec![NOW + first * HOUR]));
        let later = key(&with_credits(base, 1, vec![NOW + (first + extra) * HOUR]));
        prop_assert_ne!(sooner.partial_cmp(&later), Some(Ordering::Greater));
    }
}
