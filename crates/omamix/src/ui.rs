use eframe::egui;
use omamix_core::Strip;
use omamix_core::comp::{
    COMP_ATTACK_MAX_MS, COMP_ATTACK_MIN_MS, COMP_MAKEUP_MAX_DB, COMP_MAKEUP_MIN_DB, COMP_RATIO_MAX,
    COMP_RATIO_MIN, COMP_RELEASE_MAX_MS, COMP_RELEASE_MIN_MS, COMP_THRESHOLD_MAX_DB,
    COMP_THRESHOLD_MIN_DB, CompSettings,
};
use omamix_core::db::format_db;
use omamix_core::effect::{MasterChain, reorder_move_enabled};
use omamix_core::eq::{
    EQ_BAND_HZ, EQ_BANDS, EQ_GAIN_MAX_DB, EQ_GAIN_MIN_DB, EqSettings, HPF_MAX_HZ, HPF_MIN_HZ,
};
use omamix_core::fader::{self, FADER_MAX_DB, FADER_MIN_DB};
use omamix_core::graph::{INPUT_GAIN_MAX_DB, INPUT_GAIN_MIN_DB};
use omamix_core::meter::{METER_BAR_FLOOR_DB, MeterDisplay};
use omamix_core::route::SourceKind;

/// Minimum width of one strip column in the mixer row.
pub const STRIP_PANEL_MIN_WIDTH: f32 = 168.0;
/// Fixed width of the master side panel, wide enough for every EQ band.
pub const MASTER_SIDE_WIDTH: f32 = 480.0;
/// Default window width: master panel plus one strip and layout margin.
pub const WINDOW_MIN_WIDTH: f32 = MASTER_SIDE_WIDTH + STRIP_PANEL_MIN_WIDTH + 32.0;

#[derive(Clone)]
pub struct StripMeters {
    pub peak: MeterDisplay,
    pub rms_db: f32,
}

impl StripMeters {
    pub fn silent() -> Self {
        Self {
            peak: MeterDisplay::new(),
            rms_db: f32::NEG_INFINITY,
        }
    }
}

pub fn source_label(source: &SourceKind) -> String {
    match source {
        SourceKind::DefaultSource => "default source".to_string(),
        SourceKind::OtherDesktopAudio => "other desktop".to_string(),
        SourceKind::CaptureDevice { node_name } => node_name.clone(),
        SourceKind::Application(key) => {
            if key.media_name.is_empty() {
                format!("{} ({})", key.application_name, key.binary)
            } else {
                format!("{} / {}", key.application_name, key.media_name)
            }
        }
    }
}

pub struct EqEdit {
    pub enabled: bool,
    pub reset: bool,
    pub hpf_enabled: bool,
    pub hpf_hz: f32,
    pub bands: [f32; EQ_BANDS],
}

pub struct CompEdit {
    pub enabled: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
}

pub struct ChainEdit {
    pub bypass: Vec<bool>,
    pub move_up: Option<usize>,
    pub move_down: Option<usize>,
}

pub struct StripEdit {
    pub fader_db: f32,
    pub trim_db: f32,
    pub mute: bool,
    pub solo: bool,
    pub remove: bool,
    pub eq: EqEdit,
    pub comp: CompEdit,
}

pub struct MasterEdit {
    pub fader_db: f32,
    pub eq: EqEdit,
    pub comp: CompEdit,
    pub chain: ChainEdit,
}

pub struct MasterView<'a> {
    pub fader_db: f32,
    pub meters: &'a StripMeters,
    pub eq_enabled: bool,
    pub eq: EqSettings,
    pub comp: CompSettings,
    pub chain: &'a MasterChain,
    pub reduction_db: f32,
    pub latency_ms: f32,
    pub sample_rate: u32,
}

pub fn strip_panel(
    ui: &mut egui::Ui,
    strip: &Strip,
    meters: &StripMeters,
    reduction_db: f32,
    sample_rate: u32,
) -> StripEdit {
    let mut edit = StripEdit {
        fader_db: strip.fader_db,
        trim_db: strip.input_gain_db,
        mute: strip.mute,
        solo: strip.solo,
        remove: false,
        eq: eq_edit(strip.eq_enabled, strip.eq),
        comp: comp_edit(strip.comp),
    };
    ui.group(|ui| {
        ui.set_min_width(STRIP_PANEL_MIN_WIDTH);
        ui.vertical(|ui| {
            ui.strong(&strip.name);
            ui.label(source_label(&strip.source));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                meter_column(ui, meters);
                ui.vertical(|ui| {
                    ui.label("level");
                    db_fader(ui, &mut edit.fader_db);
                    ui.label(format_db(edit.fader_db));
                });
                ui.vertical(|ui| {
                    ui.label("trim");
                    trim_fader(ui, &mut edit.trim_db);
                    ui.label(format_db(edit.trim_db));
                });
            });
            ui.label(format!(
                "peak {}  rms {}",
                format_db(meters.peak.peak_db()),
                format_db(meters.rms_db)
            ));
            ui.horizontal(|ui| {
                ui.toggle_value(&mut edit.mute, "M");
                ui.toggle_value(&mut edit.solo, "S");
                if ui.button("Remove").clicked() {
                    edit.remove = true;
                }
            });
            egui::CollapsingHeader::new("Effects")
                .id_salt(("strip-effects", strip.id.0))
                .show(ui, |ui| {
                    ui.set_min_width(420.0);
                    eq_controls(ui, strip.id.0, &mut edit.eq, sample_rate);
                    comp_controls(ui, strip.id.0, &mut edit.comp);
                    ui.label(format!("gain reduction {}", format_db(reduction_db)));
                });
        });
    });
    edit
}

pub fn master_panel(ui: &mut egui::Ui, view: MasterView<'_>) -> MasterEdit {
    let mut edit = MasterEdit {
        fader_db: view.fader_db,
        eq: eq_edit(view.eq_enabled, view.eq),
        comp: comp_edit(view.comp),
        chain: ChainEdit {
            bypass: view
                .chain
                .blocks()
                .iter()
                .map(|block| block.bypassed())
                .collect(),
            move_up: None,
            move_down: None,
        },
    };
    ui.group(|ui| {
        ui.set_min_width(MASTER_SIDE_WIDTH - 16.0);
        ui.vertical(|ui| {
            ui.strong("Master");
            ui.label("mix");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                meter_column(ui, view.meters);
                ui.vertical(|ui| {
                    ui.label("level");
                    db_fader(ui, &mut edit.fader_db);
                    ui.label(format_db(edit.fader_db));
                });
            });
            ui.label(format!(
                "peak {}  rms {}",
                format_db(view.meters.peak.peak_db()),
                format_db(view.meters.rms_db)
            ));
            egui::CollapsingHeader::new("EQ")
                .id_salt("master-eq")
                .show(ui, |ui| {
                    ui.set_min_width(MASTER_SIDE_WIDTH - 24.0);
                    eq_controls(ui, u64::MAX, &mut edit.eq, view.sample_rate);
                });
            ui.label("Chain");
            chain_rows(ui, view.chain, &mut edit.chain);
            ui.label(format!("latency {:.1} ms", view.latency_ms));
            comp_controls(ui, u64::MAX - 1, &mut edit.comp);
            ui.label(format!("gain reduction {}", format_db(view.reduction_db)));
        });
    });
    edit
}

fn eq_edit(enabled: bool, settings: EqSettings) -> EqEdit {
    EqEdit {
        enabled,
        reset: false,
        hpf_enabled: settings.hpf_enabled(),
        hpf_hz: settings.hpf_hz(),
        bands: settings.band_db(),
    }
}

fn comp_edit(settings: CompSettings) -> CompEdit {
    CompEdit {
        enabled: settings.enabled(),
        threshold_db: settings.threshold_db(),
        ratio: settings.ratio(),
        attack_ms: settings.attack_ms(),
        release_ms: settings.release_ms(),
        makeup_db: settings.makeup_db(),
    }
}

fn eq_controls(ui: &mut egui::Ui, salt: u64, edit: &mut EqEdit, sample_rate: u32) {
    let started_bands = edit.bands;
    let started_hpf = edit.hpf_enabled;
    let started_hz = edit.hpf_hz;
    ui.horizontal(|ui| {
        ui.checkbox(&mut edit.enabled, "EQ");
        if ui.button("Flat").clicked() {
            edit.reset = true;
        }
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut edit.hpf_enabled, "High-pass");
        ui.add(
            egui::Slider::new(&mut edit.hpf_hz, HPF_MIN_HZ..=HPF_MAX_HZ)
                .suffix(" Hz")
                .text("cutoff"),
        );
    });
    ui.horizontal(|ui| {
        for (index, hz) in EQ_BAND_HZ.iter().enumerate() {
            ui.push_id((salt, index), |ui| {
                ui.vertical(|ui| {
                    ui.set_min_width(40.0);
                    ui.label(band_caption(*hz));
                    let inactive = EqSettings::flat(sample_rate).band_disabled(index);
                    band_fader(ui, &mut edit.bands[index], !inactive);
                    ui.label(format!("{:.1}", edit.bands[index]));
                });
            });
        }
    });
    let curve_moved =
        edit.bands != started_bands || edit.hpf_enabled != started_hpf || edit.hpf_hz != started_hz;
    if curve_moved {
        edit.enabled = true;
    }
}

fn band_caption(hz: f32) -> String {
    if hz >= 1000.0 {
        format!("{:.0}k", hz / 1000.0)
    } else if hz < 100.0 {
        format!("{hz:.1}")
    } else {
        format!("{hz:.0}")
    }
}

fn band_fader(ui: &mut egui::Ui, db: &mut f32, active: bool) {
    let height = 120.0;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(28.0, height), egui::Sense::click_and_drag());
    if !db.is_finite() {
        *db = 0.0;
    }
    *db = db.clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
    if active {
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let notches = wheel_notches(ui, &response);
        if response.hovered() && response.has_focus() && notches != 0 {
            *db =
                (*db + notches as f32 * fader::WHEEL_STEP_DB).clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
        }
        if response.dragged() {
            let span = EQ_GAIN_MAX_DB - EQ_GAIN_MIN_DB;
            let delta = response.drag_delta().y;
            if delta.is_finite() && delta != 0.0 {
                *db = (*db - delta / height * span).clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
            }
        }
    }
    paint_fader(
        ui,
        rect,
        *db,
        EQ_GAIN_MIN_DB,
        EQ_GAIN_MAX_DB,
        active && response.has_focus(),
    );
}

fn comp_controls(ui: &mut egui::Ui, salt: u64, edit: &mut CompEdit) {
    ui.push_id(("comp", salt), |ui| {
        ui.checkbox(&mut edit.enabled, "Compressor");
        range_slider(
            ui,
            "threshold",
            &mut edit.threshold_db,
            COMP_THRESHOLD_MIN_DB..=COMP_THRESHOLD_MAX_DB,
            " dB",
        );
        range_slider(
            ui,
            "ratio",
            &mut edit.ratio,
            COMP_RATIO_MIN..=COMP_RATIO_MAX,
            ":1",
        );
        range_slider(
            ui,
            "attack",
            &mut edit.attack_ms,
            COMP_ATTACK_MIN_MS..=COMP_ATTACK_MAX_MS,
            " ms",
        );
        range_slider(
            ui,
            "release",
            &mut edit.release_ms,
            COMP_RELEASE_MIN_MS..=COMP_RELEASE_MAX_MS,
            " ms",
        );
        range_slider(
            ui,
            "makeup",
            &mut edit.makeup_db,
            COMP_MAKEUP_MIN_DB..=COMP_MAKEUP_MAX_DB,
            " dB",
        );
    });
}

fn range_slider(
    ui: &mut egui::Ui,
    name: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
) {
    ui.add(egui::Slider::new(value, range).text(name).suffix(suffix));
}

fn chain_rows(ui: &mut egui::Ui, chain: &MasterChain, edit: &mut ChainEdit) {
    let blocks = chain.blocks();
    for index in 0..blocks.len() {
        let (up_ok, down_ok) = reorder_move_enabled(blocks, index);
        ui.push_id(("chain", index), |ui| {
            ui.horizontal(|ui| {
                if index < edit.bypass.len() {
                    ui.checkbox(&mut edit.bypass[index], "bypass");
                }
                ui.label(blocks[index].kind_id());
                if up_ok && ui.button("Up").clicked() {
                    edit.move_up = Some(index);
                }
                if down_ok && ui.button("Down").clicked() {
                    edit.move_down = Some(index);
                }
            });
        });
    }
}

fn meter_column(ui: &mut egui::Ui, meters: &StripMeters) {
    ui.vertical(|ui| {
        ui.label("meter");
        let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 220.0), egui::Sense::hover());
        paint_meter(ui, rect, meters.rms_db, meters.peak.peak_db());
    });
}

fn paint_meter(ui: &mut egui::Ui, rect: egui::Rect, rms_db: f32, peak_db: f32) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(3),
        egui::Color32::from_rgb(24, 24, 28),
    );
    let inner = rect.shrink(3.0);
    let rms_top = meter_y(inner, rms_db);
    let filled = egui::Rect::from_min_max(egui::pos2(inner.left(), rms_top), inner.right_bottom());
    painter.rect_filled(
        filled,
        egui::CornerRadius::same(2),
        egui::Color32::from_rgb(72, 168, 96),
    );
    let peak_y = meter_y(inner, peak_db);
    painter.hline(
        inner.x_range(),
        peak_y,
        egui::Stroke::new(2.0, egui::Color32::from_rgb(230, 210, 90)),
    );
}

fn meter_y(rect: egui::Rect, db: f32) -> f32 {
    let floor = METER_BAR_FLOOR_DB;
    let top = 0.0;
    let t = if db.is_finite() {
        ((db - floor) / (top - floor)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    rect.bottom() - t * rect.height()
}

/// Vertical dB fader. Drag always moves it. The wheel moves it only when the
/// pointer is over this fader and the fader has focus. Each raw detent is
/// [`fader::WHEEL_STEP_DB`].
fn db_fader(ui: &mut egui::Ui, db: &mut f32) {
    let height = 220.0;
    let (rect, response) = allocate_fader(ui, height);
    *db = fader::sanitize_fader_db(*db);
    if response.clicked() {
        response.request_focus();
        *db = fader::apply_pointer(*db, fader::PointerGesture::FocusClick);
    }
    if response.drag_started() {
        response.request_focus();
    }
    let notches = wheel_notches(ui, &response);
    *db = fader::apply_wheel(*db, response.hovered(), response.has_focus(), notches);
    if response.dragged() {
        *db = fader::apply_pointer(
            *db,
            fader::PointerGesture::Drag {
                delta_y: response.drag_delta().y,
                height,
            },
        );
    }
    let hovered = response.hovered();
    let focused = response.has_focus();
    paint_fader(ui, rect, *db, FADER_MIN_DB, FADER_MAX_DB, focused);
    let _ = response.on_hover_text(if hovered && focused {
        "Wheel changes this fader by 0.5 dB per notch"
    } else {
        "Click the fader, then scroll while the pointer is still on it"
    });
}

fn trim_fader(ui: &mut egui::Ui, db: &mut f32) {
    let height = 220.0;
    let (rect, response) = allocate_fader(ui, height);
    if !db.is_finite() {
        *db = 0.0;
    }
    *db = db.clamp(INPUT_GAIN_MIN_DB, INPUT_GAIN_MAX_DB);
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    let notches = wheel_notches(ui, &response);
    if response.hovered() && response.has_focus() && notches != 0 {
        *db = (*db + notches as f32 * fader::WHEEL_STEP_DB)
            .clamp(INPUT_GAIN_MIN_DB, INPUT_GAIN_MAX_DB);
    }
    if response.dragged() {
        let span = INPUT_GAIN_MAX_DB - INPUT_GAIN_MIN_DB;
        let delta = response.drag_delta().y;
        if delta.is_finite() && delta != 0.0 {
            *db = (*db - delta / height * span).clamp(INPUT_GAIN_MIN_DB, INPUT_GAIN_MAX_DB);
        }
    }
    let hovered = response.hovered();
    let focused = response.has_focus();
    paint_fader(ui, rect, *db, INPUT_GAIN_MIN_DB, INPUT_GAIN_MAX_DB, focused);
    let _ = response.on_hover_text(if hovered && focused {
        "Wheel changes trim by 0.5 dB per notch"
    } else {
        "Click the fader, then scroll while the pointer is still on it"
    });
}

fn allocate_fader(ui: &mut egui::Ui, height: f32) -> (egui::Rect, egui::Response) {
    ui.allocate_exact_size(egui::vec2(28.0, height), egui::Sense::click_and_drag())
}

fn wheel_notches(ui: &mut egui::Ui, response: &egui::Response) -> i32 {
    let focused = response.has_focus();
    let hovered = response.hovered();
    let wheel: Vec<(fader::WheelUnit, f32)> = ui.input(|input| {
        input
            .raw
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::MouseWheel { unit, delta, .. } => {
                    let unit = match unit {
                        egui::MouseWheelUnit::Line => fader::WheelUnit::Line,
                        egui::MouseWheelUnit::Point => fader::WheelUnit::Point,
                        egui::MouseWheelUnit::Page => fader::WheelUnit::Page,
                    };
                    Some((unit, delta.y))
                }
                _ => None,
            })
            .collect()
    });
    if hovered && focused {
        let now = ui.input(|input| input.time);
        let mut notches = 0;
        ui.ctx().data_mut(|data| {
            let accum = data.get_temp_mut_or_default::<fader::WheelAccum>(response.id);
            for (unit, delta_y) in &wheel {
                notches += accum.push_at(*unit, *delta_y, now);
            }
        });
        // This fader owns the wheel for every hovered, focused frame. egui
        // keeps a smoothed delta for a few frames after the raw events stop.
        // smooth_scroll_delta is what a parent ScrollArea reads. egui 0.36 has
        // no raw_scroll_delta field; the raw events above are the wheel.
        ui.input_mut(|input| {
            if !wheel.is_empty() {
                input.raw.events.retain(|event| !is_mouse_wheel(event));
                input.events.retain(|event| !is_mouse_wheel(event));
            }
            input.smooth_scroll_delta = egui::Vec2::ZERO;
        });
        notches
    } else {
        ui.ctx().data_mut(|data| {
            data.remove_temp::<fader::WheelAccum>(response.id);
        });
        0
    }
}

fn is_mouse_wheel(event: &egui::Event) -> bool {
    matches!(event, egui::Event::MouseWheel { .. })
}

fn paint_fader(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    db: f32,
    min_db: f32,
    max_db: f32,
    focused: bool,
) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(3),
        egui::Color32::from_rgb(24, 24, 28),
    );
    let finite = if db.is_finite() { db } else { min_db };
    let span = (max_db - min_db).max(1.0);
    let t = ((finite - min_db) / span).clamp(0.0, 1.0);
    let y = rect.bottom() - t * rect.height();
    let track = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.center().y),
        egui::vec2(4.0, rect.height() - 12.0),
    );
    painter.rect_filled(
        track,
        egui::CornerRadius::same(2),
        egui::Color32::from_rgb(60, 60, 68),
    );
    let knob = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, y),
        egui::vec2(rect.width() - 8.0, 12.0),
    );
    let color = if focused {
        egui::Color32::from_rgb(120, 186, 255)
    } else {
        egui::Color32::from_rgb(176, 176, 184)
    };
    painter.rect_filled(knob, egui::CornerRadius::same(2), color);
}
