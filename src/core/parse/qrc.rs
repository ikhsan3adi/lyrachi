use super::{
    ParseError, ParsedLine, ParsedTrack, WordTimeMode, parse_line_header,
    parse_tagged_words_decoded, trim_line_with_words,
};
use crate::core::model::LineRole;

pub fn parse_qrc(input: &str, track_duration_ms: Option<u64>) -> Result<ParsedTrack, ParseError> {
    let content = unwrap_qrc(input);
    let mut track = ParsedTrack {
        track_duration_ms,
        ..ParsedTrack::default()
    };

    for raw_line in content.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        if apply_metadata_tag(&mut track, line) {
            continue;
        }
        let Some((start_ms, duration_ms, rest)) = parse_line_header(line) else {
            continue;
        };

        let (text, words, explicit_last_word_end_ms) = parse_tagged_words_decoded(
            rest,
            start_ms,
            '(',
            ')',
            WordTimeMode::Absolute,
            decode_entities,
        );
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

fn unwrap_qrc(input: &str) -> &str {
    let input = input.trim();
    if !input.starts_with('<') {
        return input;
    }

    let Some(start) = input.find('[') else {
        return "";
    };
    let Some(end) = input.rfind('"') else {
        return "";
    };
    if start < end { &input[start..end] } else { "" }
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#039;", "'")
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
    fn unwraps_xml_and_parses_absolute_words() {
        let xml = r#"<QrcInfos><LyricInfo><Lyric_1 LyricContent="[1000,1000](1000,400,0)Look &amp; (1400,600,0)shine"/></LyricInfo></QrcInfos>"#;
        let track = parse_qrc(xml, None).unwrap();

        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].text, "Look & shine");
        assert_eq!(track.lines[0].words[0].start_ms, 1_000);
        assert_eq!(track.lines[0].words[1].start_ms, 1_400);

        let line = &track.lines[0];
        let first = line.words[0].byte_range;
        let second = line.words[1].byte_range;
        assert_eq!(&line.text[first.0..first.1], "Look &");
        assert_eq!(&line.text[second.0..second.1], "shine");
    }

    #[test]
    fn preserves_literal_parentheses() {
        let track = parse_qrc("[0,1000](0,400,0)Hello (world)", None).unwrap();
        assert_eq!(track.lines[0].text, "Hello (world)");
    }

    #[test]
    fn accepts_plain_qrc_text() {
        let track = parse_qrc("[0,500](0,500,0)Hello", None).unwrap();
        assert_eq!(track.lines[0].text, "Hello");
        assert_eq!(track.lines[0].end_ms, Some(500));
    }
}
