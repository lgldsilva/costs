//! `install-service` / `uninstall-service`: gera as definições de agendamento do SO.
//! Nunca executa launchctl/systemctl/schtasks: apenas grava arquivos (com `--apply`) e
//! imprime os comandos para o usuário rodar.

use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use costs_core::service::{Definition, HostEnv, JobSpec, Os, Schedule, definition};

const SYNC_LABEL: &str = "com.github.lgldsilva.costs.sync";
const BACKUP_LABEL: &str = "com.github.lgldsilva.costs.backup";
const SYNC_EVERY_HOURS: u32 = 1;
const BACKUP_HOUR: u8 = 3;
const BACKUP_MINUTE: u8 = 30;

/// Os dois jobs padrão; os logs ficam em `log_dir`.
pub fn jobs(exe: &Path, log_dir: &Path) -> Vec<JobSpec> {
    let spec = |label: &'static str, arg: &str, schedule, log: &str| JobSpec {
        label,
        exe: exe.to_path_buf(),
        args: vec![arg.to_string()],
        schedule,
        log_path: log_dir.join(log),
    };
    vec![
        spec(SYNC_LABEL, "sync", Schedule::EveryHours(SYNC_EVERY_HOURS), "sync.log"),
        spec(
            BACKUP_LABEL,
            "backup",
            Schedule::DailyAt {
                hour: BACKUP_HOUR,
                minute: BACKUP_MINUTE,
            },
            "backup.log",
        ),
    ]
}

fn definitions(os: Os, specs: &[JobSpec], env: &HostEnv) -> Vec<Definition> {
    specs.iter().map(|s| definition(os, s, env)).collect()
}

fn write_files(defs: &[Definition], log_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(log_dir)?;
    for (path, bytes) in defs.iter().flat_map(|d| &d.files) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
    }
    Ok(())
}

/// Parâmetros comuns dos dois subcomandos.
pub struct Request {
    pub os: Os,
    pub env: HostEnv,
    pub exe: PathBuf,
    pub log_dir: PathBuf,
    pub apply: bool,
}

/// `install-service`: sem `apply` só mostra o plano.
pub fn install(r: &Request, out: &mut dyn Write) -> Result<i32, Box<dyn Error>> {
    let defs = definitions(r.os, &jobs(&r.exe, &r.log_dir), &r.env);
    let verb = if r.apply {
        "gravando"
    } else {
        "dry-run, nada gravado; use --apply"
    };
    writeln!(out, "# install-service ({verb})")?;
    for (path, _) in defs.iter().flat_map(|d| &d.files) {
        writeln!(out, "arquivo: {}", path.display())?;
    }
    if r.apply {
        write_files(&defs, &r.log_dir)?;
    }
    writeln!(out, "# rode você mesmo:")?;
    for c in defs.iter().flat_map(|d| &d.install) {
        writeln!(out, "{c}")?;
    }
    Ok(0)
}

/// `uninstall-service`: sem `apply` só imprime; com `apply` remove os arquivos gerados.
pub fn uninstall(r: &Request, out: &mut dyn Write) -> Result<i32, Box<dyn Error>> {
    let defs = definitions(r.os, &jobs(&r.exe, &r.log_dir), &r.env);
    let verb = if r.apply {
        "removendo"
    } else {
        "dry-run, nada removido; use --apply"
    };
    writeln!(out, "# uninstall-service ({verb})")?;
    for (path, _) in defs.iter().flat_map(|d| &d.files) {
        writeln!(out, "arquivo: {}", path.display())?;
        if r.apply {
            match fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
    }
    writeln!(out, "# rode você mesmo:")?;
    for c in defs.iter().flat_map(|d| &d.uninstall) {
        writeln!(out, "{c}")?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use costs_core::service::Os;

    fn env(home: &Path) -> HostEnv {
        HostEnv {
            home: home.to_path_buf(),
            xdg_config_home: None,
            user: "u".into(),
            uid: 501,
        }
    }

    fn req(os: Os, e: &HostEnv, apply: bool) -> Request {
        Request {
            os,
            env: e.clone(),
            exe: PathBuf::from("/bin/costs"),
            log_dir: e.home.join("logs"),
            apply,
        }
    }

    type Action = fn(&Request, &mut dyn Write) -> Result<i32, Box<dyn Error>>;

    fn run(f: Action, r: &Request) -> String {
        let mut out = Vec::new();
        assert_eq!(f(r, &mut out).unwrap(), 0);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn jobs_have_expected_schedule_and_args() {
        let j = jobs(Path::new("/x"), Path::new("/l"));
        assert_eq!(
            (j[0].label, j[0].args.as_slice()),
            (SYNC_LABEL, ["sync".to_string()].as_slice())
        );
        assert_eq!(j[0].schedule, Schedule::EveryHours(1));
        assert_eq!(j[1].schedule, Schedule::DailyAt { hour: 3, minute: 30 });
        assert_eq!(j[1].log_path, Path::new("/l/backup.log"));
    }

    #[test]
    fn dry_run_writes_nothing_on_every_os() {
        for os in [Os::MacOs, Os::Linux, Os::Windows] {
            let home = tempfile::tempdir().unwrap();
            let e = env(home.path());
            let text = run(install, &req(os, &e, false));
            assert!(text.contains("dry-run") && text.contains("arquivo:"), "{text}");
            assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0, "{os:?}");
        }
    }

    #[test]
    fn apply_writes_files_and_uninstall_removes_them() {
        let home = tempfile::tempdir().unwrap();
        let e = env(home.path());
        let text = run(install, &req(Os::MacOs, &e, true));
        let plist = home
            .path()
            .join("Library/LaunchAgents/com.github.lgldsilva.costs.sync.plist");
        assert!(plist.exists() && home.path().join("logs").is_dir());
        assert!(text.contains("launchctl"), "{text}");

        let dry = run(uninstall, &req(Os::MacOs, &e, false));
        assert!(dry.contains("launchctl") && plist.exists(), "{dry}");
        run(uninstall, &req(Os::MacOs, &e, true));
        assert!(!plist.exists());
        run(uninstall, &req(Os::MacOs, &e, true)); // idempotente
    }
}
