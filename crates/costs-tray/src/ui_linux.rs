//! Bandeja do Linux via StatusNotifierItem (`ksni`), com fallback sem bandeja.

use std::io::stdout;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use ksni::blocking::TrayMethods;
use ksni::menu::{StandardItem, SubMenu};

use crate::app::{Controller, DataSource, LogBackend, TrayBackend};
use crate::icon;
use crate::model::{Action, MenuItem, MenuModel};

struct SniTray {
    model: Option<MenuModel>,
    tx: Sender<Action>,
}

fn convert(item: &MenuItem) -> ksni::MenuItem<SniTray> {
    match item {
        MenuItem::Separator => ksni::MenuItem::Separator,
        MenuItem::Label(text) => StandardItem {
            label: text.clone(),
            enabled: false,
            ..Default::default()
        }
        .into(),
        MenuItem::Action { label, action } => {
            let action = *action;
            StandardItem {
                label: label.clone(),
                activate: Box::new(move |t: &mut SniTray| {
                    let _ = t.tx.send(action);
                }),
                ..Default::default()
            }
            .into()
        }
        MenuItem::Submenu { label, children } => SubMenu {
            label: label.clone(),
            submenu: children
                .iter()
                .map(|c| {
                    StandardItem {
                        label: c.clone(),
                        enabled: false,
                        ..Default::default()
                    }
                    .into()
                })
                .collect(),
            ..Default::default()
        }
        .into(),
    }
}

impl ksni::Tray for SniTray {
    fn id(&self) -> String {
        "costs-tray".into()
    }

    fn title(&self) -> String {
        self.model.as_ref().map_or_else(|| "costs".into(), |m| m.title.clone())
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let text = self.model.as_ref().map_or("-", |m| m.icon_text.as_str());
        vec![ksni::Icon {
            width: icon::SIZE as i32,
            height: icon::SIZE as i32,
            data: icon::to_argb(&icon::render(text)),
        }]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let text = self.model.as_ref().map(|m| m.tooltip.clone()).unwrap_or_default();
        let mut lines = text.lines();
        ksni::ToolTip {
            title: lines.next().unwrap_or_default().to_owned(),
            description: lines.collect::<Vec<_>>().join("\n"),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        self.model
            .as_ref()
            .map_or_else(Vec::new, |m| m.sections.iter().map(convert).collect())
    }
}

struct SniBackend {
    handle: ksni::blocking::Handle<SniTray>,
}

impl TrayBackend for SniBackend {
    fn apply(&mut self, model: &MenuModel) {
        let model = model.clone();
        if self.handle.update(move |t| t.model = Some(model)).is_none() {
            eprintln!("costs-tray: o serviço da bandeja foi encerrado");
        }
    }
}

/// Roda o app. Sem host de bandeja, registra o motivo e segue só com o refresh periódico.
pub fn run(source: impl DataSource, interval: Duration) -> Result<(), String> {
    let (tx, rx) = channel();
    match (SniTray { model: None, tx }).spawn() {
        Ok(handle) => Controller::new(SniBackend { handle }, source, stdout()).run_headless(&rx, interval),
        Err(e) => {
            eprintln!("costs-tray: sem host de bandeja (StatusNotifierItem): {e}; seguindo sem ícone");
            Controller::new(LogBackend, source, stdout()).run_headless(&rx, interval);
        }
    }
    Ok(())
}
