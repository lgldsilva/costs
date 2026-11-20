//! Geração pura das definições de agendamento por SO (launchd, systemd, Task Scheduler).
//! Nada aqui altera o sistema: só devolve texto e caminhos.

use std::path::{Path, PathBuf};
use std::process::Command;

const SECS_PER_HOUR: u32 = 3600;
const EXTRA_PATH: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

/// Sistema operacional alvo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

/// SO em que o binário está rodando.
pub fn current() -> Os {
    if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    }
}

/// Periodicidade de um job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    EveryHours(u32),
    DailyAt { hour: u8, minute: u8 },
}

/// Descrição de um job agendado.
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub label: &'static str,
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub schedule: Schedule,
    pub log_path: PathBuf,
}

/// Dados do host necessários para gerar caminhos e comandos.
#[derive(Debug, Clone)]
pub struct HostEnv {
    pub home: PathBuf,
    pub xdg_config_home: Option<PathBuf>,
    pub user: String,
    pub uid: u32,
}

impl HostEnv {
    /// Lê o ambiente atual (HOME, XDG_CONFIG_HOME, USER, uid via `id -u`).
    pub fn detect() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let home = var("HOME")
            .or_else(|| var("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let user = var("USER")
            .or_else(|| var("USERNAME"))
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        let uid = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        Self {
            home,
            xdg_config_home: var("XDG_CONFIG_HOME").map(PathBuf::from),
            user,
            uid,
        }
    }
}

/// Resultado: arquivos a gravar e comandos (apenas texto) de instalar/remover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub files: Vec<(PathBuf, Vec<u8>)>,
    pub install: Vec<String>,
    pub uninstall: Vec<String>,
}

/// Gera a definição do job para o SO informado.
pub fn definition(os: Os, spec: &JobSpec, env: &HostEnv) -> Definition {
    match os {
        Os::MacOs => macos(spec, env),
        Os::Linux => linux(spec, env),
        Os::Windows => windows(spec, env),
    }
}

/// Escapa `& < > " '` para XML/plist.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn disp(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ---------- macOS ----------

fn macos(spec: &JobSpec, env: &HostEnv) -> Definition {
    let path = env
        .home
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", spec.label));
    let target = format!("gui/{}", env.uid);
    Definition {
        files: vec![(path.clone(), plist(spec, env).into_bytes())],
        install: vec![format!("launchctl bootstrap {target} \"{}\"", disp(&path))],
        uninstall: vec![format!("launchctl bootout {target} \"{}\"", disp(&path))],
    }
}

/// Conteúdo do LaunchAgent.
pub fn plist(spec: &JobSpec, env: &HostEnv) -> String {
    let mut args = vec![disp(&spec.exe)];
    args.extend(spec.args.iter().cloned());
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    let (schedule, run_at_load) = match spec.schedule {
        Schedule::EveryHours(h) => (
            format!(
                "  <key>StartInterval</key>\n  <integer>{}</integer>\n",
                h * SECS_PER_HOUR
            ),
            "  <key>RunAtLoad</key>\n  <true/>\n",
        ),
        Schedule::DailyAt { hour, minute } => (
            format!(
                "  <key>StartCalendarInterval</key>\n  <dict>\n    <key>Hour</key>\n    <integer>{hour}</integer>\n    <key>Minute</key>\n    <integer>{minute}</integer>\n  </dict>\n"
            ),
            "",
        ),
    };
    let mut path_parts = vec![EXTRA_PATH[0].to_string(), disp(&env.home.join(".local/bin"))];
    path_parts.extend(EXTRA_PATH[1..].iter().map(|s| s.to_string()));
    let log = xml_escape(&disp(&spec.log_path));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\x20 <key>Label</key>\n  <string>{label}</string>\n\
\x20 <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n\
{schedule}{run_at_load}\
\x20 <key>ProcessType</key>\n  <string>Background</string>\n\
\x20 <key>LowPriorityIO</key>\n  <true/>\n\
\x20 <key>Nice</key>\n  <integer>10</integer>\n\
\x20 <key>StandardOutPath</key>\n  <string>{log}</string>\n\
\x20 <key>StandardErrorPath</key>\n  <string>{log}</string>\n\
\x20 <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key>\n    <string>{path}</string>\n  </dict>\n\
</dict>\n\
</plist>\n",
        label = xml_escape(spec.label),
        path = xml_escape(&path_parts.join(":")),
    )
}

// ---------- Linux ----------

fn systemd_dir(env: &HostEnv) -> PathBuf {
    env.xdg_config_home
        .clone()
        .unwrap_or_else(|| env.home.join(".config"))
        .join("systemd/user")
}

fn linux(spec: &JobSpec, env: &HostEnv) -> Definition {
    let dir = systemd_dir(env);
    let name = spec.label;
    Definition {
        files: vec![
            (dir.join(format!("{name}.service")), systemd_service(spec).into_bytes()),
            (dir.join(format!("{name}.timer")), systemd_timer(spec).into_bytes()),
        ],
        install: vec![
            "systemctl --user daemon-reload".into(),
            format!("systemctl --user enable --now {name}.timer"),
        ],
        uninstall: vec![
            format!("systemctl --user disable --now {name}.timer"),
            "systemctl --user daemon-reload".into(),
        ],
    }
}

/// Cita um argumento para `ExecStart=` (escapa `\`, `"`, `%` e `$`).
pub fn systemd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Conteúdo de `<label>.service`.
pub fn systemd_service(spec: &JobSpec) -> String {
    let mut parts = vec![systemd_quote(&disp(&spec.exe))];
    parts.extend(spec.args.iter().map(|a| systemd_quote(a)));
    let log = disp(&spec.log_path);
    format!(
        "[Unit]\nDescription={}\n\n[Service]\nType=oneshot\nExecStart={}\n{SYSTEMD_LIMITS}StandardOutput=append:{log}\nStandardError=append:{log}\n",
        spec.label,
        parts.join(" ")
    )
}

/// Teto e prioridade do job: um sync travado não bloqueia os seguintes (o timer só dispara de
/// novo quando o oneshot termina) e a coleta cede disco e CPU ao resto da máquina.
const SYSTEMD_LIMITS: &str = "TimeoutStartSec=15min\nNice=10\nIOSchedulingClass=idle\n";

/// Conteúdo de `<label>.timer`.
pub fn systemd_timer(spec: &JobSpec) -> String {
    let trigger = match spec.schedule {
        Schedule::EveryHours(h) => format!("OnBootSec=1min\nOnUnitActiveSec={h}h\n"),
        Schedule::DailyAt { hour, minute } => {
            format!("OnCalendar=*-*-* {hour:02}:{minute:02}:00\nPersistent=true\n")
        }
    };
    format!(
        "[Unit]\nDescription={} timer\n\n[Timer]\n{trigger}\n[Install]\nWantedBy=timers.target\n",
        spec.label
    )
}

// ---------- Windows ----------

fn windows(spec: &JobSpec, env: &HostEnv) -> Definition {
    let path = env.home.join("AppData/Local/costs").join(format!("{}.xml", spec.label));
    Definition {
        files: vec![(path.clone(), task_xml_utf16(spec, &env.user))],
        install: vec![format!(
            "schtasks /Create /TN \"{}\" /XML \"{}\" /F",
            spec.label,
            disp(&path)
        )],
        uninstall: vec![format!("schtasks /Delete /TN \"{}\" /F", spec.label)],
    }
}

/// Cita um argumento para linha de comando do Windows.
pub fn windows_quote(s: &str) -> String {
    if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || c == '"') {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('"', "\\\""))
}

/// XML do Task Scheduler (texto, declarado como UTF-16).
pub fn task_xml(spec: &JobSpec, user: &str) -> String {
    let trigger = match spec.schedule {
        Schedule::EveryHours(h) => format!(
            "    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <Repetition>\n        <Interval>PT{h}H</Interval>\n        <StopAtDurationEnd>false</StopAtDurationEnd>\n      </Repetition>\n    </LogonTrigger>\n"
        ),
        Schedule::DailyAt { hour, minute } => format!(
            "    <CalendarTrigger>\n      <StartBoundary>2000-01-01T{hour:02}:{minute:02}:00</StartBoundary>\n      <Enabled>true</Enabled>\n      <ScheduleByDay>\n        <DaysInterval>1</DaysInterval>\n      </ScheduleByDay>\n    </CalendarTrigger>\n"
        ),
    };
    let arguments = spec.args.iter().map(|a| windows_quote(a)).collect::<Vec<_>>().join(" ");
    let user = xml_escape(user);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
\x20 <RegistrationInfo>\n    <Description>{label}</Description>\n  </RegistrationInfo>\n\
\x20 <Triggers>\n{trigger}  </Triggers>\n\
\x20 <Principals>\n    <Principal id=\"Author\">\n      <UserId>{user}</UserId>\n      <LogonType>InteractiveToken</LogonType>\n    </Principal>\n  </Principals>\n\
\x20 <Settings>\n    <StartWhenAvailable>true</StartWhenAvailable>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n  </Settings>\n\
\x20 <Actions Context=\"Author\">\n    <Exec>\n      <Command>{exe}</Command>\n      <Arguments>{arguments}</Arguments>\n    </Exec>\n  </Actions>\n\
</Task>\n",
        label = xml_escape(spec.label),
        exe = xml_escape(&disp(&spec.exe)),
        arguments = xml_escape(&arguments),
    )
}

/// XML do Task Scheduler em UTF-16 LE com BOM, pronto para gravar.
pub fn task_xml_utf16(spec: &JobSpec, user: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in task_xml(spec, user).encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HostEnv {
        HostEnv {
            home: PathBuf::from("/home/u"),
            xdg_config_home: None,
            user: "PC\\luiz".into(),
            uid: 501,
        }
    }

    fn sync_spec() -> JobSpec {
        JobSpec {
            label: "com.github.lgldsilva.costs.sync",
            exe: PathBuf::from("/usr/local/bin/costs"),
            args: vec!["sync".into(), "--quiet".into()],
            schedule: Schedule::EveryHours(1),
            log_path: PathBuf::from("/home/u/costs.log"),
        }
    }

    fn backup_spec() -> JobSpec {
        JobSpec {
            label: "com.github.lgldsilva.costs.backup",
            args: vec!["backup".into()],
            schedule: Schedule::DailyAt { hour: 3, minute: 30 },
            ..sync_spec()
        }
    }

    #[test]
    fn current_is_consistent() {
        assert_eq!(current() == Os::MacOs, cfg!(target_os = "macos"));
    }

    #[test]
    fn macos_sync_golden() {
        let d = definition(Os::MacOs, &sync_spec(), &env());
        assert_eq!(
            d.files[0].0,
            PathBuf::from("/home/u/Library/LaunchAgents/com.github.lgldsilva.costs.sync.plist")
        );
        let text = String::from_utf8(d.files[0].1.clone()).unwrap();
        let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>Label</key>
  <string>com.github.lgldsilva.costs.sync</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/costs</string>
    <string>sync</string>
    <string>--quiet</string>
  </array>
  <key>StartInterval</key>
  <integer>3600</integer>
  <key>RunAtLoad</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>LowPriorityIO</key>
  <true/>
  <key>Nice</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>/home/u/costs.log</string>
  <key>StandardErrorPath</key>
  <string>/home/u/costs.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/opt/homebrew/bin:/home/u/.local/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>
</dict>
</plist>
";
        assert_eq!(text, expected);
        assert_eq!(
            d.install,
            vec!["launchctl bootstrap gui/501 \"/home/u/Library/LaunchAgents/com.github.lgldsilva.costs.sync.plist\""]
        );
        assert!(d.uninstall[0].starts_with("launchctl bootout gui/501 "));
    }

    #[test]
    fn macos_backup_uses_calendar_without_run_at_load() {
        let text = plist(&backup_spec(), &env());
        assert!(text.contains(
            "<key>StartCalendarInterval</key>\n  <dict>\n    <key>Hour</key>\n    <integer>3</integer>\n    <key>Minute</key>\n    <integer>30</integer>"
        ));
        assert!(!text.contains("RunAtLoad"));
        assert!(!text.contains("StartInterval"));
    }

    #[test]
    fn linux_sync_golden() {
        let d = definition(Os::Linux, &sync_spec(), &env());
        assert_eq!(
            d.files[0].0,
            PathBuf::from("/home/u/.config/systemd/user/com.github.lgldsilva.costs.sync.service")
        );
        assert_eq!(
            String::from_utf8(d.files[0].1.clone()).unwrap(),
            "[Unit]\nDescription=com.github.lgldsilva.costs.sync\n\n[Service]\nType=oneshot\nExecStart=\"/usr/local/bin/costs\" \"sync\" \"--quiet\"\nTimeoutStartSec=15min\nNice=10\nIOSchedulingClass=idle\nStandardOutput=append:/home/u/costs.log\nStandardError=append:/home/u/costs.log\n"
        );
        assert_eq!(
            String::from_utf8(d.files[1].1.clone()).unwrap(),
            "[Unit]\nDescription=com.github.lgldsilva.costs.sync timer\n\n[Timer]\nOnBootSec=1min\nOnUnitActiveSec=1h\n\n[Install]\nWantedBy=timers.target\n"
        );
        assert_eq!(
            d.install,
            vec![
                "systemctl --user daemon-reload",
                "systemctl --user enable --now com.github.lgldsilva.costs.sync.timer"
            ]
        );
    }

    #[test]
    fn linux_backup_golden_and_xdg() {
        let mut e = env();
        e.xdg_config_home = Some(PathBuf::from("/x/cfg"));
        let d = definition(Os::Linux, &backup_spec(), &e);
        assert_eq!(
            d.files[1].0,
            PathBuf::from("/x/cfg/systemd/user/com.github.lgldsilva.costs.backup.timer")
        );
        assert_eq!(
            String::from_utf8(d.files[1].1.clone()).unwrap(),
            "[Unit]\nDescription=com.github.lgldsilva.costs.backup timer\n\n[Timer]\nOnCalendar=*-*-* 03:30:00\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n"
        );
        assert_eq!(d.uninstall.len(), 2);
    }

    #[test]
    fn windows_sync_golden() {
        let d = definition(Os::Windows, &sync_spec(), &env());
        let bytes = &d.files[0].1;
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let units: Vec<u16> = bytes[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let text = String::from_utf16(&units).unwrap();
        let expected = "<?xml version=\"1.0\" encoding=\"UTF-16\"?>
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">
  <RegistrationInfo>
    <Description>com.github.lgldsilva.costs.sync</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <Repetition>
        <Interval>PT1H</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id=\"Author\">
      <UserId>PC\\luiz</UserId>
      <LogonType>InteractiveToken</LogonType>
    </Principal>
  </Principals>
  <Settings>
    <StartWhenAvailable>true</StartWhenAvailable>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
  </Settings>
  <Actions Context=\"Author\">
    <Exec>
      <Command>/usr/local/bin/costs</Command>
      <Arguments>sync --quiet</Arguments>
    </Exec>
  </Actions>
</Task>
";
        assert_eq!(text, expected);
        assert!(d.install[0].starts_with("schtasks /Create /TN \"com.github.lgldsilva.costs.sync\" /XML \""));
        assert!(d.install[0].ends_with("\" /F"));
        assert_eq!(
            d.uninstall[0],
            "schtasks /Delete /TN \"com.github.lgldsilva.costs.sync\" /F"
        );
    }

    #[test]
    fn windows_backup_is_daily() {
        let text = task_xml(&backup_spec(), "u");
        assert!(text.contains("<StartBoundary>2000-01-01T03:30:00</StartBoundary>"));
        assert!(text.contains("<DaysInterval>1</DaysInterval>"));
        assert!(!text.contains("LogonTrigger"));
    }

    #[test]
    fn escaping() {
        assert_eq!(xml_escape("a&b<c>\"d'"), "a&amp;b&lt;c&gt;&quot;d&apos;");
        assert_eq!(systemd_quote("a \"b\" %h $x \\"), "\"a \\\"b\\\" %%h $$x \\\\\"");
        assert_eq!(windows_quote("plain"), "plain");
        assert_eq!(windows_quote("a b\"c"), "\"a b\\\"c\"");
        assert_eq!(windows_quote(""), "\"\"");
        let mut spec = sync_spec();
        spec.args = vec!["a&b".into()];
        assert!(plist(&spec, &env()).contains("<string>a&amp;b</string>"));
        assert!(task_xml(&spec, "d<u>").contains("<UserId>d&lt;u&gt;</UserId>"));
    }
}
