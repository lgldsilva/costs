//! Bandeja de macOS (barra de menus) e Windows (área de notificação) via `tray-icon`.

use std::io::stdout;
use std::time::Duration;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{IsMenuItem, Menu, MenuEvent, MenuItem as TrayItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::app::{Controller, DataSource, TrayBackend};
use crate::icon;
use crate::model::{Action, MenuItem, MenuModel};

struct DesktopBackend {
    tray: TrayIcon,
}

fn build_menu(sections: &[MenuItem]) -> Result<Menu, tray_icon::menu::Error> {
    let menu = Menu::new();
    for section in sections {
        match section {
            MenuItem::Separator => menu.append(&PredefinedMenuItem::separator())?,
            MenuItem::Label(text) => menu.append(&TrayItem::new(text, false, None))?,
            MenuItem::Action { label, action } => {
                menu.append(&TrayItem::with_id(action.id(), label, true, None))?;
            }
            MenuItem::Submenu { label, children } => {
                let items: Vec<TrayItem> = children.iter().map(|c| TrayItem::new(c, false, None)).collect();
                let refs: Vec<&dyn IsMenuItem> = items.iter().map(|i| i as &dyn IsMenuItem).collect();
                menu.append(&Submenu::with_items(label, true, &refs)?)?;
            }
        }
    }
    Ok(menu)
}

impl TrayBackend for DesktopBackend {
    fn apply(&mut self, model: &MenuModel) {
        // macOS mostra texto na barra; o Windows só mostra o ícone.
        #[cfg(target_os = "macos")]
        self.tray.set_title(Some(model.title.as_str()));
        #[cfg(target_os = "windows")]
        match Icon::from_rgba(icon::render(&model.icon_text), icon::SIZE, icon::SIZE) {
            Ok(i) => {
                if let Err(e) = self.tray.set_icon(Some(i)) {
                    eprintln!("costs-tray: falha ao definir o ícone: {e}");
                }
            }
            Err(e) => eprintln!("costs-tray: ícone inválido: {e}"),
        }
        if let Err(e) = self.tray.set_tooltip(Some(model.tooltip.as_str())) {
            eprintln!("costs-tray: falha ao definir o tooltip: {e}");
        }
        match build_menu(&model.sections) {
            Ok(menu) => self.tray.set_menu(Some(Box::new(menu))),
            Err(e) => eprintln!("costs-tray: falha ao montar o menu: {e}"),
        }
    }
}

fn create_tray() -> Result<TrayIcon, String> {
    let icon = Icon::from_rgba(icon::render("-"), icon::SIZE, icon::SIZE).map_err(|e| e.to_string())?;
    TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("costs")
        .build()
        .map_err(|e| e.to_string())
}

/// Roda o laço de eventos (bloqueia até "Sair").
pub fn run(source: impl DataSource + 'static, interval: Duration) -> Result<(), String> {
    let mut builder = EventLoopBuilder::<Action>::with_user_event();
    let event_loop = builder.build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        let mut event_loop = event_loop;
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
        drive(event_loop, source, interval)
    }
    #[cfg(not(target_os = "macos"))]
    drive(event_loop, source, interval)
}

fn drive(
    event_loop: tao::event_loop::EventLoop<Action>,
    source: impl DataSource + 'static,
    interval: Duration,
) -> Result<(), String> {
    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some(action) = Action::from_id(e.id.as_ref()) {
            let _ = menu_proxy.send_event(action);
        }
    }));
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);
            if proxy.send_event(Action::Refresh).is_err() {
                break;
            }
        }
    });

    let mut source = Some(source);
    let mut controller: Option<Controller<DesktopBackend, _, _>> = None;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // O ícone precisa ser criado depois que o laço de eventos inicia.
            Event::NewEvents(StartCause::Init) => match (create_tray(), source.take()) {
                (Ok(tray), Some(src)) => {
                    let mut c = Controller::new(DesktopBackend { tray }, src, stdout());
                    c.refresh();
                    controller = Some(c);
                }
                (Err(e), _) => {
                    eprintln!("costs-tray: bandeja indisponível: {e}");
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            Event::UserEvent(action) => {
                if let Some(c) = controller.as_mut()
                    && !c.handle(action)
                {
                    controller = None;
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    })
}
