//! UI helpers: the dark/light theme and a handful of small widgets used by every
//! panel, so the app has one consistent look instead of ad-hoc styling.

use eframe::egui::{self, Color32, CornerRadius, RichText, Stroke, Ui};

/// A small, deliberate palette. `dark` is the default.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub border: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub bad: Color32,
}

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x10, 0x12, 0x18),
    panel: Color32::from_rgb(0x17, 0x1a, 0x22),
    card: Color32::from_rgb(0x1e, 0x22, 0x2c),
    border: Color32::from_rgb(0x2c, 0x31, 0x3d),
    text: Color32::from_rgb(0xe4, 0xe7, 0xee),
    dim: Color32::from_rgb(0x93, 0x9b, 0xab),
    accent: Color32::from_rgb(0x5c, 0x9c, 0xff),
    ok: Color32::from_rgb(0x54, 0xc8, 0x8a),
    warn: Color32::from_rgb(0xe8, 0xbc, 0x5e),
    bad: Color32::from_rgb(0xef, 0x74, 0x68),
};

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xf3, 0xf4, 0xf8),
    panel: Color32::from_rgb(0xea, 0xec, 0xf2),
    card: Color32::from_rgb(0xff, 0xff, 0xff),
    border: Color32::from_rgb(0xd3, 0xd7, 0xe0),
    text: Color32::from_rgb(0x1c, 0x1f, 0x27),
    dim: Color32::from_rgb(0x60, 0x68, 0x78),
    accent: Color32::from_rgb(0x1b, 0x5f, 0xd8),
    ok: Color32::from_rgb(0x1f, 0x8f, 0x54),
    warn: Color32::from_rgb(0xa9, 0x74, 0x00),
    bad: Color32::from_rgb(0xc0, 0x39, 0x2b),
};

impl Palette {
    pub fn by_name(name: &str) -> Palette {
        if name.eq_ignore_ascii_case("light") {
            LIGHT
        } else {
            DARK
        }
    }
}

/// Install the palette into the egui context.
pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut visuals = if p.bg.r() > 128 {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    visuals.panel_fill = p.panel;
    visuals.window_fill = p.panel;
    visuals.extreme_bg_color = p.bg;
    visuals.faint_bg_color = p.card;
    visuals.override_text_color = Some(p.text);
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.selection.bg_fill = p.accent.gamma_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0, p.accent);
    visuals.hyperlink_color = p.accent;

    let r = CornerRadius::same(7);
    visuals.widgets.noninteractive.corner_radius = r;
    visuals.widgets.inactive.corner_radius = r;
    visuals.widgets.hovered.corner_radius = r;
    visuals.widgets.active.corner_radius = r;
    visuals.widgets.inactive.bg_fill = p.card;
    visuals.widgets.inactive.weak_bg_fill = p.card;
    visuals.widgets.hovered.bg_fill = p.border;
    visuals.widgets.hovered.weak_bg_fill = p.border;
    visuals.widgets.active.bg_fill = p.accent.gamma_multiply(0.55);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);

    ctx.set_visuals(visuals);

    let theme = ctx.theme();
    let mut style = (*ctx.style_of(theme)).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 22.0;

    // Solid (non-floating) scroll bars. egui's stock style floats the bar over the
    // content and fades the handle to *zero* opacity while it is idle, so a pane
    // whose content continues below the fold looks like it was simply cut off
    // rather than scrollable. The controls pane is exactly that case — its content
    // is around twice the height of a normal window — so the bar must always show.
    style.spacing.scroll = egui::style::ScrollStyle::solid();

    ctx.set_style_of(theme, style);
}

/// A section heading with a rule under it.
pub fn section(ui: &mut Ui, p: &Palette, title: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(title)
            .color(p.dim)
            .size(11.5)
            .strong()
            .extra_letter_spacing(0.6),
    );
    let rect = ui.available_rect_before_wrap();
    let y = rect.top() + 2.0;
    ui.painter().line_segment(
        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
        Stroke::new(1.0, p.border),
    );
    ui.add_space(4.0);
}

/// A rounded status chip.
pub fn pill(ui: &mut Ui, _p: &Palette, text: &str, color: Color32) {
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_owned(), egui::FontId::proportional(12.0), color);
    let pad = egui::vec2(9.0, 4.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(9), color.gamma_multiply(0.16));
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(9),
        Stroke::new(1.0, color.gamma_multiply(0.5)),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.min + pad, galley, color);
}

/// `label            value` row.
///
/// The value is right-aligned, so a value that is too long for the row would
/// overlap the label rather than wrap. It is shortened to what fits instead.
pub fn kv(ui: &mut Ui, p: &Palette, label: &str, value: &str, value_color: Option<Color32>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(p.dim).size(12.5));

        // ~7 px per character for the monospace face at 12.5 pt.
        let budget = ((ui.available_width() / 7.0) as usize).max(8);
        let shown: String = if value.chars().count() > budget {
            let keep = budget.saturating_sub(1);
            format!("{}…", value.chars().take(keep).collect::<String>())
        } else {
            value.to_string()
        };

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(shown)
                    .color(value_color.unwrap_or(p.text))
                    .size(12.5)
                    .monospace(),
            );
        });
    });
}

/// A labelled toggle with an explanation underneath.
pub fn toggle(ui: &mut Ui, p: &Palette, label: &str, value: &mut bool, hint: &str) -> bool {
    let changed = ui.checkbox(value, label).changed();
    if !hint.is_empty() {
        ui.label(RichText::new(hint).color(p.dim).size(11.0));
    }
    let _ = p;
    changed
}

/// A card frame, used for grouped controls.
pub fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(egui::Margin::same(10))
        .show(ui, add)
        .inner
}

/// Big monospace read-out, for the video overlay and stats.
pub fn badge(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).monospace().size(11.5).color(p.dim));
}

/// A small vertical gap.
pub fn blank(ui: &mut Ui) {
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_lookup_defaults_to_dark() {
        assert_eq!(Palette::by_name("light").bg, LIGHT.bg);
        assert_eq!(Palette::by_name("DARK").bg, DARK.bg);
        assert_eq!(Palette::by_name("nonsense").bg, DARK.bg);
    }

    #[test]
    fn dark_background_is_actually_dark() {
        assert!(DARK.bg.r() < 128);
        assert!(LIGHT.bg.r() > 128);
    }
}
