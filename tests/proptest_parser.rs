use std::fmt::Write;

use lyrachi::core::model::SyncedTrack;
use lyrachi::core::normalize::normalize_track;
use lyrachi::core::parse::lrc::parse_lrc;
use lyrachi::core::text::{grapheme_count, wrap_text};
use proptest::prelude::*;

fn assert_track_invariants(track: &SyncedTrack) {
    for line in &track.lines {
        assert!(line.start_ms <= line.end_ms);
        for word in &line.words {
            let (start, end) = word.byte_range;
            assert!(start < end);
            assert!(end <= line.text.len());
            assert!(line.text.is_char_boundary(start));
            assert!(line.text.is_char_boundary(end));
            let slice = &line.text[start..end];
            assert_eq!(slice.trim(), slice);
            assert!(word.start_ms <= word.end_ms);
            assert!(word.start_ms >= line.start_ms);
            assert!(word.end_ms <= line.end_ms);
        }
        for pair in line.words.windows(2) {
            assert!(pair[0].end_ms <= pair[1].start_ms);
        }
    }

    for pair in track.lines.windows(2) {
        assert!(pair[0].start_ms < pair[1].start_ms);
        assert!(pair[0].end_ms < pair[1].start_ms);
    }
}

proptest! {
    #[test]
    fn arbitrary_lrc_never_panics(input in proptest::collection::vec(proptest::char::any(), 0..2000)) {
        let input: String = input.into_iter().collect();
        if let Ok(parsed) = parse_lrc(&input, None) {
            let track = normalize_track(parsed, "fuzz", None, 0.0, None, 0);
            assert_track_invariants(&track);
        }
    }

    #[test]
    fn generated_lrc_keeps_normalization_invariants(
        lines in proptest::collection::vec(
            (
                0u64..10,
                0u64..60,
                0u64..100,
                proptest::sample::select(vec!["hello", "世界", "✨", "", "abc def"]),
            ),
            0..20,
        ),
    ) {
        let mut input = String::new();
        for (minutes, seconds, centis, text) in lines {
            writeln!(&mut input, "[{minutes:02}:{seconds:02}.{centis:02}]{text}").unwrap();
        }

        if let Ok(parsed) = parse_lrc(&input, None) {
            let track = normalize_track(parsed, "fuzz", None, 0.0, None, 0);
            assert_track_invariants(&track);
        }
    }

    #[test]
    fn generated_elrc_keeps_word_and_line_invariants(
        lines in proptest::collection::vec(
            (
                0u64..10,
                0u64..60,
                0u64..100,
                proptest::collection::vec(
                    (
                        0u64..10,
                        0u64..60,
                        0u64..100,
                        proptest::sample::select(vec![
                            "hello ", "世界 ", "✨ ", "hel", "lo ", "世", "界 ", "", "end",
                        ]),
                    ),
                    1..8,
                ),
            ),
            1..15,
        ),
    ) {
        let mut input = String::new();
        for (l_min, l_sec, l_cs, words) in lines {
            write!(&mut input, "[{l_min:02}:{l_sec:02}.{l_cs:02}]").unwrap();
            for (w_min, w_sec, w_cs, word) in words {
                write!(&mut input, "<{w_min:02}:{w_sec:02}.{w_cs:02}>{word}").unwrap();
            }
            writeln!(&mut input).unwrap();
        }

        if let Ok(parsed) = parse_lrc(&input, None) {
            let track = normalize_track(parsed, "fuzz", None, 0.0, None, 0);
            assert_track_invariants(&track);
        }
    }

    #[test]
    fn wrapping_preserves_graphemes_and_source_ranges(
        text in proptest::collection::vec(
            proptest::sample::select(vec!["a", " ", "世", "👩🏽‍💻", "✨", "é"]),
            0..80,
        ),
        max_width in 1u16..80,
    ) {
        let text: String = text.into_iter().collect();
        for segment in wrap_text(&text, max_width) {
            let (start, end) = segment.source_range;
            assert!(text.is_char_boundary(start));
            assert!(text.is_char_boundary(end));
            assert_eq!(&text[start..end], segment.text);
            if grapheme_count(&segment.text) > 1 {
                assert!(segment.display_width <= max_width);
            }
        }
    }
}
