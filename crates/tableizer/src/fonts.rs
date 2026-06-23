//! Font management: an OS-native chrome font (with bundled Inter as a cross-platform fallback) and a
//! user-selectable data-cell font, both resolved from the system font database via `fontdb`.

use eframe::egui::{FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

/// egui custom-family name used for data cells.
pub const TABLE_FONT: &str = "table";

/// Whether ab_glyph (egui's font backend) can parse these bytes — guards the atlas build against
/// unsupported fonts crashing the app.
fn parseable(data: &[u8]) -> bool {
    ab_glyph::FontRef::try_from_slice(data).is_ok()
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
    use ab_glyph::Font;
    db.with_face_data(id, |data, index| {
        let Ok(font) = ab_glyph::FontRef::try_from_slice_and_index(data, index) else {
            return false;
        };
        let (i, m) = (font.glyph_id('i'), font.glyph_id('M'));
        if i.0 == 0 || m.0 == 0 {
            return false; // not a Latin text font
        }
        let narrow = font.h_advance_unscaled(i);
        let wide = font.h_advance_unscaled(m);
        narrow > 0.0 && (narrow - wide).abs() < 1.0
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
