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
            .with_min_inner_size([560.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "omrdr",
        options,
        Box::new(|_cc| Ok(Box::new(app::App::new()))),
    )
}