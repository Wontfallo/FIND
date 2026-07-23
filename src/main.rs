//! FIND — instant file search for your PC.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod preview;
#[cfg(target_os = "windows")]
mod tray;

fn window_icon() -> eframe::egui::IconData {
    let png = include_bytes!("../assets/icon-256.png");
    match image::load_from_memory(png) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            eframe::egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(_) => eframe::egui::IconData::default(),
    }
}

/// Only one FIND may run: a second launch focuses the existing window and
/// exits, instead of spawning a duplicate scanner + tray icon that would
/// fight over the index file. Returns false if another instance exists.
#[cfg(target_os = "windows")]
fn ensure_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    let name: Vec<u16> = "FIND_instant_file_search_single_instance\0"
        .encode_utf16()
        .collect();
    unsafe {
        // Handle intentionally leaked: the mutex must live as long as the app.
        let _handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return true;
        }
        let title: Vec<u16> = "FIND — instant file search\0".encode_utf16().collect();
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
        false
    }
}

fn main() -> eframe::Result {
    #[cfg(target_os = "windows")]
    if !ensure_single_instance() {
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([700.0, 400.0])
            .with_icon(std::sync::Arc::new(window_icon()))
            .with_title("FIND — instant file search"),
        ..Default::default()
    };
    eframe::run_native(
        "FIND",
        options,
        Box::new(|cc| Ok(Box::new(app::FindApp::new(cc)))),
    )
}
