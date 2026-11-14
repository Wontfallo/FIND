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

/// Service and installer modes. Handled before anything else: the installer
/// runs (elevated) while the GUI is open, so it must not hit the
/// single-instance check, and the service has no window at all.
#[cfg(target_os = "windows")]
fn run_cli_mode() -> Option<i32> {
    use find_core::service;
    let arg = std::env::args().nth(1)?;
    if arg != "--service" {
        // The release build is a GUI app with no console of its own; borrow
        // the terminal it was started from so messages are visible.
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(u32::MAX); // ATTACH_PARENT_PROCESS
        }
    }
    match arg.as_str() {
        "--service" => {
            service::run_as_service();
            Some(0)
        }
        "--install-service" | "--uninstall-service" => {
            let install = arg == "--install-service";
            let what = if install { "install" } else { "uninstall" };
            // From a normal terminal: ask for elevation (one UAC prompt) and
            // let the elevated copy do the work.
            let result = if service::is_elevated() {
                if install {
                    service::install()
                } else {
                    service::uninstall()
                }
            } else {
                println!("FIND: requesting administrator rights to {what} the service…");
                service::run_elevated(&arg)
            };
            let msg = match &result {
                Ok(()) if install => format!(
                    "FIND service installed and running ({}).",
                    service::installed_exe().display()
                ),
                Ok(()) => "FIND service removed.".to_string(),
                Err(e) => format!("FIND service {what} failed: {e}"),
            };
            println!("{msg}");
            // Also leave a note on disk: an elevated child has no terminal.
            let _ = std::fs::write(std::env::temp_dir().join("find-service-install.log"), &msg);
            Some(if result.is_ok() { 0 } else { 1 })
        }
        // Diagnostics: how fast is a whole-drive index, and through what?
        // Writes a report file (the release build has no console).
        "--index-stats" => {
            let letter = std::env::args()
                .nth(2)
                .and_then(|a| a.chars().next())
                .unwrap_or('C');
            let report = index_stats(letter);
            let out = std::env::temp_dir().join("find-index-stats.txt");
            let _ = std::fs::write(&out, &report);
            println!("{report}");
            Some(0)
        }
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn index_stats(letter: char) -> String {
    use find_core::{index::Index, mft, service};
    use std::fmt::Write;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    let mut r = String::new();
    let access = service::access_for(letter);
    let _ = writeln!(r, "drive {letter}: access = {access:?}, service = {:?}", service::status());
    match service::query_journal(letter) {
        Ok(j) => {
            let _ = writeln!(r, "journal id {:#x}, next usn {}", j.journal_id, j.next_usn);
        }
        Err(e) => {
            let _ = writeln!(r, "journal: {e}");
        }
    }
    let progress = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let t = std::time::Instant::now();
    let mut index = Index::default();
    let root = std::path::PathBuf::from(format!("{letter}:\\"));
    let ok = mft::enumerate_volume(&mut index, &root, &[], &progress, &cancel);
    let secs = t.elapsed().as_secs_f64();
    let files = index.entries.iter().filter(|e| !e.is_dir()).count();
    let dirs = index.entries.len() - files;
    let with_meta = index
        .entries
        .iter()
        .filter(|e| !e.is_dir() && e.modified != 0)
        .count();
    let _ = writeln!(
        r,
        "fast path: {ok}; {files} files, {dirs} folders in {secs:.2}s; {with_meta} files have size/date from the MFT"
    );
    if let Some(v) = index.volumes.first() {
        let _ = writeln!(r, "recorded journal position {} for live updates", v.next_usn);
        match service::read_journal(letter, v.journal_id, v.next_usn) {
            Ok((changes, next)) => {
                let _ = writeln!(r, "journal read ok: {} changes since, next {}", changes.len(), next);
            }
            Err(e) => {
                let _ = writeln!(r, "journal read failed: {e}");
            }
        }
    }
    // A few sample paths so the tree shape is visibly right.
    for (i, e) in index.entries.iter().enumerate().filter(|(_, e)| !e.is_dir()).take(5) {
        let _ = writeln!(r, "  {}  ({} bytes)", index.full_path(i as u32).display(), e.size);
    }
    r
}

fn main() -> eframe::Result {
    #[cfg(target_os = "windows")]
    if let Some(code) = run_cli_mode() {
        std::process::exit(code);
    }
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
