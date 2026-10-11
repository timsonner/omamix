//! Placeholder mixer window. Launching it is left to Tim; tests cover the
//! fader rule in `omamix-core` and this binary is not started by `cargo test`.

mod ui;

use eframe::egui;
use omamix_core::Scene;
use omamix_pw::{AudioEngine, EngineCommand, NullEngine};

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("omamix")
            .with_inner_size([980.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "omamix",
        native_options,
        Box::new(|_cc| Ok(Box::new(OmamixApp::new()))),
    )
}

struct OmamixApp {
    scene: Scene,
    /// Scaffold engine. It never opens a PipeWire client.
    engine: Box<dyn AudioEngine>,
}

impl OmamixApp {
    fn new() -> Self {
        Self {
            scene: Scene::placeholder(),
            engine: Box::new(NullEngine::new()),
        }
    }
}

impl eframe::App for OmamixApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("omamix");
                ui.label("pre-alpha");
                ui.separator();
                ui.label(format!("engine: {}", self.engine.name()));
                ui.label("virtual mic not published");
            });
            ui.separator();
            ui.label("Click a fader, then scroll while the pointer is still on it.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if let Some(strip) = self.scene.strips.first_mut() {
                    ui::strip_panel(ui, strip);
                }
            });
        });
    }

    fn on_exit(&mut self) {
        // Enqueue only. The audio thread applies Shutdown; this call does not.
        self.engine.command_sender().submit(EngineCommand::Shutdown);
    }
}
