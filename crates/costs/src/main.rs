mod app;
mod cli;
mod menubar;
mod quota_cmd;
mod service_cmd;
mod status;
mod util;

use std::io;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;

/// Código de saída de falha em tempo de execução.
const EXIT_FAILURE: u8 = 1;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (mut out, mut err) = (io::stdout().lock(), io::stderr().lock());
    match app::run(&cli, &mut io::stdin().lock(), &mut out, &mut err) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(EXIT_FAILURE)),
        Err(e) => {
            eprintln!("erro: {e}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Command;
    use clap::CommandFactory;
    use std::path::PathBuf;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_global_options_and_subcommand() {
        let cli = Cli::try_parse_from(["costs", "sync", "--db", "x.db", "--config", "c.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Sync { .. }));
        assert_eq!(cli.db, Some(PathBuf::from("x.db")));
        assert_eq!(cli.config, Some(PathBuf::from("c.toml")));
    }

    #[test]
    fn invalid_group_is_a_usage_error() {
        assert!(Cli::try_parse_from(["costs", "report", "--by", "nope"]).is_err());
    }

    #[test]
    fn service_subcommands_parse_and_dry_run_conflicts_with_apply() {
        let cli = Cli::try_parse_from(["costs", "install-service", "--apply"]).unwrap();
        assert!(matches!(cli.command, Command::InstallService { apply: true, .. }));
        let cli = Cli::try_parse_from(["costs", "uninstall-service"]).unwrap();
        assert!(matches!(cli.command, Command::UninstallService { apply: false }));
        assert!(Cli::try_parse_from(["costs", "install-service", "--dry-run", "--apply"]).is_err());
    }

    #[test]
    fn collect_requires_an_agent_and_only_accepts_stdin_context() {
        let cli = Cli::try_parse_from(["costs", "collect", "--agent", "a", "--agent", "b", "--context", "-"]).unwrap();
        let Command::Collect { host, agents, context } = cli.command else {
            panic!("esperava collect");
        };
        assert_eq!(
            (host.as_str(), agents, context.as_deref()),
            ("local", vec!["a".to_string(), "b".to_string()], Some("-"))
        );
        assert!(Cli::try_parse_from(["costs", "collect"]).is_err());
        assert!(Cli::try_parse_from(["costs", "collect", "--agent", "a", "--context", "x.json"]).is_err());
        assert!(Cli::try_parse_from(["costs", "grok-collect"]).is_err());
    }

    #[test]
    fn version_flag_is_available() {
        let e = Cli::try_parse_from(["costs", "--version"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(e.to_string().contains(costs_core::version()));
    }
}
