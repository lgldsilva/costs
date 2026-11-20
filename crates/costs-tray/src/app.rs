//! Lógica do app residente, independente de toolkit: testável sem display.

use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use crate::model::{Action, MenuModel, ModelInput, build, error_model};

/// Superfície de bandeja: aplica o modelo atual (título, ícone, tooltip, menu).
pub trait TrayBackend {
    fn apply(&mut self, model: &MenuModel);
}

/// Origem dos dados (banco em modo leitura no app real).
pub trait DataSource {
    fn load(&mut self) -> Result<ModelInput, String>;
    fn report(&mut self) -> Result<String, String>;
}

pub struct Controller<B, D, W> {
    backend: B,
    source: D,
    out: W,
}

impl<B: TrayBackend, D: DataSource, W: Write> Controller<B, D, W> {
    pub fn new(backend: B, source: D, out: W) -> Self {
        Self { backend, source, out }
    }

    /// Relê os dados e atualiza a bandeja; falha vira modelo de erro.
    pub fn refresh(&mut self) {
        let model = match self.source.load() {
            Ok(input) => build(&input),
            Err(e) => {
                eprintln!("costs-tray: falha ao ler dados: {e}");
                error_model(&e)
            }
        };
        self.backend.apply(&model);
    }

    /// Trata uma ação; `false` pede o encerramento.
    pub fn handle(&mut self, action: Action) -> bool {
        match action {
            Action::Refresh => self.refresh(),
            Action::Report => self.print_report(),
            Action::Quit => return false,
        }
        true
    }

    // Primeira versão: o relatório vai para stdout.
    fn print_report(&mut self) {
        match self.source.report() {
            Ok(text) => {
                if let Err(e) = writeln!(self.out, "{text}") {
                    eprintln!("costs-tray: falha ao escrever o relatório: {e}");
                }
            }
            Err(e) => eprintln!("costs-tray: falha ao gerar o relatório: {e}"),
        }
    }

    /// Laço sem toolkit: ações da fila e refresh periódico, até `Quit` ou fila fechada.
    #[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
    pub fn run_headless(&mut self, rx: &Receiver<Action>, interval: Duration) {
        self.refresh();
        loop {
            let keep_going = match rx.recv_timeout(interval) {
                Ok(action) => self.handle(action),
                Err(RecvTimeoutError::Timeout) => self.handle(Action::Refresh),
                Err(RecvTimeoutError::Disconnected) => false,
            };
            if !keep_going {
                break;
            }
        }
    }
}

/// Backend de fallback: sem bandeja disponível, apenas registra o título no stderr.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
pub struct LogBackend;

impl TrayBackend for LogBackend {
    fn apply(&mut self, model: &MenuModel) {
        eprintln!("costs-tray: {}", model.title);
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    pub struct Recorder(pub Arc<Mutex<Vec<MenuModel>>>);

    impl TrayBackend for Recorder {
        fn apply(&mut self, model: &MenuModel) {
            self.0.lock().unwrap().push(model.clone());
        }
    }

    struct Fake {
        fail: bool,
    }

    impl DataSource for Fake {
        fn load(&mut self) -> Result<ModelInput, String> {
            if self.fail {
                return Err("boom".into());
            }
            Ok(ModelInput {
                spend: 3.0,
                sync_age: "agora".into(),
                ..Default::default()
            })
        }

        fn report(&mut self) -> Result<String, String> {
            if self.fail {
                Err("sem relatório".into())
            } else {
                Ok("RELATORIO".into())
            }
        }
    }

    fn controller(fail: bool) -> (Controller<Recorder, Fake, Vec<u8>>, Recorder) {
        let rec = Recorder::default();
        (Controller::new(rec.clone(), Fake { fail }, Vec::new()), rec)
    }

    #[test]
    fn refresh_applies_built_model() {
        let (mut c, rec) = controller(false);
        assert!(c.handle(Action::Refresh));
        assert_eq!(rec.0.lock().unwrap()[0].icon_text, "3");
    }

    #[test]
    fn refresh_failure_applies_error_model() {
        let (mut c, rec) = controller(true);
        c.refresh();
        assert!(rec.0.lock().unwrap()[0].title.starts_with("⚠"));
    }

    #[test]
    fn report_goes_to_output_and_errors_are_swallowed() {
        let (mut c, _) = controller(false);
        assert!(c.handle(Action::Report));
        assert_eq!(String::from_utf8(c.out.clone()).unwrap(), "RELATORIO\n");
        let (mut bad, _) = controller(true);
        assert!(bad.handle(Action::Report));
        assert!(bad.out.is_empty());
    }

    #[test]
    fn quit_stops() {
        let (mut c, _) = controller(false);
        assert!(!c.handle(Action::Quit));
    }

    #[test]
    fn headless_refreshes_on_timeout_and_stops_on_quit() {
        let (mut c, rec) = controller(false);
        let (tx, rx) = mpsc::channel();
        tx.send(Action::Refresh).unwrap();
        tx.send(Action::Quit).unwrap();
        c.run_headless(&rx, Duration::from_millis(5));
        assert_eq!(rec.0.lock().unwrap().len(), 2);

        let (mut c, rec) = controller(false);
        let (tx, rx) = mpsc::channel::<Action>();
        // Só desconecta depois de observar o refresh inicial + um por timeout (sem depender de relógio).
        let seen = rec.clone();
        let t = std::thread::spawn(move || {
            while seen.0.lock().unwrap().len() < 2 {
                std::thread::yield_now();
            }
            drop(tx);
        });
        c.run_headless(&rx, Duration::from_millis(10));
        t.join().unwrap();
        assert!(rec.0.lock().unwrap().len() >= 2);
    }

    #[test]
    fn log_backend_does_not_panic() {
        LogBackend.apply(&crate::model::error_model("x"));
    }
}
