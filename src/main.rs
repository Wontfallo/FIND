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

fn main() -> eframe::Result {
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
