//! Modelo puro do menu: sem dependência de toolkit de bandeja.

use costs_core::fmt::fmt_balance;
use serde_json::Value;

/// Ação disparada por um item do menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Refresh,
    Report,
    Quit,
}

/// Identificadores usados só pelo backend de macOS/Windows (`ui_desktop`).
#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Action {
    /// Identificador estável usado pelos toolkits para reportar o clique.
    pub fn id(self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::Report => "report",
            Self::Quit => "quit",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Self::Refresh, Self::Report, Self::Quit]
            .into_iter()
            .find(|a| a.id() == id)
    }
}

/// Linha de provedor: gasto do mês e saldo (quando a quota informa).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRow {
    pub name: String,
    pub spend: f64,
    pub tokens: i64,
    pub balance: Option<String>,
}

/// Entradas do modelo.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInput {
    pub spend: f64,
    pub sync_age: String,
    pub stale: bool,
    pub providers: Vec<ProviderRow>,
    /// Entradas de quota (`{"provider": .., "balance": {currency, amount}}`).
    pub quotas: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuItem {
    /// Texto sem ação.
    Label(String),
    Separator,
    Action {
        label: String,
        action: Action,
    },
    /// Provedor com submenu de detalhes.
    Submenu {
        label: String,
        children: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuModel {
    /// Texto da barra de menus (macOS).
    pub title: String,
    /// Texto curto desenhado no ícone (Windows/Linux).
    pub icon_text: String,
    pub tooltip: String,
    pub sections: Vec<MenuItem>,
}

/// US$ com duas casas.
pub fn money(v: f64) -> String {
    format!("US$ {v:.2}")
}

/// Texto do ícone: inteiro em dólares, `1.2k` a partir de mil.
pub fn icon_text(spend: f64) -> String {
    if spend >= 1000.0 {
        format!("{:.1}k", spend / 1000.0)
    } else {
        format!("{spend:.0}")
    }
}

fn merge_quotas(rows: &[ProviderRow], quotas: &[Value]) -> Vec<ProviderRow> {
    let mut out = rows.to_vec();
    for q in quotas {
        let (Some(name), Some(balance)) = (q.get("provider").and_then(Value::as_str), q.get("balance")) else {
            continue;
        };
        if !balance.is_object() {
            continue;
        }
        let text = fmt_balance(balance);
        match out.iter_mut().find(|r| r.name == name) {
            Some(row) => row.balance = Some(text),
            None => out.push(ProviderRow {
                name: name.to_owned(),
                spend: 0.0,
                tokens: 0,
                balance: Some(text),
            }),
        }
    }
    out
}

fn provider_item(row: &ProviderRow) -> MenuItem {
    let mut label = format!("{}: {}", row.name, money(row.spend));
    let mut children = vec![
        format!("Gasto no mês: {}", money(row.spend)),
        format!("Tokens: {}", row.tokens),
    ];
    if let Some(b) = &row.balance {
        label.push_str(&format!(" · saldo {b}"));
        children.push(format!("Saldo: {b}"));
    }
    MenuItem::Submenu { label, children }
}

fn action(label: &str, action: Action) -> MenuItem {
    MenuItem::Action {
        label: label.to_owned(),
        action,
    }
}

/// Monta o modelo do menu a partir das entradas.
pub fn build(input: &ModelInput) -> MenuModel {
    let warn = if input.stale { "⚠ " } else { "" };
    let title = format!("{warn}≈ {} · {}", money(input.spend), input.sync_age);
    let rows = merge_quotas(&input.providers, &input.quotas);
    let mut sections = vec![
        MenuItem::Label(title.clone()),
        MenuItem::Separator,
        MenuItem::Label(format!("{warn}Coletado {}", input.sync_age)),
    ];
    sections.extend(rows.iter().map(provider_item));
    sections.extend([
        MenuItem::Separator,
        action("Atualizar", Action::Refresh),
        action("Relatório do mês", Action::Report),
        action("Sair", Action::Quit),
    ]);
    let tooltip = std::iter::once(title.clone())
        .chain(rows.iter().map(|r| match &r.balance {
            Some(b) => format!("{}: {} (saldo {b})", r.name, money(r.spend)),
            None => format!("{}: {}", r.name, money(r.spend)),
        }))
        .collect::<Vec<_>>()
        .join("\n");
    MenuModel {
        title,
        icon_text: icon_text(input.spend),
        tooltip,
        sections,
    }
}

/// Modelo exibido quando a leitura falha.
pub fn error_model(message: &str) -> MenuModel {
    MenuModel {
        title: "⚠ costs".to_owned(),
        icon_text: "-".to_owned(),
        tooltip: format!("Erro: {message}"),
        sections: vec![
            MenuItem::Label(format!("Erro: {message}")),
            MenuItem::Separator,
            action("Atualizar", Action::Refresh),
            action("Sair", Action::Quit),
        ],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn input() -> ModelInput {
        ModelInput {
            spend: 12.345,
            sync_age: "5m".into(),
            stale: false,
            providers: vec![
                ProviderRow {
                    name: "anthropic".into(),
                    spend: 10.0,
                    tokens: 1500,
                    balance: None,
                },
                ProviderRow {
                    name: "openai".into(),
                    spend: 2.345,
                    tokens: 20,
                    balance: None,
                },
            ],
            quotas: vec![],
        }
    }

    #[test]
    fn builds_title_and_structure() {
        let m = build(&input());
        assert_eq!(m.title, "≈ US$ 12.35 · 5m");
        assert_eq!(m.icon_text, "12");
        assert_eq!(m.sections.len(), 9);
        assert_eq!(m.sections[1], MenuItem::Separator);
        assert_eq!(m.sections[2], MenuItem::Label("Coletado 5m".into()));
        assert!(matches!(&m.sections[3], MenuItem::Submenu { label, .. } if label == "anthropic: US$ 10.00"));
        assert_eq!(m.sections[8], action("Sair", Action::Quit));
        assert!(m.tooltip.contains("openai: US$ 2.35"));
    }

    #[test]
    fn stale_flag_adds_warning() {
        let mut i = input();
        i.stale = true;
        let m = build(&i);
        assert!(m.title.starts_with("⚠ "));
        assert_eq!(m.sections[2], MenuItem::Label("⚠ Coletado 5m".into()));
    }

    #[test]
    fn quota_balance_merges_and_appends() {
        let mut i = input();
        i.quotas = vec![
            json!({"provider": "openai", "balance": {"currency": "USD", "amount": 5.5}}),
            json!({"provider": "deepseek", "balance": {"currency": "CNY", "amount": 3.0}}),
            json!({"provider": "x"}),
            json!({"provider": "y", "balance": 3}),
            json!({"balance": {"amount": 1}}),
        ];
        let m = build(&i);
        let labels: Vec<_> = m
            .sections
            .iter()
            .filter_map(|s| match s {
                MenuItem::Submenu { label, children } => Some((label.clone(), children.len())),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 3);
        assert!(labels[1].0.contains("saldo"));
        assert_eq!(labels[1].1, 3);
        assert!(labels[2].0.starts_with("deepseek: US$ 0.00"));
        assert!(m.tooltip.contains("(saldo"));
    }

    #[test]
    fn icon_text_abbreviates_thousands() {
        assert_eq!(icon_text(0.4), "0");
        assert_eq!(icon_text(999.4), "999");
        assert_eq!(icon_text(1234.0), "1.2k");
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn action_ids_roundtrip() {
        for a in [Action::Refresh, Action::Report, Action::Quit] {
            assert_eq!(Action::from_id(a.id()), Some(a));
        }
        assert_eq!(Action::from_id("nope"), None);
    }

    #[test]
    fn error_model_has_refresh_and_quit() {
        let m = error_model("db");
        assert_eq!(m.icon_text, "-");
        assert!(m.tooltip.contains("db"));
        assert_eq!(m.sections.len(), 4);
    }
}
