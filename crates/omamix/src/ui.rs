use eframe::egui;
use omamix_core::Strip;
use omamix_core::db::format_db;
use omamix_core::fader::{self, FADER_MAX_DB, FADER_MIN_DB};

pub fn strip_panel(ui: &mut egui::Ui, strip: &mut Strip) {
    ui.group(|ui| {
        ui.set_min_width(120.0);
        ui.vertical(|ui| {
            ui.strong(&strip.name);
            ui.label("placeholder");
            ui.add_space(6.0);
            db_fader(ui, &mut strip.fader_db);
            ui.add_space(6.0);
            ui.label(format_db(strip.fader_db));
            ui.horizontal(|ui| {
                ui.toggle_value(&mut strip.mute, "M");
                ui.toggle_value(&mut strip.solo, "S");
            });
        });
    });
}

/// Vertical dB fader. Drag always moves it. The wheel moves it only when the
/// pointer is over this fader and the fader has focus. Each raw detent is
/// [`fader::WHEEL_STEP_DB`].
fn db_fader(ui: &mut egui::Ui, db: &mut f32) {
    let width = 36.0;
    let height = 220.0;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());

    *db = fader::sanitize_fader_db(*db);

    if response.clicked() {
        response.request_focus();
        *db = fader::apply_pointer(*db, fader::PointerGesture::FocusClick);
    }
    if response.drag_started() {
        response.request_focus();
    }

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
        if notches != 0 {
            *db = fader::apply_wheel(*db, true, true, notches);
        }
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
    } else {
        ui.ctx().data_mut(|data| {
            data.remove_temp::<fader::WheelAccum>(response.id);
        });
    }

    if response.dragged() {
        *db = fader::apply_pointer(
            *db,
            fader::PointerGesture::Drag {
                delta_y: response.drag_delta().y,
                height,
            },
        );
    }

    paint_fader(ui, rect, *db, focused);
    let _ = response.on_hover_text(if hovered && focused {
        "Wheel changes this fader by 0.5 dB per notch"
    } else {
        "Click the fader, then scroll while the pointer is still on it"
    });
}

fn is_mouse_wheel(event: &egui::Event) -> bool {
    matches!(event, egui::Event::MouseWheel { .. })
}

fn paint_fader(ui: &mut egui::Ui, rect: egui::Rect, db: f32, focused: bool) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(3),
        egui::Color32::from_rgb(24, 24, 28),
    );
    let t = ((db - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB)).clamp(0.0, 1.0);
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
