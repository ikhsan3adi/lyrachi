use lyrachi::config::RibbonMode;
use lyrachi::core::clock::PlaybackStatus;
use lyrachi::core::model::{
    Annotation, AnnotationKind, LineRole, LyricLine, SyncGranularity, SyncedTrack, WordToken,
};
use lyrachi::media::TrackMeta;
use lyrachi::ui::{LyricsView, UiState, render};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Modifier},
};

fn meta() -> TrackMeta {
    TrackMeta {
        title: "Yellow".to_string(),
        artist: "Coldplay".to_string(),
        album: Some("Parachutes".to_string()),
        duration_ms: Some(267_000),
        track_id: Some("/org/mpris/MediaPlayer2/Track/1".to_string()),
        url: None,
    }
}

fn word(_text: &str, start: usize, end: usize, start_ms: u64, end_ms: u64) -> WordToken {
    WordToken {
        byte_range: (start, end),
        start_ms,
        end_ms,
        display_width: (end - start) as u16,
    }
}

fn line(
    start_ms: u64,
    end_ms: u64,
    text: &str,
    words: Vec<WordToken>,
    annotations: Vec<Annotation>,
) -> LyricLine {
    LyricLine {
        start_ms,
        end_ms,
        role: LineRole::Primary,
        text: text.to_string(),
        words,
        annotations,
    }
}

fn word_track() -> SyncedTrack {
    SyncedTrack {
        source_provider: "lrclib".to_string(),
        source_track_id: Some("1".to_string()),
        parser_version: 1,
        is_instrumental: false,
        granularity: SyncGranularity::Word,
        intrinsic_offset_ms: 0,
        user_offset_ms: 0,
        match_confidence: 0.97,
        lines: vec![
            line(
                1_000,
                4_000,
                "Look at the stars",
                vec![
                    word("Look ", 0, 5, 1_000, 1_400),
                    word("at ", 5, 8, 1_400, 1_800),
                    word("the ", 8, 12, 1_800, 2_200),
                    word("stars", 12, 17, 2_200, 3_000),
                ],
                Vec::new(),
            ),
            line(
                4_200,
                7_000,
                "Look how they shine for you",
                vec![
                    word("Look ", 0, 5, 4_200, 4_600),
                    word("how ", 5, 9, 4_600, 5_000),
                    word("they ", 9, 14, 5_000, 5_400),
                    word("shine ", 14, 20, 5_400, 5_800),
                    word("for ", 20, 24, 5_800, 6_200),
                    word("you", 24, 27, 6_200, 7_000),
                ],
                Vec::new(),
            ),
            line(
                9_000,
                12_000,
                "And everything you do",
                vec![
                    word("And ", 0, 4, 9_000, 9_300),
                    word("everything ", 4, 15, 9_300, 9_600),
                    word("you ", 15, 19, 9_600, 10_200),
                    word("do", 19, 21, 10_200, 12_000),
                ],
                vec![Annotation {
                    kind: AnnotationKind::Translation,
                    text: "Dan semua yang kau lakukan".to_string(),
                }],
            ),
            LyricLine {
                start_ms: 13_000,
                end_ms: 16_000,
                role: LineRole::InstrumentalBreak,
                text: "[Instrumental]".to_string(),
                words: Vec::new(),
                annotations: Vec::new(),
            },
            line(
                17_000,
                20_000,
                "Yeah, they were all yellow",
                vec![
                    word("Yeah, ", 0, 6, 17_000, 17_400),
                    word("they ", 6, 11, 17_400, 17_800),
                    word("were ", 11, 16, 17_800, 18_200),
                    word("all ", 16, 20, 18_200, 18_600),
                    word("yellow", 20, 26, 18_600, 20_000),
                ],
                Vec::new(),
            ),
        ],
    }
}

fn unsynced_track() -> SyncedTrack {
    SyncedTrack {
        source_provider: "lrclib".to_string(),
        source_track_id: Some("2".to_string()),
        parser_version: 1,
        is_instrumental: false,
        granularity: SyncGranularity::Unsynced,
        intrinsic_offset_ms: 0,
        user_offset_ms: 0,
        match_confidence: 0.91,
        lines: (0..40)
            .map(|index| {
                line(
                    index * 1_000,
                    (index + 1) * 1_000,
                    &format!("Plain lyric line {}", index + 1),
                    Vec::new(),
                    Vec::new(),
                )
            })
            .collect(),
    }
}

fn instrumental_track() -> SyncedTrack {
    SyncedTrack {
        source_provider: "lrclib".to_string(),
        source_track_id: Some("4".to_string()),
        parser_version: 1,
        is_instrumental: true,
        granularity: SyncGranularity::Line,
        intrinsic_offset_ms: 0,
        user_offset_ms: 0,
        match_confidence: 0.95,
        lines: vec![line(0, 267_000, "[Instrumental]", Vec::new(), Vec::new())],
    }
}

fn base_state() -> UiState {
    let mut state = UiState::new();
    state.player = Some("limusic".to_string());
    state.playback = PlaybackStatus::Playing;
    state.track = Some(meta());
    state
}

fn no_player() -> UiState {
    UiState::new()
}

fn no_track() -> UiState {
    let mut state = base_state();
    state.track = None;
    state.lyrics = LyricsView::Idle;
    state
}

fn loading() -> UiState {
    let mut state = base_state();
    state.lyrics = LyricsView::Loading;
    state
}

fn not_found() -> UiState {
    let mut state = base_state();
    state.lyrics = LyricsView::NotFound;
    state
}

fn network_error() -> UiState {
    let mut state = base_state();
    state.lyrics = LyricsView::Error("LRCLIB request failed: operation timed out".to_string());
    state
}

fn unsynced() -> UiState {
    let mut state = base_state();
    state.set_lyrics(unsynced_track());
    state.scroll = 2;
    state
}

fn word_sync() -> UiState {
    let mut state = base_state();
    state.set_lyrics(word_track());
    state
}

fn instrumental() -> UiState {
    let mut state = base_state();
    state.set_lyrics(instrumental_track());
    state
}

fn fullscreen() -> UiState {
    let mut state = word_sync();
    state.configure_layout(true, true, true);
    state
}

fn render_state(state: &mut UiState, width: u16, height: u16, effective_ms: u64) -> String {
    state.resize(width, height);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render::render(frame, state, effective_ms))
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            if let Some(cell) = buffer.cell((x, y)) {
                output.push_str(cell.symbol());
            }
        }
        while output.ends_with(' ') {
            output.pop();
        }
        output.push('\n');
    }
    output
}

fn snapshot(state: &mut UiState, width: u16, height: u16, effective_ms: u64, name: &str) {
    insta::assert_snapshot!(name, render_state(state, width, height, effective_ms));
}

#[test]
fn no_player_80x24() {
    snapshot(&mut no_player(), 80, 24, 0, "80x24_no_player");
}

#[test]
fn no_player_120x40() {
    snapshot(&mut no_player(), 120, 40, 0, "120x40_no_player");
}

#[test]
fn no_track_80x24() {
    snapshot(&mut no_track(), 80, 24, 0, "80x24_no_track");
}

#[test]
fn no_track_120x40() {
    snapshot(&mut no_track(), 120, 40, 0, "120x40_no_track");
}

#[test]
fn loading_80x24() {
    snapshot(&mut loading(), 80, 24, 0, "80x24_loading");
}

#[test]
fn loading_120x40() {
    snapshot(&mut loading(), 120, 40, 0, "120x40_loading");
}

#[test]
fn not_found_80x24() {
    snapshot(&mut not_found(), 80, 24, 0, "80x24_not_found");
}

#[test]
fn not_found_120x40() {
    snapshot(&mut not_found(), 120, 40, 0, "120x40_not_found");
}

#[test]
fn network_error_80x24() {
    snapshot(&mut network_error(), 80, 24, 0, "80x24_network_error");
}

#[test]
fn network_error_120x40() {
    snapshot(&mut network_error(), 120, 40, 0, "120x40_network_error");
}

#[test]
fn unsynced_80x24() {
    snapshot(&mut unsynced(), 80, 24, 0, "80x24_unsynced");
}

#[test]
fn unsynced_120x40() {
    snapshot(&mut unsynced(), 120, 40, 0, "120x40_unsynced");
}

#[test]
fn word_sync_80x24() {
    snapshot(&mut word_sync(), 80, 24, 9_500, "80x24_word_sync");
}

#[test]
fn word_sync_120x40() {
    snapshot(&mut word_sync(), 120, 40, 9_500, "120x40_word_sync");
}

#[test]
fn fullscreen_80x24() {
    snapshot(&mut fullscreen(), 80, 24, 9_500, "80x24_fullscreen");
}

#[test]
fn fullscreen_120x40() {
    snapshot(&mut fullscreen(), 120, 40, 9_500, "120x40_fullscreen");
}

#[test]
fn active_word_uses_accent_style() {
    let mut state = word_sync();
    state.resize(80, 24);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render::render(frame, &state, 9_500))
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mut found = false;
    for y in 0..24 {
        let row: String = (0..80)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
            .collect();
        if let Some(offset) = row.find("And everything") {
            let x = u16::try_from(offset + "And ".len()).unwrap();
            let cell = buffer.cell((x, y)).unwrap();
            assert_eq!(cell.fg, Color::Cyan);
            assert!(cell.modifier.contains(Modifier::BOLD));
            found = true;
            break;
        }
    }
    assert!(found, "active line was not rendered");
}

#[test]
fn instrumental_80x24() {
    snapshot(&mut instrumental(), 80, 24, 0, "80x24_instrumental");
}

#[test]
fn instrumental_120x40() {
    snapshot(&mut instrumental(), 120, 40, 0, "120x40_instrumental");
}

#[test]
fn inter_line_gap_keeps_viewport_anchored_at_current_position() {
    let mut track = word_track();
    track.lines = (0..10)
        .map(|index| {
            let start = (index as u64) * 3_000 + 1_000;
            line(
                start,
                start + 1_500,
                &format!("Verse line {index}"),
                Vec::new(),
                Vec::new(),
            )
        })
        .collect();

    let mut state = base_state();
    state.set_lyrics(track);
    let rendered = render_state(&mut state, 80, 24, 5_800);

    assert!(
        rendered.contains("Verse line 1"),
        "expected line 1 to stay visible during gap after line 1:\n{rendered}"
    );
    assert!(
        rendered.contains("Verse line 2"),
        "expected upcoming line 2 to stay visible during gap:\n{rendered}"
    );
    assert!(
        !rendered.contains("Verse line 9"),
        "viewport jumped to end of song during inter-line gap:\n{rendered}"
    );
}

#[test]
fn midline_pause_preserves_sung_and_unsung_word_styles() {
    let mut track = word_track();
    track.lines = vec![line(
        1_000,
        5_000,
        "Hello  world",
        vec![
            word("Hello", 0, 5, 1_500, 2_000),
            word("world", 7, 12, 3_000, 4_000),
        ],
        Vec::new(),
    )];

    let mut state = base_state();
    state.set_lyrics(track);
    state.resize(80, 24);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // Pause between "Hello" (ends 2_000) and "world" (starts 3_000)
    terminal
        .draw(|frame| render::render(frame, &state, 2_500))
        .unwrap();

    let buffer = terminal.backend().buffer();
    let mut found = false;
    for y in 0..24 {
        let row: String = (0..80)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
            .collect();
        if let Some(offset) = row.find("Hello  world") {
            let hello_cell = buffer.cell((u16::try_from(offset).unwrap(), y)).unwrap();
            let world_cell = buffer
                .cell((u16::try_from(offset + "Hello  ".len()).unwrap(), y))
                .unwrap();

            assert_ne!(hello_cell.fg, Color::Cyan);
            assert!(!hello_cell.modifier.contains(Modifier::DIM));
            assert!(!hello_cell.modifier.contains(Modifier::BOLD));

            assert_ne!(world_cell.fg, Color::Cyan);
            assert!(world_cell.modifier.contains(Modifier::DIM));

            found = true;
            break;
        }
    }
    assert!(found, "active line was not rendered during midline pause");

    // Before first word starts (line starts 1_000, first word starts 1_500)
    terminal
        .draw(|frame| render::render(frame, &state, 1_200))
        .unwrap();
    let buffer = terminal.backend().buffer();
    for y in 0..24 {
        let row: String = (0..80)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
            .collect();
        if let Some(offset) = row.find("Hello  world") {
            let hello_cell = buffer.cell((u16::try_from(offset).unwrap(), y)).unwrap();
            assert_ne!(hello_cell.fg, Color::Cyan);
            assert!(hello_cell.modifier.contains(Modifier::DIM));
        }
    }
}

#[test]
fn waiting_countdown_80x24() {
    snapshot(&mut word_sync(), 80, 24, 14_500, "80x24_waiting_countdown");
}

#[test]
fn waiting_countdown_120x40() {
    snapshot(
        &mut word_sync(),
        120,
        40,
        14_500,
        "120x40_waiting_countdown",
    );
}

#[test]
fn help_overlay_80x24() {
    let mut state = word_sync();
    state.open_help();
    snapshot(&mut state, 80, 24, 9_500, "80x24_help_overlay");
}

#[test]
fn help_overlay_120x40() {
    let mut state = word_sync();
    state.open_help();
    snapshot(&mut state, 120, 40, 9_500, "120x40_help_overlay");
}

#[test]
fn waiting_countdown_respects_stopped_paused_and_ascii_fallback() {
    let mut state = word_sync();
    state.terminal_unicode = false;
    let ascii = render_state(&mut state, 80, 24, 14_500);
    assert!(
        ascii.contains("Instrumental  * * .  3s"),
        "expected ASCII countdown dots, got:\n{ascii}"
    );

    let mut paused = word_sync();
    paused.playback = PlaybackStatus::Paused;
    let paused_out = render_state(&mut paused, 80, 24, 14_500);
    assert!(paused_out.contains("Instrumental  ● ● ○  3s"));

    let mut stopped = word_sync();
    stopped.playback = PlaybackStatus::Stopped;
    let stopped_out = render_state(&mut stopped, 80, 24, 14_500);
    assert!(!stopped_out.contains("●"));
    assert!(stopped_out.contains("[Instrumental]"));
}

#[test]
fn ribbon_top_80x24() {
    let mut state = word_sync();
    state.configure_ribbon(RibbonMode::Top);
    snapshot(&mut state, 80, 24, 9_500, "80x24_ribbon_top");
}

#[test]
fn ribbon_bottom_120x40() {
    let mut state = word_sync();
    state.configure_ribbon(RibbonMode::Bottom);
    snapshot(&mut state, 120, 40, 9_500, "120x40_ribbon_bottom");
}

#[test]
fn ribbon_hides_below_8_rows() {
    let mut state = word_sync();
    state.configure_ribbon(RibbonMode::Top);

    let short = render_state(&mut state, 80, 7, 9_500);
    assert!(
        !short.contains("word-hl:"),
        "unexpected ribbon on 7 rows:\n{short}"
    );

    let tall = render_state(&mut state, 80, 8, 9_500);
    assert!(
        tall.contains("word-hl:"),
        "expected ribbon on 8 rows:\n{tall}"
    );
}
