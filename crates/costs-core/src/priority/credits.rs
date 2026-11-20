//! Reset credit availability at snapshot time, not just at the last API fetch.

use serde_json::Value;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ResetAvailability {
    pub available: i64,
    pub next_expiry: Option<f64>,
}

/// Missing expiry dates remain unknown; known expired credits no longer count.
pub fn reset_availability(entry: &Value, now: f64) -> ResetAvailability {
    credits_availability(entry.get("reset_credits"), now)
}

/// Same computation for any credit field (e.g. `week_reset_credits`).
pub fn credits_availability(credits: Option<&Value>, now: f64) -> ResetAvailability {
    let Some(credits) = credits else {
        return ResetAvailability::default();
    };
    let reported = credits.get("available").and_then(Value::as_i64).unwrap_or(0).max(0);
    let has_raw_expiry_groups = credits.get("expiry_groups").and_then(Value::as_array).is_some();
    // Z.ai supplies `available` from the provider and raw, timezone-naive expiry
    // labels. Do not let cached legacy guessed epochs override that authority.
    let expires: Vec<f64> = if has_raw_expiry_groups {
        Vec::new()
    } else {
        credits
            .get("expires")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_f64)
            .filter(|at| at.is_finite() && *at > 0.0)
            .collect()
    };
    let expired = expires.iter().filter(|at| **at <= now).count();
    let available = reported
        .saturating_sub(i64::try_from(expired).unwrap_or(i64::MAX))
        .max(0);
    let next_expiry = expires.into_iter().filter(|at| *at > now).reduce(f64::min);
    ResetAvailability {
        available,
        next_expiry: next_expiry.filter(|_| available > 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn expiry_reduces_known_credits_but_preserves_unknown_ones() {
        let entry = json!({"reset_credits": {"available": 3, "expires": [900, 1100]}});
        assert_eq!(
            reset_availability(&entry, 1000.0),
            ResetAvailability {
                available: 2,
                next_expiry: Some(1100.0),
            }
        );
        assert_eq!(
            reset_availability(&entry, 1200.0),
            ResetAvailability {
                available: 1,
                next_expiry: None,
            }
        );
    }

    #[test]
    fn provider_availability_and_raw_groups_override_legacy_guessed_epochs() {
        let cached_zai = json!({"reset_credits": {
            "available": 2,
            "expires": [500, 900],
            "expiry_groups": [{"available": 2, "expires_at_raw": "2026-10-28 11:02:31"}]
        }});
        assert_eq!(
            reset_availability(&cached_zai, 1000.0),
            ResetAvailability {
                available: 2,
                next_expiry: None
            }
        );
        let no_normalized_dates = json!({"reset_credits": {
            "available": 2,
            "expiry_groups": [{"available": 2, "expires_at_raw": "2026-10-28 11:02:31"}]
        }});
        assert_eq!(reset_availability(&no_normalized_dates, 9_999_999_999.0).available, 2);
    }

    #[test]
    fn unavailable_or_invalid_credits_have_no_deadline() {
        for entry in [
            json!({}),
            json!({"reset_credits": {"available": -1, "expires": [2000]}}),
            json!({"reset_credits": {"available": 0, "expires": [2000]}}),
            json!({"reset_credits": {"available": 1, "expires": [500, 600]}}),
        ] {
            assert_eq!(reset_availability(&entry, 1000.0), ResetAvailability::default());
        }
        let unknown = json!({"reset_credits": {"available": 1, "expires": [null, 0, -1, "bad"]}});
        assert_eq!(
            reset_availability(&unknown, 1000.0),
            ResetAvailability {
                available: 1,
                next_expiry: None,
            }
        );
    }

    proptest! {
        #[test]
        fn time_never_increases_available_credits(
            count in 0i64..20, dates in proptest::collection::vec(1i64..10000, 0..20),
            now in 0i64..10000, elapsed in 0i64..10000,
        ) {
            let entry = json!({"reset_credits": {"available": count, "expires": dates}});
            let before = reset_availability(&entry, now as f64);
            let after = reset_availability(&entry, (now + elapsed) as f64);
            prop_assert!(0 <= after.available && after.available <= before.available && before.available <= count);
            if let Some(at) = after.next_expiry { prop_assert!(at > (now + elapsed) as f64); }
        }
    }
}
