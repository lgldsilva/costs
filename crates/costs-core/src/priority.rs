//! Ordem de uso das quotas e textos curtos.
//!
//! Porta de `use_priority`, `governing_window` e `quota_items` de
//! Quota priority ordering. Entradas chegam como `serde_json::Value` (JSON v1).

use crate::fmt::{fmt_balance, fmt_reset};
use crate::pace::{WEEKLY_LABELS, entry_windows, fresh_windows, label_of};
use serde_json::Value;

mod credits;
pub use credits::{ResetAvailability, credits_availability, reset_availability};

/// Lower sorts first: tier, negative burn rate, deadline distance, negative reserve count.
pub type PriorityKey = (u8, f64, f64, i64);

const TIER_INSIDE_HORIZON: u8 = 0;
const TIER_BEYOND_HORIZON: u8 = 2;
const TIER_BALANCE: u8 = 3;
const TIER_EXHAUSTED: u8 = 4;
const TIER_NO_DATA: u8 = 5;
const SECS_PER_HOUR: f64 = 3600.0;
const MONTHLY_LABELS: [&str; 5] = ["mês", "mes", "mensal", "month", "monthly"];

fn reset_of(w: &Value) -> Option<f64> {
    w.get("resets_at").and_then(Value::as_f64).filter(|n| *n != 0.0)
}

fn remaining_of(w: &Value) -> Option<f64> {
    w.get("remaining_pct").and_then(Value::as_f64)
}

/// Total allowance: monthly before weekly, otherwise the farthest reset.
/// Auxiliary labels such as `mcp` never replace a named total allowance.
/// Expired windows are ignored; ties preserve input order.
pub fn governing_window(windows: &[Value], now: f64) -> Option<&Value> {
    let fresh = fresh_windows(windows, now);
    for labels in [MONTHLY_LABELS.as_slice(), WEEKLY_LABELS.as_slice()] {
        if let Some(w) = fresh.iter().find(|w| labels.contains(&label_lower(w).as_str())) {
            return Some(w);
        }
    }
    let mut best: Option<(&Value, f64)> = None;
    for w in fresh {
        let key = reset_of(w).unwrap_or(f64::INFINITY);
        if best.is_none_or(|(_, k)| key > k) {
            best = Some((w, key));
        }
    }
    best.map(|(w, _)| w)
}

fn label_lower(w: &Value) -> String {
    match w.get("label") {
        None => String::new(),
        Some(_) => label_of(w).to_lowercase(),
    }
}

fn blocked(windows: &[Value], now: f64) -> bool {
    fresh_windows(windows, now)
        .iter()
        .any(|w| remaining_of(w).is_some_and(|r| r <= 0.0))
}

/// Consume usable quota before its total allowance renews or a reserve reset expires.
/// Credits advance the deadline, not the percentage available; unknown expiry dates
/// never invent urgency. Reserve count only breaks otherwise equal priorities.
/// Any exhausted fresh window still blocks use until reset/redemption is confirmed.
pub fn use_priority(entry: &Value, now: f64, horizon_s: f64) -> PriorityKey {
    // Retained cache data is useful for display, not a confirmed recommendation.
    if entry.get("ok") == Some(&Value::Bool(false)) {
        return (TIER_NO_DATA, 0.0, 0.0, 0);
    }
    let windows = entry_windows(entry);
    if blocked(windows, now) {
        return (TIER_EXHAUSTED, 0.0, 0.0, 0);
    }
    let credits = reset_availability(entry, now);
    if let Some(gov) = governing_window(windows, now)
        && let Some(remaining) = remaining_of(gov).filter(|r| *r > 0.0)
    {
        let deadline = reset_of(gov).into_iter().chain(credits.next_expiry).reduce(f64::min);
        let left = deadline.map_or(f64::INFINITY, |ra| (ra - now).max(0.0));
        let rate = if left.is_infinite() {
            0.0
        } else {
            remaining / left.max(1.0) * SECS_PER_HOUR
        };
        let tier = if left <= horizon_s {
            TIER_INSIDE_HORIZON
        } else {
            TIER_BEYOND_HORIZON
        };
        return (tier, -rate, left, -credits.available);
    }
    non_quota_priority(entry, windows.is_empty())
}

fn non_quota_priority(entry: &Value, no_windows: bool) -> PriorityKey {
    let has_balance = entry
        .get("balance")
        .and_then(|b| b.get("amount"))
        .and_then(Value::as_f64)
        .is_some_and(|a| a > 0.0);
    let tier = if has_balance {
        TIER_BALANCE
    } else if no_windows {
        TIER_NO_DATA
    } else {
        TIER_EXHAUSTED
    };
    (tier, 0.0, 0.0, 0)
}

/// Explain the governing deadline without presenting unredeemed credits as usable quota.
pub fn priority_hint(entry: &Value, now: f64) -> Option<String> {
    if entry.get("ok") == Some(&Value::Bool(false)) {
        return Some("Fora da ordem: atualize a consulta de cota".to_string());
    }
    let windows = entry_windows(entry);
    let credits = reset_availability(entry, now);
    if blocked(windows, now) {
        return (credits.available > 0).then(|| "Esgotado; há reset para resgate manual".to_string());
    }
    let gov = governing_window(windows, now)?;
    if let Some(at) = credits
        .next_expiry
        .filter(|at| *at < reset_of(gov).unwrap_or(f64::INFINITY))
    {
        return Some(format!(
            "Ordem: aproveitar reset extra que vence em {}",
            fmt_reset(Some(at), now)
        ));
    }
    let label = label_lower(gov);
    let basis = if MONTHLY_LABELS.contains(&label.as_str()) {
        "cota mensal".to_string()
    } else if WEEKLY_LABELS.contains(&label.as_str()) {
        "cota semanal".to_string()
    } else {
        format!("janela {}", label_of(gov))
    };
    Some(format!("Ordem pela {basis}"))
}

/// Textos curtos de uma entrada: janelas ou saldo. Vazio se nada.
pub fn quota_items(entry: &Value, now: f64) -> Vec<String> {
    let mut items: Vec<String> = entry_windows(entry)
        .iter()
        .filter_map(|w| {
            let left = w.get("remaining_pct").filter(|v| !v.is_null())?;
            let reset = fmt_reset(reset_of(w), now);
            Some(format!("{} {left}% ↻{reset}", label_of(w)))
        })
        .collect();
    if let Some(b) = entry
        .get("balance")
        .filter(|b| b.as_object().is_some_and(|o| !o.is_empty()))
    {
        items.push(format!("saldo {}", fmt_balance(b)));
    }
    items
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod renewal_tests;
