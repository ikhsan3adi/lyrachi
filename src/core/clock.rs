use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Stopped,
    Paused,
    Playing,
}

impl PlaybackStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Paused => "paused",
            Self::Playing => "playing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EffectiveOffsets {
    pub intrinsic_ms: i64,
    pub global_ms: i64,
    pub player_ms: i64,
    pub user_ms: i64,
    pub lookahead_ms: i64,
}

impl EffectiveOffsets {
    pub fn apply(&self, raw_ms: u64) -> u64 {
        let raw_ms = i64::try_from(raw_ms).unwrap_or(i64::MAX);
        raw_ms
            .saturating_add(self.intrinsic_ms)
            .saturating_sub(self.global_ms)
            .saturating_sub(self.player_ms)
            .saturating_sub(self.user_ms)
            .saturating_add(self.lookahead_ms)
            .max(0) as u64
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClockState {
    Stopped,
    Paused {
        position_ms: u64,
    },
    Playing {
        anchor_pos_ms: u64,
        anchor_instant: Instant,
        rate: f64,
        last_polled_ms: Option<u64>,
        stale_poll_count: u8,
    },
}

impl ClockState {
    pub const fn status(&self) -> PlaybackStatus {
        match self {
            Self::Stopped => PlaybackStatus::Stopped,
            Self::Paused { .. } => PlaybackStatus::Paused,
            Self::Playing { .. } => PlaybackStatus::Playing,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlaybackClock {
    state: ClockState,
    rate: f64,
}

impl Default for PlaybackClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackClock {
    pub fn new() -> Self {
        Self {
            state: ClockState::Stopped,
            rate: 1.0,
        }
    }

    pub fn state(&self) -> &ClockState {
        &self.state
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn raw_position_at(&self, now: Instant) -> u64 {
        match &self.state {
            ClockState::Stopped => 0,
            ClockState::Paused { position_ms } => *position_ms,
            ClockState::Playing {
                anchor_pos_ms,
                anchor_instant,
                rate,
                ..
            } => {
                let elapsed_ms =
                    (now.saturating_duration_since(*anchor_instant).as_secs_f64() * *rate * 1000.0)
                        .round() as u64;
                anchor_pos_ms.saturating_add(elapsed_ms)
            }
        }
    }

    pub fn effective_lyric_ms(&self, now: Instant, offsets: EffectiveOffsets) -> u64 {
        offsets.apply(self.raw_position_at(now))
    }

    pub fn on_status_changed(&mut self, new_status: PlaybackStatus, now: Instant) {
        let next = match new_status {
            PlaybackStatus::Stopped => ClockState::Stopped,
            PlaybackStatus::Paused => match &self.state {
                ClockState::Playing { .. } => ClockState::Paused {
                    position_ms: self.raw_position_at(now),
                },
                ClockState::Paused { position_ms } => ClockState::Paused {
                    position_ms: *position_ms,
                },
                ClockState::Stopped => ClockState::Paused { position_ms: 0 },
            },
            PlaybackStatus::Playing => match &self.state {
                ClockState::Playing { .. } => self.state.clone(),
                ClockState::Paused { position_ms } => ClockState::Playing {
                    anchor_pos_ms: *position_ms,
                    anchor_instant: now,
                    rate: self.rate,
                    last_polled_ms: None,
                    stale_poll_count: 0,
                },
                ClockState::Stopped => ClockState::Playing {
                    anchor_pos_ms: 0,
                    anchor_instant: now,
                    rate: self.rate,
                    last_polled_ms: None,
                    stale_poll_count: 0,
                },
            },
        };

        self.state = next;
    }

    pub fn on_rate_changed(&mut self, new_rate: f64, now: Instant) {
        let safe_rate = if new_rate > 0.05 && new_rate <= 8.0 {
            new_rate
        } else {
            1.0
        };

        let current_pos = self.raw_position_at(now);
        if let ClockState::Playing {
            anchor_pos_ms,
            anchor_instant,
            rate,
            ..
        } = &mut self.state
        {
            *anchor_pos_ms = current_pos;
            *anchor_instant = now;
            *rate = safe_rate;
        }

        self.rate = safe_rate;
    }

    pub fn on_seeked(&mut self, new_pos_ms: u64, now: Instant) {
        match &mut self.state {
            ClockState::Playing {
                anchor_pos_ms,
                anchor_instant,
                stale_poll_count,
                ..
            } => {
                *anchor_pos_ms = new_pos_ms;
                *anchor_instant = now;
                *stale_poll_count = 0;
            }
            ClockState::Paused { position_ms } => *position_ms = new_pos_ms,
            ClockState::Stopped => {}
        }
    }

    pub fn on_position_poll(&mut self, polled_ms: u64, ipc_rtt: Duration, now: Instant) {
        let predicted_ms = self.raw_position_at(now);
        let ClockState::Playing {
            anchor_pos_ms,
            anchor_instant,
            last_polled_ms,
            stale_poll_count,
            ..
        } = &mut self.state
        else {
            return;
        };

        if Some(polled_ms) == *last_polled_ms {
            *stale_poll_count = stale_poll_count.saturating_add(1);
            if *stale_poll_count >= 1 {
                return;
            }
        } else {
            *last_polled_ms = Some(polled_ms);
            *stale_poll_count = 0;
        }

        let rtt_ms = u64::try_from(ipc_rtt.as_millis()).unwrap_or(u64::MAX);
        let compensated_ms = polled_ms.saturating_add(rtt_ms / 2);
        let drift_ms = to_i64(compensated_ms).saturating_sub(to_i64(predicted_ms));
        let drift_abs = drift_ms.unsigned_abs();

        if drift_abs <= 45 {
            return;
        }

        if drift_abs <= 300 {
            let adjusted_ms = to_i64(predicted_ms).saturating_add(drift_ms / 2).max(0) as u64;
            *anchor_pos_ms = adjusted_ms;
            *anchor_instant = now;
        } else {
            *anchor_pos_ms = compensated_ms;
            *anchor_instant = now;
        }
    }
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn playing_clock(base: Instant, position_ms: u64) -> PlaybackClock {
        let mut clock = PlaybackClock::new();
        clock.on_status_changed(PlaybackStatus::Playing, base);
        if position_ms > 0 {
            clock.on_seeked(position_ms, base);
        }
        clock
    }

    fn anchor_state(clock: &PlaybackClock) -> (u64, Instant) {
        match clock.state() {
            ClockState::Playing {
                anchor_pos_ms,
                anchor_instant,
                ..
            } => (*anchor_pos_ms, *anchor_instant),
            _ => panic!("clock is not playing"),
        }
    }

    #[test]
    fn stopped_and_paused_positions_are_static() {
        let base = Instant::now();
        let mut clock = PlaybackClock::new();

        assert_eq!(clock.raw_position_at(at(base, 5_000)), 0);

        clock.on_status_changed(PlaybackStatus::Paused, base);
        assert_eq!(clock.raw_position_at(at(base, 5_000)), 0);
        clock.on_seeked(4_000, at(base, 5_000));
        assert_eq!(clock.raw_position_at(at(base, 9_000)), 4_000);

        clock.on_status_changed(PlaybackStatus::Stopped, at(base, 9_000));
        assert_eq!(clock.raw_position_at(at(base, 9_000)), 0);
    }

    #[test]
    fn playing_paused_and_resumed_table() {
        let base = Instant::now();
        let mut clock = PlaybackClock::new();

        clock.on_status_changed(PlaybackStatus::Playing, base);
        assert_eq!(clock.raw_position_at(at(base, 1_500)), 1_500);

        clock.on_status_changed(PlaybackStatus::Paused, at(base, 2_000));
        assert_eq!(clock.raw_position_at(at(base, 9_000)), 2_000);

        clock.on_status_changed(PlaybackStatus::Playing, at(base, 9_000));
        assert_eq!(clock.raw_position_at(at(base, 10_000)), 3_000);

        clock.on_status_changed(PlaybackStatus::Playing, at(base, 10_500));
        assert_eq!(clock.raw_position_at(at(base, 11_000)), 4_000);
    }

    #[test]
    fn all_status_transition_edges_are_deterministic() {
        let base = Instant::now();
        let mut clock = PlaybackClock::new();

        clock.on_status_changed(PlaybackStatus::Paused, base);
        assert_eq!(clock.raw_position_at(at(base, 2_000)), 0);

        clock.on_status_changed(PlaybackStatus::Playing, at(base, 1_000));
        assert_eq!(clock.raw_position_at(at(base, 2_000)), 1_000);

        clock.on_status_changed(PlaybackStatus::Paused, at(base, 2_000));
        assert_eq!(clock.raw_position_at(at(base, 9_000)), 1_000);

        clock.on_status_changed(PlaybackStatus::Stopped, at(base, 3_000));
        assert_eq!(clock.raw_position_at(at(base, 9_000)), 0);

        clock.on_status_changed(PlaybackStatus::Playing, at(base, 4_000));
        assert_eq!(clock.raw_position_at(at(base, 5_000)), 1_000);

        clock.on_status_changed(PlaybackStatus::Stopped, at(base, 5_000));
        assert_eq!(clock.raw_position_at(at(base, 9_000)), 0);

        clock.on_status_changed(PlaybackStatus::Paused, at(base, 6_000));
        assert_eq!(clock.raw_position_at(at(base, 9_000)), 0);
    }

    #[test]
    fn rate_change_reanchors_without_double_counting() {
        let base = Instant::now();
        let mut clock = playing_clock(base, 0);

        assert_eq!(clock.raw_position_at(at(base, 1_000)), 1_000);
        clock.on_rate_changed(2.0, at(base, 1_000));
        assert_eq!(clock.raw_position_at(at(base, 2_000)), 3_000);

        clock.on_rate_changed(0.0, at(base, 2_000));
        assert_eq!(clock.rate(), 1.0);
        assert_eq!(clock.raw_position_at(at(base, 3_000)), 4_000);

        clock.on_rate_changed(8.0, at(base, 3_000));
        assert_eq!(clock.rate(), 8.0);
        clock.on_rate_changed(8.01, at(base, 3_000));
        assert_eq!(clock.rate(), 1.0);

        clock.on_rate_changed(0.05, at(base, 3_000));
        assert_eq!(clock.rate(), 1.0);
        clock.on_rate_changed(f64::NAN, at(base, 3_000));
        assert_eq!(clock.rate(), 1.0);
        clock.on_rate_changed(f64::INFINITY, at(base, 3_000));
        assert_eq!(clock.rate(), 1.0);

        clock.on_status_changed(PlaybackStatus::Paused, at(base, 4_000));
        clock.on_rate_changed(1.5, at(base, 4_500));
        assert_eq!(clock.rate(), 1.5);
        assert_eq!(clock.raw_position_at(at(base, 5_000)), 5_000);
        clock.on_status_changed(PlaybackStatus::Playing, at(base, 5_000));
        assert_eq!(clock.raw_position_at(at(base, 7_000)), 8_000);
    }

    #[test]
    fn seek_updates_playing_and_paused_states() {
        let base = Instant::now();
        let mut clock = playing_clock(base, 0);

        clock.on_seeked(5_000, at(base, 1_000));
        assert_eq!(clock.raw_position_at(at(base, 1_500)), 5_500);
        assert_eq!(anchor_state(&clock).0, 5_000);

        clock.on_status_changed(PlaybackStatus::Paused, at(base, 2_000));
        clock.on_seeked(9_000, at(base, 3_000));
        assert_eq!(clock.raw_position_at(at(base, 4_000)), 9_000);
    }

    #[test]
    fn position_poll_drift_zones_table() {
        let cases = [
            (1_044, 1_000, 1_000),
            (1_045, 1_000, 1_000),
            (1_046, 1_000, 1_023),
            (1_299, 1_000, 1_149),
            (1_300, 1_000, 1_150),
            (1_301, 1_000, 1_301),
            (956, 1_000, 1_000),
            (955, 1_000, 1_000),
            (954, 1_000, 977),
            (701, 1_000, 851),
            (700, 1_000, 850),
            (699, 1_000, 699),
        ];

        for (polled_ms, predicted_ms, expected_anchor_ms) in cases {
            let base = Instant::now();
            let mut clock = playing_clock(base, 0);
            clock.on_position_poll(polled_ms, Duration::ZERO, at(base, predicted_ms));

            assert_eq!(
                clock.raw_position_at(at(base, predicted_ms)),
                expected_anchor_ms,
                "polled={polled_ms}"
            );
        }
    }

    #[test]
    fn position_poll_compensates_half_rtt() {
        let base = Instant::now();
        let mut clock = playing_clock(base, 0);

        clock.on_position_poll(1_200, Duration::from_millis(400), at(base, 1_000));
        assert_eq!(clock.raw_position_at(at(base, 1_000)), 1_400);
    }

    #[test]
    fn stale_polls_are_ignored_from_the_first_repeat() {
        let base = Instant::now();
        let mut clock = playing_clock(base, 0);

        clock.on_position_poll(1_046, Duration::ZERO, at(base, 1_000));
        assert_eq!(clock.raw_position_at(at(base, 1_000)), 1_023);

        clock.on_position_poll(1_046, Duration::ZERO, at(base, 1_100));
        assert_eq!(clock.raw_position_at(at(base, 1_100)), 1_123);

        clock.on_position_poll(1_046, Duration::ZERO, at(base, 1_200));
        assert_eq!(clock.raw_position_at(at(base, 1_200)), 1_223);

        clock.on_position_poll(1_700, Duration::ZERO, at(base, 1_300));
        assert_eq!(clock.raw_position_at(at(base, 1_300)), 1_700);
        assert!(matches!(
            clock.state(),
            ClockState::Playing {
                last_polled_ms: Some(1_700),
                stale_poll_count: 0,
                ..
            }
        ));

        clock.on_position_poll(1_700, Duration::ZERO, at(base, 1_400));
        assert_eq!(clock.raw_position_at(at(base, 1_400)), 1_800);

        let mut stepped = playing_clock(base, 178_025);
        stepped.on_position_poll(178_025, Duration::ZERO, at(base, 0));
        stepped.on_position_poll(178_025, Duration::ZERO, at(base, 1_000));
        assert_eq!(stepped.raw_position_at(at(base, 1_000)), 179_025);
        stepped.on_position_poll(178_025, Duration::ZERO, at(base, 2_000));
        assert_eq!(stepped.raw_position_at(at(base, 2_000)), 180_025);
        stepped.on_position_poll(181_055, Duration::ZERO, at(base, 3_000));
        assert_eq!(stepped.raw_position_at(at(base, 3_000)), 181_025);
    }

    #[test]
    fn effective_offsets_follow_standard_lrc_and_delay_signs() {
        let base = Instant::now();
        let clock = playing_clock(base, 0);

        let offsets = EffectiveOffsets {
            intrinsic_ms: 300,
            global_ms: 50,
            player_ms: 20,
            user_ms: 100,
            lookahead_ms: 80,
        };

        assert_eq!(clock.effective_lyric_ms(at(base, 1_000), offsets), 1_210);
        assert_eq!(
            clock.effective_lyric_ms(
                at(base, 1_000),
                EffectiveOffsets {
                    user_ms: -100,
                    ..EffectiveOffsets::default()
                }
            ),
            1_100
        );
        assert_eq!(
            clock.effective_lyric_ms(
                at(base, 1_000),
                EffectiveOffsets {
                    intrinsic_ms: -2_000,
                    ..EffectiveOffsets::default()
                }
            ),
            0
        );
    }
}
