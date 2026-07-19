//! FIND — instant file search for your PC.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod preview;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([700.0, 400.0])
            .with_title("FIND — instant file search"),
        ..Default::default()
    };
    eframe::run_native(
        "FIND",
        options,
        Box::new(|cc| Ok(Box::new(app::FindApp::new(cc)))),
    )
}
