//! Inclusive local-calendar ranges, with an injectable reference date.
use chrono::{Datelike, Duration, NaiveDate};

pub fn parse(value: &str) -> Result<NaiveDate, String> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| format!("invalid date: {value}"))?;
    if date.format("%Y-%m-%d").to_string() != value {
        return Err(format!("invalid date: {value}"));
    }
    Ok(date)
}

fn month_range(month: &str) -> Result<(NaiveDate, NaiveDate), String> {
    if !super::query::valid_month(month) {
        return Err("invalid month: expected YYYY-MM".into());
    }
    let start = parse(&format!("{month}-01"))?;
    let next = if start.month() == 12 {
        NaiveDate::from_ymd_opt(start.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(start.year(), start.month() + 1, 1)
    }
    .ok_or("month out of range")?;
    Ok((start, next - Duration::days(1)))
}

fn custom_range(from: Option<&str>, to: Option<&str>) -> Result<(NaiveDate, NaiveDate), String> {
    Ok((
        parse(from.ok_or("--from and --to are required together")?)?,
        parse(to.ok_or("--from and --to are required together")?)?,
    ))
}

fn period_range(period: Option<&str>, today: NaiveDate) -> Result<Option<(NaiveDate, NaiveDate)>, String> {
    let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    match period {
        Some("last-30-days") => Ok(Some((today - Duration::days(29), today))),
        Some("this-week") => Ok(Some((monday, today))),
        Some("last-week") => Ok(Some((monday - Duration::days(7), monday - Duration::days(1)))),
        Some(_) => Err("invalid period".into()),
        None => Ok(None),
    }
}

fn date_range(
    month: Option<&str>,
    last_days: Option<u32>,
    period: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    today: NaiveDate,
) -> Result<Option<(NaiveDate, NaiveDate)>, String> {
    let custom = from.is_some() || to.is_some();
    if [month.is_some(), last_days.is_some(), period.is_some(), custom]
        .into_iter()
        .filter(|v| *v)
        .count()
        > 1
    {
        return Err("ambiguous date selection".into());
    }
    if let Some(month) = month {
        return month_range(month).map(Some);
    }
    if custom {
        return custom_range(from, to).map(Some);
    }
    if let Some(days) = last_days {
        if days == 0 || days > 365000 {
            return Err("--last-days must be between 1 and 365000".into());
        }
        return Ok(Some((today - Duration::days(i64::from(days) - 1), today)));
    }
    period_range(period, today)
}

pub fn resolve(
    month: Option<&str>,
    last_days: Option<u32>,
    period: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    today: NaiveDate,
) -> Result<Option<(String, String)>, String> {
    date_range(month, last_days, period, from, to, today)?
        .map(|(start, end)| {
            if start > end {
                Err("reversed date range".into())
            } else {
                Ok((start.to_string(), end.to_string()))
            }
        })
        .transpose()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_range_requires_both_endpoints() {
        let today = parse("2024-01-01").unwrap();
        assert_eq!(
            resolve(None, None, None, Some("2024-01-01"), None, today),
            Err("--from and --to are required together".into())
        );
        assert_eq!(
            resolve(None, None, None, None, Some("2024-01-01"), today),
            Err("--from and --to are required together".into())
        );
        // Datas não canônicas são rejeitadas mesmo quando `%Y-%m-%d` aceita ("%m" aceita 1 dígito).
        assert_eq!(parse("2024-1-03").map(|_| ()), Err("invalid date: 2024-1-03".into()));
        assert_eq!(parse("banana").map(|_| ()), Err("invalid date: banana".into()));
    }

    #[test]
    fn boundaries() {
        let today = parse("2024-01-01").unwrap();
        assert_eq!(
            resolve(None, None, Some("last-30-days"), None, None, today)
                .unwrap()
                .unwrap(),
            ("2023-12-03".into(), "2024-01-01".into())
        );
        assert_eq!(
            resolve(None, None, Some("this-week"), None, None, today)
                .unwrap()
                .unwrap(),
            ("2024-01-01".into(), "2024-01-01".into())
        );
        assert_eq!(
            resolve(None, Some(3), None, None, None, today).unwrap().unwrap(),
            ("2023-12-30".into(), "2024-01-01".into())
        );
        assert_eq!(
            resolve(None, None, Some("last-week"), None, None, today)
                .unwrap()
                .unwrap(),
            ("2023-12-25".into(), "2023-12-31".into())
        );
        assert_eq!(
            resolve(Some("2024-02"), None, None, None, None, today)
                .unwrap()
                .unwrap()
                .1,
            "2024-02-29"
        );
        // Dezembro rola para o ano seguinte.
        assert_eq!(
            resolve(Some("2024-12"), None, None, None, None, today)
                .unwrap()
                .unwrap()
                .1,
            "2024-12-31"
        );
        assert!(resolve(None, None, None, Some("2024-03-01"), Some("2024-02-29"), today).is_err());
        assert!(resolve(Some("2024-01"), Some(30), None, None, None, today).is_err());
        assert_eq!(
            resolve(None, Some(0), None, None, None, today),
            Err("--last-days must be between 1 and 365000".into())
        );
        assert_eq!(
            resolve(None, Some(365001), None, None, None, today),
            Err("--last-days must be between 1 and 365000".into())
        );
        assert_eq!(
            resolve(Some("2024-13"), None, None, None, None, today),
            Err("invalid month: expected YYYY-MM".into())
        );
        assert_eq!(
            resolve(None, None, Some("invalid"), None, None, today),
            Err("invalid period".into())
        );
        assert!(parse("2023-02-29").is_err());
    }
}
