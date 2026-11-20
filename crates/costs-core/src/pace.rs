//! Ritmo da janela: cor, sobra prevista e linhas de janela.
//!
//! Port of the pace logic. Entradas de quota chegam como `serde_json::Value`.
//! Arredondamento: `bar` usa meio-para-par (como `round` do Python) e a
//! formatação `{:.0}` do Rust também arredonda meio-para-par (como `:.0f`).

use crate::fmt::fmt_reset;
use serde_json::Value;

/// Razão uso/tempo até a qual o ritmo é considerado folgado.
pub const PACE_OK: f64 = 0.85;
/// Sobra mínima (%) para mostrar a dica de uso.
pub const SPARE_HINT: f64 = 25.0;
/// Antes disso (fração decorrida) não há projeção.
const MIN_ELAPSED: f64 = 0.05;
const PROJ_CAP: f64 = 999.0;

/// Rótulos que identificam a janela semanal.
pub const WEEKLY_LABELS: [&str; 4] = ["sem", "semanal", "semana", "weekly"];

/// Cor lógica do ritmo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaceColor {
    Gray,
    Green,
    Orange,
    Red,
}

impl PaceColor {
    /// Nome lógico (`green`, `orange`, `red`, `gray`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Gray => "gray",
            Self::Green => "green",
            Self::Orange => "orange",
            Self::Red => "red",
        }
    }

    /// Gravidade: red 3, orange 2, green 1, gray 0.
    pub fn rank(self) -> u8 {
        match self {
            Self::Gray => 0,
            Self::Green => 1,
            Self::Orange => 2,
            Self::Red => 3,
        }
    }

    /// Parâmetro `color=` do SwiftBar (claro,escuro quando o par importa).
    pub fn menu_color(self) -> &'static str {
        match self {
            Self::Green => "#1B7F3B,#3DDC84",
            Self::Orange => "orange",
            Self::Red => "red",
            Self::Gray => "gray",
        }
    }

    /// Ícone do menu.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Green => "🟢",
            Self::Orange => "🟠",
            Self::Red => "🔴",
            Self::Gray => "⚪",
        }
    }
}

/// Número de um campo, tratando ausente/nulo/zero como "sem valor" (falsy do Python).
fn truthy_num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|n| *n != 0.0)
}

/// Fração da janela já decorrida, ou `None` se faltar `window_s`/`resets_at`.
pub fn elapsed_frac(window: &Value, now: f64) -> Option<f64> {
    let ws = truthy_num(window, "window_s")?;
    let ra = truthy_num(window, "resets_at")?;
    Some((1.0 - (ra - now) / ws).clamp(0.0, 1.0))
}

/// Segundos parado para o ritmo voltar ao saudável (verde): o decorrido alcança
/// `usado / PACE_OK`. `None` se já está saudável ou sem tempo conhecido.
pub fn wait_to_healthy(window: &Value, now: f64) -> Option<f64> {
    let ws = truthy_num(window, "window_s")?;
    let used = 100.0 - window.get("remaining_pct").and_then(Value::as_f64)?;
    let elapsed = elapsed_frac(window, now).filter(|e| *e >= MIN_ELAPSED)?;
    let target = used / 100.0 / PACE_OK;
    if target <= elapsed {
        return None;
    }
    Some((target - elapsed) * ws)
}

/// Verde: dentro do ritmo. Laranja: no limite. Vermelho: vai estourar.
pub fn pace_color(used_pct: f64, elapsed: Option<f64>) -> PaceColor {
    let remaining = 100.0 - used_pct;
    if remaining <= 0.0 {
        return PaceColor::Red;
    }
    let Some(elapsed) = elapsed else {
        return if remaining < 20.0 {
            PaceColor::Red
        } else if remaining < 50.0 {
            PaceColor::Orange
        } else {
            PaceColor::Green
        };
    };
    if elapsed < MIN_ELAPSED {
        return if used_pct <= 50.0 {
            PaceColor::Green
        } else {
            PaceColor::Orange
        };
    }
    let ratio = (used_pct / 100.0) / elapsed;
    if ratio <= PACE_OK {
        PaceColor::Green
    } else if ratio <= 1.0 {
        PaceColor::Orange
    } else {
        PaceColor::Red
    }
}

/// Barra de progresso de `width` células.
pub fn bar(used_pct: f64, width: usize) -> String {
    let filled = (used_pct.clamp(0.0, 100.0) / 100.0 * width as f64).round_ties_even() as usize;
    let filled = filled.min(width);
    "█".repeat(filled) + &"░".repeat(width - filled)
}

/// Janelas que ainda valem: reset no futuro (ou sem reset informado).
pub fn fresh_windows(windows: &[Value], now: f64) -> Vec<&Value> {
    windows
        .iter()
        .filter(|w| truthy_num(w, "resets_at").is_none_or(|ra| ra > now))
        .collect()
}

/// Janelas de uma entrada (lista vazia se ausente).
pub fn entry_windows(entry: &Value) -> &[Value] {
    entry
        .get("windows")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Rótulo da janela como texto (`?` se ausente).
pub fn label_of(w: &Value) -> String {
    match w.get("label") {
        None => "?".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Uma linha de janela do menu.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowLine {
    pub label: String,
    pub text: String,
    pub color: PaceColor,
    pub used: f64,
    pub spare: Option<f64>,
    /// Tempo até o reset, já formatado (`3h36`, `6d19h`).
    pub reset: String,
    /// Epoch do reset, quando informado.
    pub resets_at: Option<f64>,
    /// Segundos parado para o ritmo voltar ao saudável. `None` se já está ok.
    pub wait_s: Option<f64>,
}

/// Uma linha por janela fresca com `remaining_pct`.
///
/// Projeção linear: usado / decorrido. Sobra = 100 - projeção; sem projeção
/// no começo da janela (<5% decorrido).
pub fn window_lines(entry: &Value, now: f64) -> Vec<WindowLine> {
    fresh_windows(entry_windows(entry), now)
        .into_iter()
        .filter_map(|w| window_line(w, now))
        .collect()
}

fn window_line(w: &Value, now: f64) -> Option<WindowLine> {
    let left = w.get("remaining_pct").and_then(Value::as_f64)?;
    let used = 100.0 - left;
    let el = elapsed_frac(w, now);
    let color = pace_color(used, el);
    let proj = el.filter(|e| *e >= MIN_ELAPSED).map(|e| used / e);
    let spare = proj.map(|p| (100.0 - p).max(0.0));
    let reset = fmt_reset(truthy_num(w, "resets_at"), now);
    let resets_at = truthy_num(w, "resets_at");
    let label = label_of(w);
    let time_txt = el.map_or_else(|| "sem tempo".to_owned(), |e| format!("{:.0}% do tempo", e * 100.0));
    let proj_txt = proj.map_or_else(String::new, |p| format!(" → {:.0}%", p.min(PROJ_CAP)));
    let hint = match spare {
        Some(s) if s >= SPARE_HINT && !matches!(color, PaceColor::Red | PaceColor::Orange) => {
            format!(" ⬆ sobra {s:.0}%")
        }
        _ => String::new(),
    };
    let text = format!(
        "{} {label:<4} {used:.0}% usado · {time_txt}{proj_txt}{hint} · ⏱ {reset}",
        color.icon()
    );
    Some(WindowLine {
        label,
        text,
        color,
        used,
        spare,
        reset,
        resets_at,
        wait_s: wait_to_healthy(w, now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pace_thresholds() {
        assert_eq!(pace_color(30.0, Some(0.5)), PaceColor::Green);
        assert_eq!(pace_color(45.0, Some(0.5)), PaceColor::Orange);
        assert_eq!(pace_color(80.0, Some(0.5)), PaceColor::Red);
        assert_eq!(pace_color(100.0, Some(0.9)), PaceColor::Red);
    }

    #[test]
    fn wait_to_healthy_matches_example() {
        // 70% usado com metade do tempo (2h30 de 5h): (0.70/0.85 - 0.50) * 5h.
        let w = json!({"remaining_pct": 30.0, "resets_at": 1000.0 + 9000.0, "window_s": 18000.0});
        let wait = wait_to_healthy(&w, 1000.0).unwrap();
        assert!((wait - 5823.5).abs() < 1.0, "{wait}");
        let ok = json!({"remaining_pct": 70.0, "resets_at": 1000.0 + 9000.0, "window_s": 18000.0});
        assert_eq!(wait_to_healthy(&ok, 1000.0), None);
        let timeless = json!({"remaining_pct": 10.0});
        assert_eq!(wait_to_healthy(&timeless, 1000.0), None);
    }

    #[test]
    fn start_of_window_only_alerts_above_half() {
        assert_eq!(pace_color(10.0, Some(0.01)), PaceColor::Green);
        assert_eq!(pace_color(60.0, Some(0.01)), PaceColor::Orange);
    }

    #[test]
    fn no_window_uses_balance() {
        assert_eq!(pace_color(90.0, None), PaceColor::Red);
        assert_eq!(pace_color(60.0, None), PaceColor::Orange);
        assert_eq!(pace_color(10.0, None), PaceColor::Green);
    }

    #[test]
    fn menu_green_is_luminous_in_dark() {
        let (light, dark) = PaceColor::Green.menu_color().split_once(',').unwrap();
        assert_eq!(light, "#1B7F3B");
        assert_eq!(dark, "#3DDC84");
        assert_ne!(dark.to_lowercase(), "#008000");
        assert_eq!(PaceColor::Red.menu_color(), "red");
        assert_eq!(PaceColor::Orange.menu_color(), "orange");
        assert_eq!(PaceColor::Gray.menu_color(), "gray");
    }

    #[test]
    fn rank_and_icon() {
        assert!(PaceColor::Red.rank() > PaceColor::Orange.rank());
        assert!(PaceColor::Orange.rank() > PaceColor::Green.rank());
        assert!(PaceColor::Green.rank() > PaceColor::Gray.rank());
        assert_eq!(PaceColor::Gray.icon(), "⚪");
        assert_eq!(PaceColor::Gray.name(), "gray");
    }

    #[test]
    fn elapsed_from_resets_at() {
        let now = 1_000_000.0;
        let w = json!({"window_s": 18000, "resets_at": 1_009_000});
        assert!((elapsed_frac(&w, now).unwrap() - 0.5).abs() < 1e-9);
        assert_eq!(elapsed_frac(&json!({"resets_at": 1_000_000}), now), None);
    }

    #[test]
    fn bar_is_proportional() {
        assert_eq!(bar(50.0, 10), "█████░░░░░");
        assert_eq!(bar(0.0, 4), "░░░░");
        assert_eq!(bar(150.0, 4), "████");
        assert_eq!(bar(-5.0, 4), "░░░░");
        assert_eq!(bar(12.5, 4), "░░░░"); // 0.5 -> 0 (banker's)
    }

    #[test]
    fn linear_projection_and_spare() {
        let now = 1_000_000.0;
        let entry = json!({"windows": [{"label": "mês", "remaining_pct": 60,
            "window_s": 100 * 86400, "resets_at": now as i64 + 27 * 86400}]});
        let lines = window_lines(&entry, now);
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert_eq!(l.label, "mês");
        assert!((l.used - 40.0).abs() < 1e-9);
        assert!((l.spare.unwrap() - (100.0 - 40.0 / 0.73)).abs() < 0.5);
        assert!(l.text.contains("⬆ sobra"));
    }

    #[test]
    fn no_projection_at_window_start() {
        let now = 1_000_000.0;
        let entry = json!({"windows": [{"label": "5h", "remaining_pct": 90,
            "window_s": 18000, "resets_at": 1_017_900}]});
        let lines = window_lines(&entry, now);
        assert_eq!(lines[0].spare, None);
        assert!(!lines[0].text.contains("fecha em"));
        assert!(!lines[0].text.contains('→'));
    }

    #[test]
    fn hint_suppressed_when_not_green_and_stale_windows_skipped() {
        let now = 1_000_000.0;
        let entry = json!({"windows": [
            {"label": "a", "remaining_pct": null, "resets_at": 1_010_000},
            {"label": "old", "remaining_pct": 50, "resets_at": 999_000},
            {"label": "x", "remaining_pct": 5, "window_s": 1000, "resets_at": 1_000_500},
            {"label": "nows", "remaining_pct": 50}]});
        let lines = window_lines(&entry, now);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].color, PaceColor::Red);
        assert!(!lines[0].text.contains("sobra"));
        assert!(lines[1].text.contains("sem tempo"));
        assert!(lines[1].text.contains("⏱ -"));
        assert_eq!(lines[1].text, "🟢 nows 50% usado · sem tempo · ⏱ -");
    }

    #[test]
    fn fresh_windows_keeps_no_reset_and_future() {
        let ws = vec![json!({"resets_at": 5}), json!({"resets_at": 50}), json!({})];
        assert_eq!(fresh_windows(&ws, 10.0).len(), 2);
    }
}
