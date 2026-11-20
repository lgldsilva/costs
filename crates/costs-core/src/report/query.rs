//! Bound, exact report predicates shared by both renderers.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub range: Option<(String, String)>,
    pub provider: Option<String>,
    pub agent: Option<String>,
    pub host: Option<String>,
    pub model: Option<String>,
}

pub fn canonical_provider(value: &str) -> &str {
    match value {
        "zai" => "zhipu",
        "claude" => "anthropic",
        "codex" => "openai",
        other => other,
    }
}

pub fn valid_month(value: &str) -> bool {
    value.len() == 7
        && value.as_bytes()[4] == b'-'
        && value.bytes().enumerate().all(|(i, b)| i == 4 || b.is_ascii_digit())
        && value[5..].parse::<u8>().is_ok_and(|m| (1..=12).contains(&m))
        && &value[..4] != "0000"
}

impl Filters {
    pub(crate) fn predicate(&self, month: Option<&str>) -> (String, Vec<String>) {
        let mut clauses = vec!["1=1".to_owned()];
        let mut values = Vec::new();
        if let Some(month) = month {
            clauses.push("date LIKE ?".into());
            values.push(format!("{month}-%"));
        }
        if let Some((start, end)) = &self.range {
            clauses.push("date >= ? AND date <= ?".into());
            values.extend([start.clone(), end.clone()]);
        }
        for (column, value) in [
            ("provider", self.provider.as_deref().map(canonical_provider)),
            ("agent", self.agent.as_deref()),
            ("host", self.host.as_deref()),
            ("model", self.model.as_deref()),
        ] {
            if let Some(value) = value {
                clauses.push(format!("{column} = ?"));
                values.push(value.to_owned());
            }
        }
        (clauses.join(" AND "), values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_month_without_unicode_slicing_panics() {
        for value in ["2026-01", "2026-12"] {
            assert!(valid_month(value));
        }
        for value in ["2026-00", "2026-13", "2026-1", "0000-01", "2026-01%", "ééé-1", "' OR 1"] {
            assert!(!valid_month(value));
        }
    }
    #[test]
    fn aliases_are_exact() {
        assert_eq!(canonical_provider("codex"), "openai");
        assert_eq!(canonical_provider("zai"), "zhipu");
        assert_eq!(canonical_provider("Claude"), "Claude");
    }
    proptest::proptest! {
        #[test]
        fn hostile_filter_is_bound(value in ".*") {
            let f = Filters { model: Some(value.clone()), ..Default::default() };
            let (sql, params) = f.predicate(None);
            proptest::prop_assert_eq!(sql, "1=1 AND model = ?");
            proptest::prop_assert_eq!(params, vec![value]);
        }
    }
}
