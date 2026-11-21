#![cfg_attr(windows, windows_subsystem = "windows")]
mod account;
mod app;
mod config;
mod exif;
mod store;
mod sync;
mod telegram;

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]).with_title("Ulap"),
        ..Default::default()
    };
    eframe::run_native("Ulap", opts, Box::new(|cc| {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        Ok(Box::new(app::App::new()))
    }))
}
