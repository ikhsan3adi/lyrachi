use std::time::{Duration, Instant};

use ratatui::{buffer::Buffer, layout::Rect, style::Color};
use tachyonfx::{Effect, EffectManager, EffectTimer, Interpolation, Motion, fx};

use crate::{
    core::clock::PlaybackStatus,
    style::{LineChangeEffect, LineChangeEffectKind, ResolvedStyle, color::ColorCapability},
};

use super::UiState;

const PULSE_DURATION_MS: u32 = 300;

/// Runtime-owned tachyonfx effects. Kept outside `UiState` because it is not `Clone`.
#[derive(Debug, Default)]
pub struct UiEffects {
    manager: EffectManager<()>,
    last_line: Option<Option<usize>>,
    last_tick: Option<Instant>,
}

impl UiEffects {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_running(&self) -> bool {
        self.manager.is_running()
    }

    pub fn reset(&mut self) {
        self.manager = EffectManager::default();
        self.last_line = None;
        self.last_tick = None;
    }

    /// Track the active line and queue the configured transition only when it changes.
    pub fn observe_line(
        &mut self,
        active_line: Option<usize>,
        style: &ResolvedStyle,
        playing: bool,
        modal_open: bool,
    ) {
        if self.last_line == Some(active_line) {
            return;
        }
        self.last_line = Some(active_line);
        if playing
            && !modal_open
            && !style.reduced_motion
            && let Some(_) = active_line
        {
            self.trigger_line_change(style);
        }
    }

    /// Queue all declarative line-change effects for a style. Public for deterministic tests.
    pub fn trigger_line_change(&mut self, style: &ResolvedStyle) {
        if style.reduced_motion {
            return;
        }
        let no_color = style.capability == ColorCapability::NoColor;
        for effect in &style.on_line_change {
            if no_color && effect_mutates_only_color(effect.effect) {
                continue;
            }
            self.manager.add_effect(build_effect(effect, style, false));
        }
        if style.onset_pulse && !no_color {
            let pulse = LineChangeEffect {
                effect: LineChangeEffectKind::Pulse,
                duration_ms: PULSE_DURATION_MS,
            };
            self.manager.add_effect(build_effect(&pulse, style, true));
        }
    }

    pub fn observe_state(&mut self, state: &UiState, effective_ms: u64) {
        let active_line = state.timeline().and_then(|timeline| {
            timeline
                .evaluate_for_status(effective_ms, state.playback)
                .active_line_idx
        });
        self.observe_line(
            active_line,
            &state.style,
            matches!(state.playback, PlaybackStatus::Playing),
            state.is_modal_open(),
        );
    }

    pub fn process_now(&mut self, now: Instant, state: &UiState, buf: &mut Buffer, area: Rect) {
        if !matches!(state.playback, PlaybackStatus::Playing) {
            self.last_tick = Some(now);
            return;
        }
        let elapsed = self
            .last_tick
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.last_tick = Some(now);
        self.process_elapsed(elapsed, buf, area, &state.style, state.is_modal_open());
    }

    pub fn process_elapsed(
        &mut self,
        elapsed: Duration,
        buf: &mut Buffer,
        area: Rect,
        style: &ResolvedStyle,
        modal_open: bool,
    ) {
        if modal_open || style.reduced_motion || area.width == 0 || area.height == 0 {
            return;
        }
        if self.manager.is_running() {
            self.manager.process_effects(elapsed, buf, area);
        }
    }
}

const fn effect_mutates_only_color(kind: LineChangeEffectKind) -> bool {
    matches!(
        kind,
        LineChangeEffectKind::FadeIn
            | LineChangeEffectKind::FadeOut
            | LineChangeEffectKind::Sweep
            | LineChangeEffectKind::Pulse
    )
}

fn build_effect(effect: &LineChangeEffect, style: &ResolvedStyle, pulse: bool) -> Effect {
    let duration = effect.duration_ms.min(5_000);
    let timer = EffectTimer::from_ms(duration, Interpolation::SineOut);
    let fade_color = style.bg.unwrap_or(Color::Reset);
    let accent = style.accent.or(style.active_fg).unwrap_or(Color::LightCyan);

    let effect = if pulse {
        fx::fade_to_fg(accent, timer)
    } else {
        match effect.effect {
            LineChangeEffectKind::FadeIn => fx::fade_from_fg(fade_color, timer),
            LineChangeEffectKind::FadeOut => fx::fade_to_fg(fade_color, timer),
            LineChangeEffectKind::Sweep => {
                fx::sweep_in(Motion::LeftToRight, 8, 0, fade_color, timer)
            }
            LineChangeEffectKind::Dissolve => fx::dissolve(timer),
            LineChangeEffectKind::Coalesce => fx::coalesce(timer),
            LineChangeEffectKind::Pulse => fx::fade_to_fg(accent, timer),
        }
    };

    effect.with_color_space(style.animation_color_space.to_tachyonfx())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::StyleProfile;

    fn style_with_effects() -> ResolvedStyle {
        let mut profile = StyleProfile::default();
        profile.animation.on_line_change = vec![LineChangeEffect {
            effect: LineChangeEffectKind::FadeIn,
            duration_ms: 120,
        }];
        profile.resolve(
            "test",
            crate::style::color::ColorCapability::TrueColor,
            None,
        )
    }

    #[test]
    fn effects_finish_and_do_not_touch_cells_outside_stage_area() {
        let style = style_with_effects();
        let full = Rect::new(0, 0, 24, 10);
        let stage = Rect::new(4, 3, 12, 4);
        let mut buf = Buffer::empty(full);
        for cell in buf.content.iter_mut() {
            cell.set_symbol("Z");
            cell.fg = Color::Red;
        }

        let mut effects = UiEffects::new();
        effects.trigger_line_change(&style);
        assert!(effects.is_running());

        effects.process_elapsed(Duration::from_millis(500), &mut buf, stage, &style, false);

        assert!(!effects.is_running());
        for y in full.y..full.bottom() {
            for x in full.x..full.right() {
                if stage.contains(ratatui::layout::Position { x, y }) {
                    continue;
                }
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), "Z");
                assert_eq!(cell.fg, Color::Red);
            }
        }
    }

    #[test]
    fn reset_colors_are_safe_and_modal_freeze_skips_processing() {
        let mut profile = StyleProfile::default();
        profile.palette.bg = crate::style::color::StyleColor::new(Color::Reset);
        profile.animation.on_line_change = vec![LineChangeEffect {
            effect: LineChangeEffectKind::Sweep,
            duration_ms: 120,
        }];
        let style = profile.resolve(
            "reset",
            crate::style::color::ColorCapability::TrueColor,
            None,
        );
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);

        let mut effects = UiEffects::new();
        effects.trigger_line_change(&style);
        effects.process_elapsed(Duration::from_millis(60), &mut buf, area, &style, true);
        assert!(effects.is_running(), "modal must freeze effects");

        effects.process_elapsed(Duration::from_millis(500), &mut buf, area, &style, false);
        assert!(!effects.is_running());
    }

    #[test]
    fn reduced_motion_never_starts_effects() {
        let mut style = style_with_effects();
        style.reduced_motion = true;
        let mut effects = UiEffects::new();
        effects.trigger_line_change(&style);
        assert!(!effects.is_running());
    }

    #[test]
    fn no_color_capability_skips_color_only_effects() {
        let mut profile = StyleProfile::default();
        profile.animation.onset_pulse = true;
        profile.animation.on_line_change = vec![
            LineChangeEffect {
                effect: LineChangeEffectKind::FadeIn,
                duration_ms: 120,
            },
            LineChangeEffect {
                effect: LineChangeEffectKind::Sweep,
                duration_ms: 120,
            },
        ];
        let style = profile.resolve("no-color", ColorCapability::NoColor, None);
        let mut effects = UiEffects::new();
        effects.trigger_line_change(&style);
        assert!(!effects.is_running());
    }
}
