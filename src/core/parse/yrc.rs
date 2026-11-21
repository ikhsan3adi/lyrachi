use super::{
    ParseError, ParsedLine, ParsedTrack, WordTimeMode, parse_line_header, parse_tagged_words,
    trim_line_with_words,
};
use crate::core::model::LineRole;

pub fn parse_yrc(input: &str, track_duration_ms: Option<u64>) -> Result<ParsedTrack, ParseError> {
    let mut track = ParsedTrack {
        track_duration_ms,
        ..ParsedTrack::default()
    };

    for raw_line in input.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('{') {
            continue;
        }
        let Some((start_ms, duration_ms, rest)) = parse_line_header(line) else {
            continue;
        };

        let (text, words, explicit_last_word_end_ms) =
            parse_tagged_words(rest, start_ms, '(', ')', WordTimeMode::Absolute);
        let (text, words) = trim_line_with_words(text, words);
        if text.is_empty() && words.is_empty() {
            continue;
        }

        track.lines.push(ParsedLine {
            start_ms,
            end_ms: Some(start_ms.saturating_add(duration_ms)),
            role: LineRole::Primary,
            text,
            words,
            annotations: Vec::new(),
            explicit_last_word_end_ms,
        });
    }

    track.lines.sort_by_key(|line| line.start_ms);
    if track.lines.is_empty() {
        return Err(ParseError::Empty);
    }
    Ok(track)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_absolute_word_times() {
        let track = parse_yrc(
            "[1000,1000](1000,400,0)Look (1400,600,0)at stars\n[2500,500](2500,500,0)Shine",
            None,
        )
        .unwrap();

        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].text, "Look at stars");
        assert_eq!(track.lines[0].words[0].start_ms, 1_000);
        assert_eq!(track.lines[0].words[1].start_ms, 1_400);
        assert_eq!(track.lines[1].start_ms, 2_500);
        assert_eq!(track.lines[1].end_ms, Some(3_000));
    }

    #[test]
    fn ignores_metadata_and_empty_lines() {
        let track = parse_yrc("{\"t\":0}\n[0,1000](0,1000,0) hi", None).unwrap();
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].text, "hi");
    }
}
