use lyrachi::core::model::{
    AnnotationKind, LineRole, PARSER_VERSION, SyncGranularity, SyncedTrack, is_intra_word_join,
};
use lyrachi::core::normalize::normalize_track;
use lyrachi::core::parse::lrc::parse_lrc;
use lyrachi::core::parse::lyricsfile::parse_lyricsfile;
use lyrachi::core::text::{display_width, grapheme_count};

const STANDARD: &str = include_str!("fixtures/standard_multi_tag.lrc");
const ELRC: &str = include_str!("fixtures/enhanced_karaoke.elrc");
const SYLLABLE_ELRC: &str = include_str!("fixtures/syllable_karaoke.elrc");
const MULTILINGUAL: &str = include_str!("fixtures/multilingual_cjk_emoji.lrc");
const YELLOW: &str = include_str!("fixtures/lrclib_yellow_wordsync.json");

fn assert_track_invariants(track: &SyncedTrack) {
    for line in &track.lines {
        assert!(
            line.start_ms <= line.end_ms,
            "line start {} > end {}",
            line.start_ms,
            line.end_ms
        );

        for word in &line.words {
            let (start, end) = word.byte_range;
            assert!(start < end, "empty word range {start}..{end}");
            assert!(end <= line.text.len(), "word range past text");
            assert!(line.text.is_char_boundary(start));
            assert!(line.text.is_char_boundary(end));
            assert!(
                word.start_ms <= word.end_ms,
                "word start {} > end {}",
                word.start_ms,
                word.end_ms
            );
            assert!(word.start_ms >= line.start_ms);
            assert!(word.end_ms <= line.end_ms);
        }

        for pair in line.words.windows(2) {
            assert!(
                pair[0].end_ms <= pair[1].start_ms,
                "word end {} overlaps next word start {}",
                pair[0].end_ms,
                pair[1].start_ms
            );
        }
    }

    for pair in track.lines.windows(2) {
        assert!(
            pair[0].start_ms < pair[1].start_ms,
            "line starts are not increasing: {} then {}",
            pair[0].start_ms,
            pair[1].start_ms
        );
        assert!(
            pair[0].end_ms < pair[1].start_ms,
            "line end {} overlaps next start {}",
            pair[0].end_ms,
            pair[1].start_ms
        );
    }
}

#[test]
fn standard_lrc_keeps_metadata_offsets_and_instrumental_gaps() {
    let parsed = parse_lrc(STANDARD, None).unwrap();
    assert_eq!(parsed.metadata.title.as_deref(), Some("Signal Test"));
    assert_eq!(parsed.intrinsic_offset_ms, -300);
    assert_eq!(parsed.track_duration_ms, Some(120_000));
    assert_eq!(
        parsed
            .lines
            .iter()
            .filter(|line| line.text == "Repeated chorus")
            .count(),
        2
    );

    let track = normalize_track(parsed, "fixture", None, 1.0, None, 0);
    assert_eq!(track.granularity, SyncGranularity::Line);
    let second_line = track
        .lines
        .iter()
        .find(|line| line.text == "Second line")
        .unwrap();
    assert_eq!(second_line.start_ms, 3_500);
    assert_eq!(second_line.end_ms, 5_000);
    assert_eq!(track.lines.last().unwrap().end_ms, 120_000);
    assert!(
        track
            .lines
            .iter()
            .any(|line| line.role == LineRole::InstrumentalBreak && line.start_ms == 5_001)
    );
    assert_track_invariants(&track);
}

#[test]
fn elrc_resolves_word_end_from_next_word_or_trailing_tag() {
    let track = normalize_track(
        parse_lrc(ELRC, None).unwrap(),
        "fixture",
        None,
        1.0,
        None,
        0,
    );

    assert_eq!(track.granularity, SyncGranularity::Word);
    assert_eq!(track.lines[0].words.len(), 4);
    assert_eq!(track.lines[0].words[0].end_ms, 10_400);
    assert_eq!(track.lines[0].words[3].end_ms, 12_100);
    assert_eq!(track.lines[2].words.last().unwrap().end_ms, 16_000);
    assert_track_invariants(&track);
}

#[test]
fn elrc_intra_word_tags_produce_syllable_granularity() {
    let track = normalize_track(
        parse_lrc(SYLLABLE_ELRC, None).unwrap(),
        "fixture",
        None,
        1.0,
        None,
        0,
    );

    let line = &track.lines[0];
    assert_eq!(track.parser_version, PARSER_VERSION);
    assert_eq!(track.granularity, SyncGranularity::Syllable);
    assert_eq!(line.text, "hello world");
    assert_eq!(line.words.len(), 3);
    assert!(is_intra_word_join(
        &line.text,
        &line.words[0],
        &line.words[1]
    ));
    assert!(!is_intra_word_join(
        &line.text,
        &line.words[1],
        &line.words[2]
    ));
    assert_track_invariants(&track);
}

#[test]
fn elrc_space_separated_tags_produce_word_granularity() {
    let track = normalize_track(
        parse_lrc(
            "[00:01.00]<00:01.00>hel <00:01.20>lo <00:01.50>world\n",
            None,
        )
        .unwrap(),
        "fixture",
        None,
        1.0,
        None,
        0,
    );

    let line = &track.lines[0];
    assert_eq!(track.granularity, SyncGranularity::Word);
    assert_eq!(line.text, "hel lo world");
    assert_eq!(line.words.len(), 3);
    assert!(!is_intra_word_join(
        &line.text,
        &line.words[0],
        &line.words[1]
    ));
    assert_track_invariants(&track);
}

#[test]
fn real_yellow_lyricsfile_parses_with_word_ranges() {
    let value: serde_json::Value = serde_json::from_str(YELLOW).unwrap();
    let lyricsfile = value["lyricsfile"].as_str().unwrap();
    let parsed = parse_lyricsfile(lyricsfile).unwrap();

    assert_eq!(parsed.metadata.title.as_deref(), Some("Yellow"));
    assert_eq!(parsed.metadata.artist.as_deref(), Some("Coldplay"));
    assert_eq!(parsed.track_duration_ms, Some(267_000));
    assert!(!parsed.lines.is_empty());

    let track = normalize_track(parsed, "lrclib", Some("16232".to_string()), 0.95, None, 0);

    assert_eq!(track.granularity, SyncGranularity::Word);
    assert_eq!(track.lines[0].start_ms, 33_370);
    assert_eq!(track.lines[0].end_ms, 34_667);
    assert_eq!(track.lines[0].words.len(), 4);
    assert_eq!(
        &track.lines[0].text
            [track.lines[0].words[0].byte_range.0..track.lines[0].words[0].byte_range.1],
        "Look "
    );
    assert_track_invariants(&track);
}

#[test]
fn multilingual_lines_merge_into_typed_annotations() {
    let track = normalize_track(
        parse_lrc(MULTILINGUAL, None).unwrap(),
        "fixture",
        None,
        1.0,
        None,
        0,
    );

    let first = track
        .lines
        .iter()
        .find(|line| line.start_ms == 10_000)
        .unwrap();
    assert_eq!(first.annotations.len(), 2);
    assert_eq!(first.annotations[0].kind, AnnotationKind::Romanization);
    assert_eq!(first.annotations[1].kind, AnnotationKind::Romanization);

    let secondary = track
        .lines
        .iter()
        .find(|line| line.start_ms == 12_000)
        .unwrap();
    assert_eq!(secondary.annotations.len(), 1);
    assert_eq!(secondary.annotations[0].kind, AnnotationKind::Secondary);

    assert_eq!(grapheme_count("👩🏽‍💻"), 1);
    assert_eq!(display_width("世界"), 4);
    assert_track_invariants(&track);
}
