//! Formato v1 do cache (`<id>.json`): mesma ordem de chaves do `ai-usage`.

use serde::{Deserialize, Serialize};

/// TTL (s) de uma entrada com janelas.
pub const TTL_WINDOWS_S: i64 = 120;
/// TTL (s) de uma entrada só de saldo.
pub const TTL_BALANCE_S: i64 = 300;
/// TTL (s) de uma entrada com erro.
pub const TTL_ERROR_S: i64 = 300;

/// Janela de cota normalizada.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub remaining_pct: Option<i64>,
    #[serde(default)]
    pub resets_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_s: Option<i64>,
    /// Stable provider classification, absent if unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "kind")]
    pub window_kind: Option<String>,
}

/// Saldo em dinheiro.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub amount: f64,
    pub currency: String,
}

/// Créditos que zeram a cota antes do reset (Codex e Claude): quantos dá para resgatar
/// e quando cada um vence (epoch s, crescente; pode ter menos itens que `available`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetCredits {
    pub available: i64,
    #[serde(default)]
    pub expires: Vec<i64>,
    /// Available credits grouped by their exact unmodified provider expiry text.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "expiry_groups")]
    pub expiry_groups: Option<Vec<ResetExpiryGroup>>,
}

/// One group of available resets sharing the same raw expiry timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetExpiryGroup {
    pub available: i64,
    pub expires_at_raw: String,
}

/// Entrada de cache v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub v: u32,
    pub provider: String,
    #[serde(default)]
    pub source: String,
    pub fetched_at: i64,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub data_at: Option<i64>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub windows: Vec<Window>,
    #[serde(default)]
    pub balance: Option<Balance>,
    /// Extensão do costs (o `ai-usage` não grava): ausente quando o provider não informa.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<ResetCredits>,
    /// Resets semanais (extensão zai): ausente quando o provider não informa; arquivos
    /// v1 antigos simplesmente não têm o campo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub week_reset_credits: Option<ResetCredits>,
}

/// Resultado parcial de um provider; `None` = chave ausente (mantém o anterior),
/// `Some(None)` = `null` explícito (limpa).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Partial {
    pub plan: Option<Option<String>>,
    pub windows: Option<Vec<Window>>,
    pub balance: Option<Option<Balance>>,
    pub reset_credits: Option<Option<ResetCredits>>,
    pub week_reset_credits: Option<Option<ResetCredits>>,
    /// Quando o dado foi observado, se não foi agora (ex.: rollout local antigo).
    pub data_at: Option<i64>,
}

impl Entry {
    /// Defasada se `now - fetched_at >= ttl` (janelas 120 s, saldo 300 s, erro 300 s).
    pub fn is_stale(&self, now: i64) -> bool {
        let ttl = if !self.ok {
            TTL_ERROR_S
        } else if !self.windows.is_empty() {
            TTL_WINDOWS_S
        } else {
            TTL_BALANCE_S
        };
        now - self.fetched_at >= ttl
    }
}
