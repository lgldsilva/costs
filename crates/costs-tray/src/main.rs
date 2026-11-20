//! `costs-tray`: app residente de bandeja (macOS, Windows e Linux/StatusNotifierItem).

mod app;
mod data;
mod icon;
mod model;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod ui_desktop;
#[cfg(target_os = "linux")]
mod ui_linux;

use std::process::ExitCode;
use std::time::Duration;

use costs_core::config::default_paths;

use data::DbSource;

const DEFAULT_INTERVAL_SECS: u64 = 300;
const INTERVAL_FLAG: &str = "--interval-secs";

/// Intervalo do refresh: `--interval-secs N` (N > 0) ou 5 minutos.
fn parse_interval(args: &[String]) -> Result<Duration, String> {
    let Some(pos) = args.iter().position(|a| a == INTERVAL_FLAG) else {
        return Ok(Duration::from_secs(DEFAULT_INTERVAL_SECS));
    };
    let value = args
        .get(pos + 1)
        .ok_or_else(|| format!("{INTERVAL_FLAG} exige um valor"))?;
    match value.parse::<u64>() {
        Ok(n) if n > 0 => Ok(Duration::from_secs(n)),
        _ => Err(format!("{INTERVAL_FLAG}: valor inválido: {value}")),
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
use ui_desktop::run;
#[cfg(target_os = "linux")]
use ui_linux::run;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn run(_: impl app::DataSource, _: Duration) -> Result<(), String> {
    Err("plataforma sem suporte a bandeja".to_owned())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let interval = match parse_interval(&args) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("costs-tray: {e}");
            return ExitCode::from(2);
        }
    };
    match run(DbSource::new(default_paths()), interval) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("costs-tray: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn default_interval_is_five_minutes() {
        assert_eq!(parse_interval(&[]).unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn custom_interval() {
        assert_eq!(
            parse_interval(&args(&["--interval-secs", "30"])).unwrap(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn invalid_intervals_are_rejected() {
        assert!(parse_interval(&args(&["--interval-secs"])).is_err());
        assert!(parse_interval(&args(&["--interval-secs", "0"])).is_err());
        assert!(parse_interval(&args(&["--interval-secs", "x"])).is_err());
    }
}
