//! Formatação numérica no estilo do Python (`{:,.Nf}`).

/// Número com `decimals` casas e separador de milhar.
pub fn commas(v: f64, decimals: usize) -> String {
    let text = format!("{v:.decimals$}");
    let (sign, digits) = text.strip_prefix('-').map_or(("", text.as_str()), |d| ("-", d));
    let (int, frac) = digits.split_once('.').map_or((digits, None), |(i, f)| (i, Some(f)));
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    match frac {
        Some(f) => format!("{sign}{out}.{f}"),
        None => format!("{sign}{out}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands_and_keeps_sign() {
        assert_eq!(commas(1234567.891, 2), "1,234,567.89");
        assert_eq!(commas(999.0, 0), "999");
        assert_eq!(commas(-1234.5, 1), "-1,234.5");
        assert_eq!(commas(0.0, 2), "0.00");
    }
}
