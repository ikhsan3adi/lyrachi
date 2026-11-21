use super::{
    ParseError, ParsedLine, ParsedTrack, WordTimeMode, parse_line_header, parse_tagged_words,
    trim_line_with_words,
};
use crate::core::model::LineRole;

pub fn parse_krc(input: &str, track_duration_ms: Option<u64>) -> Result<ParsedTrack, ParseError> {
    let mut track = ParsedTrack {
        track_duration_ms,
        ..ParsedTrack::default()
    };

    for raw_line in input.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with("[id:") || line.starts_with("[language:") {
            continue;
        }
        if apply_metadata_tag(&mut track, line) {
            continue;
        }
        let Some((start_ms, duration_ms, rest)) = parse_line_header(line) else {
            continue;
        };

        let (text, words, explicit_last_word_end_ms) =
            parse_tagged_words(rest, start_ms, '<', '>', WordTimeMode::Relative);
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

fn apply_metadata_tag(track: &mut ParsedTrack, line: &str) -> bool {
    let Some(tag) = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let Some((key, value)) = tag.split_once(':') else {
        return false;
    };
    let value = value.trim();
    match key.trim().to_ascii_lowercase().as_str() {
        "ti" => track.metadata.title = (!value.is_empty()).then(|| value.to_string()),
        "ar" => track.metadata.artist = (!value.is_empty()).then(|| value.to_string()),
        "al" => track.metadata.album = (!value.is_empty()).then(|| value.to_string()),
        "la" | "language" => {
            track.metadata.language = (!value.is_empty()).then(|| value.to_string());
        }
        "offset" => {
            if let Ok(offset_ms) = value.parse::<i64>() {
                track.intrinsic_offset_ms = offset_ms;
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relative_word_times() {
        let track = parse_krc(
            "[1000,2000]<0,500,0>Hel<500,500,0>lo <1000,1000,0>you",
            None,
        )
        .unwrap();

        let line = &track.lines[0];
        assert_eq!(line.text, "Hello you");
        assert_eq!(line.words[0].start_ms, 1_000);
        assert_eq!(line.words[1].start_ms, 1_500);
        assert_eq!(line.words[2].end_ms, Some(3_000));
    }

    #[test]
    fn skips_krc_headers() {
        let track =
            parse_krc("[id:$00000000]\n[language:abc]\n[0,1000]<0,1000,0>hi", None).unwrap();
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].text, "hi");
    }

    #[test]
    fn keeps_text_when_tag_is_unclosed() {
        let track = parse_krc("[0,1000]<0,500,0>I <3 you", None).unwrap();
        assert_eq!(track.lines[0].text, "I <3 you");
    }
}
