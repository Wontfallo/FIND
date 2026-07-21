//! System tray integration (Windows): FIND keeps running in the tray when the
//! window is closed, exactly like Everything. Left-click the tray icon to
//! bring the window back; right-click for a menu.
#![cfg(target_os = "windows")]

use crossbeam_channel::Receiver;
use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub enum TrayMsg {
    Show,
    Rescan,
    Quit,
}

pub struct Tray {
    _icon: TrayIcon,
    pub rx: Receiver<TrayMsg>,
}

pub fn init(ctx: egui::Context) -> Option<Tray> {
    let (tx, rx) = crossbeam_channel::unbounded();

    let png = include_bytes!("../assets/icon-256.png");
    let img = image::load_from_memory(png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let icon = tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()?;

    let menu = Menu::new();
    let show_item = MenuItem::new("Show FIND", true, None);
    let rescan_item = MenuItem::new("Rescan index", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append_items(&[&show_item, &rescan_item, &quit_item])
        .ok()?;
    let show_id = show_item.id().clone();
    let rescan_id = rescan_item.id().clone();
    let quit_id = quit_item.id().clone();

    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("FIND — instant file search")
        .with_menu(Box::new(menu))
        .build()
        .ok()?;

    // Handlers run on the event-loop thread; forward into a channel and wake
    // the UI so it processes them even while the window is hidden.
    {
        let tx = tx.clone();
        let ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let _ = tx.send(TrayMsg::Show);
                ctx.request_repaint();
            }
        }));
    }
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let msg = if event.id() == &show_id {
            TrayMsg::Show
        } else if event.id() == &rescan_id {
            TrayMsg::Rescan
        } else if event.id() == &quit_id {
            TrayMsg::Quit
        } else {
            return;
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
    }));

    Some(Tray { _icon: tray, rx })
}
