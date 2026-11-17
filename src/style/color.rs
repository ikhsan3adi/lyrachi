use std::fmt;

use ratatui::style::Color;
use serde::{Deserialize, Deserializer};

const MAX_FADE_MIX: f32 = 0.85;

/// User preference for terminal color depth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    #[default]
    Auto,
    Truecolor,
    #[serde(rename = "256")]
    Ansi256,
    #[serde(rename = "16")]
    Ansi16,
}

impl ColorMode {
    /// Canonical lowercase configuration string for this color mode.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Truecolor => "truecolor",
            Self::Ansi256 => "256",
            Self::Ansi16 => "16",
        }
    }
}

/// Detected or forced terminal color capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ColorCapability {
    #[default]
    TrueColor,
    Ansi256,
    Ansi16,
    NoColor,
}

/// Environment variables consulted during `color_mode = "auto"` detection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalEnv<'a> {
    pub no_color: Option<&'a str>,
    pub colorterm: Option<&'a str>,
    pub term: Option<&'a str>,
}

/// Detect terminal color capability deterministically without mutating process env.
///
/// Precedence in `Auto` mode follows `NO_COLOR > COLORTERM > TERM`.
pub fn detect_color_capability(mode: ColorMode, env: TerminalEnv<'_>) -> ColorCapability {
    match mode {
        ColorMode::Truecolor => ColorCapability::TrueColor,
        ColorMode::Ansi256 => ColorCapability::Ansi256,
        ColorMode::Ansi16 => ColorCapability::Ansi16,
        ColorMode::Auto => {
            if env.no_color.is_some_and(|value| !value.is_empty()) {
                return ColorCapability::NoColor;
            }
            if let Some(colorterm) = env.colorterm {
                let lower = colorterm.trim().to_ascii_lowercase();
                if lower.contains("truecolor") || lower.contains("24bit") {
                    return ColorCapability::TrueColor;
                }
            }
            if let Some(term) = env.term {
                let lower = term.trim().to_ascii_lowercase();
                if lower == "dumb" {
                    return ColorCapability::NoColor;
                }
                if lower.contains("truecolor")
                    || lower.contains("24bit")
                    || lower.contains("direct")
                {
                    return ColorCapability::TrueColor;
                }
                if lower.contains("256color") {
                    return ColorCapability::Ansi256;
                }
            }
            ColorCapability::Ansi16
        }
    }
}

/// Read `NO_COLOR`, `COLORTERM`, and `TERM` from the current process environment.
pub fn detect_process_color_capability(mode: ColorMode) -> ColorCapability {
    let no_color = std::env::var_os("NO_COLOR");
    let colorterm = std::env::var_os("COLORTERM");
    let term = std::env::var_os("TERM");

    detect_color_capability(
        mode,
        TerminalEnv {
            no_color: no_color.as_deref().and_then(|value| value.to_str()),
            colorterm: colorterm.as_deref().and_then(|value| value.to_str()),
            term: term.as_deref().and_then(|value| value.to_str()),
        },
    )
}

/// Wrapper around `ratatui::style::Color` supporting TOML color string parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StyleColor(pub Color);

impl Default for StyleColor {
    fn default() -> Self {
        Self(Color::Reset)
    }
}

impl StyleColor {
    /// Wrap an existing ratatui `Color`.
    pub const fn new(color: Color) -> Self {
        Self(color)
    }

    /// Resolve to a concrete ratatui color under the target terminal capability.
    ///
    /// Returns `None` for `Color::Reset` or `ColorCapability::NoColor`.
    pub fn resolve(self, capability: ColorCapability) -> Option<Color> {
        degrade_color(self.0, capability)
    }

    /// Return true if this color keeps the terminal default foreground/background.
    pub fn is_reset(self) -> bool {
        self.0 == Color::Reset
    }
}

impl<'de> Deserialize<'de> for StyleColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        parse_color(&raw)
            .map(StyleColor)
            .map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for StyleColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Color::Reset => f.write_str("reset"),
            Color::Rgb(r, g, b) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            Color::Indexed(index) => write!(f, "indexed({index})"),
            Color::Black => f.write_str("black"),
            Color::Red => f.write_str("red"),
            Color::Green => f.write_str("green"),
            Color::Yellow => f.write_str("yellow"),
            Color::Blue => f.write_str("blue"),
            Color::Magenta => f.write_str("magenta"),
            Color::Cyan => f.write_str("cyan"),
            Color::Gray => f.write_str("gray"),
            Color::DarkGray => f.write_str("dark_gray"),
            Color::LightRed => f.write_str("light_red"),
            Color::LightGreen => f.write_str("light_green"),
            Color::LightYellow => f.write_str("light_yellow"),
            Color::LightBlue => f.write_str("light_blue"),
            Color::LightMagenta => f.write_str("light_magenta"),
            Color::LightCyan => f.write_str("light_cyan"),
            Color::White => f.write_str("white"),
        }
    }
}

/// Parse `"reset"`, `#RRGGBB` / `#RGB` hex, or a named ANSI color into `Color`.
pub fn parse_color(raw: &str) -> Result<Color, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("color value cannot be empty".to_string());
    }

    if trimmed.starts_with('#') {
        return parse_hex_color(trimmed).map(|(r, g, b)| Color::Rgb(r, g, b));
    }

    let normalized = trimmed.to_ascii_lowercase().replace(['-', ' '], "_");
    match normalized.as_str() {
        "reset" | "default" => Ok(Color::Reset),
        "black" => Ok(Color::Black),
        "red" => Ok(Color::Red),
        "green" => Ok(Color::Green),
        "yellow" => Ok(Color::Yellow),
        "blue" => Ok(Color::Blue),
        "magenta" => Ok(Color::Magenta),
        "cyan" => Ok(Color::Cyan),
        "gray" | "grey" => Ok(Color::Gray),
        "dark_gray" | "darkgray" | "dark_grey" | "darkgrey" | "bright_black" => Ok(Color::DarkGray),
        "light_red" | "lightred" | "bright_red" => Ok(Color::LightRed),
        "light_green" | "lightgreen" | "bright_green" => Ok(Color::LightGreen),
        "light_yellow" | "lightyellow" | "bright_yellow" => Ok(Color::LightYellow),
        "light_blue" | "lightblue" | "bright_blue" => Ok(Color::LightBlue),
        "light_magenta" | "lightmagenta" | "bright_magenta" => Ok(Color::LightMagenta),
        "light_cyan" | "lightcyan" | "bright_cyan" => Ok(Color::LightCyan),
        "white" | "bright_white" => Ok(Color::White),
        _ => Err(format!("invalid color `{trimmed}`")),
    }
}

/// Parse a concrete color hint (hex or named ANSI, not `"reset"`) into RGB.
pub fn parse_rgb_hint(raw: &str) -> Result<(u8, u8, u8), String> {
    let color = parse_color(raw)?;
    color_to_rgb(color).ok_or_else(|| "bg_blend_hint cannot be `reset`".to_string())
}

fn parse_hex_color(raw: &str) -> Result<(u8, u8, u8), String> {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16)
                .map_err(|_| format!("invalid hex color `{raw}`"))?;
            let g = u8::from_str_radix(&hex[2..4], 16)
                .map_err(|_| format!("invalid hex color `{raw}`"))?;
            let b = u8::from_str_radix(&hex[4..6], 16)
                .map_err(|_| format!("invalid hex color `{raw}`"))?;
            Ok((r, g, b))
        }
        3 => {
            let mut nibbles = [0u8; 3];
            for (idx, ch) in hex.chars().enumerate() {
                let value = u8::from_str_radix(&ch.to_string(), 16)
                    .map_err(|_| format!("invalid hex color `{raw}`"))?;
                nibbles[idx] = value * 17;
            }
            Ok((nibbles[0], nibbles[1], nibbles[2]))
        }
        _ => Err(format!(
            "invalid hex color `{raw}` (expected #RRGGBB or #RGB)"
        )),
    }
}

/// Convert a concrete color to RGB, returning `None` for `Color::Reset`.
pub fn color_to_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(index) => Some(xterm_256_to_rgb(index)),
        Color::Black => Some(ANSI_16_PALETTE[0].1),
        Color::Red => Some(ANSI_16_PALETTE[1].1),
        Color::Green => Some(ANSI_16_PALETTE[2].1),
        Color::Yellow => Some(ANSI_16_PALETTE[3].1),
        Color::Blue => Some(ANSI_16_PALETTE[4].1),
        Color::Magenta => Some(ANSI_16_PALETTE[5].1),
        Color::Cyan => Some(ANSI_16_PALETTE[6].1),
        Color::Gray => Some(ANSI_16_PALETTE[7].1),
        Color::DarkGray => Some(ANSI_16_PALETTE[8].1),
        Color::LightRed => Some(ANSI_16_PALETTE[9].1),
        Color::LightGreen => Some(ANSI_16_PALETTE[10].1),
        Color::LightYellow => Some(ANSI_16_PALETTE[11].1),
        Color::LightBlue => Some(ANSI_16_PALETTE[12].1),
        Color::LightMagenta => Some(ANSI_16_PALETTE[13].1),
        Color::LightCyan => Some(ANSI_16_PALETTE[14].1),
        Color::White => Some(ANSI_16_PALETTE[15].1),
    }
}

/// Degrade a color to the target terminal capability (`TrueColor -> 256 -> 16 -> NoColor`).
pub fn degrade_color(color: Color, capability: ColorCapability) -> Option<Color> {
    if color == Color::Reset || capability == ColorCapability::NoColor {
        return None;
    }

    match capability {
        ColorCapability::TrueColor => Some(color),
        ColorCapability::Ansi256 => match color {
            Color::Rgb(r, g, b) => Some(Color::Indexed(rgb_to_xterm_256(r, g, b))),
            other => Some(other),
        },
        ColorCapability::Ansi16 => match color {
            Color::Rgb(r, g, b) => Some(nearest_ansi_16(r, g, b)),
            Color::Indexed(index) => {
                let (r, g, b) = xterm_256_to_rgb(index);
                Some(nearest_ansi_16(r, g, b))
            }
            other => Some(other),
        },
        ColorCapability::NoColor => None,
    }
}

/// Interpolation curve for distance-based line fading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadeCurve {
    #[default]
    Linear,
    EaseOut,
    Step,
}

/// Resolved foreground color and dim hint after applying `lines.fade`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FadeResult {
    pub fg: Option<Color>,
    pub dim: bool,
}

/// Blend line foreground safely with respect to `Color::Reset` and `bg_blend_hint`.
///
/// Distance `0` uses `active_fg`, distance `1` uses `inactive_fg`, and distances `>= 2`
/// progress along `curve` * `falloff` toward `fade_bg_rgb` when both source and background
/// RGB values are known. When either endpoint is `Color::Reset` without a hint, the result
/// falls back to `Modifier::DIM` without fabricating an arbitrary RGB background.
pub fn blend_fade_color(
    active_fg: Color,
    inactive_fg: Color,
    fade_bg_rgb: Option<(u8, u8, u8)>,
    distance: usize,
    curve: FadeCurve,
    falloff: f32,
    capability: ColorCapability,
) -> FadeResult {
    if capability == ColorCapability::NoColor {
        return FadeResult {
            fg: None,
            dim: distance > 0,
        };
    }

    if distance == 0 {
        return FadeResult {
            fg: degrade_color(active_fg, capability),
            dim: false,
        };
    }

    let falloff = falloff.clamp(0.0, 1.0);
    if distance == 1 || falloff <= 0.0 {
        return FadeResult {
            fg: degrade_color(inactive_fg, capability),
            dim: false,
        };
    }

    let steps = (distance.saturating_sub(1) as f32 * falloff).clamp(0.0, 1.0);
    let progress = match curve {
        FadeCurve::Linear => steps,
        FadeCurve::EaseOut => {
            let inv = 1.0 - steps;
            1.0 - inv * inv
        }
        FadeCurve::Step => {
            if steps > 0.0 {
                1.0
            } else {
                0.0
            }
        }
    };

    let Some(bg_rgb) = fade_bg_rgb else {
        return FadeResult {
            fg: degrade_color(inactive_fg, capability),
            dim: progress > 0.0,
        };
    };

    let Some((src_rgb, mix)) = resolve_fade_source(active_fg, inactive_fg, progress) else {
        return FadeResult {
            fg: None,
            dim: progress > 0.0,
        };
    };

    let blended = lerp_rgb(src_rgb, bg_rgb, mix);
    FadeResult {
        fg: degrade_color(Color::Rgb(blended.0, blended.1, blended.2), capability),
        dim: capability == ColorCapability::Ansi16 && progress >= 0.35,
    }
}

fn resolve_fade_source(
    active_fg: Color,
    inactive_fg: Color,
    progress: f32,
) -> Option<((u8, u8, u8), f32)> {
    if let Some(inactive_rgb) = color_to_rgb(inactive_fg) {
        return Some((
            inactive_rgb,
            (progress * MAX_FADE_MIX).clamp(0.0, MAX_FADE_MIX),
        ));
    }
    color_to_rgb(active_fg).map(|active_rgb| {
        let mix = (0.35 + progress * (MAX_FADE_MIX - 0.35)).clamp(0.0, MAX_FADE_MIX);
        (active_rgb, mix)
    })
}

fn lerp_rgb(from: (u8, u8, u8), to: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let blend = |a: u8, b: u8| -> u8 {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (
        blend(from.0, to.0),
        blend(from.1, to.1),
        blend(from.2, to.2),
    )
}

const ANSI_16_PALETTE: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (205, 49, 49)),
    (Color::Green, (13, 188, 121)),
    (Color::Yellow, (229, 229, 16)),
    (Color::Blue, (36, 114, 200)),
    (Color::Magenta, (188, 63, 188)),
    (Color::Cyan, (17, 168, 205)),
    (Color::Gray, (229, 229, 229)),
    (Color::DarkGray, (102, 102, 102)),
    (Color::LightRed, (241, 76, 76)),
    (Color::LightGreen, (35, 209, 139)),
    (Color::LightYellow, (245, 245, 67)),
    (Color::LightBlue, (59, 142, 234)),
    (Color::LightMagenta, (214, 112, 214)),
    (Color::LightCyan, (41, 184, 219)),
    (Color::White, (255, 255, 255)),
];

const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn xterm_256_to_rgb(index: u8) -> (u8, u8, u8) {
    if usize::from(index) < ANSI_16_PALETTE.len() {
        return ANSI_16_PALETTE[usize::from(index)].1;
    }
    if index >= 232 {
        let level = 8 + (index - 232) * 10;
        return (level, level, level);
    }
    let idx = index - 16;
    let r = CUBE_LEVELS[usize::from(idx / 36)];
    let g = CUBE_LEVELS[usize::from((idx % 36) / 6)];
    let b = CUBE_LEVELS[usize::from(idx % 6)];
    (r, g, b)
}

fn rgb_to_xterm_256(r: u8, g: u8, b: u8) -> u8 {
    let cube_idx = |value: u8| -> u8 {
        CUBE_LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| value.abs_diff(**level))
            .map_or(0, |(idx, _)| idx as u8)
    };
    let cr = cube_idx(r);
    let cg = cube_idx(g);
    let cb = cube_idx(b);
    let cube_index = 16 + 36 * cr + 6 * cg + cb;
    let cube_rgb = (
        CUBE_LEVELS[usize::from(cr)],
        CUBE_LEVELS[usize::from(cg)],
        CUBE_LEVELS[usize::from(cb)],
    );

    let avg = ((u16::from(r) + u16::from(g) + u16::from(b)) / 3) as u8;
    let gray_step = if avg < 8 {
        0
    } else if avg > 238 {
        23
    } else {
        (avg - 8) / 10
    };
    let gray_index = 232 + gray_step;
    let gray_val = 8 + gray_step * 10;
    let gray_rgb = (gray_val, gray_val, gray_val);

    if rgb_dist_sq((r, g, b), gray_rgb) < rgb_dist_sq((r, g, b), cube_rgb) {
        gray_index
    } else {
        cube_index
    }
}

fn nearest_ansi_16(r: u8, g: u8, b: u8) -> Color {
    ANSI_16_PALETTE
        .iter()
        .min_by_key(|(_, palette_rgb)| rgb_dist_sq((r, g, b), *palette_rgb))
        .map_or(Color::White, |(color, _)| *color)
}

fn rgb_dist_sq(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let dr = i32::from(a.0) - i32::from(b.0);
    let dg = i32::from(a.1) - i32::from(b.1);
    let db = i32::from(a.2) - i32::from(b.2);
    (dr * dr + dg * dg + db * db) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_named_and_reset_colors() {
        assert_eq!(parse_color("reset"), Ok(Color::Reset));
        assert_eq!(parse_color("#7aa2f7"), Ok(Color::Rgb(0x7a, 0xa2, 0xf7)));
        assert_eq!(parse_color("#abc"), Ok(Color::Rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(parse_color("LightMagenta"), Ok(Color::LightMagenta));
        assert_eq!(parse_color("dark_gray"), Ok(Color::DarkGray));
        assert!(parse_color("#12345").is_err());
        assert!(parse_color("not-a-color").is_err());
    }

    #[test]
    fn detects_color_capability_with_deterministic_precedence() {
        // NO_COLOR overrides COLORTERM and TERM in auto mode.
        assert_eq!(
            detect_color_capability(
                ColorMode::Auto,
                TerminalEnv {
                    no_color: Some("1"),
                    colorterm: Some("truecolor"),
                    term: Some("xterm-256color"),
                },
            ),
            ColorCapability::NoColor
        );

        // Empty NO_COLOR is ignored per no-color.org.
        assert_eq!(
            detect_color_capability(
                ColorMode::Auto,
                TerminalEnv {
                    no_color: Some(""),
                    colorterm: Some("truecolor"),
                    term: Some("xterm"),
                },
            ),
            ColorCapability::TrueColor
        );

        // COLORTERM=24bit yields TrueColor.
        assert_eq!(
            detect_color_capability(
                ColorMode::Auto,
                TerminalEnv {
                    no_color: None,
                    colorterm: Some("24bit"),
                    term: Some("xterm"),
                },
            ),
            ColorCapability::TrueColor
        );

        // TERM fallback distinguishes 256color, dumb, and basic 16-color terminals.
        assert_eq!(
            detect_color_capability(
                ColorMode::Auto,
                TerminalEnv {
                    no_color: None,
                    colorterm: None,
                    term: Some("xterm-256color"),
                },
            ),
            ColorCapability::Ansi256
        );
        assert_eq!(
            detect_color_capability(
                ColorMode::Auto,
                TerminalEnv {
                    no_color: None,
                    colorterm: None,
                    term: Some("dumb"),
                },
            ),
            ColorCapability::NoColor
        );
        assert_eq!(
            detect_color_capability(
                ColorMode::Auto,
                TerminalEnv {
                    no_color: None,
                    colorterm: None,
                    term: Some("xterm"),
                },
            ),
            ColorCapability::Ansi16
        );

        // Explicit color_mode overrides environment auto-detection.
        assert_eq!(
            detect_color_capability(
                ColorMode::Ansi256,
                TerminalEnv {
                    no_color: Some("1"),
                    colorterm: Some("truecolor"),
                    term: Some("dumb"),
                },
            ),
            ColorCapability::Ansi256
        );
    }

    #[test]
    fn degrades_truecolor_to_256_and_16_deterministically() {
        let accent = Color::Rgb(0x7a, 0xa2, 0xf7);
        assert_eq!(
            degrade_color(accent, ColorCapability::TrueColor),
            Some(accent)
        );
        assert!(matches!(
            degrade_color(accent, ColorCapability::Ansi256),
            Some(Color::Indexed(_))
        ));
        assert_eq!(
            degrade_color(accent, ColorCapability::Ansi16),
            Some(Color::LightBlue)
        );
        assert_eq!(degrade_color(accent, ColorCapability::NoColor), None);
        assert_eq!(
            degrade_color(Color::Reset, ColorCapability::TrueColor),
            None
        );
    }

    #[test]
    fn fade_handles_color_reset_safely_without_inventing_black() {
        let no_hint = blend_fade_color(
            Color::Reset,
            Color::Reset,
            None,
            2,
            FadeCurve::Linear,
            0.5,
            ColorCapability::TrueColor,
        );
        assert_eq!(
            no_hint,
            FadeResult {
                fg: None,
                dim: true
            }
        );

        let concrete_without_hint = blend_fade_color(
            Color::Rgb(240, 240, 240),
            Color::Rgb(150, 150, 180),
            None,
            3,
            FadeCurve::EaseOut,
            0.4,
            ColorCapability::TrueColor,
        );
        assert_eq!(
            concrete_without_hint,
            FadeResult {
                fg: Some(Color::Rgb(150, 150, 180)),
                dim: true,
            }
        );

        let reset_inactive_with_active_and_hint = blend_fade_color(
            Color::Rgb(220, 220, 240),
            Color::Reset,
            Some((20, 20, 30)),
            2,
            FadeCurve::Linear,
            0.5,
            ColorCapability::TrueColor,
        );
        match reset_inactive_with_active_and_hint.fg {
            Some(Color::Rgb(r, g, b)) => {
                assert!(r < 220 && r > 20);
                assert!(g < 220 && g > 20);
                assert!(b < 240 && b > 30);
            }
            other => panic!("expected blended RGB color, got {other:?}"),
        }
    }
}
