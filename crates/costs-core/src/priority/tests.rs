use super::*;
use proptest::prelude::*;
use serde_json::json;
use std::cmp::Ordering;

const NOW: f64 = 1_000_000.0;
const H: f64 = 86400.0;

fn win(remaining: f64, left_h: Option<f64>) -> Value {
    json!({"label": "w", "remaining_pct": remaining,
           "resets_at": left_h.map(|h| NOW + h * 3600.0)})
}

fn cmp(a: &PriorityKey, b: &PriorityKey) -> Ordering {
    a.0.cmp(&b.0)
        .then(a.1.total_cmp(&b.1))
        .then(a.2.total_cmp(&b.2))
        .then(a.3.cmp(&b.3))
}

fn prio(e: &Value) -> PriorityKey {
    use_priority(e, NOW, H)
}

#[test]
fn window_inside_horizon_first() {
    let soon = json!({"windows": [win(70.0, Some(2.0))]});
    let late = json!({"windows": [win(90.0, Some(100.0))]});
    assert_eq!(cmp(&prio(&soon), &prio(&late)), Ordering::Less);
}

#[test]
fn rate_per_hour_decides_within_horizon() {
    let xai = json!({"windows": [win(53.0, Some(4.0))]});
    let claude = json!({"windows": [win(27.0, Some(2.4))]});
    assert_eq!(cmp(&prio(&xai), &prio(&claude)), Ordering::Less);
}

#[test]
fn weekly_window_governs_order() {
    let a = json!({"windows": [
        {"label": "5h", "remaining_pct": 100, "resets_at": NOW + 3600.0},
        {"label": "sem", "remaining_pct": 10, "resets_at": NOW + 360000.0}]});
    let b = json!({"windows": [
        {"label": "5h", "remaining_pct": 10, "resets_at": NOW + 3600.0},
        {"label": "sem", "remaining_pct": 90, "resets_at": NOW + 360000.0}]});
    assert_eq!(cmp(&prio(&b), &prio(&a)), Ordering::Less);
}

#[test]
fn weekly_rate_not_capped_by_horizon() {
    let far = json!({"windows": [{"label": "sem", "remaining_pct": 50, "resets_at": NOW + 360000.0}]});
    let key = prio(&far);
    assert_eq!(key.0, 2);
    assert!((-key.1 - 0.5).abs() < 1e-6);
}

#[test]
fn fixed_balance_goes_last_among_quota() {
    let bal = json!({"windows": [], "balance": {"amount": 5.0, "currency": "USD"}});
    let late = json!({"windows": [win(90.0, Some(100.0))]});
    let inside = json!({"windows": [win(10.0, Some(5.0))]});
    let mut v = [&late, &bal, &inside];
    v.sort_by(|a, b| cmp(&prio(a), &prio(b)));
    assert!(std::ptr::eq(v[0], &inside));
    assert!(std::ptr::eq(v[2], &bal));
}

#[test]
fn stale_window_ignored() {
    let stale = json!({"windows": [win(100.0, Some(-40.0))]});
    assert_eq!(prio(&stale).0, 4);
}

#[test]
fn exhausted_and_no_data() {
    assert_eq!(prio(&json!({"windows": [win(0.0, Some(3.0))]})).0, 4);
    assert_eq!(prio(&json!({})), (5, 0.0, 0.0, 0));
}

#[test]
fn window_without_reset_is_beyond_horizon_with_zero_rate() {
    let e = json!({"windows": [win(50.0, None)]});
    let k = prio(&e);
    assert_eq!((k.0, k.1), (2, 0.0));
    assert!(k.2.is_infinite());
}

#[test]
fn governing_picks_farthest_reset_and_first_on_tie() {
    let ws = vec![
        json!({"label": "a", "remaining_pct": 1, "resets_at": 20}),
        json!({"label": "b", "remaining_pct": 1, "resets_at": 30}),
        json!({"label": "c", "remaining_pct": 1, "resets_at": 30}),
    ];
    assert_eq!(governing_window(&ws, 10.0).unwrap()["label"], "b");
    assert!(governing_window(&ws, 100.0).is_none());
    let weekly = vec![
        json!({"label": "x", "resets_at": 99}),
        json!({"label": "Weekly", "resets_at": 11}),
    ];
    assert_eq!(governing_window(&weekly, 10.0).unwrap()["label"], "Weekly");
}

#[test]
fn quota_items_texts() {
    let entry = json!({
        "windows": [
            {"label": "5h", "remaining_pct": 60, "resets_at": 1_007_200},
            {"label": "x", "remaining_pct": null},
            {"remaining_pct": 12.5}],
        "balance": {"amount": 5, "currency": "CNY"}});
    assert_eq!(
        quota_items(&entry, NOW),
        vec!["5h 60% ↻2h00", "? 12.5% ↻-", "saldo ¥ 5.00"]
    );
    assert!(quota_items(&json!({"balance": null}), NOW).is_empty());
}

proptest! {
    #[test]
    fn tuple_invariants(
        remaining in -10.0f64..=100.0,
        left_h in proptest::option::of(-50.0f64..500.0),
        horizon in 0.0f64..1e6,
    ) {
        let e = json!({"windows": [win(remaining, left_h)]});
        let (tier, neg_rate, left, reserves) = use_priority(&e, NOW, horizon);
        prop_assert_eq!(reserves, 0);
        prop_assert!([0u8, 2, 4].contains(&tier));
        prop_assert!(neg_rate <= 0.0 && left >= 0.0);
        if tier == 4 { prop_assert_eq!((neg_rate, left), (0.0, 0.0)); }
        if tier == 0 { prop_assert!(left <= horizon); }
        if tier == 2 { prop_assert!(left > horizon); }
    }

    #[test]
    fn exhausted_always_after_usable(
        remaining in 0.1f64..=100.0,
        left_h in 0.01f64..500.0,
        horizon in 0.0f64..1e6,
    ) {
        let usable = json!({"windows": [win(remaining, Some(left_h))]});
        let dead = json!({"windows": [win(0.0, Some(left_h))]});
        let none = json!({});
        let bal = json!({"balance": {"amount": 1.0}});
        let u = use_priority(&usable, NOW, horizon);
        let (d, n, b) = (
            use_priority(&dead, NOW, horizon),
            use_priority(&none, NOW, horizon),
            use_priority(&bal, NOW, horizon),
        );
        prop_assert_eq!(cmp(&u, &b), Ordering::Less);
        prop_assert_eq!(cmp(&b, &d), Ordering::Less);
        prop_assert_eq!(cmp(&d, &n), Ordering::Less);
    }

    #[test]
    fn higher_rate_sorts_first_within_same_reset(
        r1 in 1.0f64..=100.0,
        r2 in 1.0f64..=100.0,
        left_h in 0.01f64..20.0,
    ) {
        let a = json!({"windows": [win(r1, Some(left_h))]});
        let b = json!({"windows": [win(r2, Some(left_h))]});
        let (ka, kb) = (use_priority(&a, NOW, H), use_priority(&b, NOW, H));
        if r1 > r2 { prop_assert_ne!(cmp(&ka, &kb), Ordering::Greater); }
        if r1 < r2 { prop_assert_ne!(cmp(&ka, &kb), Ordering::Less); }
    }
}
