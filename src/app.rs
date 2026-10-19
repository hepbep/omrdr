use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
};

use eframe::egui;

use crate::{
    job::{self, Prepared},
    settings::Settings,
};

enum Msg {
    Log(String),
    Prepared(Result<Prepared, String>),
    Progress(u64, u64),
    Finished(Result<PathBuf, String>),
}

enum Stage {
    Idle,
    Preparing,
    Ready,
    Rendering,
    Done(PathBuf),
    Failed(String),
}

pub struct App {
    settings: Settings,
    dirty: bool,
    stage: Stage,
    prepared: Option<Arc<Prepared>>,
    replay_path: Option<PathBuf>,
    out_name: String,
    log: Vec<String>,
    progress: (u64, u64),
    cancel: Arc<AtomicBool>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    confirm_clear: bool,
}

impl App {
    pub fn new() -> Self {
        let settings = Settings::load();
        let _ = settings.ensure_dirs();
        let (tx, rx) = mpsc::channel();
        Self {
            settings,
            dirty: false,
            confirm_clear: false,
            stage: Stage::Idle,
            prepared: None,
            replay_path: None,
            out_name: String::new(),
            log: Vec::new(),
            progress: (0, 1),
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
        }
    }

    fn pump(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Log(s) => self.log.push(s),
                Msg::Prepared(Ok(p)) => {
                    self.log.push("ready to render".into());
                    self.prepared = Some(Arc::new(p));
                    self.stage = Stage::Ready;
                }
                Msg::Prepared(Err(e)) => {
                    self.log.push(format!("error: {e}"));
                    self.stage = Stage::Failed(e);
                }
                Msg::Progress(a, b) => self.progress = (a, b),
                Msg::Finished(Ok(p)) => {
                    self.log.push(format!("saved {}", p.display()));
                    self.stage = Stage::Done(p);
                }
                Msg::Finished(Err(e)) => {
                    if e == "cancelled" {
                        self.log.push("render cancelled".into());
                        self.stage = Stage::Ready;
                    } else {
                        self.log.push(format!("error: {e}"));
                        self.stage = Stage::Failed(e);
                    }
                }
            }
        }
    }

    fn busy(&self) -> bool {
        matches!(self.stage, Stage::Preparing | Stage::Rendering)
    }

    fn start_prepare(&mut self, ctx: &egui::Context, path: PathBuf) {
        if self.busy() {
            self.log.push("busy, wait for the current job to finish".into());
            return;
        }
        self.stage = Stage::Preparing;
        self.prepared = None;
        self.log.clear();
        self.replay_path = Some(path.clone());

        let (tx, st, ctx) = (self.tx.clone(), self.settings.clone(), ctx.clone());
        thread::spawn(move || {
            let log = {
                let tx = tx.clone();
                let ctx = ctx.clone();
                move |s: String| {
                    let _ = tx.send(Msg::Log(s));
                    ctx.request_repaint();
                }
            };
            let res = job::prepare(&path, &st, &log).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Prepared(res));
            ctx.request_repaint();
        });
    }

    fn add_osz(&mut self, ctx: &egui::Context, path: &Path) {
        match job::copy_into(path, &self.settings.beatmaps_dir()) {
            Ok(dest) => self.log.push(format!("added {} to the beatmaps folder", dest.display())),
            Err(e) => {
                self.log.push(format!("could not copy the .osz: {e:#}"));
                return;
            }
        }
        let failed = matches!(self.stage, Stage::Failed(_));
        let replay = self.replay_path.clone();
        if failed {
            if let Some(r) = replay {
                self.start_prepare(ctx, r);
            }
        }
    }

    fn start_render(&mut self, ctx: &egui::Context) {
        let Some(p) = self.prepared.clone() else { return };
        let out = job::output_path(&self.settings, &p.replay_path, &self.out_name);
        self.settings.save();
        self.log.push(format!("rendering to {}", out.display()));
        self.cancel.store(false, Ordering::Relaxed);
        self.stage = Stage::Rendering;
        self.progress = (0, 1);

        let (tx, st, ctx, cancel) =
            (self.tx.clone(), self.settings.clone(), ctx.clone(), self.cancel.clone());
        thread::spawn(move || {
            let mut progress = |a: u64, b: u64| {
                let _ = tx.send(Msg::Progress(a, b));
                ctx.request_repaint();
            };
            let res = job::render_job(&p, &st, &out, &cancel, &mut progress)
                .map(|_| out.clone())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Finished(res));
            ctx.request_repaint();
        });
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let files: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect()
        });
        for path in files {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            match ext.as_str() {
                "osr" => self.start_prepare(ctx, path),
                "osz" => self.add_osz(ctx, &path),
                _ => self.log.push(format!(
                    "ignored {}: drop a .osr replay or an .osz beatmap",
                    path.display()
                )),
            }
        }
    }
}

impl eframe::App for App {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        self.pump();
        self.handle_drops(ctx);

        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let busy = self.busy();
        let before = self.settings.clone();
        let (mut want_render, mut want_cancel, mut pick_replay, mut pick_osz) =
            (false, false, false, false);
        let mut want_clear = false;

        egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("omrdr");

                // Drop zone
                let zone = if hovering {
                    ui.visuals().selection.bg_fill
                } else {
                    ui.visuals().faint_bg_color
                };
                egui::Frame::group(ui.style()).fill(zone).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(80.0);
                    ui.vertical_centered(|ui| {
                        ui.add_space(14.0);
                        ui.label("Drop an osu!mania replay (.osr) here");
                        if ui.add_enabled(!busy, egui::Button::new("Browse...")).clicked() {
                            pick_replay = true;
                        }
                    });
                });
                ui.add_space(6.0);

                // Status
                match &self.stage {
                    Stage::Idle => {
                        ui.label("No replay loaded yet.");
                    }
                    Stage::Preparing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Reading the replay and getting the beatmap...");
                        });
                    }
                    Stage::Ready => {
                        ui.label("Ready to render.");
                    }
                    Stage::Rendering => {
                        let (a, b) = self.progress;
                        let frac = if b == 0 { 0.0 } else { a as f32 / b as f32 };
                        ui.add(egui::ProgressBar::new(frac).show_percentage());
                        ui.label(format!("frame {a} of {b}"));
                    }
                    Stage::Done(p) => {
                        ui.label(format!("Saved: {}", p.display()));
                        if ui.button("Open output folder").clicked() {
                            let _ = open::that(self.settings.output_dir());
                        }
                    }
                    Stage::Failed(e) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, e.as_str());
                        if self.prepared.is_none() {
                            ui.label("Drop the matching .osz anywhere on this window, or:");
                            if ui.button("Pick .osz...").clicked() {
                                pick_osz = true;
                            }
                        }
                    }
                }

                // Replay and beatmap info
                if let Some(p) = &self.prepared {
                    ui.add_space(6.0);
                    ui.group(|ui| {
                        ui.set_width(ui.available_width());
                        ui.strong(format!("{} [{}]", p.bm.title, p.bm.version));
                        ui.label(format!(
                            "Played by {}   |   {}K   |   OD {}   |   mods: {}",
                            p.replay.player,
                            p.bm.keys,
                            p.bm.od,
                            job::mods_text(p.replay.mods)
                        ));
                        let r = job::replay_totals(&p.replay);
                        let sm = job::sim_totals(&p.split);
                        ui.label(format!(
                            "Replay: {:.2}%   geki {} | 300 {} | katu {} | 100 {} | 50 {} | miss {}",
                            job::accuracy(r), r[0], r[1], r[2], r[3], r[4], r[5]
                        ));
                        ui.label(format!(
                            "Sim:    {:.2}%   geki {} | 300 {} | katu {} | 100 {} | 50 {} | miss {}",
                            job::accuracy(sm), sm[0], sm[1], sm[2], sm[3], sm[4], sm[5]
                        ));
                        if job::has_difficulty_mods(p.replay.mods) {
                            ui.colored_label(
                                egui::Color32::YELLOW,
                                "EZ/HR/DT/HT change hit windows, which are not applied to the judgement yet.",
                            );
                        }
                    });
                }

                // Output name
                ui.add_space(6.0);
                let default_name = self
                    .replay_path
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label("Output name");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.out_name)
                            .hint_text(default_name)
                            .desired_width(320.0),
                    );
                    ui.label(".mp4");
                });

                ui.horizontal(|ui| {
                    let can_render = self.prepared.is_some() && !busy;
                    if ui.add_enabled(can_render, egui::Button::new("Render video")).clicked() {
                        want_render = true;
                    }
                    if matches!(self.stage, Stage::Rendering) && ui.button("Cancel").clicked() {
                        want_cancel = true;
                    }
                });

                // Settings
                ui.add_space(8.0);
                egui::CollapsingHeader::new("Settings").default_open(true).show(ui, |ui| {
                    let st = &mut self.settings;

                    egui::ComboBox::from_label("Resolution")
                        .selected_text(format!("{}x{}", st.width, st.height))
                        .show_ui(ui, |ui| {
                            for (w, h) in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
                                if ui
                                    .selectable_label(st.width == w && st.height == h, format!("{w}x{h}"))
                                    .clicked()
                                {
                                    st.width = w;
                                    st.height = h;
                                }
                            }
                        });
                    egui::ComboBox::from_label("Frame rate")
                        .selected_text(format!("{} fps", st.fps))
                        .show_ui(ui, |ui| {
                            for f in [30, 60, 120] {
                                if ui.selectable_label(st.fps == f, format!("{f} fps")).clicked() {
                                    st.fps = f;
                                }
                            }
                        });
                    ui.add(
                        egui::Slider::new(&mut st.mania_speed, 1.0..=40.0)
                            .step_by(1.0)
                            .text("Scroll speed"),
                    );
                    ui.small(format!(
                        "Notes take {:.0} ms to travel from the top of the screen to the hit line",
                        11485.0 / st.mania_speed.max(1.0)
                    ));
                    ui.checkbox(&mut st.use_sv, "Follow the map's scroll velocity (SV) changes");
                    ui.add(egui::Slider::new(&mut st.column_width, 40.0..=110.0).text("Column width"));
                    ui.add(egui::Slider::new(&mut st.hit_position, 0.6..=0.95).text("Hit line height"));
                    ui.add(egui::Slider::new(&mut st.music_volume, 0.0..=1.0).text("Music volume"));
                    ui.add(
                        egui::Slider::new(&mut st.quality_crf, 14..=28)
                            .text("Video quality (lower is better, bigger file)"),
                    );
                    ui.checkbox(&mut st.show_combo, "Show combo");
                    ui.checkbox(&mut st.show_accuracy, "Show accuracy");
                    ui.checkbox(&mut st.show_hit_error, "Show hit error bar");
                    ui.checkbox(&mut st.show_progress, "Show progress bar");
                    ui.checkbox(&mut st.show_judgements, "Show judgement counts");

                    ui.separator();
                    ui.label(format!("Data folder: {}", st.data_dir.display()));
                    ui.horizontal(|ui| {
                        if ui.button("Change...").clicked() {
                            if let Some(d) = rfd::FileDialog::new().pick_folder() {
                                st.data_dir = d;
                            }
                        }
                        if ui.button("Replays").clicked() {
                            let _ = open::that(st.replays_dir());
                        }
                        if ui.button("Beatmaps").clicked() {
                            let _ = open::that(st.beatmaps_dir());
                        }
                        if ui.button("Output").clicked() {
                            let _ = open::that(st.output_dir());
                        }
                    });

                    egui::CollapsingHeader::new("osu! API (optional, helps find beatmaps)").show(
                        ui,
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Client ID");
                                ui.text_edit_singleline(&mut st.osu_client_id);
                            });
                            ui.horizontal(|ui| {
                                ui.label("Client secret");
                                ui.add(egui::TextEdit::singleline(&mut st.osu_client_secret).password(true));
                            });
                            
                        },
                    );
                    ui.separator();
                    let red = egui::Color32::from_rgb(180, 40, 40);
                    if !self.confirm_clear {
                        let btn = egui::Button::new(
                            egui::RichText::new("Clear replays and beatmaps").color(egui::Color32::WHITE),
                        )
                        .fill(red);
                        if ui.add_enabled(!busy, btn).clicked() {
                            self.confirm_clear = true;
                        }
                    } else {
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            "Delete every replay and beatmap in the folders? Videos in output are kept.",
                        );
                        ui.horizontal(|ui| {
                            let yes = egui::Button::new(
                                egui::RichText::new("Yes, delete").color(egui::Color32::WHITE),
                            )
                            .fill(red);
                            if ui.add(yes).clicked() {
                                want_clear = true;
                            }
                            if ui.button("Cancel").clicked() {
                                self.confirm_clear = false;
                            }
                        });
                    }
                });

                // Log
                ui.add_space(8.0);
                ui.label("Log");
                egui::ScrollArea::vertical()
                    .id_salt("log")
                    .max_height(160.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.monospace(line);
                        }
                    });
            });
        });

        if self.settings != before {
            self.dirty = true;
            if self.settings.data_dir != before.data_dir {
                let _ = self.settings.ensure_dirs();
            }
        }
        if self.dirty && !ctx.input(|i| i.pointer.any_down()) {
            self.settings.save();
            self.dirty = false;
        }

        if pick_replay && !self.busy() {
            if let Some(p) = rfd::FileDialog::new().add_filter("osu! replay", &["osr"]).pick_file() {
                self.start_prepare(ctx, p);
            }
        }
        if pick_osz {
            if let Some(p) = rfd::FileDialog::new().add_filter("osu! beatmap", &["osz"]).pick_file() {
                self.add_osz(ctx, &p);
            }
        }
        if want_cancel {
            self.cancel.store(true, Ordering::Relaxed);
        }
        if want_render {
            self.start_render(ctx);
        }

                if want_render {
            self.start_render(ctx);
        }
        if want_clear && !self.busy() {
            self.confirm_clear = false;
            let replays = job::clear_dir(&self.settings.replays_dir());
            let maps = job::clear_dir(&self.settings.beatmaps_dir());
            self.prepared = None;
            self.replay_path = None;
            self.stage = Stage::Idle;
            self.log.clear();
            self.log.push(format!("deleted {replays} replay file(s) and {maps} beatmap file(s)"));
        }
    }
}