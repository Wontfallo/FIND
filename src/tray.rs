//! System tray integration (Windows): FIND keeps running in the tray when the
//! window is closed, exactly like Everything. Left-click the tray icon to
//! bring the window back; right-click for a menu.
//!
//! Important subtlety: while the window is hidden, Windows delivers no paint
//! events, so the egui update loop is asleep and can't process anything. Tray
//! events therefore restore the window DIRECTLY via Win32 (ShowWindow) from
//! the event thread — that revives the update loop, which then handles the
//! queued message normally.
#![cfg(target_os = "windows")]

use crossbeam_channel::Receiver;
use eframe::egui;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
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

/// Restore and focus the main window via Win32, waking the egui loop.
fn force_show(hwnd: &AtomicIsize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    let handle = hwnd.load(Ordering::Relaxed);
    if handle != 0 {
        unsafe {
            ShowWindow(handle as _, SW_SHOW);
            ShowWindow(handle as _, SW_RESTORE);
            SetForegroundWindow(handle as _);
        }
    }
}

pub fn init(ctx: egui::Context, hwnd: Arc<AtomicIsize>) -> Option<Tray> {
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

    // Left-click on the tray icon: bring the window back.
    {
        let tx = tx.clone();
        let ctx = ctx.clone();
        let hwnd = hwnd.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                force_show(&hwnd);
                let _ = tx.send(TrayMsg::Show);
                ctx.request_repaint();
            }
        }));
    }
    // Right-click menu items. Every action first revives the window so the
    // update loop is guaranteed to be running to process it.
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
        force_show(&hwnd);
        let _ = tx.send(msg);
        ctx.request_repaint();
    }));

    Some(Tray { _icon: tray, rx })
}
