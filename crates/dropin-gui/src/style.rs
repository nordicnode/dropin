//! dropin design system: a professional dark theme with clear hierarchy.
//!
//! Everything visual lives here so both the install dialog and the manager
//! share one look: one palette, one spacing scale, one set of components.

use eframe::egui;

/// The dropin palette: a sophisticated dark theme with depth.
///
/// Layers go BG < SURFACE < CARD < OVERLAY, each a step lighter.
/// Borders are subtle; accent is used sparingly for primary actions.
pub mod color {
    use eframe::egui::Color32;

    /// Window background: deep, not pure black.
    pub const BG: Color32 = Color32::from_rgb(10, 12, 16);
    /// Panels, toolbar, header: subtle lift from background.
    pub const SURFACE: Color32 = Color32::from_rgb(17, 19, 25);
    /// Cards: clear elevation above the surface.
    pub const CARD: Color32 = Color32::from_rgb(24, 27, 34);
    /// Popups, menus, dialogs: highest elevation.
    pub const OVERLAY: Color32 = Color32::from_rgb(32, 36, 45);
    /// Borders and dividers: low contrast.
    pub const BORDER: Color32 = Color32::from_rgb(38, 42, 52);
    /// Stronger border for interactive elements.
    pub const BORDER_STRONG: Color32 = Color32::from_rgb(52, 57, 70);
    /// Primary accent: emerald, matches the dropin brand.
    /// Used sparingly: primary buttons, active states, links.
    pub const ACCENT: Color32 = Color32::from_rgb(16, 185, 129);
    /// Accent hover: slightly brighter.
    pub const ACCENT_HOVER: Color32 = Color32::from_rgb(52, 211, 153);
    /// Accent text on dark: for links and highlights.
    pub const ACCENT_TEXT: Color32 = Color32::from_rgb(110, 231, 183);
    /// Primary text: near-white, not harsh.
    pub const TEXT: Color32 = Color32::from_rgb(242, 244, 248);
    /// Secondary text: muted.
    pub const TEXT_DIM: Color32 = Color32::from_rgb(156, 163, 175);
    /// Tertiary text: captions, metadata.
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(107, 114, 128);
    /// Success.
    pub const GOOD: Color32 = Color32::from_rgb(52, 211, 153);
    /// Warning.
    pub const WARN: Color32 = Color32::from_rgb(251, 191, 36);
    /// Danger / error.
    pub const BAD: Color32 = Color32::from_rgb(248, 113, 113);
    /// Info.
    pub const INFO: Color32 = Color32::from_rgb(96, 165, 250);
    /// App badge fill: subtle, not neon.
    pub const BADGE_A: Color32 = Color32::from_rgb(55, 65, 81);
    /// App badge fill accent: for the gradient.
    pub const BADGE_B: Color32 = Color32::from_rgb(75, 85, 105);
}

/// Apply the dropin theme to the egui context. Call once at startup.
pub fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = color::BG;
    visuals.panel_fill = color::SURFACE;
    visuals.extreme_bg_color = color::BG;
    visuals.faint_bg_color = color::CARD;
    visuals.code_bg_color = color::CARD;

    // Text.
    visuals.override_text_color = Some(color::TEXT);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, color::TEXT_DIM);

    // Widget backgrounds: subtle, bordered.
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.bg_fill = color::CARD;
        w.bg_stroke = egui::Stroke::new(1.0_f32, color::BORDER);
        w.corner_radius = egui::CornerRadius::same(8);
    }
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(38, 43, 53);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(42, 48, 59);

    // Selection.
    visuals.selection.bg_fill = color::ACCENT;
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, color::ACCENT);

    // Links.
    visuals.hyperlink_color = color::ACCENT;

    ctx.set_visuals(visuals);

    // Spacing and typography.
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.window_margin = egui::Margin::same(20);
    style.spacing.menu_margin = egui::Margin::same(12);

    // Slightly larger default text for readability.
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(22.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(12.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(12.0, egui::FontFamily::Monospace),
    );

    ctx.set_style(style);
}

/// A primary action button (Install, Confirm, ...). Returns clicked.
pub fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(label).strong().color(color::TEXT))
        .fill(color::ACCENT)
        .corner_radius(8.0)
        .min_size(egui::vec2(120.0, 36.0));
    ui.add(btn)
}

/// A primary button that can be disabled.
pub fn primary_button_enabled(ui: &mut egui::Ui, enabled: bool, label: &str) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).strong().color(color::TEXT))
            .fill(if enabled {
                color::ACCENT
            } else {
                color::BORDER
            })
            .corner_radius(8.0)
            .min_size(egui::vec2(120.0, 36.0)),
    )
}

/// A quiet secondary button.
pub fn ghost_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(color::TEXT_DIM))
            .fill(egui::Color32::TRANSPARENT)
            .corner_radius(8.0)
            .min_size(egui::vec2(80.0, 32.0)),
    )
}

/// A "…" tools menu button, sized to match neighboring buttons.
/// Uses a larger glyph so the button sizes naturally instead of
/// collapsing to the tiny default.
pub fn tools_menu_button<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    ui.menu_button(egui::RichText::new("…").size(20.0).strong(), add_contents)
}

/// A dropdown button with vertically centered text, for filter rows.
/// egui's ComboBox can render text slightly high; this uses a plain
/// Button with explicit centering for pixel-perfect alignment.
pub fn filter_dropdown<R>(
    ui: &mut egui::Ui,
    selected_text: &str,
    width: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    let button_text = format!("{selected_text}  v");
    let resp = ui.add_sized(
        [width, 32.0],
        egui::Button::new(egui::RichText::new(button_text).size(14.0)).corner_radius(8.0),
    );
    let inner = egui::Popup::menu(&resp).show(add_contents);
    egui::InnerResponse::new(inner.map(|i| i.inner), resp)
}

/// A small pill badge, e.g. the format tag or version.
pub fn badge(ui: &mut egui::Ui, text: &str, bg: egui::Color32) {
    egui::Frame::NONE
        .fill(bg)
        .corner_radius(6.0)
        .inner_margin(egui::Margin {
            left: 8,
            right: 8,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(text)
                    .small()
                    .strong()
                    .color(color::TEXT),
            );
        });
}

/// Format tag colors: one hue per family so rows scan quickly.
pub fn format_color(format: &str) -> egui::Color32 {
    if format.contains("AppImage") {
        egui::Color32::from_rgb(56, 120, 200)
    } else if format.contains("Debian") || format.contains("RPM") {
        egui::Color32::from_rgb(160, 90, 60)
    } else if format.contains("script") {
        egui::Color32::from_rgb(140, 90, 180)
    } else if format.contains("archive") || format.contains("Tarball") || format.contains("Zip") {
        egui::Color32::from_rgb(60, 140, 110)
    } else if format.contains("Flatpak") {
        egui::Color32::from_rgb(70, 130, 160)
    } else if format.contains("Windows") {
        egui::Color32::from_rgb(0, 120, 180)
    } else {
        egui::Color32::from_rgb(90, 98, 112)
    }
}

/// Draw the app icon badge: rounded square with the initial.
pub fn app_badge(ui: &mut egui::Ui, name: &str, size: f32) {
    let initial = name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    // Two-tone: solid fill reads cleaner than a gradient at small sizes.
    ui.painter().rect_filled(rect, size * 0.22, color::BADGE_A);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(size * 0.48),
        egui::Color32::WHITE,
    );
}

/// Find the installed icon for an app by slug.
/// Icons are saved as ~/.local/share/icons/hicolor/256x256/apps/dropin-{slug}.{ext}.
pub fn find_app_icon(slug: &str) -> Option<std::path::PathBuf> {
    // First, check the .desktop file's Icon= field directly.
    let desktop_path = dropin_core::home_dir()
        .join(".local/share/applications")
        .join(format!("dropin-{slug}.desktop"));
    if let Ok(text) = std::fs::read_to_string(&desktop_path) {
        for line in text.lines() {
            if let Some(icon) = line.strip_prefix("Icon=") {
                let icon = icon.trim();
                // Absolute path.
                if icon.starts_with('/') {
                    let p = std::path::PathBuf::from(icon);
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
        }
    }
    // Fallback: look for dropin-{slug}.* in the icons dir.
    let icons_dir = dropin_core::home_dir().join(".local/share/icons/hicolor/256x256/apps");
    // Try common extensions.
    for ext in ["png", "svg", "xpm", "ico"] {
        let p = icons_dir.join(format!("dropin-{slug}.{ext}"));
        if p.exists() {
            return Some(p);
        }
    }
    // Fallback: any file starting with dropin-{slug}.
    if let Ok(entries) = std::fs::read_dir(&icons_dir) {
        for e in entries.filter_map(|e| e.ok()) {
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with(&format!("dropin-{slug}.")) {
                    return Some(e.path());
                }
            }
        }
    }
    None
}

/// App icon: shows the real icon if available, falls back to letter badge.
pub fn app_icon(ui: &mut egui::Ui, slug: &str, name: &str, size: f32) {
    if let Some(path) = find_app_icon(slug) {
        let uri = format!("file://{}", path.display());
        // Rounded image; if it fails to load egui shows a placeholder,
        // which is still better than nothing. The badge fallback happens
        // when no icon file exists at all.
        ui.add(
            egui::Image::new(uri)
                .fit_to_exact_size(egui::vec2(size, size))
                .corner_radius(size * 0.22),
        );
    } else {
        app_badge(ui, name, size);
    }
}

/// A card frame for app rows and sections.
pub fn card_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(color::CARD)
        .stroke(egui::Stroke::new(1.0_f32, color::BORDER))
        .corner_radius(10.0)
        .inner_margin(16.0)
}

/// Section heading with a subtle divider underneath.
pub fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(egui::RichText::new(title).strong().size(15.0));
    ui.add_space(2.0);
    ui.separator();
    ui.add_space(6.0);
}

/// Muted secondary text.
pub fn dim_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(color::TEXT_DIM));
}

/// Faint caption text.
pub fn faint_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().color(color::TEXT_FAINT));
}

/// Human-readable byte size.
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
