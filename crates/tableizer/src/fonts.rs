//! Font management: an OS-native chrome font (with bundled Inter as a cross-platform fallback) and a
//! user-selectable data-cell font, both resolved from the system font database via `fontdb`.

use eframe::egui::{FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

/// egui custom-family name used for data cells.
pub const TABLE_FONT: &str = "table";

/// Whether egui's font backend (skrifa, the same version egui uses) can parse these bytes — guards
/// the atlas build against unsupported fonts crashing the app. Mirrors egui's own load: face 0.
fn parseable(data: &[u8]) -> bool {
    skrifa::FontRef::from_index(data, 0).is_ok()
}

/// Candidate OS-native UI font families, best first.
fn os_ui_families() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    return &[
        "SF Pro Text",
        "SF Pro",
        ".AppleSystemUIFont",
        "Helvetica Neue",
    ];
    #[cfg(target_os = "windows")]
    return &["Segoe UI Variable Text", "Segoe UI"];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return &[
        "Cantarell",
        "Ubuntu",
        "Noto Sans",
        "DejaVu Sans",
        "Liberation Sans",
    ];
}

/// Bytes of the first of `names` that resolves to a parseable font in `db`.
fn load_family(db: &fontdb::Database, names: &[&str]) -> Option<Vec<u8>> {
    for &name in names {
        let Some(id) = db.query(&fontdb::Query {
            families: &[fontdb::Family::Name(name)],
            ..Default::default()
        }) else {
            continue;
        };
        if let Some(bytes) = db.with_face_data(id, |data, _| data.to_vec())
            && parseable(&bytes)
        {
            return Some(bytes);
        }
    }
    None
}

/// Whether a face is actually monospaced, measured by comparing the advance widths of a narrow
/// and a wide glyph. `fontdb`'s `monospaced` flag only reads `post.isFixedPitch`, which some
/// genuinely-monospaced fonts (e.g. Monaco) leave unset — so we measure to be sure.
fn measured_monospaced(db: &fontdb::Database, id: fontdb::ID) -> bool {
    use skrifa::MetadataProvider;
    use skrifa::instance::{LocationRef, Size};
    db.with_face_data(id, |data, index| {
        let Ok(font) = skrifa::FontRef::from_index(data, index) else {
            return false;
        };
        let charmap = font.charmap();
        let (Some(i), Some(m)) = (charmap.map('i'), charmap.map('M')) else {
            return false; // not a Latin text font
        };
        let advances = font.glyph_metrics(Size::unscaled(), LocationRef::default());
        match (advances.advance_width(i), advances.advance_width(m)) {
            (Some(narrow), Some(wide)) => narrow > 0.0 && (narrow - wide).abs() < 1.0,
            _ => false,
        }
    })
    .unwrap_or(false)
}

/// Sorted installed font families, each paired with a monospaced flag — for the picker's
/// "Monospace" filter. When `measure` is false the flag is just `fontdb`'s declaration (fast,
/// metadata only); when true it falls back to a measured advance-width check (catches fonts like
/// Monaco that don't declare it, but parses each family's font, so it runs off the UI thread).
pub fn installed_families(db: &fontdb::Database, measure: bool) -> Vec<(String, bool)> {
    use std::collections::BTreeMap;
    let mut families: BTreeMap<String, bool> = BTreeMap::new();
    for face in db.faces() {
        let Some((name, _)) = face.families.first() else {
            continue;
        };
        // Skip private system fonts: macOS hides families whose name starts with '.'.
        if name.starts_with('.') || families.contains_key(name) {
            continue;
        }
        let mono = face.monospaced || (measure && measured_monospaced(db, face.id));
        families.insert(name.clone(), mono);
    }
    families.into_iter().collect()
}

/// Build egui font definitions: OS-native proportional chrome font (+ bundled Inter + egui's
/// defaults as fallbacks), and the chosen `table_family` for data cells (falling back to the
/// proportional stack when unset or unloadable).
pub fn definitions(db: &fontdb::Database, table_family: Option<&str>) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();

    let mut proportional = Vec::new();
    if let Some(bytes) = load_family(db, os_ui_families()) {
        fonts
            .font_data
            .insert("os-ui".to_owned(), Arc::new(FontData::from_owned(bytes)));
        proportional.push("os-ui".to_owned());
    }
    fonts.font_data.insert(
        "Inter".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/InterVariable.ttf"
        ))),
    );
    proportional.push("Inter".to_owned());
    if let Some(defaults) = fonts.families.get(&FontFamily::Proportional) {
        proportional.extend(defaults.iter().cloned());
    }
    fonts
        .families
        .insert(FontFamily::Proportional, proportional.clone());

    let mut table = Vec::new();
    if let Some(name) = table_family
        && let Some(bytes) = load_family(db, &[name])
    {
        fonts
            .font_data
            .insert("table".to_owned(), Arc::new(FontData::from_owned(bytes)));
        table.push("table".to_owned());
    }
    table.extend(proportional);
    fonts
        .families
        .insert(FontFamily::Name(TABLE_FONT.into()), table);

    fonts
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    const INTER: &[u8] = include_bytes!("../assets/InterVariable.ttf");

    /// Bytes of one of egui's bundled default fonts (e.g. the monospace "Hack").
    fn egui_default_font(name: &str) -> Vec<u8> {
        FontDefinitions::default().font_data[name].font.to_vec()
    }

    /// Whether egui itself loads `bytes` as a font. egui panics on a font it can't parse when it
    /// builds the atlas (the crash `parseable` guards against), so install it and run one frame.
    fn egui_loads(bytes: &[u8]) -> bool {
        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert(
            "probe".to_owned(),
            Arc::new(FontData::from_owned(bytes.to_vec())),
        );
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "probe".to_owned());
        let ctx = egui::Context::default();
        ctx.set_fonts(fonts);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // No renderer here to apply the atlas upload; egui asserts unapplied deltas on drop.
            ctx.run_ui(egui::RawInput::default(), |_| {})
                .textures_delta
                .clear();
        }))
        .is_ok()
    }

    #[test]
    fn parseable_agrees_with_eguis_font_loader() {
        // A bare sfnt header with no tables: loadable by egui's parser, so it must not be rejected
        // (nor anything egui would choke on be accepted).
        let bare_sfnt: &[u8] = &[0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for (name, bytes) in [
            ("garbage", &b"not a font"[..]),
            ("bare sfnt", bare_sfnt),
            ("Inter", INTER),
        ] {
            assert_eq!(parseable(bytes), egui_loads(bytes), "{name}");
        }
    }

    #[test]
    fn measured_monospaced_tells_fixed_from_proportional() {
        let mut db = fontdb::Database::new();
        db.load_font_data(egui_default_font("Hack"));
        db.load_font_data(INTER.to_vec());
        let mono = |family: &str| {
            let face = db.faces().find(|f| f.families[0].0 == family).unwrap();
            measured_monospaced(&db, face.id)
        };
        assert!(mono("Hack"));
        assert!(!mono("Inter Variable"));
    }
}
