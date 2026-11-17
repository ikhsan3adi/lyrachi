pub mod color;
pub mod loader;

use std::collections::BTreeMap;
use std::path::Path;

use ratatui::{
    layout::Alignment,
    style::{Color, Modifier, Style},
};
use serde::{Deserialize, Deserializer};
use tachyonfx::ColorSpace as FxColorSpace;

use crate::core::model::LineRole;

use self::color::{
    ColorCapability, FadeCurve, StyleColor, blend_fade_color, color_to_rgb, degrade_color,
};

pub const DEFAULT_STYLE_NAME: &str = "builtin:default";
pub const BUILTIN_PRESETS: &[&str] = &[
    "builtin:default",
    "builtin:dark",
    "builtin:aurora",
    "builtin:minimal",
];

/// List all available style presets, including built-ins and user `.toml` styles in `styles_dir`.
pub fn available_style_names(styles_dir: Option<&Path>) -> Vec<String> {
    let mut names = BUILTIN_PRESETS
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    if let Some(dir) = styles_dir
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        let mut custom = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml")
                && path.is_file()
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !stem.starts_with('.')
                && !custom.contains(&stem.to_string())
            {
                custom.push(stem.to_string());
            }
        }
        custom.sort();
        names.extend(custom);
    }

    names
}

/// Advance to the next style preset, wrapping around to the first when the end is reached.
pub fn cycle_next_style(current: &str, styles_dir: Option<&Path>) -> String {
    let all = available_style_names(styles_dir);
    if all.is_empty() {
        return DEFAULT_STYLE_NAME.to_string();
    }
    if let Some(pos) = all.iter().position(|s| s == current) {
        all[(pos + 1) % all.len()].clone()
    } else {
        all[0].clone()
    }
}

const PADDING_MAX: u16 = 32;
const LINE_SPACING_MAX: u16 = 4;
const VISIBLE_CONTEXT_MAX: usize = 10;
const MIN_GAP_MS_MAX: u64 = 60_000;
const DUET_RIGHT_ALIGN_MIN_WIDTH: u16 = 60;
const HIGHLIGHT_TRAIL_MAX: u16 = 32;
const ANIMATION_EFFECTS_MAX: usize = 8;
const ANIMATION_EFFECT_DURATION_MAX_MS: u32 = 5_000;

const PRESET_DEFAULT_TOML: &str = r##"
schema_version = 1

[meta]
name = "default"
author = "lyrachi"
description = "Terminal-default colors with word-synced cyan highlight"

[palette]
bg = "reset"
fg = "reset"
accent = "cyan"
dim = "reset"
duet = "light_magenta"
backing = "dark_gray"
backing_active = "gray"

[layout]
align = "center"
padding = [0, 2]
line_spacing = 0
visible_prev = 2
visible_next = 3

[lines.active]
fg = "$fg"
modifiers = []
prefix = ""

[lines.inactive]
fg = "$dim"
modifiers = ["dim"]
prefix = ""

[lines.fade]
curve = "linear"
falloff = 0.0

[highlight]
mode = "word"
active_fg = "$accent"

[instrumental]
style = "dots"
min_gap_ms = 4000
text = "♪"

[annotations]
show = true
position = "below"
fg = "reset"
modifiers = ["dim", "italic"]

[header]
show = true

[big_text]
max_size = "auto"
overflow = "shrink"
"##;

const PRESET_DARK_TOML: &str = r##"
schema_version = 1

[meta]
name = "dark"
author = "lyrachi"
description = "Deep slate canvas with indigo accents and smooth distance fade"

[palette]
bg = "#1a1b26"
fg = "#c0caf5"
accent = "#7aa2f7"
dim = "#565f89"
duet = "#bb9af7"
backing = "#565f89"
backing_active = "#9ece6a"
sung = "#a9b1d6"
unsung = "#565f89"

[layout]
align = "center"
max_width = "80%"
padding = [1, 3]
line_spacing = 1
visible_prev = 2
visible_next = 3

[layout.breakpoints.compact]
max_cols = 80
max_width = "100%"
padding = [0, 1]
line_spacing = 0
visible_prev = 1
visible_next = 2

[lines.active]
fg = "$fg"
modifiers = ["bold"]
prefix = ""

[lines.inactive]
fg = "$dim"
modifiers = []
prefix = ""

[lines.fade]
curve = "ease_out"
falloff = 0.35

[highlight]
mode = "word"
active_fg = "$accent"
sung_fg = "$sung"
unsung_fg = "$unsung"

[instrumental]
style = "dots"
min_gap_ms = 4000
text = "♪"

[annotations]
show = true
position = "below"
fg = "$dim"
modifiers = ["italic"]

[header]
show = true
format = "{artist} - {title}"

[big_text]
max_size = "auto"
overflow = "shrink"
"##;

const PRESET_AURORA_TOML: &str = r##"
schema_version = 1

[meta]
name = "aurora"
author = "lyrachi"
description = "High-contrast northern lights palette with active line prefix"

[palette]
bg = "#0f172a"
fg = "#e2e8f0"
accent = "#38bdf8"
dim = "#64748b"
duet = "#f472b6"
backing = "#475569"
backing_active = "#34d399"
sung = "#7dd3fc"
unsung = "#64748b"

[layout]
align = "center"
max_width = "75%"
padding = [1, 2]
line_spacing = 1
visible_prev = 2
visible_next = 3

[layout.breakpoints.compact]
max_cols = 70
max_width = "100%"
padding = [0, 1]
line_spacing = 0
visible_prev = 1
visible_next = 2

[lines.active]
fg = "$fg"
modifiers = ["bold"]
prefix = "▸ "

[lines.inactive]
fg = "$dim"
modifiers = []
prefix = ""

[lines.fade]
curve = "ease_out"
falloff = 0.4

[highlight]
mode = "word"
active_fg = "$accent"
sung_fg = "$sung"
unsung_fg = "$unsung"

[instrumental]
style = "dots"
min_gap_ms = 4000
text = "♪"

[annotations]
show = true
position = "below"
fg = "$dim"
modifiers = ["italic"]

[header]
show = true
format = "{artist} - {title}"

[big_text]
max_size = "block"
overflow = "shrink"
"##;

const PRESET_MINIMAL_TOML: &str = r##"
schema_version = 1

[meta]
name = "minimal"
author = "lyrachi"
description = "Focused monochrome line highlight without persistent header"

[palette]
bg = "reset"
fg = "reset"
accent = "white"
dim = "dark_gray"
duet = "gray"
backing = "dark_gray"
backing_active = "gray"

[layout]
align = "center"
padding = [0, 2]
line_spacing = 0
visible_prev = 1
visible_next = 1

[lines.active]
fg = "$fg"
modifiers = ["bold"]
prefix = ""

[lines.inactive]
fg = "$dim"
modifiers = ["dim"]
prefix = ""

[lines.fade]
curve = "step"
falloff = 0.5

[highlight]
mode = "line"
active_fg = "$accent"

[instrumental]
style = "text"
min_gap_ms = 4000
text = "♪"

[annotations]
show = true
position = "below"
fg = "$dim"
modifiers = ["dim", "italic"]

[header]
show = false

[big_text]
max_size = "compact"
overflow = "fallback_normal"
"##;

/// Return the raw TOML definition for a `builtin:<name>` preset.
pub fn builtin_preset_source(name: &str) -> Option<&'static str> {
    match name {
        "builtin:default" => Some(PRESET_DEFAULT_TOML),
        "builtin:dark" => Some(PRESET_DARK_TOML),
        "builtin:aurora" => Some(PRESET_AURORA_TOML),
        "builtin:minimal" => Some(PRESET_MINIMAL_TOML),
        _ => None,
    }
}

/// Complete visual theme schema active in M33 (`styles/*.toml`).
#[derive(Debug, Clone, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleProfile {
    pub schema_version: Option<u32>,
    pub extends: Option<String>,
    pub meta: StyleMeta,
    pub palette: StylePalette,
    pub layout: StyleLayout,
    pub lines: StyleLines,
    pub highlight: StyleHighlight,
    pub animation: StyleAnimation,
    pub instrumental: StyleInstrumental,
    pub annotations: StyleAnnotations,
    pub header: StyleHeader,
    pub big_text: StyleBigText,
}

impl Default for StyleProfile {
    fn default() -> Self {
        Self {
            schema_version: Some(1),
            extends: None,
            meta: StyleMeta::default(),
            palette: StylePalette::default(),
            layout: StyleLayout::default(),
            lines: StyleLines::default(),
            highlight: StyleHighlight::default(),
            animation: StyleAnimation::default(),
            instrumental: StyleInstrumental::default(),
            annotations: StyleAnnotations::default(),
            header: StyleHeader::default(),
            big_text: StyleBigText::default(),
        }
    }
}

impl StyleProfile {
    /// Load one of the 4 built-in presets at the given viewport width.
    pub fn builtin(name: &str, viewport_width: u16) -> Option<Self> {
        builtin_preset_source(name)?;
        loader::load_style(name, None, viewport_width)
            .ok()
            .map(|outcome| outcome.profile)
    }

    /// Clamp numeric style parameters into safe rendering bounds and emit warnings.
    pub fn clamp(&mut self, warnings: &mut Vec<String>) {
        self.layout.padding.clamp(warnings);
        self.layout.line_spacing = clamp_u16(
            self.layout.line_spacing,
            0,
            LINE_SPACING_MAX,
            "layout.line_spacing",
            warnings,
        );
        self.layout.visible_prev = clamp_usize(
            self.layout.visible_prev,
            0,
            VISIBLE_CONTEXT_MAX,
            "layout.visible_prev",
            warnings,
        );
        self.layout.visible_next = clamp_usize(
            self.layout.visible_next,
            0,
            VISIBLE_CONTEXT_MAX,
            "layout.visible_next",
            warnings,
        );
        if self.lines.fade.falloff.is_nan() {
            warnings.push("style: lines.fade.falloff is NaN; using 0.0".to_string());
            self.lines.fade.falloff = 0.0;
        } else {
            let clamped = self.lines.fade.falloff.clamp(0.0, 1.0);
            if (clamped - self.lines.fade.falloff).abs() > f32::EPSILON {
                warnings.push(format!(
                    "style: lines.fade.falloff {} is outside 0.0..=1.0; using {clamped}",
                    self.lines.fade.falloff
                ));
                self.lines.fade.falloff = clamped;
            }
        }
        self.instrumental.min_gap_ms = clamp_u64(
            self.instrumental.min_gap_ms,
            0,
            MIN_GAP_MS_MAX,
            "instrumental.min_gap_ms",
            warnings,
        );
        self.highlight.trail = clamp_u16(
            self.highlight.trail,
            0,
            HIGHLIGHT_TRAIL_MAX,
            "highlight.trail",
            warnings,
        );
        if self.animation.on_line_change.len() > ANIMATION_EFFECTS_MAX {
            warnings.push(format!(
                "style: animation.on_line_change has {} effects; using first {ANIMATION_EFFECTS_MAX}",
                self.animation.on_line_change.len()
            ));
            self.animation
                .on_line_change
                .truncate(ANIMATION_EFFECTS_MAX);
        }
        for effect in &mut self.animation.on_line_change {
            effect.duration_ms = clamp_u32(
                effect.duration_ms,
                0,
                ANIMATION_EFFECT_DURATION_MAX_MS,
                "animation.on_line_change[].duration_ms",
                warnings,
            );
        }
        self.big_text.max_chars_per_phrase = clamp_u16(
            self.big_text.max_chars_per_phrase,
            1,
            120,
            "big_text.max_chars_per_phrase",
            warnings,
        );
    }

    /// Resolve this profile for a concrete terminal capability and background hint.
    pub fn resolve(
        &self,
        display_name: impl Into<String>,
        capability: ColorCapability,
        bg_blend_hint: Option<(u8, u8, u8)>,
    ) -> ResolvedStyle {
        let name = if self.meta.name.trim().is_empty() {
            display_name.into()
        } else {
            self.meta.name.clone()
        };
        let stage_bg_rgb = color_to_rgb(self.palette.bg.0).or(bg_blend_hint);
        let active_raw_fg = self.lines.active.fg.0;
        let inactive_raw_fg = self.lines.inactive.fg.0;

        ResolvedStyle {
            name,
            capability,
            bg: self.palette.bg.resolve(capability),
            stage_bg_rgb,
            align: self.layout.align,
            max_width: self.layout.max_width,
            padding: [self.layout.padding.vertical, self.layout.padding.horizontal],
            line_spacing: self.layout.line_spacing,
            visible_prev: self.layout.visible_prev,
            visible_next: self.layout.visible_next,
            active_fg: degrade_color(active_raw_fg, capability),
            active_raw_fg,
            active_modifiers: collect_modifiers(&self.lines.active.modifiers),
            active_prefix: self.lines.active.prefix.clone(),
            inactive_fg: degrade_color(inactive_raw_fg, capability),
            inactive_raw_fg,
            inactive_modifiers: collect_modifiers(&self.lines.inactive.modifiers),
            inactive_prefix: self.lines.inactive.prefix.clone(),
            fade_curve: self.lines.fade.curve,
            fade_falloff: self.lines.fade.falloff,
            highlight_mode: self.highlight.mode,
            highlight_trail: self.highlight.trail,
            animation_color_space: self.animation.color_space,
            on_line_change: self.animation.on_line_change.clone(),
            onset_pulse: self.animation.onset_pulse,
            reduced_motion: self.meta.reduced_motion,
            synthetic_word_timing: self.meta.synthetic_word_timing,
            accent: self
                .highlight
                .active_fg
                .unwrap_or(self.palette.accent)
                .resolve(capability),
            sung_fg: self.highlight.sung_fg.and_then(|c| c.resolve(capability)),
            unsung_fg: self.highlight.unsung_fg.and_then(|c| c.resolve(capability)),
            duet: self.palette.duet.resolve(capability),
            backing: self.palette.backing.resolve(capability),
            backing_active: self.palette.backing_active.resolve(capability),
            instrumental_style: self.instrumental.style,
            instrumental_min_gap_ms: self.instrumental.min_gap_ms,
            instrumental_text: self.instrumental.text.clone(),
            annotations_show: self.annotations.show,
            annotations_position: self.annotations.position,
            annotations_fg: self.annotations.fg.resolve(capability),
            annotations_modifiers: collect_modifiers(&self.annotations.modifiers),
            header_show: self.header.show,
            header_format: self.header.format.clone(),
            big_text_max_size: self.big_text.max_size,
            big_text_overflow: self.big_text.overflow,
            big_text_max_chars_per_phrase: self.big_text.max_chars_per_phrase.clamp(1, 120),
        }
    }
}

/// Descriptive style metadata (`[meta]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleMeta {
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub reduced_motion: bool,
    pub synthetic_word_timing: bool,
}

/// Named color tokens plus custom user variables for `$var` expansion.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StylePalette {
    pub bg: StyleColor,
    pub fg: StyleColor,
    pub accent: StyleColor,
    pub dim: StyleColor,
    pub duet: StyleColor,
    pub backing: StyleColor,
    pub backing_active: StyleColor,
    #[serde(flatten)]
    pub tokens: BTreeMap<String, StyleColor>,
}

impl Default for StylePalette {
    fn default() -> Self {
        Self {
            bg: StyleColor::new(Color::Reset),
            fg: StyleColor::new(Color::Reset),
            accent: StyleColor::new(Color::Cyan),
            dim: StyleColor::new(Color::Reset),
            duet: StyleColor::new(Color::LightMagenta),
            backing: StyleColor::new(Color::DarkGray),
            backing_active: StyleColor::new(Color::Gray),
            tokens: BTreeMap::new(),
        }
    }
}

/// Horizontal text alignment for lyrics lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TextAlign {
    Left,
    #[default]
    Center,
    Right,
}

impl From<TextAlign> for Alignment {
    fn from(value: TextAlign) -> Self {
        match value {
            TextAlign::Left => Self::Left,
            TextAlign::Center => Self::Center,
            TextAlign::Right => Self::Right,
        }
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum RawMaxWidth {
    Int(u16),
    Text(String),
}

/// Maximum lyric wrap width expressed as percentage or fixed columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, schemars::JsonSchema)]
#[schemars(with = "RawMaxWidth")]
pub enum MaxWidth {
    Columns(u16),
    Percent(u16),
}

impl MaxWidth {
    /// Resolve this constraint into concrete columns for a viewport width.
    pub fn resolve(self, viewport_width: u16) -> u16 {
        match self {
            Self::Columns(cols) => cols.clamp(1, 500),
            Self::Percent(pct) => {
                let pct = pct.clamp(10, 100);
                ((u32::from(viewport_width) * u32::from(pct)) / 100).max(1) as u16
            }
        }
    }
}

impl<'de> Deserialize<'de> for MaxWidth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match RawMaxWidth::deserialize(deserializer)? {
            RawMaxWidth::Int(cols) => Ok(Self::Columns(cols.clamp(1, 500))),
            RawMaxWidth::Text(raw) => {
                let trimmed = raw.trim();
                if let Some(pct_str) = trimmed.strip_suffix('%') {
                    let pct: u16 = pct_str.trim().parse().map_err(|_| {
                        serde::de::Error::custom(format!(
                            "invalid max_width percentage `{trimmed}`"
                        ))
                    })?;
                    Ok(Self::Percent(pct.clamp(10, 100)))
                } else {
                    let cols: u16 = trimmed.parse().map_err(|_| {
                        serde::de::Error::custom(format!("invalid max_width `{trimmed}`"))
                    })?;
                    Ok(Self::Columns(cols.clamp(1, 500)))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum RawPadding {
    Scalar(u16),
    Pair([u16; 2]),
}

/// Vertical and horizontal outer padding (`[vertical, horizontal]` or scalar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, schemars::JsonSchema)]
#[schemars(with = "RawPadding")]
pub struct Padding {
    pub vertical: u16,
    pub horizontal: u16,
}

impl Default for Padding {
    fn default() -> Self {
        Self {
            vertical: 0,
            horizontal: crate::compiler::timeline::HORIZONTAL_PADDING,
        }
    }
}

impl Padding {
    fn clamp(&mut self, warnings: &mut Vec<String>) {
        self.vertical = clamp_u16(self.vertical, 0, PADDING_MAX, "layout.padding[0]", warnings);
        self.horizontal = clamp_u16(
            self.horizontal,
            0,
            PADDING_MAX,
            "layout.padding[1]",
            warnings,
        );
    }
}

impl<'de> Deserialize<'de> for Padding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match RawPadding::deserialize(deserializer)? {
            RawPadding::Scalar(value) => Ok(Self {
                vertical: value,
                horizontal: value,
            }),
            RawPadding::Pair([vertical, horizontal]) => Ok(Self {
                vertical,
                horizontal,
            }),
        }
    }
}

/// Layout and viewport window settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleLayout {
    pub align: TextAlign,
    pub max_width: Option<MaxWidth>,
    pub padding: Padding,
    pub line_spacing: u16,
    pub visible_prev: usize,
    pub visible_next: usize,
}

impl Default for StyleLayout {
    fn default() -> Self {
        Self {
            align: TextAlign::Center,
            max_width: None,
            padding: Padding::default(),
            line_spacing: 0,
            visible_prev: 2,
            visible_next: 3,
        }
    }
}

/// Supported terminal text modifiers in `styles/*.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StyleModifier {
    Bold,
    Dim,
    Italic,
    Underlined,
}

impl StyleModifier {
    fn into_ratatui(self) -> Modifier {
        match self {
            Self::Bold => Modifier::BOLD,
            Self::Dim => Modifier::DIM,
            Self::Italic => Modifier::ITALIC,
            Self::Underlined => Modifier::UNDERLINED,
        }
    }
}

fn collect_modifiers(list: &[StyleModifier]) -> Modifier {
    list.iter()
        .fold(Modifier::empty(), |acc, item| acc | item.into_ratatui())
}

/// Line styling rule for active or inactive lines.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LineRule {
    pub fg: StyleColor,
    pub modifiers: Vec<StyleModifier>,
    pub prefix: String,
}

impl Default for LineRule {
    fn default() -> Self {
        Self {
            fg: StyleColor::new(Color::Reset),
            modifiers: Vec::new(),
            prefix: String::new(),
        }
    }
}

/// Distance fade configuration between adjacent and far lines.
#[derive(Debug, Clone, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct FadeConfig {
    pub curve: FadeCurve,
    pub falloff: f32,
}

impl Default for FadeConfig {
    fn default() -> Self {
        Self {
            curve: FadeCurve::Linear,
            falloff: 0.0,
        }
    }
}

/// Line appearance rules (`[lines]`).
#[derive(Debug, Clone, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleLines {
    pub active: LineRule,
    pub inactive: LineRule,
    pub fade: FadeConfig,
}

impl Default for StyleLines {
    fn default() -> Self {
        Self {
            active: LineRule::default(),
            inactive: LineRule {
                fg: StyleColor::new(Color::Reset),
                modifiers: vec![StyleModifier::Dim],
                prefix: String::new(),
            },
            fade: FadeConfig::default(),
        }
    }
}

/// Word highlight granularity supported in M35.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HighlightMode {
    Line,
    #[default]
    Word,
    Wipe,
}

/// Highlight settings (`[highlight]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleHighlight {
    pub mode: HighlightMode,
    pub active_fg: Option<StyleColor>,
    pub sung_fg: Option<StyleColor>,
    pub unsung_fg: Option<StyleColor>,
    pub trail: u16,
}

impl Default for StyleHighlight {
    fn default() -> Self {
        Self {
            mode: HighlightMode::default(),
            active_fg: None,
            sung_fg: None,
            unsung_fg: None,
            trail: 3,
        }
    }
}

/// Color space used for wipe gradients and line-change effects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AnimationColorSpace {
    Rgb,
    #[default]
    Hsl,
    Hsv,
}

impl AnimationColorSpace {
    pub const fn to_tachyonfx(self) -> FxColorSpace {
        match self {
            Self::Rgb => FxColorSpace::Rgb,
            Self::Hsl => FxColorSpace::Hsl,
            Self::Hsv => FxColorSpace::Hsv,
        }
    }

    pub fn lerp(self, from: Color, to: Color, alpha: f32) -> Color {
        self.to_tachyonfx().lerp(&from, &to, alpha)
    }
}

/// Built-in effects that can run when the active lyric line changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineChangeEffectKind {
    #[default]
    FadeIn,
    FadeOut,
    Sweep,
    Dissolve,
    Coalesce,
    Pulse,
}

/// One declarative line-change effect.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LineChangeEffect {
    pub effect: LineChangeEffectKind,
    pub duration_ms: u32,
}

impl Default for LineChangeEffect {
    fn default() -> Self {
        Self {
            effect: LineChangeEffectKind::FadeIn,
            duration_ms: 250,
        }
    }
}

/// Line-change animation settings (`[animation]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleAnimation {
    pub color_space: AnimationColorSpace,
    pub on_line_change: Vec<LineChangeEffect>,
    pub onset_pulse: bool,
}

/// Instrumental wait countdown visual style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum InstrumentalStyle {
    #[default]
    Dots,
    Bar,
    Text,
    Hidden,
}

/// Waiting gap indicator settings (`[instrumental]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleInstrumental {
    pub style: InstrumentalStyle,
    pub min_gap_ms: u64,
    pub text: String,
}

impl Default for StyleInstrumental {
    fn default() -> Self {
        Self {
            style: InstrumentalStyle::Dots,
            min_gap_ms: 4_000,
            text: "♪".to_string(),
        }
    }
}

/// Where lyric annotations appear relative to the main line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationPosition {
    #[default]
    Below,
    Inline,
}

/// Annotation styling (`[annotations]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleAnnotations {
    pub show: bool,
    pub position: AnnotationPosition,
    pub fg: StyleColor,
    pub modifiers: Vec<StyleModifier>,
}

impl Default for StyleAnnotations {
    fn default() -> Self {
        Self {
            show: true,
            position: AnnotationPosition::Below,
            fg: StyleColor::new(Color::Reset),
            modifiers: vec![StyleModifier::Dim, StyleModifier::Italic],
        }
    }
}

/// Track header display rules (`[header]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleHeader {
    pub show: bool,
    pub format: Option<String>,
}

impl Default for StyleHeader {
    fn default() -> Self {
        Self {
            show: true,
            format: None,
        }
    }
}

/// Maximum bitmap font tier allowed for Block display mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BigTextMaxSize {
    #[default]
    Auto,
    Block,
    Compact,
    Mini,
    Plain,
}

/// Fallback strategy when a phrase exceeds the Block display budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BigTextOverflow {
    #[default]
    Shrink,
    Wrap,
    FallbackNormal,
}

/// Block mode typography limits (`[big_text]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StyleBigText {
    pub max_size: BigTextMaxSize,
    pub overflow: BigTextOverflow,
    pub max_chars_per_phrase: u16,
}

impl Default for StyleBigText {
    fn default() -> Self {
        Self {
            max_size: BigTextMaxSize::default(),
            overflow: BigTextOverflow::default(),
            max_chars_per_phrase: 18,
        }
    }
}

/// Lightweight, `Clone + PartialEq` presentation snapshot stored on `UiState`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedStyle {
    pub name: String,
    pub capability: ColorCapability,
    pub bg: Option<Color>,
    pub stage_bg_rgb: Option<(u8, u8, u8)>,
    pub align: TextAlign,
    pub max_width: Option<MaxWidth>,
    pub padding: [u16; 2],
    pub line_spacing: u16,
    pub visible_prev: usize,
    pub visible_next: usize,
    pub active_fg: Option<Color>,
    pub active_raw_fg: Color,
    pub active_modifiers: Modifier,
    pub active_prefix: String,
    pub inactive_fg: Option<Color>,
    pub inactive_raw_fg: Color,
    pub inactive_modifiers: Modifier,
    pub inactive_prefix: String,
    pub fade_curve: FadeCurve,
    pub fade_falloff: f32,
    pub highlight_mode: HighlightMode,
    pub highlight_trail: u16,
    pub animation_color_space: AnimationColorSpace,
    pub on_line_change: Vec<LineChangeEffect>,
    pub onset_pulse: bool,
    pub reduced_motion: bool,
    pub synthetic_word_timing: bool,
    pub accent: Option<Color>,
    pub sung_fg: Option<Color>,
    pub unsung_fg: Option<Color>,
    pub duet: Option<Color>,
    pub backing: Option<Color>,
    pub backing_active: Option<Color>,
    pub instrumental_style: InstrumentalStyle,
    pub instrumental_min_gap_ms: u64,
    pub instrumental_text: String,
    pub annotations_show: bool,
    pub annotations_position: AnnotationPosition,
    pub annotations_fg: Option<Color>,
    pub annotations_modifiers: Modifier,
    pub header_show: bool,
    pub header_format: Option<String>,
    pub big_text_max_size: BigTextMaxSize,
    pub big_text_overflow: BigTextOverflow,
    pub big_text_max_chars_per_phrase: u16,
}

impl Default for ResolvedStyle {
    fn default() -> Self {
        StyleProfile::default().resolve("default", ColorCapability::TrueColor, None)
    }
}

impl ResolvedStyle {
    /// Resolve `max_width` against a viewport column count.
    pub fn resolved_max_width(&self, viewport_width: u16) -> Option<u16> {
        self.max_width.map(|spec| spec.resolve(viewport_width))
    }

    /// Base canvas style when a non-reset background color is configured.
    pub fn canvas_style(&self) -> Style {
        let mut style = Style::default();
        if let Some(bg) = self.bg {
            style = style.bg(bg);
        }
        if let Some(fg) = self.active_fg {
            style = style.fg(fg);
        }
        style
    }

    /// Compute ratatui `Style` for a line considering role, active state, and distance from anchor.
    pub fn line_style(&self, role: LineRole, distance: usize, active: bool) -> Style {
        match role {
            LineRole::Primary => {
                if active {
                    apply_opt_fg(Style::default(), self.active_fg)
                        .add_modifier(self.active_modifiers)
                } else {
                    let faded = blend_fade_color(
                        self.active_raw_fg,
                        self.inactive_raw_fg,
                        self.stage_bg_rgb,
                        distance.max(1),
                        self.fade_curve,
                        self.fade_falloff,
                        self.capability,
                    );
                    let mut style = apply_opt_fg(Style::default(), faded.fg)
                        .add_modifier(self.inactive_modifiers);
                    if faded.dim {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    style
                }
            }
            LineRole::DuetSecondary => {
                let mut style = apply_opt_fg(Style::default(), self.duet);
                if !active {
                    style = style.add_modifier(Modifier::DIM);
                }
                style
            }
            LineRole::BackgroundVocal => {
                let mut style =
                    apply_opt_fg(Style::default(), self.backing).add_modifier(Modifier::ITALIC);
                if !active {
                    style = style.add_modifier(Modifier::DIM);
                }
                style
            }
            LineRole::InstrumentalBreak => apply_opt_fg(Style::default(), self.inactive_fg)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        }
    }

    /// Compute ratatui `Style` for active/sung/unsung word tokens.
    pub fn word_style(&self, role: LineRole, active: bool, sung: bool) -> Style {
        match role {
            LineRole::Primary => {
                if active {
                    apply_opt_fg(Style::default(), self.accent).add_modifier(Modifier::BOLD)
                } else if sung {
                    apply_opt_fg(Style::default(), self.sung_fg.or(self.active_fg))
                } else {
                    let unsung_fg = self.unsung_fg;
                    let mut style = apply_opt_fg(Style::default(), unsung_fg);
                    if unsung_fg.is_none() {
                        style = style.add_modifier(Modifier::DIM);
                    }
                    style
                }
            }
            LineRole::DuetSecondary => {
                let style = apply_opt_fg(Style::default(), self.duet);
                if active {
                    style.add_modifier(Modifier::BOLD)
                } else if sung {
                    style
                } else {
                    style.add_modifier(Modifier::DIM)
                }
            }
            LineRole::BackgroundVocal => {
                if active {
                    apply_opt_fg(Style::default(), self.backing_active)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC)
                } else {
                    let style =
                        apply_opt_fg(Style::default(), self.backing).add_modifier(Modifier::ITALIC);
                    if sung {
                        style
                    } else {
                        style.add_modifier(Modifier::DIM)
                    }
                }
            }
            LineRole::InstrumentalBreak => apply_opt_fg(Style::default(), self.inactive_fg)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        }
    }

    /// Compute a wipe-gradient trail style where `mix` 0.0 is active and 1.0 is sung.
    pub fn wipe_style(&self, role: LineRole, mix: f32) -> Style {
        let active = self.word_style(role, true, true);
        let sung = self.word_style(role, false, true);
        let mix = mix.clamp(0.0, 1.0);
        if mix <= 0.0 {
            return active;
        }
        if mix >= 1.0 {
            return sung;
        }
        let (Some(from), Some(to)) = (active.fg, sung.fg) else {
            return if mix < 0.5 { active } else { sung };
        };
        let blended = self.animation_color_space.lerp(from, to, mix);
        match degrade_color(blended, self.capability) {
            Some(color) => sung.fg(color),
            None => sung,
        }
    }

    /// Compute annotation text `Style`.
    pub fn annotation_style(&self) -> Style {
        apply_opt_fg(Style::default(), self.annotations_fg).add_modifier(self.annotations_modifiers)
    }

    /// Resolve horizontal alignment for a line role and viewport width.
    pub fn alignment(&self, role: LineRole, viewport_width: u16) -> Alignment {
        match role {
            LineRole::DuetSecondary if viewport_width >= DUET_RIGHT_ALIGN_MIN_WIDTH => {
                Alignment::Right
            }
            LineRole::DuetSecondary => Alignment::Left,
            LineRole::Primary | LineRole::BackgroundVocal | LineRole::InstrumentalBreak => {
                self.align.into()
            }
        }
    }
}

fn apply_opt_fg(style: Style, fg: Option<Color>) -> Style {
    match fg {
        Some(color) => style.fg(color),
        None => style,
    }
}

fn clamp_u16(value: u16, min: u16, max: u16, field: &str, warnings: &mut Vec<String>) -> u16 {
    let clamped = value.clamp(min, max);
    if clamped != value {
        warnings.push(format!(
            "style: {field} {value} is outside {min}..={max}; using {clamped}"
        ));
    }
    clamped
}

fn clamp_u32(value: u32, min: u32, max: u32, field: &str, warnings: &mut Vec<String>) -> u32 {
    let clamped = value.clamp(min, max);
    if clamped != value {
        warnings.push(format!(
            "style: {field} {value} is outside {min}..={max}; using {clamped}"
        ));
    }
    clamped
}

fn clamp_u64(value: u64, min: u64, max: u64, field: &str, warnings: &mut Vec<String>) -> u64 {
    let clamped = value.clamp(min, max);
    if clamped != value {
        warnings.push(format!(
            "style: {field} {value} is outside {min}..={max}; using {clamped}"
        ));
    }
    clamped
}

fn clamp_usize(
    value: usize,
    min: usize,
    max: usize,
    field: &str,
    warnings: &mut Vec<String>,
) -> usize {
    let clamped = value.clamp(min, max);
    if clamped != value {
        warnings.push(format!(
            "style: {field} {value} is outside {min}..={max}; using {clamped}"
        ));
    }
    clamped
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lyrachi-style-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn style_cycling_advances_through_builtins_and_custom_presets() {
        let dir = temp_dir("cycling");
        fs::write(dir.join("zebra.toml"), "[meta]\nname = \"zebra\"\n").unwrap();
        fs::write(dir.join("alpha.toml"), "[meta]\nname = \"alpha\"\n").unwrap();
        fs::write(dir.join(".hidden.toml"), "[meta]\nname = \"hidden\"\n").unwrap();
        fs::write(dir.join("not_toml.txt"), "hello").unwrap();

        let all = available_style_names(Some(&dir));
        assert_eq!(
            all,
            vec![
                "builtin:default",
                "builtin:dark",
                "builtin:aurora",
                "builtin:minimal",
                "alpha",
                "zebra",
            ]
        );

        assert_eq!(
            cycle_next_style("builtin:default", Some(&dir)),
            "builtin:dark"
        );
        assert_eq!(cycle_next_style("builtin:minimal", Some(&dir)), "alpha");
        assert_eq!(cycle_next_style("alpha", Some(&dir)), "zebra");
        assert_eq!(cycle_next_style("zebra", Some(&dir)), "builtin:default");
        assert_eq!(cycle_next_style("unknown", Some(&dir)), "builtin:default");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn schema_for_style_profile_is_valid_2020_12() {
        let schema = schemars::schema_for!(StyleProfile);
        let json: serde_json::Value = serde_json::to_value(&schema).unwrap();
        assert_eq!(
            json["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert!(json["properties"]["palette"].is_object());
        assert!(json["properties"]["layout"].is_object());
    }

    #[test]
    fn clamps_wipe_trail_and_animation_limits() {
        let mut profile = StyleProfile::default();
        profile.highlight.trail = 99;
        profile.animation.on_line_change = (0..12)
            .map(|_| LineChangeEffect {
                effect: LineChangeEffectKind::FadeIn,
                duration_ms: 30_000,
            })
            .collect();
        let mut warnings = Vec::new();
        profile.clamp(&mut warnings);

        assert_eq!(profile.highlight.trail, HIGHLIGHT_TRAIL_MAX);
        assert_eq!(
            profile.animation.on_line_change.len(),
            ANIMATION_EFFECTS_MAX
        );
        assert!(
            profile
                .animation
                .on_line_change
                .iter()
                .all(|effect| effect.duration_ms == ANIMATION_EFFECT_DURATION_MAX_MS)
        );
        assert!(warnings.iter().any(|warning| warning.contains("trail")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("on_line_change"))
        );
    }

    #[test]
    fn wipe_style_hits_active_and_sung_endpoints() {
        let mut profile = StyleProfile::default();
        profile.highlight.active_fg = Some(StyleColor::new(Color::Rgb(20, 40, 80)));
        profile.highlight.sung_fg = Some(StyleColor::new(Color::Rgb(220, 200, 180)));
        let resolved = profile.resolve("wipe", ColorCapability::TrueColor, None);

        assert_eq!(
            resolved.wipe_style(LineRole::Primary, 0.0),
            resolved.word_style(LineRole::Primary, true, true)
        );
        assert_eq!(
            resolved.wipe_style(LineRole::Primary, 1.0),
            resolved.word_style(LineRole::Primary, false, true)
        );
        let midpoint = resolved.wipe_style(LineRole::Primary, 0.5);
        assert!(midpoint.fg.is_some());
        assert_ne!(
            midpoint.fg,
            resolved.word_style(LineRole::Primary, true, true).fg
        );
        assert_ne!(
            midpoint.fg,
            resolved.word_style(LineRole::Primary, false, true).fg
        );
    }
}
