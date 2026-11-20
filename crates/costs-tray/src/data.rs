//! Leitura dos dados via `costs-core`.

use std::path::PathBuf;

use chrono::{Local, Utc};
use costs_core::config::{self, Config, Paths};
use costs_core::fmt::sync_age;
use costs_core::quota::{self, RealEnv};
use costs_core::report::{GroupBy, report};
use costs_core::store::Store;
use serde_json::Value;

use crate::app::DataSource;
use crate::model::{ModelInput, ProviderRow};

pub struct DbSource {
    paths: Paths,
    cache_dir: PathBuf,
}

impl DbSource {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            cache_dir: quota::default_cache_dir(&RealEnv),
        }
    }

    /// Somente leitura: a bandeja nunca cria schema nem migra o banco.
    fn open(&self) -> Result<Store, String> {
        Store::open_readonly(&self.paths.db).map_err(|e| e.to_string())
    }

    /// Cache de quota dos 7 provedores (sem rede); ausente/ilegível é ignorado.
    fn quotas(&self) -> Vec<Value> {
        quota::PROVIDERS
            .iter()
            .filter_map(|p| quota::read_cached(p, &self.cache_dir))
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect()
    }
}

fn month_now() -> String {
    Local::now().format("%Y-%m").to_string()
}

/// Pares (host, agente) habilitados na config.
fn sync_pairs(cfg: &Config) -> Vec<(String, String)> {
    cfg.hosts
        .iter()
        .filter(|h| h.enabled)
        .flat_map(|h| {
            let agents = if h.agents.is_empty() { &cfg.agents } else { &h.agents };
            agents.iter().map(|a| (h.name.clone(), a.clone()))
        })
        .collect()
}

fn providers_of_month(store: &Store, month: &str) -> Result<Vec<String>, String> {
    let con = store.connection();
    let mut stmt = con
        .prepare("SELECT DISTINCT provider FROM usage WHERE date LIKE ? ORDER BY provider")
        .map_err(|e| e.to_string())?;
    stmt.query_map([format!("{month}%")], |r| r.get::<_, String>(0))
        .and_then(|rows| rows.collect())
        .map_err(|e| e.to_string())
}

impl DataSource for DbSource {
    fn load(&mut self) -> Result<ModelInput, String> {
        let store = self.open()?;
        let cfg = config::load_config_or_default(&self.paths.config).map_err(|e| e.to_string())?;
        let month = month_now();
        let spend = store.month_spend(&month).map_err(|e| e.to_string())?;
        let oldest = store.oldest_ok_sync(&sync_pairs(&cfg)).map_err(|e| e.to_string())?;
        let now = Utc::now().timestamp_millis() as f64 / 1000.0;
        let (age, stale) = sync_age(oldest.as_deref(), now);
        let mut providers = Vec::new();
        for name in providers_of_month(&store, &month)? {
            let (total, rows) = store
                .spend_by_provider(&[name.as_str()], &month)
                .map_err(|e| e.to_string())?;
            providers.push(ProviderRow {
                name,
                spend: total,
                tokens: rows.iter().map(|r| r.2).sum(),
                balance: None,
            });
        }
        providers.sort_by(|a, b| b.spend.total_cmp(&a.spend));
        Ok(ModelInput {
            spend,
            sync_age: age,
            stale,
            providers,
            quotas: self.quotas(),
        })
    }

    fn report(&mut self) -> Result<String, String> {
        let store = self.open()?;
        report(store.connection(), GroupBy::Provider, Some(&month_now())).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_database_directory_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join("costs-tray-no-such-dir").join("x");
        let mut src = DbSource::new(config::paths_in(&dir));
        assert!(src.load().is_err());
        assert!(src.report().is_err());
    }

    #[test]
    fn loads_from_empty_database() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let paths = config::paths_in(&dir);
        Store::open(&paths.db, "", costs_core::store::utc_now()).unwrap();
        let mut src = DbSource::new(paths);
        src.cache_dir = dir.join("cache");
        let input = src.load().unwrap();
        assert_eq!(input.spend, 0.0);
        assert!(input.stale);
        assert!(input.providers.is_empty());
        assert!(src.report().is_ok());
    }

    #[test]
    fn reads_cached_quota_balances() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = config::paths_in(tmp.path());
        Store::open(&paths.db, "", costs_core::store::utc_now()).unwrap();
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let entry = r#"{"v":1,"provider":"deepseek","source":"api","fetched_at":1,"ok":true,"balance":{"amount":3.0,"currency":"USD"}}"#;
        std::fs::write(cache.join("deepseek.json"), entry).unwrap();
        let mut src = DbSource::new(paths);
        src.cache_dir = cache;
        let quotas = src.load().unwrap().quotas;
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0]["provider"], "deepseek");
    }
}
