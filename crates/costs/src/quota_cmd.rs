//! Comando `quota` e leitura das entradas de quota (cache nativo do costs-core).

use std::io::{self, Write};
use std::path::Path;
use std::thread;

use costs_core::fmt::age_text;
use costs_core::priority::quota_items;
use costs_core::quota::{self, Entry, RealEnv, UreqClient};
use serde_json::{Value, json};

const ERROR_MAX_CHARS: usize = 60;
const RULE_WIDTH: usize = 92;

fn to_value(provider: &str, entry: Option<Entry>) -> Value {
    entry
        .and_then(|e| serde_json::to_value(e).ok())
        .unwrap_or_else(|| json!({"provider": provider, "ok": false, "error": "sem cache"}))
}

/// Entrada do cache, sem rede; ausente vira `ok=false`.
pub fn cached(provider: &str, cache_dir: &Path) -> Value {
    to_value(provider, quota::read_cached(provider, cache_dir))
}

/// De onde vêm as entradas de quota.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// Só o cache, sem rede.
    Cache,
    /// Rede só para o que passou do TTL do cache: timers e abertura de UI não martelam as
    /// APIs (várias não documentadas, com as credenciais dos próprios agentes).
    IfStale,
    /// Rede sempre (botão explícito de atualizar).
    Force,
}

impl Fetch {
    pub fn from_flags(refresh: bool, if_stale: bool) -> Self {
        match (refresh, if_stale) {
            (true, _) => Self::Force,
            (false, true) => Self::IfStale,
            (false, false) => Self::Cache,
        }
    }
}

/// Busca na API (em paralelo entre provedores) ou lê o cache; mantém a ordem de `providers`.
pub fn entries(providers: &[String], mode: Fetch, now: i64, cache_dir: &Path) -> Vec<Value> {
    if mode == Fetch::Cache {
        return providers.iter().map(|p| cached(p, cache_dir)).collect();
    }
    let force = mode == Fetch::Force;
    thread::scope(|s| {
        let handles: Vec<_> = providers
            .iter()
            .map(|p| s.spawn(move || quota::fetch(p, now, &UreqClient, &RealEnv, force, cache_dir)))
            .collect();
        providers
            .iter()
            .zip(handles)
            .map(|(p, h)| to_value(p, h.join().ok()))
            .collect()
    })
}

fn status(entry: &Value, has_items: bool) -> String {
    if entry.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return "ok".to_owned();
    }
    let error: String = entry
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(ERROR_MAX_CHARS)
        .collect();
    let stale = if entry.get("ok") == Some(&Value::Bool(false)) && has_items {
        " (mostrando último valor)"
    } else {
        ""
    };
    format!("erro: {}{stale}", costs_core::fmt::quota_error_text(&error))
}

/// Tabela do `costs quota`.
pub fn render(out: &mut dyn Write, providers: &[String], entries: &[Value], now: f64) -> io::Result<()> {
    writeln!(
        out,
        "{:<14} {:<50} {:<12} status",
        "provedor", "janela/saldo", "atualizado"
    )?;
    writeln!(out, "{}", "-".repeat(RULE_WIDTH))?;
    for (provider, entry) in providers.iter().zip(entries) {
        let items = quota_items(entry, now);
        let text = if items.is_empty() {
            "-".to_owned()
        } else {
            items.join(" · ")
        };
        writeln!(
            out,
            "{provider:<14} {text:<50} {:<12} {}",
            age_text(entry, now),
            status(entry, !items.is_empty())
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_cache_is_an_error_row() {
        let dir = tempfile::tempdir().unwrap();
        let providers = vec!["deepseek".to_owned()];
        let entries = entries(&providers, Fetch::Cache, 0, dir.path());
        let mut out = Vec::new();
        render(&mut out, &providers, &entries, 1000.0).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("provedor"));
        assert!(text.contains("deepseek"));
        assert!(text.contains("erro: sem cache"));
    }

    #[test]
    fn cached_balance_shows_ok_and_value() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"v":1,"provider":"deepseek","source":"api","fetched_at":900,"ok":true,"balance":{"amount":3.0,"currency":"USD"}}"#;
        std::fs::write(dir.path().join("deepseek.json"), body).unwrap();
        let providers = vec!["deepseek".to_owned()];
        let mut out = Vec::new();
        render(
            &mut out,
            &providers,
            &entries(&providers, Fetch::Cache, 0, dir.path()),
            1000.0,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("saldo"), "{text}");
        assert!(text.trim_end().ends_with("ok"));
    }

    #[test]
    fn failed_entry_with_old_value_says_so() {
        let entry = json!({"ok": false, "error": "http", "balance": {"amount": 1.0, "currency": "USD"}});
        assert_eq!(status(&entry, true), "erro: falha de rede/API (mostrando último valor)");
        assert_eq!(status(&entry, false), "erro: falha de rede/API");
        assert_eq!(
            status(&json!({"ok": false, "error": "expired"}), false),
            "erro: login expirado"
        );
    }

    #[test]
    fn flags_viram_modo_e_refresh_e_if_stale_se_excluem() {
        assert_eq!(Fetch::from_flags(false, false), Fetch::Cache);
        assert_eq!(Fetch::from_flags(false, true), Fetch::IfStale);
        assert_eq!(Fetch::from_flags(true, false), Fetch::Force);
        use clap::Parser;
        let parse =
            |args: &[&str]| crate::cli::Cli::try_parse_from(std::iter::once("costs").chain(args.iter().copied()));
        assert!(parse(&["quota", "--if-stale"]).is_ok());
        assert!(
            parse(&["quota", "--refresh", "--if-stale"]).is_err(),
            "modos excludentes"
        );
    }
}
