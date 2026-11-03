// made by CasualArclamp
//! SDR++-style frequency display: big digits you tune one at a time.
//!
//! - **scroll** over a digit steps it (carrying into the next);
//! - **click** its upper half to step up, its lower half to step down;
//! - **right-click** a digit to zero it and every digit to its right, as SDR++
//!   does (right-click the 100 kHz digit of 12.345.678 → 12.000.000).
//!
//! Leading zeros are dimmed; groups of three are separated by dots.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, PointerButton, Sense, Ui, pos2, vec2,
};

/// Digits shown: up to 999.999.999.999 Hz, enough for Ku and Ka band RF.
const DIGITS: usize = 12;

/// Draw the display for `hz` and apply any edits. Returns true when the value
/// changed; the result is clamped to `[min, max]`.
pub fn show(ui: &mut Ui, hz: &mut f64, min: f64, max: f64, height: f32) -> bool {
    let font = FontId::monospace(height * 0.82);
    let digit_w = ui
        .painter()
        .layout_no_wrap("0".into(), font.clone(), Color32::WHITE)
        .size()
        .x
        + 2.0;
    let sep_w = digit_w * 0.45;

    let value = hz.round().clamp(0.0, 10f64.powi(DIGITS as i32) - 1.0) as u64;
    let digits: Vec<u8> = (0..DIGITS)
        .map(|i| ((value / 10u64.pow((DIGITS - 1 - i) as u32)) % 10) as u8)
        .collect();
    let first_significant = digits.iter().position(|&d| d != 0).unwrap_or(DIGITS - 1);

    let text = ui.visuals().strong_text_color();
    let dim = ui.visuals().weak_text_color().gamma_multiply(0.5);
    let hover_bg = ui.visuals().widgets.hovered.weak_bg_fill;

    let mut delta: i64 = 0;
    let mut zero_from: Option<u32> = None;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, &d) in digits.iter().enumerate() {
            let place = (DIGITS - 1 - i) as u32;
            let step = 10i64.pow(place);
            let (rect, resp) = ui.allocate_exact_size(vec2(digit_w, height), Sense::click());

            if resp.hovered() {
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(3), hover_bg);
                // One step per wheel event, whatever the platform's notch size
                // or egui's smoothing: count the raw MouseWheel events.
                let notches: i64 = ui.input(|inp| {
                    inp.raw
                        .events
                        .iter()
                        .map(|e| match e {
                            egui::Event::MouseWheel { delta, .. } if delta.y > 0.0 => 1,
                            egui::Event::MouseWheel { delta, .. } if delta.y < 0.0 => -1,
                            _ => 0,
                        })
                        .sum()
                });
                delta += notches * step;
            }
            if resp.clicked()
                && let Some(p) = resp.interact_pointer_pos()
            {
                delta += if p.y < rect.center().y { step } else { -step };
            }
            if resp.clicked_by(PointerButton::Secondary) {
                zero_from = Some(place);
            }

            let color = if i < first_significant { dim } else { text };
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                d.to_string(),
                font.clone(),
                color,
            );

            // A dot after every group of three, except at the end.
            if place.is_multiple_of(3) && place != 0 {
                let (r, _) = ui.allocate_exact_size(vec2(sep_w, height), Sense::hover());
                let c = if i < first_significant { dim } else { text };
                ui.painter().circle_filled(
                    pos2(r.center().x, r.bottom() - height * 0.22),
                    height * 0.05,
                    c,
                );
            }
        }
    });

    let mut new = value as i64 + delta;
    if let Some(place) = zero_from {
        new = zeroed_from(new, place);
    }
    let new = (new as f64).clamp(min, max);
    if (new - *hz).abs() >= 0.5 {
        *hz = new;
        true
    } else {
        false
    }
}

/// `value` with the digit at `place` (0 = units) and all below it set to 0.
fn zeroed_from(value: i64, place: u32) -> i64 {
    let unit = 10i64.pow(place + 1);
    value / unit * unit
}

#[cfg(test)]
mod tests {
    #[test]
    fn right_click_zeroes_the_digit_and_everything_below() {
        // 12.345.678: right-click the 100 kHz digit (the 3, place 5).
        assert_eq!(super::zeroed_from(12_345_678, 5), 12_000_000);
        // The units digit: just that one.
        assert_eq!(super::zeroed_from(12_345_678, 0), 12_345_670);
        // The top digit: everything.
        assert_eq!(super::zeroed_from(12_345_678, 7), 0);
    }

    #[test]
    fn digit_extraction_matches_the_value() {
        let v: u64 = 12_331_370_000;
        let d: Vec<u8> = (0..super::DIGITS)
            .map(|i| ((v / 10u64.pow((super::DIGITS - 1 - i) as u32)) % 10) as u8)
            .collect();
        assert_eq!(d, vec![0, 1, 2, 3, 3, 1, 3, 7, 0, 0, 0, 0]);
    }
}
