#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod beatmap;
mod fetch;
mod job;
mod render;
mod replay;
mod settings;
mod sim;

use eframe::egui;

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 860.0])
            .with_min_inner_size([560.0, 500.0])
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "omrdr",
        options,
        Box::new(|_cc| Ok(Box::new(app::App::new()))),
    )
}

fn load_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("could not read assets/icon.png")
        .into_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData { rgba: img.into_raw(), width, height }
}