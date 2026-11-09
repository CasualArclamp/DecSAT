// made by CasualArclamp
//! A round dial: a 240-degree scale, a green band for the range that is
//! good, a needle and the value under it.

use eframe::egui::{Align2, FontId, Pos2, Sense, Shape, Stroke, Ui, Vec2, pos2};

use crate::ui::{EDGE, GOOD, MUTED, TEXT, WARN};

/// The scale starts lower left and runs clockwise over the top.
const START_DEG: f32 = 150.0;
const SWEEP_DEG: f32 = 240.0;

pub struct Dial<'a> {
    pub label: &'a str,
    pub unit: &'a str,
    pub value: Option<f32>,
    pub min: f32,
    pub max: f32,
    /// The good range, drawn green.
    pub good: Option<(f32, f32)>,
    pub decimals: usize,
}

fn on_circle(c: Pos2, r: f32, deg: f32) -> Pos2 {
    let a = deg.to_radians();
    pos2(c.x + r * a.cos(), c.y + r * a.sin())
}

fn arc(c: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    let steps = ((to - from).abs() / 4.0).ceil().max(1.0) as usize;
    (0..=steps)
        .map(|i| on_circle(c, r, from + (to - from) * i as f32 / steps as f32))
        .collect()
}

pub fn dial(ui: &mut Ui, size: Vec2, d: &Dial) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter_at(rect);
    let r = (size.y * 0.40).min(size.x * 0.34);
    let c = pos2(rect.center().x, rect.min.y + r + 12.0);
    let angle = |v: f32| START_DEG + SWEEP_DEG * ((v - d.min) / (d.max - d.min)).clamp(0.0, 1.0);

    p.add(Shape::line(
        arc(c, r, START_DEG, START_DEG + SWEEP_DEG),
        Stroke::new(4.0, EDGE),
    ));
    if let Some((lo, hi)) = d.good {
        p.add(Shape::line(
            arc(c, r, angle(lo), angle(hi)),
            Stroke::new(4.0, GOOD),
        ));
    }
    for k in 0..=4 {
        let a = START_DEG + SWEEP_DEG * k as f32 / 4.0;
        p.line_segment(
            [on_circle(c, r + 3.0, a), on_circle(c, r + 7.0, a)],
            Stroke::new(1.0, MUTED),
        );
    }
    let end = |v: f32, a: f32| {
        p.text(
            on_circle(c, r + 2.0, a) + Vec2::new(0.0, 9.0),
            Align2::CENTER_CENTER,
            format!("{v:.0}"),
            FontId::monospace(9.5),
            MUTED,
        );
    };
    end(d.min, START_DEG);
    end(d.max, START_DEG + SWEEP_DEG);

    let in_range = d.value.zip(d.good).map(|(v, (lo, hi))| v >= lo && v <= hi);
    let colour = match in_range {
        Some(true) => GOOD,
        Some(false) => WARN,
        None => TEXT,
    };
    if let Some(v) = d.value {
        p.line_segment([c, on_circle(c, r - 5.0, angle(v))], Stroke::new(2.0, TEXT));
        p.circle_filled(c, 3.0, TEXT);
    }
    let shown = d.value.map_or("--".to_string(), |v| {
        format!("{v:.*} {}", d.decimals, d.unit)
    });
    p.text(
        pos2(c.x, c.y + r * 0.62),
        Align2::CENTER_CENTER,
        shown,
        FontId::monospace(13.0),
        if d.value.is_some() { colour } else { MUTED },
    );
    p.text(
        pos2(rect.min.x + 2.0, rect.min.y + 7.0),
        Align2::LEFT_CENTER,
        d.label,
        FontId::proportional(11.0),
        TEXT,
    );
}
