//! CodeVox's visual style, for the iced shell.
//!
//! The constants mirror [`design/tokens.json`](../design/tokens.json). A unit
//! test reads that file so the two cannot drift. The faces are the SIL OFL
//! fonts named by the tokens; their licence texts are in `assets/fonts/`.
//!
//! The placeholder only paints a subset. The rest stay so the next surface
//! can use them without a second copy of the file.

#![allow(dead_code)]

use std::sync::LazyLock;

use iced::font::Weight;
use iced::theme::palette::{self, Palette};
use iced::widget::container;
use iced::{Border, Color, Font, Shadow, Theme};

/// `#FF6A00`.
pub const ACCENT: Color = Color::from_rgba8(0xFF, 0x6A, 0x00, 1.0);

/// `#FF6A00` at half alpha.
pub const ACCENT_DIM: Color = Color::from_rgba8(0xFF, 0x6A, 0x00, 0.5);

/// Window background, `#060606`.
pub const BACKGROUND: Color = Color::from_rgba8(0x06, 0x06, 0x06, 1.0);

/// Raised surface, `#0A0A0A`.
pub const PANEL: Color = Color::from_rgba8(0x0A, 0x0A, 0x0A, 1.0);

/// `#111111`.
pub const DARK_LIGHT: Color = Color::from_rgba8(0x11, 0x11, 0x11, 1.0);

/// `#1A1A1A`.
pub const DARK_LIGHTER: Color = Color::from_rgba8(0x1A, 0x1A, 0x1A, 1.0);

/// `#FAFAFA`.
pub const TEXT_PRIMARY: Color = Color::from_rgba8(0xFA, 0xFA, 0xFA, 1.0);

/// `#FAFAFA` at 0.55 alpha.
pub const TEXT_SECONDARY: Color = Color::from_rgba8(0xFA, 0xFA, 0xFA, 0.55);

/// `#FAFAFA` at 0.4 alpha.
pub const TEXT_TERTIARY: Color = Color::from_rgba8(0xFA, 0xFA, 0xFA, 0.4);

/// `#FF5A5A`.
pub const DESTRUCTIVE: Color = Color::from_rgba8(0xFF, 0x5A, 0x5A, 1.0);

/// `#FB923C`.
pub const WARNING: Color = Color::from_rgba8(0xFB, 0x92, 0x3C, 1.0);

/// White at 0.09 alpha. One-pixel borders use this.
pub const HAIRLINE: Color = Color::from_rgba8(0xFF, 0xFF, 0xFF, 0.09);

/// Ink drawn on [`ACCENT`], `#0A0A0A`.
pub const ON_ACCENT: Color = Color::from_rgba8(0x0A, 0x0A, 0x0A, 1.0);

/// Flat Industrial HUD. Zero means no bloom and no drop shadow.
pub const GLOW: f32 = 0.0;

/// Chamfer cuts, in logical pixels. iced has no chamfer primitive yet.
pub const CHAMFER_SM: f32 = 3.0;
pub const CHAMFER_MD: f32 = 4.0;
pub const CHAMFER_LG: f32 = 7.0;

/// Legacy soft-container radii, in logical pixels.
pub const RADIUS_SM: f32 = 6.0;
pub const RADIUS_MD: f32 = 8.0;

pub const HEADING_FAMILY: &str = "Space Mono";
pub const BODY_FAMILY: &str = "IBM Plex Mono";
pub const MONO_FAMILY: &str = "IBM Plex Mono";

/// CSS fallback stack from the tokens. iced's generic for that stack is
/// [`FALLBACK`].
pub const HEADING_FALLBACKS: &[&str] = &["ui-monospace", "monospace"];
pub const BODY_FALLBACKS: &[&str] = &["ui-monospace", "monospace"];
pub const MONO_FALLBACKS: &[&str] = &["ui-monospace", "monospace"];

pub const HEADING_POSTSCRIPT_REGULAR: &str = "SpaceMono-Regular";
pub const HEADING_POSTSCRIPT_BOLD: &str = "SpaceMono-Bold";
pub const BODY_POSTSCRIPT_REGULAR: &str = "IBMPlexMono-Regular";
pub const BODY_POSTSCRIPT_MEDIUM: &str = "IBMPlexMono-Medium";
pub const MONO_POSTSCRIPT_REGULAR: &str = "IBMPlexMono-Regular";
pub const MONO_POSTSCRIPT_MEDIUM: &str = "IBMPlexMono-Medium";

/// Space Mono Bold. The family name matches the font's name table.
pub const HEADING: Font = Font {
    weight: Weight::Bold,
    ..Font::with_name(HEADING_FAMILY)
};

/// IBM Plex Mono Regular.
pub const BODY: Font = Font::with_name(BODY_FAMILY);

/// IBM Plex Mono Medium. Loaded with the other faces; the shell body uses
/// [`BODY`].
pub const BODY_MEDIUM: Font = Font {
    weight: Weight::Medium,
    ..Font::with_name(BODY_FAMILY)
};

/// Same face as [`BODY`]. The token file lists it separately.
pub const MONO: Font = BODY;

/// System monospace. The application default, matching the token fallback
/// stack (`ui-monospace`, then `monospace`).
pub const FALLBACK: Font = Font::MONOSPACE;

const SPACE_MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/SpaceMono-Regular.ttf");
const SPACE_MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/SpaceMono-Bold.ttf");
const IBM_PLEX_MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf");
const IBM_PLEX_MONO_MEDIUM: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Medium.ttf");

/// Faces loaded at startup, in the token file's PostScript names.
pub const FONTS: [&[u8]; 4] = [
    SPACE_MONO_REGULAR,
    SPACE_MONO_BOLD,
    IBM_PLEX_MONO_REGULAR,
    IBM_PLEX_MONO_MEDIUM,
];

/// iced palette built from the tokens.
///
/// The token file has no success colour. iced requires the field, and the
/// shell does not paint it, so it reuses the primary text colour.
pub const PALETTE: Palette = Palette {
    background: BACKGROUND,
    text: TEXT_PRIMARY,
    primary: ACCENT,
    success: TEXT_PRIMARY,
    warning: WARNING,
    danger: DESTRUCTIVE,
};

static CODEVOX: LazyLock<Theme> =
    LazyLock::new(|| Theme::custom_with_fn("CodeVox", PALETTE, extend));

/// The shell theme. Built once; cloning only clones an `Arc`.
pub fn theme() -> Theme {
    CODEVOX.clone()
}

/// Panel surface: token fill, hairline border, no shadow ([`GLOW`] is 0).
///
/// The corner radius stays square. [`CHAMFER_SM`] and the radius tokens are
/// cuts and legacy soft corners; iced draws a radius, not a chamfer.
pub fn panel(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: Some(TEXT_PRIMARY),
        background: Some(PANEL.into()),
        border: Border {
            color: HAIRLINE,
            width: 1.0,
            radius: 0.0.into(),
        },
        // `glow` is the blur. Zero is the flat HUD, so this is no shadow.
        shadow: Shadow {
            blur_radius: GLOW,
            ..Shadow::default()
        },
        ..container::Style::default()
    }
}

fn extend(palette: Palette) -> palette::Extended {
    let mut extended = palette::Extended::generate(palette);

    // Generated slots are mixes. These are the token values.
    // weak → weakest steps up from the background: panel, darkLight, darkLighter.
    extended.background.base.color = palette.background;
    extended.background.base.text = TEXT_PRIMARY;
    extended.background.weak.color = PANEL;
    extended.background.weak.text = TEXT_SECONDARY;
    extended.background.weaker.color = DARK_LIGHT;
    extended.background.weaker.text = TEXT_SECONDARY;
    extended.background.weakest.color = DARK_LIGHTER;
    extended.background.weakest.text = TEXT_TERTIARY;
    extended.primary.base.color = palette.primary;
    extended.primary.base.text = ON_ACCENT;
    extended.primary.weak.color = ACCENT_DIM;
    extended.primary.weak.text = ON_ACCENT;
    extended.is_dark = true;

    extended
}

#[cfg(test)]
mod tests {
    use super::*;

    use iced::theme::Base;
    use serde_json::Value;

    #[test]
    fn constants_match_design_tokens() {
        let tokens: Value = serde_json::from_str(include_str!("../design/tokens.json"))
            .expect("design/tokens.json is json");
        let root = tokens.as_object().expect("object");

        for key in root.keys() {
            assert!(
                key.starts_with('$')
                    || matches!(
                        key.as_str(),
                        "color" | "scalar" | "chamfer" | "radius" | "type"
                    ),
                "unexpected token key {key}"
            );
        }

        let note = tokens["$note"].as_str().expect("$note");
        assert!(
            note.contains("Mirrors CodeVox"),
            "header note should say this file mirrors CodeVox's design system"
        );

        let colors = tokens["color"].as_object().expect("color");
        let mut color_names: Vec<_> = colors.keys().cloned().collect();
        color_names.sort();
        assert_eq!(
            color_names,
            [
                "accent",
                "accentDim",
                "background",
                "darkLight",
                "darkLighter",
                "destructive",
                "hairline",
                "onAccent",
                "panel",
                "textPrimary",
                "textSecondary",
                "textTertiary",
                "warning",
            ]
        );

        assert_color(ACCENT, &colors["accent"]);
        assert_color(ACCENT_DIM, &colors["accentDim"]);
        assert_color(BACKGROUND, &colors["background"]);
        assert_color(PANEL, &colors["panel"]);
        assert_color(DARK_LIGHT, &colors["darkLight"]);
        assert_color(DARK_LIGHTER, &colors["darkLighter"]);
        assert_color(TEXT_PRIMARY, &colors["textPrimary"]);
        assert_color(TEXT_SECONDARY, &colors["textSecondary"]);
        assert_color(TEXT_TERTIARY, &colors["textTertiary"]);
        assert_color(DESTRUCTIVE, &colors["destructive"]);
        assert_color(WARNING, &colors["warning"]);
        assert_color(HAIRLINE, &colors["hairline"]);
        assert_color(ON_ACCENT, &colors["onAccent"]);

        let scalar = tokens["scalar"].as_object().expect("scalar");
        assert_eq!(scalar.len(), 1);
        assert_number(&scalar["glow"], GLOW);

        assert_number(&tokens["chamfer"]["sm"], CHAMFER_SM);
        assert_number(&tokens["chamfer"]["md"], CHAMFER_MD);
        assert_number(&tokens["chamfer"]["lg"], CHAMFER_LG);
        assert_eq!(tokens["chamfer"].as_object().expect("chamfer").len(), 3);

        assert_number(&tokens["radius"]["sm"], RADIUS_SM);
        assert_number(&tokens["radius"]["md"], RADIUS_MD);
        assert_eq!(tokens["radius"].as_object().expect("radius").len(), 2);

        assert_face(
            &tokens["type"]["heading"],
            HEADING_FAMILY,
            HEADING_FALLBACKS,
            &[
                ("regular", HEADING_POSTSCRIPT_REGULAR),
                ("bold", HEADING_POSTSCRIPT_BOLD),
            ],
        );
        assert_face(
            &tokens["type"]["body"],
            BODY_FAMILY,
            BODY_FALLBACKS,
            &[
                ("regular", BODY_POSTSCRIPT_REGULAR),
                ("medium", BODY_POSTSCRIPT_MEDIUM),
            ],
        );
        assert_face(
            &tokens["type"]["mono"],
            MONO_FAMILY,
            MONO_FALLBACKS,
            &[
                ("regular", MONO_POSTSCRIPT_REGULAR),
                ("medium", MONO_POSTSCRIPT_MEDIUM),
            ],
        );
        assert_eq!(tokens["type"].as_object().expect("type").len(), 3);
    }

    #[test]
    fn theme_palette_uses_tokens() {
        let theme = theme();
        assert_eq!(theme.name(), "CodeVox");

        let palette = theme.palette();
        assert_eq!(palette, PALETTE);
        assert_eq!(palette.background, BACKGROUND);
        assert_eq!(palette.text, TEXT_PRIMARY);
        assert_eq!(palette.primary, ACCENT);
        assert_eq!(palette.warning, WARNING);
        assert_eq!(palette.danger, DESTRUCTIVE);

        let extended = theme.extended_palette();
        assert!(extended.is_dark);
        assert_eq!(extended.background.base.color, BACKGROUND);
        assert_eq!(extended.background.base.text, TEXT_PRIMARY);
        assert_eq!(extended.background.weak.color, PANEL);
        assert_eq!(extended.background.weak.text, TEXT_SECONDARY);
        assert_eq!(extended.background.weaker.color, DARK_LIGHT);
        assert_eq!(extended.background.weakest.color, DARK_LIGHTER);
        assert_eq!(extended.background.weakest.text, TEXT_TERTIARY);
        assert_eq!(extended.primary.base.color, ACCENT);
        assert_eq!(extended.primary.base.text, ON_ACCENT);
        assert_eq!(extended.primary.weak.color, ACCENT_DIM);
        assert_eq!(extended.danger.base.color, DESTRUCTIVE);
        assert_eq!(extended.warning.base.color, WARNING);
    }

    #[test]
    fn panel_is_flat_with_a_hairline() {
        let style = panel(&theme());
        assert_eq!(style.background, Some(PANEL.into()));
        assert_eq!(style.text_color, Some(TEXT_PRIMARY));
        assert_eq!(style.border.color, HAIRLINE);
        assert_eq!(style.border.width, 1.0);
        assert_eq!(style.border.radius, 0.0.into());
        assert_eq!(style.shadow, Shadow::default());
        assert_eq!(GLOW, 0.0);
    }

    #[test]
    fn bundled_fonts_match_token_names_and_carry_the_ofl() {
        let expected = [
            (HEADING_FAMILY, HEADING_POSTSCRIPT_REGULAR),
            (HEADING_FAMILY, HEADING_POSTSCRIPT_BOLD),
            (BODY_FAMILY, BODY_POSTSCRIPT_REGULAR),
            (BODY_FAMILY, BODY_POSTSCRIPT_MEDIUM),
        ];
        assert_eq!(FONTS.len(), expected.len());

        for (bytes, (family, postscript)) in FONTS.iter().zip(expected) {
            assert_truetype(bytes);
            let names = font_names(bytes);
            assert!(
                names
                    .iter()
                    .any(|(id, text)| (*id == 1 || *id == 16) && text == family),
                "{postscript} should name the family {family}, got {names:?}"
            );
            assert!(
                names
                    .iter()
                    .any(|(id, text)| *id == 6 && text == postscript),
                "{postscript} missing from name table {names:?}"
            );
        }

        let space_mono = include_str!("../assets/fonts/SpaceMono-OFL.txt");
        let plex = include_str!("../assets/fonts/IBMPlexMono-OFL.txt");
        assert!(space_mono.contains("SIL Open Font License"));
        assert!(space_mono.contains("Space Mono Project Authors"));
        assert!(plex.contains("SIL Open Font License"));
        assert!(plex.contains("IBM Corp."));
        assert!(plex.contains("Reserved Font Name \"Plex\""));
    }

    fn assert_color(actual: Color, token: &Value) {
        let hex = token["hex"].as_str().expect("hex");
        let alpha = token["alpha"].as_f64().expect("alpha");
        assert_eq!(token.as_object().expect("color fields").len(), 2, "{hex}");
        assert!(
            hex.starts_with('#') && hex.len() == 7,
            "expected #RRGGBB, got {hex}"
        );

        let expected = Color::from_rgba8(
            hex_byte(hex, 0),
            hex_byte(hex, 1),
            hex_byte(hex, 2),
            alpha as f32,
        );
        assert!(
            (actual.r - expected.r).abs() < 1e-5
                && (actual.g - expected.g).abs() < 1e-5
                && (actual.b - expected.b).abs() < 1e-5
                && (actual.a - expected.a).abs() < 1e-5,
            "{hex}: {actual:?} != {expected:?}"
        );
    }

    fn hex_byte(hex: &str, index: usize) -> u8 {
        let start = 1 + index * 2;
        u8::from_str_radix(&hex[start..start + 2], 16).expect("hex byte")
    }

    fn assert_number(token: &Value, value: f32) {
        let parsed = token.as_f64().expect("number");
        assert!(
            (parsed - f64::from(value)).abs() < 1e-9,
            "{parsed} != {value}"
        );
    }

    fn assert_face(token: &Value, family: &str, fallbacks: &[&str], postscript: &[(&str, &str)]) {
        assert_eq!(token["family"].as_str().expect("family"), family);
        let got: Vec<_> = token["fallbacks"]
            .as_array()
            .expect("fallbacks")
            .iter()
            .map(|value| value.as_str().expect("fallback"))
            .collect();
        assert_eq!(got, fallbacks);

        let names = token["postscript"].as_object().expect("postscript");
        assert_eq!(names.len(), postscript.len());
        for (key, value) in postscript {
            assert_eq!(names[*key].as_str().expect(key), *value);
        }
    }

    fn assert_truetype(bytes: &[u8]) {
        assert!(bytes.len() > 12, "font file is empty");
        // TrueType scaler type, big-endian 0x00010000. OpenType-CFF would be "OTTO".
        assert_eq!(&bytes[0..4], &[0x00, 0x01, 0x00, 0x00]);
    }

    /// Name IDs 1, 6, and 16 from the `name` table (family, PostScript, typographic family).
    fn font_names(bytes: &[u8]) -> Vec<(u16, String)> {
        let table = table(bytes, *b"name");
        let count = u16::from_be_bytes([table[2], table[3]]) as usize;
        let string_offset = u16::from_be_bytes([table[4], table[5]]) as usize;
        let mut names = Vec::new();

        for index in 0..count {
            let record = 6 + index * 12;
            let platform = u16::from_be_bytes([table[record], table[record + 1]]);
            let name_id = u16::from_be_bytes([table[record + 6], table[record + 7]]);
            let length = u16::from_be_bytes([table[record + 8], table[record + 9]]) as usize;
            let offset = u16::from_be_bytes([table[record + 10], table[record + 11]]) as usize;
            if !matches!(name_id, 1 | 6 | 16) {
                continue;
            }

            let raw = &table[string_offset + offset..string_offset + offset + length];
            let text = if platform == 3 {
                decode_utf16_be(raw)
            } else {
                String::from_utf8_lossy(raw).into_owned()
            };
            names.push((name_id, text));
        }

        names
    }

    fn table(bytes: &[u8], tag: [u8; 4]) -> &[u8] {
        let tables = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
        for index in 0..tables {
            let record = 12 + index * 16;
            if bytes[record..record + 4] != tag {
                continue;
            }
            let offset =
                u32::from_be_bytes(bytes[record + 8..record + 12].try_into().unwrap()) as usize;
            let length =
                u32::from_be_bytes(bytes[record + 12..record + 16].try_into().unwrap()) as usize;
            return &bytes[offset..offset + length];
        }
        panic!("font has no {} table", String::from_utf8_lossy(&tag));
    }

    fn decode_utf16_be(raw: &[u8]) -> String {
        let (pairs, _) = raw.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_be_bytes(*pair)).collect();
        String::from_utf16(&units).expect("utf-16 name")
    }
}
