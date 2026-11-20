use lyrachi::config::StatusBarMode;
use lyrachi::core::clock::PlaybackStatus;
use lyrachi::core::model::{
    Annotation, AnnotationKind, LineRole, LyricLine, SyncGranularity, SyncedTrack, WordToken,
};
use lyrachi::core::parse::ttml::parse_ttml;
use lyrachi::core::text::display_width;
use lyrachi::media::{MediaAvailability, TrackMeta};
use lyrachi::ui::{CandidateMenuItem, DisplayMode, LyricsView, UiState, render};
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
};

const SAMPLE_TTML: &str = include_str!("fixtures/sample_syllable.ttml");

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

fn role_line(
    role: LineRole,
    start_ms: u64,
    end_ms: u64,
    text: &str,
    words: Vec<WordToken>,
    annotations: Vec<Annotation>,
) -> LyricLine {
    LyricLine {
        start_ms,
        end_ms,
        role,
        text: text.to_string(),
        words,
        annotations,
    }
}

fn line(
    start_ms: u64,
    end_ms: u64,
    text: &str,
    words: Vec<WordToken>,
    annotations: Vec<Annotation>,
) -> LyricLine {
    role_line(
        LineRole::Primary,
        start_ms,
        end_ms,
        text,
        words,
        annotations,
    )
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

fn no_media_backend() -> UiState {
    let mut state = UiState::new();
    state.media_availability = MediaAvailability::Unsupported;
    state
}

fn media_session_unavailable() -> UiState {
    let mut state = UiState::new();
    state.media_availability = MediaAvailability::Unavailable;
    state
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

fn duet_ttml() -> UiState {
    let mut state = base_state();
    state.set_lyrics(parse_ttml(SAMPLE_TTML, "ttml-fixture").unwrap());
    state
}

fn lyricsplus_vocals_track() -> SyncedTrack {
    SyncedTrack {
        source_provider: "lyricsplus".to_string(),
        source_track_id: Some("lp-1".to_string()),
        parser_version: 1,
        is_instrumental: false,
        granularity: SyncGranularity::Word,
        intrinsic_offset_ms: 0,
        user_offset_ms: 0,
        match_confidence: 0.93,
        lines: vec![
            line(
                33_370,
                36_740,
                "Look at the stars",
                vec![
                    word("Look ", 0, 5, 33_370, 34_000),
                    word("at ", 5, 8, 34_000, 34_400),
                    word("the ", 8, 12, 34_500, 34_900),
                    word("stars", 12, 17, 35_000, 36_740),
                ],
                vec![
                    Annotation {
                        kind: AnnotationKind::Romanization,
                        text: "Luk at de stars".to_string(),
                    },
                    Annotation {
                        kind: AnnotationKind::Translation,
                        text: "Lihat bintang-bintang".to_string(),
                    },
                ],
            ),
            role_line(
                LineRole::DuetSecondary,
                37_000,
                39_500,
                "Yellow",
                vec![
                    word("Yel", 0, 3, 37_000, 38_200),
                    word("low", 3, 6, 38_200, 39_500),
                ],
                vec![Annotation {
                    kind: AnnotationKind::Translation,
                    text: "Kuning".to_string(),
                }],
            ),
            role_line(
                LineRole::BackgroundVocal,
                40_000,
                42_000,
                "Ooh, yellow",
                vec![
                    word("Ooh, ", 0, 5, 40_000, 40_600),
                    word("yellow", 5, 11, 40_600, 42_000),
                ],
                vec![Annotation {
                    kind: AnnotationKind::Secondary,
                    text: "echo".to_string(),
                }],
            ),
        ],
    }
}

fn background_lyricsplus() -> UiState {
    let mut state = base_state();
    state.set_lyrics(lyricsplus_vocals_track());
    state
}

fn instrumental() -> UiState {
    let mut state = base_state();
    state.set_lyrics(instrumental_track());
    state
}

fn fullscreen() -> UiState {
    let mut state = word_sync();
    state.configure_layout(true, true, StatusBarMode::Bottom);
    state
}

fn block_reveal_nfd() -> UiState {
    let mut state = base_state();
    state.display_mode = DisplayMode::Block;
    state.set_lyrics(SyncedTrack {
        source_provider: "test".to_string(),
        source_track_id: None,
        parser_version: 2,
        is_instrumental: false,
        granularity: SyncGranularity::Syllable,
        intrinsic_offset_ms: 0,
        user_offset_ms: 0,
        match_confidence: 1.0,
        lines: vec![line(
            1_000,
            4_000,
            "cafe\u{301} straße 👩🏽‍💻",
            vec![
                word("", 0, 2, 1_000, 1_400),
                word("", 2, 6, 1_400, 1_800),
                word("", 7, 11, 1_800, 2_200),
                word("", 11, 14, 2_200, 2_600),
                word("", 15, 30, 2_600, 3_000),
            ],
            Vec::new(),
        )],
    });
    state
}

fn block_word_sync() -> UiState {
    let mut state = word_sync();
    state.display_mode = DisplayMode::Block;
    state
}

fn block_duet_ttml() -> UiState {
    let mut state = duet_ttml();
    state.display_mode = DisplayMode::Block;
    state
}

fn block_background_lyricsplus() -> UiState {
    let mut state = background_lyricsplus();
    state.display_mode = DisplayMode::Block;
    state
}

fn render_buffer(state: &mut UiState, width: u16, height: u16, effective_ms: u64) -> Buffer {
    state.resize(width, height);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render::render(frame, state, effective_ms))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn find_text(buffer: &Buffer, needle: &str) -> Option<(u16, u16, String)> {
    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
            .collect();
        if let Some(offset) = row.find(needle) {
            return Some((u16::try_from(offset).unwrap(), y, row));
        }
    }
    None
}

fn render_state(state: &mut UiState, width: u16, height: u16, effective_ms: u64) -> String {
    let buffer = render_buffer(state, width, height, effective_ms);
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
fn no_media_backend_80x24() {
    snapshot(
        &mut no_media_backend(),
        80,
        24,
        0,
        "80x24_media_unsupported",
    );
}

#[test]
fn media_session_unavailable_80x24() {
    snapshot(
        &mut media_session_unavailable(),
        80,
        24,
        0,
        "80x24_media_unavailable",
    );
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
fn duet_ttml_80x24() {
    snapshot(&mut duet_ttml(), 80, 24, 3_500, "80x24_duet_ttml");
}

#[test]
fn duet_ttml_120x40() {
    snapshot(&mut duet_ttml(), 120, 40, 3_500, "120x40_duet_ttml");
}

#[test]
fn background_lyricsplus_80x24() {
    snapshot(
        &mut background_lyricsplus(),
        80,
        24,
        40_500,
        "80x24_background_lyricsplus",
    );
}

#[test]
fn background_lyricsplus_120x40() {
    snapshot(
        &mut background_lyricsplus(),
        120,
        40,
        40_500,
        "120x40_background_lyricsplus",
    );
}

#[test]
fn block_reveal_nfd_syllable_80x24() {
    snapshot(
        &mut block_reveal_nfd(),
        80,
        24,
        1_500,
        "80x24_block_reveal_nfd_syllable",
    );
}

#[test]
fn block_reveal_nfd_syllable_120x40() {
    snapshot(
        &mut block_reveal_nfd(),
        120,
        40,
        1_500,
        "120x40_block_reveal_nfd_syllable",
    );
}

#[test]
fn block_word_sync_80x24() {
    snapshot(
        &mut block_word_sync(),
        80,
        24,
        9_500,
        "80x24_block_word_sync",
    );
}

#[test]
fn block_word_sync_120x40() {
    snapshot(
        &mut block_word_sync(),
        120,
        40,
        9_500,
        "120x40_block_word_sync",
    );
}

#[test]
fn block_duet_80x24() {
    snapshot(&mut block_duet_ttml(), 80, 24, 3_500, "80x24_block_duet");
}

#[test]
fn block_duet_120x40() {
    snapshot(&mut block_duet_ttml(), 120, 40, 3_500, "120x40_block_duet");
}

#[test]
fn block_background_80x24() {
    snapshot(
        &mut block_background_lyricsplus(),
        80,
        24,
        40_500,
        "80x24_block_background",
    );
}

#[test]
fn block_background_120x40() {
    snapshot(
        &mut block_background_lyricsplus(),
        120,
        40,
        40_500,
        "120x40_block_background",
    );
}

#[test]
fn block_ascii_fallback_80x24() {
    let mut state = block_word_sync();
    state.set_terminal_unicode(false);
    snapshot(&mut state, 80, 24, 9_500, "80x24_block_ascii_fallback");
}

#[test]
fn block_ascii_fallback_120x40() {
    let mut state = block_word_sync();
    state.set_terminal_unicode(false);
    snapshot(&mut state, 120, 40, 9_500, "120x40_block_ascii_fallback");
}

#[test]
fn ascii_terminal_forces_normal_rendering() {
    let mut block = block_word_sync();
    block.set_terminal_unicode(false);
    let block_rendered = render_state(&mut block, 80, 24, 9_500);

    let mut normal = word_sync();
    normal.set_terminal_unicode(false);
    let normal_rendered = render_state(&mut normal, 80, 24, 9_500);

    assert_eq!(block_rendered, normal_rendered);
}

#[test]
fn block_mode_respects_timeline_max_width() {
    let mut state = block_word_sync();
    state.configure_layout(false, false, StatusBarMode::Off);
    state.set_max_width(Some(10));
    let buffer = render_buffer(&mut state, 80, 24, 9_500);

    for y in 0..buffer.area.height {
        let row: String = (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
            .collect();
        let row = row.trim();
        assert!(
            display_width(row) <= 10,
            "row {y} exceeded max width: {row:?}"
        );
    }
}

#[test]
fn block_mode_does_not_style_anchor_as_active_in_a_gap() {
    let buffer = render_buffer(&mut block_word_sync(), 80, 24, 0);
    let has_accent = (0..buffer.area.height).any(|y| {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)))
            .any(|cell| cell.fg == Color::Cyan)
    });
    assert!(!has_accent, "a line was styled active before playback");
}

#[test]
fn block_mode_reveals_only_the_active_word_cells() {
    let mut state = block_word_sync();
    state.configure_layout(false, false, StatusBarMode::Off);
    state.set_max_width(Some(10));
    let buffer = render_buffer(&mut state, 80, 24, 9_500);

    let (x, y, _) = find_text(&buffer, "And").expect("previous word was not rendered");
    assert_ne!(buffer.cell((x, y)).unwrap().fg, Color::Cyan);

    let (x, y, _) = find_text(&buffer, "eve").expect("active word was not rendered");
    let cell = buffer.cell((x, y)).unwrap();
    assert_eq!(cell.fg, Color::Cyan);
    assert!(cell.modifier.contains(Modifier::BOLD));
}

#[test]
fn block_mode_word_highlight_off_styles_the_whole_active_line() {
    let mut state = block_word_sync();
    state.configure_layout(false, false, StatusBarMode::Off);
    state.set_max_width(Some(10));
    state.word_highlight = false;
    let buffer = render_buffer(&mut state, 80, 24, 9_500);

    let (x, y, _) = find_text(&buffer, "And").expect("active line was not rendered");
    let cell = buffer.cell((x, y)).unwrap();
    assert_eq!(cell.fg, Color::Cyan);
    assert!(cell.modifier.contains(Modifier::BOLD));
}

#[test]
fn block_context_uses_one_font_when_one_line_falls_back() {
    let mut state = base_state();
    state.display_mode = DisplayMode::Block;
    state.set_lyrics(SyncedTrack {
        source_provider: "test".to_string(),
        source_track_id: None,
        parser_version: 2,
        is_instrumental: false,
        granularity: SyncGranularity::Line,
        intrinsic_offset_ms: 0,
        user_offset_ms: 0,
        match_confidence: 1.0,
        lines: vec![
            line(0, 1_000, "Before context", Vec::new(), Vec::new()),
            line(1_000, 3_000, "Active line", Vec::new(), Vec::new()),
            line(3_000, 5_000, "你好世界", Vec::new(), Vec::new()),
        ],
    });

    let buffer = render_buffer(&mut state, 120, 40, 1_500);
    assert!(
        find_text(&buffer, "Before context").is_some(),
        "all context lines must use plain text when one cannot render as block"
    );
    assert!(find_text(&buffer, "你").is_some());
    assert!(
        find_text(&buffer, "Active line").is_none(),
        "the active line should keep block typography"
    );
}

#[test]
fn block_mode_handles_nfd_eszett_and_emoji_without_panicking() {
    let mut state = base_state();
    state.display_mode = DisplayMode::Block;
    state.set_lyrics(SyncedTrack {
        source_provider: "test".to_string(),
        source_track_id: None,
        parser_version: 2,
        is_instrumental: false,
        granularity: SyncGranularity::Line,
        intrinsic_offset_ms: 0,
        user_offset_ms: 0,
        match_confidence: 1.0,
        lines: vec![line(
            0,
            1_000,
            "cafe\u{301} straße 👩🏽‍💻",
            Vec::new(),
            Vec::new(),
        )],
    });

    render_buffer(&mut state, 80, 24, 500);
}

#[test]
fn disabling_word_highlight_styles_the_whole_active_line() {
    let mut state = word_sync();
    state.word_highlight = false;
    let buffer = render_buffer(&mut state, 80, 24, 9_500);
    let (x, y, _) = find_text(&buffer, "And everything").expect("active line was not rendered");
    let cell = buffer.cell((x, y)).unwrap();
    assert_eq!(cell.fg, Color::Cyan);
    assert!(cell.modifier.contains(Modifier::BOLD));
}

#[test]
fn duet_secondary_uses_secondary_accent_and_right_alignment() {
    let mut state = duet_ttml();
    let buffer = render_buffer(&mut state, 80, 24, 3_500);
    let (x, y, row) = find_text(&buffer, "Second voice").expect("duet line was not rendered");
    let cell = buffer.cell((x, y)).unwrap();
    assert_eq!(cell.fg, Color::LightMagenta);
    assert!(!cell.modifier.contains(Modifier::DIM));
    assert!(
        x > u16::try_from((80 - "Second voice".len()) / 2).unwrap(),
        "duet line was not right-aligned:\n{row}"
    );
}

#[test]
fn duet_secondary_stays_readable_on_narrow_viewport() {
    let mut state = duet_ttml();
    let buffer = render_buffer(&mut state, 40, 24, 3_500);
    let (x, y, row) = find_text(&buffer, "Second voice").expect("duet line was not rendered");
    let cell = buffer.cell((x, y)).unwrap();

    assert_eq!(x, 2);
    assert_eq!(cell.fg, Color::LightMagenta);
    assert!(row.trim_end().ends_with("Second voice"));
}

#[test]
fn background_vocal_is_dim_italic_and_annotations_are_visible() {
    let mut state = background_lyricsplus();
    let buffer = render_buffer(&mut state, 80, 24, 40_500);

    let (x, y, _) = find_text(&buffer, "Ooh, yellow").expect("background line was not rendered");
    let cell = buffer.cell((x, y)).unwrap();
    assert_eq!(cell.fg, Color::Gray);
    assert!(cell.modifier.contains(Modifier::ITALIC));
    assert!(cell.modifier.contains(Modifier::BOLD));

    let (x, y, _) = find_text(&buffer, "yellow").expect("background word was not rendered");
    let cell = buffer.cell((x, y)).unwrap();
    assert_eq!(cell.fg, Color::DarkGray);
    assert!(cell.modifier.contains(Modifier::ITALIC));

    for annotation in ["Luk at de stars", "Lihat bintang-bintang", "Kuning", "echo"] {
        let (x, y, _) =
            find_text(&buffer, annotation).unwrap_or_else(|| panic!("missing {annotation}"));
        let cell = buffer.cell((x, y)).unwrap();
        assert!(cell.modifier.contains(Modifier::DIM | Modifier::ITALIC));
    }
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
        ascii.contains("* . ."),
        "expected ASCII countdown dots, got:\n{ascii}"
    );

    let mut paused = word_sync();
    paused.playback = PlaybackStatus::Paused;
    let paused_out = render_state(&mut paused, 80, 24, 14_500);
    assert!(paused_out.contains("● ○ ○"));

    let mut stopped = word_sync();
    stopped.playback = PlaybackStatus::Stopped;
    let stopped_out = render_state(&mut stopped, 80, 24, 14_500);
    assert!(!stopped_out.contains("●"));
    assert!(stopped_out.contains("♫"));
}

#[test]
fn status_bar_top_80x24() {
    let mut state = word_sync();
    state.configure_status_bar(StatusBarMode::Top);
    snapshot(&mut state, 80, 24, 9_500, "80x24_status_bar_top");
}

#[test]
fn status_bar_bottom_120x40() {
    let mut state = word_sync();
    state.configure_status_bar(StatusBarMode::Bottom);
    snapshot(&mut state, 120, 40, 9_500, "120x40_status_bar_bottom");
}

#[test]
fn status_bar_auto_hides_below_8_rows() {
    let mut state = word_sync();
    state.configure_status_bar(StatusBarMode::Auto);

    let short = render_state(&mut state, 80, 7, 9_500);
    assert!(
        !short.contains("playing · normal"),
        "unexpected status bar on 7 rows:\n{short}"
    );

    let tall = render_state(&mut state, 80, 8, 9_500);
    assert!(
        tall.contains("playing · normal"),
        "expected status bar on 8 rows:\n{tall}"
    );
}

#[test]
fn context_menu_actions_80x24() {
    let mut state = word_sync();
    state.pinned_source_label = Some("lrclib".to_string());
    assert!(state.open_context_menu());
    snapshot(&mut state, 80, 24, 9_500, "80x24_context_menu_actions");
}

#[test]
fn context_menu_actions_120x40() {
    let mut state = word_sync();
    state.pinned_source_label = Some("lrclib".to_string());
    state.pinned_style = Some("builtin:default".to_string());
    assert!(state.open_context_menu());
    snapshot(&mut state, 120, 40, 9_500, "120x40_context_menu_actions");
}

#[test]
fn context_menu_candidates_80x24() {
    let mut state = word_sync();
    state.open_candidate_loading("Coldplay - Yellow".to_string());
    state.set_candidate_results(
        vec![
            CandidateMenuItem {
                provider_label: "lyricsplus".to_string(),
                title: "Yellow".to_string(),
                artist: Some("Coldplay".to_string()),
                album: Some("Parachutes".to_string()),
                duration_ms: Some(267_000),
                tier_label: "syllable",
                score_pct: 99,
                is_active: false,
            },
            CandidateMenuItem {
                provider_label: "lrclib".to_string(),
                title: "Yellow".to_string(),
                artist: Some("Coldplay".to_string()),
                album: Some("Parachutes".to_string()),
                duration_ms: Some(267_000),
                tier_label: "word",
                score_pct: 97,
                is_active: true,
            },
            CandidateMenuItem {
                provider_label: "local".to_string(),
                title: "Yellow (Live)".to_string(),
                artist: Some("Coldplay".to_string()),
                album: None,
                duration_ms: Some(271_000),
                tier_label: "line",
                score_pct: 84,
                is_active: false,
            },
        ],
        None,
    );
    snapshot(&mut state, 80, 24, 9_500, "80x24_context_menu_candidates");
}

#[test]
fn context_menu_candidates_120x40() {
    let mut state = word_sync();
    state.open_candidate_loading("Coldplay - Yellow".to_string());
    state.set_candidate_results(
        vec![
            CandidateMenuItem {
                provider_label: "lyricsplus".to_string(),
                title: "Yellow".to_string(),
                artist: Some("Coldplay".to_string()),
                album: Some("Parachutes".to_string()),
                duration_ms: Some(267_000),
                tier_label: "syllable",
                score_pct: 99,
                is_active: false,
            },
            CandidateMenuItem {
                provider_label: "lrclib".to_string(),
                title: "Yellow".to_string(),
                artist: Some("Coldplay".to_string()),
                album: Some("Parachutes".to_string()),
                duration_ms: Some(267_000),
                tier_label: "word",
                score_pct: 97,
                is_active: true,
            },
        ],
        None,
    );
    snapshot(&mut state, 120, 40, 9_500, "120x40_context_menu_candidates");
}

#[test]
fn context_menu_search_input_80x24() {
    let mut state = word_sync();
    state.open_search_input("Coldplay".to_string(), "Yellow".to_string());
    snapshot(&mut state, 80, 24, 9_500, "80x24_search_input");
}

#[test]
fn context_menu_search_input_120x40() {
    let mut state = word_sync();
    state.open_search_input("Coldplay".to_string(), "Yellow (Acoustic)".to_string());
    snapshot(&mut state, 120, 40, 9_500, "120x40_search_input");
}

#[test]
fn block_waiting_countdown_80x24() {
    let mut state = word_sync();
    state.display_mode = DisplayMode::Block;
    snapshot(&mut state, 80, 24, 14_500, "80x24_block_waiting_countdown");
}

#[test]
fn block_waiting_countdown_120x40() {
    let mut state = word_sync();
    state.display_mode = DisplayMode::Block;
    snapshot(
        &mut state,
        120,
        40,
        14_500,
        "120x40_block_waiting_countdown",
    );
}

#[test]
fn kinetic_phrase_80x24() {
    let mut state = word_sync();
    state.display_mode = DisplayMode::Kinetic;
    snapshot(&mut state, 80, 24, 9_500, "80x24_kinetic_phrase");
}

#[test]
fn kinetic_phrase_120x40() {
    let mut state = word_sync();
    state.display_mode = DisplayMode::Kinetic;
    snapshot(&mut state, 120, 40, 9_500, "120x40_kinetic_phrase");
}
