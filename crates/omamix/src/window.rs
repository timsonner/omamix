use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use omamix_core::graph::Scene;
use omamix_pw::{AudioEngine, EngineCommand, NullEngine};

use crate::picker::{self, EditResult, EffectCommand, Picker, PickerChoice, SCENE_SAVE_DEBOUNCE};
use crate::ui::{self, CompEdit, EqEdit, StripMeters};

pub struct WindowHost {
    picker: Picker,
    scene_path: Option<PathBuf>,
    audio: Audio,
    strips: Vec<(u64, StripMeters)>,
    strip_gr: Vec<(u64, f32)>,
    strip_slots: Vec<(u64, u16)>,
    master: StripMeters,
    master_gr: f32,
    sample_rate: u32,
    quantum: u32,
    last_frame: Option<Instant>,
    save_note: String,
    retarget_note: String,
}

enum Audio {
    #[cfg(feature = "pipewire")]
    Live(omamix_pw::WindowEngine),
    Idle {
        engine: Box<NullEngine>,
        detail: String,
    },
}

struct LevelSnap {
    master_peak: f32,
    master_rms: f32,
    strips: Vec<(u64, f32, f32)>,
    strip_gr: Vec<(u64, f32)>,
    strip_slots: Vec<(u64, u16)>,
    master_gr: f32,
    sample_rate: u32,
    quantum: u32,
    catalog: Vec<omamix_core::route::ListedNode>,
    default_source: Option<String>,
    catalog_ready: bool,
}

impl WindowHost {
    pub fn new(scene: Scene, scene_path: Option<PathBuf>) -> Self {
        let audio = open_audio(scene.clone());
        Self {
            picker: Picker::from_scene(scene),
            scene_path,
            audio,
            strips: Vec::new(),
            strip_gr: Vec::new(),
            strip_slots: Vec::new(),
            master: StripMeters::silent(),
            master_gr: 0.0,
            sample_rate: 0,
            quantum: 0,
            last_frame: None,
            save_note: String::new(),
            retarget_note: String::new(),
        }
    }
}

fn open_audio(scene: Scene) -> Audio {
    open_audio_with(scene)
}

#[cfg(feature = "pipewire")]
fn open_audio_with(scene: Scene) -> Audio {
    match omamix_pw::backend::open_window_engine(scene) {
        Ok(engine) => Audio::Live(engine),
        Err(err) => {
            eprintln!("omamix: {err}");
            Audio::Idle {
                engine: Box::new(NullEngine::new()),
                detail: format!("PipeWire did not start: {err}"),
            }
        }
    }
}

#[cfg(not(feature = "pipewire"))]
fn open_audio_with(scene: Scene) -> Audio {
    let _ = scene;
    Audio::Idle {
        engine: Box::new(NullEngine::new()),
        detail: "built without PipeWire; virtual mic not published".to_string(),
    }
}

impl WindowHost {
    fn sync_levels(&mut self, dt: f32) {
        let levels = self.audio.levels();
        if levels.catalog_ready {
            self.picker.refresh(levels.catalog, levels.default_source);
            if self.picker.retarget_missing_once() {
                self.retarget_note =
                    "a saved device is not on this machine; that strip uses the default source"
                        .to_string();
                self.audio.submit_scene(self.picker.scene());
            }
        }
        let ids: Vec<u64> = self
            .picker
            .scene()
            .strips
            .iter()
            .map(|strip| strip.id.0)
            .collect();
        self.strips.retain(|(id, _)| ids.contains(id));
        for id in &ids {
            if !self.strips.iter().any(|(have, _)| have == id) {
                self.strips.push((*id, StripMeters::silent()));
            }
        }
        self.master
            .peak
            .update(levels.master_peak, levels.master_rms, dt);
        self.master.rms_db = levels.master_rms;
        self.master_gr = levels.master_gr;
        self.sample_rate = levels.sample_rate;
        self.quantum = levels.quantum;
        self.strip_gr = levels.strip_gr;
        self.strip_slots = levels.strip_slots;
        for (id, display) in &mut self.strips {
            let (peak, rms) = levels
                .strips
                .iter()
                .find(|(level_id, _, _)| level_id == id)
                .map(|(_, peak, rms)| (*peak, *rms))
                .unwrap_or((f32::NEG_INFINITY, f32::NEG_INFINITY));
            display.peak.update(peak, rms, dt);
            display.rms_db = rms;
        }
    }

    fn reduction_for(&self, id: u64) -> f32 {
        self.strip_gr
            .iter()
            .find(|(have, _)| *have == id)
            .map(|(_, gr)| *gr)
            .unwrap_or(0.0)
    }

    fn slot_for(&self, id: u64) -> Option<u16> {
        self.strip_slots
            .iter()
            .find(|(have, _)| *have == id)
            .map(|(_, slot)| *slot)
    }

    fn rate(&self) -> u32 {
        if self.sample_rate == 0 {
            48_000
        } else {
            self.sample_rate
        }
    }

    fn keep(result: EditResult, commands: &mut Vec<EffectCommand>, changed: &mut bool) {
        match result {
            EditResult::Unchanged => {}
            EditResult::Scene => *changed = true,
            EditResult::Command(command) => {
                *changed = true;
                commands.push(command);
            }
        }
    }

    fn note_eq(
        &mut self,
        id: Option<u64>,
        edit: &EqEdit,
        now: Instant,
        commands: &mut Vec<EffectCommand>,
        changed: &mut bool,
    ) {
        if edit.reset {
            let result = match id {
                Some(id) => self.picker.reset_strip_eq(id, now),
                None => self.picker.reset_master_eq(now),
            };
            Self::keep(result, commands, changed);
            return;
        }
        Self::keep(
            match id {
                Some(id) => self.picker.set_strip_eq_enabled(id, edit.enabled, now),
                None => self.picker.set_master_eq_enabled(edit.enabled, now),
            },
            commands,
            changed,
        );
        Self::keep(
            match id {
                Some(id) => self.picker.set_strip_hpf_enabled(id, edit.hpf_enabled, now),
                None => self.picker.set_master_hpf_enabled(edit.hpf_enabled, now),
            },
            commands,
            changed,
        );
        Self::keep(
            match id {
                Some(id) => self.picker.set_strip_hpf_hz(id, edit.hpf_hz, now),
                None => self.picker.set_master_hpf_hz(edit.hpf_hz, now),
            },
            commands,
            changed,
        );
        for index in 0..edit.bands.len() {
            Self::keep(
                match id {
                    Some(id) => self
                        .picker
                        .set_strip_band(id, index, edit.bands[index], now),
                    None => self.picker.set_master_band(index, edit.bands[index], now),
                },
                commands,
                changed,
            );
        }
    }

    fn note_comp(
        &mut self,
        id: Option<u64>,
        edit: &CompEdit,
        now: Instant,
        commands: &mut Vec<EffectCommand>,
        changed: &mut bool,
    ) {
        Self::keep(
            match id {
                Some(id) => self.picker.set_strip_comp_enabled(id, edit.enabled, now),
                None => self.picker.set_master_comp_enabled(edit.enabled, now),
            },
            commands,
            changed,
        );
        Self::keep(
            match id {
                Some(id) => self
                    .picker
                    .set_strip_comp_threshold(id, edit.threshold_db, now),
                None => self
                    .picker
                    .set_master_comp_threshold(edit.threshold_db, now),
            },
            commands,
            changed,
        );
        Self::keep(
            match id {
                Some(id) => self.picker.set_strip_comp_ratio(id, edit.ratio, now),
                None => self.picker.set_master_comp_ratio(edit.ratio, now),
            },
            commands,
            changed,
        );
        Self::keep(
            match id {
                Some(id) => self.picker.set_strip_comp_attack(id, edit.attack_ms, now),
                None => self.picker.set_master_comp_attack(edit.attack_ms, now),
            },
            commands,
            changed,
        );
        Self::keep(
            match id {
                Some(id) => self.picker.set_strip_comp_release(id, edit.release_ms, now),
                None => self.picker.set_master_comp_release(edit.release_ms, now),
            },
            commands,
            changed,
        );
        Self::keep(
            match id {
                Some(id) => self.picker.set_strip_comp_makeup(id, edit.makeup_db, now),
                None => self.picker.set_master_comp_makeup(edit.makeup_db, now),
            },
            commands,
            changed,
        );
    }

    fn note_chain(
        &mut self,
        edit: &ui::ChainEdit,
        now: Instant,
        commands: &mut Vec<EffectCommand>,
        changed: &mut bool,
    ) {
        for (index, bypass) in edit.bypass.iter().enumerate() {
            Self::keep(
                self.picker.set_chain_bypass(index, *bypass, now),
                commands,
                changed,
            );
        }
        if let Some(index) = edit.move_up {
            Self::keep(
                self.picker.move_chain_block(index, -1, now),
                commands,
                changed,
            );
        }
        if let Some(index) = edit.move_down {
            Self::keep(
                self.picker.move_chain_block(index, 1, now),
                commands,
                changed,
            );
        }
    }

    fn submit_effect(&self, command: EffectCommand) {
        let engine_command = match command {
            EffectCommand::StripEq { id, update } => {
                let Some(slot) = self.slot_for(id) else {
                    return;
                };
                EngineCommand::SetStripEq { slot, update }
            }
            EffectCommand::MasterEq(update) => EngineCommand::SetMasterEq(update),
            EffectCommand::StripComp { id, update } => {
                let Some(slot) = self.slot_for(id) else {
                    return;
                };
                EngineCommand::SetStripComp { slot, update }
            }
            EffectCommand::MasterComp(update) => EngineCommand::SetMasterComp(update),
            EffectCommand::Chain(chain) => EngineCommand::SetMasterChain(chain),
        };
        self.audio.submit(engine_command);
    }

    fn meters_for(&self, id: u64) -> StripMeters {
        self.strips
            .iter()
            .find(|(have, _)| *have == id)
            .map(|(_, meters)| meters.clone())
            .unwrap_or_else(StripMeters::silent)
    }

    fn push_scene(&mut self) {
        self.audio.submit_scene(self.picker.scene());
    }

    fn save_if_due(&mut self, now: Instant) {
        if !self.picker.save_due(now, SCENE_SAVE_DEBOUNCE) {
            return;
        }
        self.save_now();
    }

    fn save_now(&mut self) {
        if !self.picker.is_dirty() {
            return;
        }
        let Some(path) = &self.scene_path else {
            self.save_note = "scene path is unset; changes stay in this window".to_string();
            return;
        };
        match omamix_core::save_scene_file(path, self.picker.scene()) {
            Ok(()) => {
                self.picker.mark_saved();
                self.save_note = format!("saved {}", path.display());
            }
            Err(err) => {
                self.save_note = format!("save failed: {err}");
            }
        }
    }
}

impl Audio {
    fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "pipewire")]
            Audio::Live(engine) => engine.name(),
            Audio::Idle { engine, .. } => engine.name(),
        }
    }

    fn detail(&self) -> String {
        match self {
            #[cfg(feature = "pipewire")]
            Audio::Live(engine) => {
                if let Some(err) = engine.failure() {
                    format!("PipeWire stopped: {err}")
                } else if engine.published() {
                    "Mix Mic published".to_string()
                } else {
                    "Mix Mic not published yet".to_string()
                }
            }
            Audio::Idle { detail, .. } => detail.clone(),
        }
    }

    fn levels(&self) -> LevelSnap {
        match self {
            #[cfg(feature = "pipewire")]
            Audio::Live(engine) => {
                let snap = engine.snapshot();
                LevelSnap {
                    master_peak: snap.master_peak,
                    master_rms: snap.master_rms,
                    strips: snap.strips,
                    strip_gr: snap.strip_gr,
                    strip_slots: snap.strip_slots,
                    master_gr: snap.master_gr,
                    sample_rate: snap.sample_rate,
                    quantum: snap.quantum,
                    catalog: snap.catalog,
                    default_source: snap.default_source,
                    catalog_ready: snap.catalog_ready,
                }
            }
            Audio::Idle { .. } => LevelSnap {
                master_peak: f32::NEG_INFINITY,
                master_rms: f32::NEG_INFINITY,
                strips: Vec::new(),
                strip_gr: Vec::new(),
                strip_slots: Vec::new(),
                master_gr: 0.0,
                sample_rate: 0,
                quantum: 0,
                catalog: Vec::new(),
                default_source: None,
                catalog_ready: false,
            },
        }
    }

    fn submit_scene(&self, scene: &Scene) {
        match self {
            #[cfg(feature = "pipewire")]
            Audio::Live(engine) => engine.submit_scene(scene),
            Audio::Idle { engine, .. } => {
                engine
                    .command_sender()
                    .submit(EngineCommand::ReplaceScene(Box::new(scene.clone())));
            }
        }
    }

    fn submit(&self, command: EngineCommand) {
        match self {
            #[cfg(feature = "pipewire")]
            Audio::Live(engine) => engine.command_sender().submit(command),
            Audio::Idle { engine, .. } => engine.command_sender().submit(command),
        }
    }

    fn shutdown(&mut self) {
        match self {
            #[cfg(feature = "pipewire")]
            Audio::Live(engine) => engine.shutdown(),
            Audio::Idle { engine, .. } => {
                engine.command_sender().submit(EngineCommand::Shutdown);
            }
        }
    }
}

impl eframe::App for WindowHost {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let dt = self
            .last_frame
            .map(|then| now.saturating_duration_since(then).as_secs_f32())
            .unwrap_or(0.0);
        self.last_frame = Some(now);
        self.sync_levels(dt);
        self.save_if_due(now);
        let mut changed = false;
        let mut commands = Vec::new();
        let sample_rate = self.rate();
        let latency_ms = self.picker.latency_ms(self.sample_rate, self.quantum);
        let master_view = ui::MasterView {
            fader_db: self.picker.scene().master.fader_db(),
            meters: &self.master,
            eq_enabled: self.picker.scene().master.eq_enabled,
            eq: self.picker.scene().master.eq,
            comp: self.picker.scene().master.comp,
            chain: &self.picker.scene().master.chain,
            reduction_db: self.master_gr,
            latency_ms,
            sample_rate,
        };
        let master_edit = egui::Panel::right("omamix-master")
            .resizable(false)
            .exact_size(ui::MASTER_SIDE_WIDTH)
            .show(ui, |ui| ui::master_panel(ui, master_view));
        if self
            .picker
            .set_master_fader(master_edit.inner.fader_db, now)
        {
            changed = true;
        }
        self.note_eq(
            None,
            &master_edit.inner.eq,
            now,
            &mut commands,
            &mut changed,
        );
        self.note_comp(
            None,
            &master_edit.inner.comp,
            now,
            &mut commands,
            &mut changed,
        );
        self.note_chain(&master_edit.inner.chain, now, &mut commands, &mut changed);
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("omamix");
                ui.label(format!("engine: {}", self.audio.name()));
                ui.separator();
                ui.label(self.audio.detail());
            });
            if !self.retarget_note.is_empty() {
                ui.label(&self.retarget_note);
            }
            if !self.save_note.is_empty() {
                ui.label(&self.save_note);
            }
            ui.separator();
            ui.label("Click a fader, then scroll while the pointer is still on it.");
            ui.horizontal(|ui| {
                if ui.button("Refresh").clicked() {
                    let nodes = self.picker.catalog().to_vec();
                    let default_source = self.picker.default_source().map(str::to_string);
                    self.picker.refresh(nodes, default_source);
                }
                let selected = self.picker.selected();
                let labels: Vec<String> = self
                    .picker
                    .catalog()
                    .iter()
                    .map(picker::choice_label)
                    .collect();
                let caption = match selected {
                    PickerChoice::Default => "Default source".to_string(),
                    PickerChoice::Listed(index) => labels
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| "Default source".to_string()),
                };
                egui::ComboBox::from_id_salt("source-picker")
                    .selected_text(caption)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(selected == PickerChoice::Default, "Default source")
                            .clicked()
                        {
                            self.picker.select(PickerChoice::Default);
                        }
                        for (index, label) in labels.iter().enumerate() {
                            if ui
                                .selectable_label(
                                    selected == PickerChoice::Listed(index),
                                    label.as_str(),
                                )
                                .clicked()
                            {
                                self.picker.select(PickerChoice::Listed(index));
                            }
                        }
                    });
                if ui.button("Add strip").clicked() && self.picker.add(self.picker.selected(), now)
                {
                    changed = true;
                }
            });
            ui.add_space(8.0);
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut remove_id = None;
                    let strip_count = self.picker.scene().strips.len();
                    for index in 0..strip_count {
                        let Some(strip) = self.picker.scene().strips.get(index).cloned() else {
                            continue;
                        };
                        let id = strip.id.0;
                        let meters = self.meters_for(id);
                        let edit = ui::strip_panel(
                            ui,
                            &strip,
                            &meters,
                            self.reduction_for(id),
                            self.rate(),
                        );
                        if edit.remove {
                            remove_id = Some(id);
                        }
                        if self.picker.set_fader(id, edit.fader_db, now) {
                            changed = true;
                        }
                        if self.picker.set_trim(id, edit.trim_db, now) {
                            changed = true;
                        }
                        if self.picker.set_mute(id, edit.mute, now) {
                            changed = true;
                        }
                        if self.picker.set_solo(id, edit.solo, now) {
                            changed = true;
                        }
                        self.note_eq(Some(id), &edit.eq, now, &mut commands, &mut changed);
                        self.note_comp(Some(id), &edit.comp, now, &mut commands, &mut changed);
                    }
                    if let Some(id) = remove_id
                        && self.picker.remove(id, now)
                    {
                        changed = true;
                    }
                });
            });
        });
        if changed {
            for command in commands {
                self.submit_effect(command);
            }
            self.push_scene();
        }
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }

    fn on_exit(&mut self) {
        self.save_now();
        self.audio.shutdown();
    }
}
