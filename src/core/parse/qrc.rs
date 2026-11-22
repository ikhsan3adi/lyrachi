use super::{
    ParseError, ParsedLine, ParsedTrack, ParsedWord, WordTimeMode, parse_line_header,
    parse_tagged_words_decoded, trim_line_with_words,
};
use crate::core::model::LineRole;

pub fn parse_qrc(input: &str, track_duration_ms: Option<u64>) -> Result<ParsedTrack, ParseError> {
    let content = unwrap_qrc(input);
    let mut track = ParsedTrack {
        track_duration_ms,
        ..ParsedTrack::default()
    };
    let mut singers = SingerState::default();

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

        let (text, words, role, explicit_last_word_end_ms) =
            match strip_singer_prefix(text, words, explicit_last_word_end_ms, &mut singers) {
                SingerPrefix::Drop => continue,
                SingerPrefix::Line {
                    text,
                    words,
                    role,
                    explicit_last_word_end_ms,
                } => (text, words, role, explicit_last_word_end_ms),
            };
        if text.is_empty() && words.is_empty() {
            continue;
        }

        track.lines.push(ParsedLine {
            start_ms,
            end_ms: Some(start_ms.saturating_add(duration_ms)),
            role,
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

#[derive(Default)]
struct SingerState {
    primary: Option<String>,
    current: Option<String>,
}

enum SingerPrefix {
    Drop,
    Line {
        text: String,
        words: Vec<ParsedWord>,
        role: LineRole,
        explicit_last_word_end_ms: Option<u64>,
    },
}

const SINGER_SCAN_CHARS: usize = 40;
const SINGER_MAX_NAME_WITH_TEXT: usize = 20;
const SINGER_MAX_NAME_PREFIX_ONLY: usize = 30;

fn strip_singer_prefix(
    text: String,
    words: Vec<ParsedWord>,
    explicit_last_word_end_ms: Option<u64>,
    singers: &mut SingerState,
) -> SingerPrefix {
    let colon = text
        .char_indices()
        .take(SINGER_SCAN_CHARS)
        .find(|(_, character)| matches!(character, ':' | '：'));

    let Some((colon_byte, colon_char)) = colon else {
        return SingerPrefix::Line {
            role: singers.role(),
            text,
            words,
            explicit_last_word_end_ms,
        };
    };

    let name = text[..colon_byte].trim();
    let rest = &text[colon_byte + colon_char.len_utf8()..];
    if name.is_empty() || is_credit_label(name) {
        return SingerPrefix::Line {
            role: singers.role(),
            text,
            words,
            explicit_last_word_end_ms,
        };
    }

    let prefix_only = rest.trim().is_empty();
    let max_name = if prefix_only {
        SINGER_MAX_NAME_PREFIX_ONLY
    } else {
        SINGER_MAX_NAME_WITH_TEXT
    };
    if name.chars().count() > max_name {
        return SingerPrefix::Line {
            role: singers.role(),
            text,
            words,
            explicit_last_word_end_ms,
        };
    }

    let role = singers.register(name);
    if prefix_only {
        return SingerPrefix::Drop;
    }

    let prefix_end = colon_byte + colon_char.len_utf8() + (rest.len() - rest.trim_start().len());
    let stripped = text[prefix_end..].to_string();
    let words = words
        .into_iter()
        .filter_map(|word| {
            if word.byte_range.1 <= prefix_end {
                return None;
            }
            let start = word.byte_range.0.max(prefix_end) - prefix_end;
            let end = word.byte_range.1 - prefix_end;
            Some(ParsedWord {
                byte_range: (start, end),
                start_ms: word.start_ms,
                end_ms: word.end_ms,
            })
        })
        .collect::<Vec<_>>();
    let (text, words) = trim_line_with_words(stripped, words);
    let explicit_last_word_end_ms = words.last().and_then(|word| word.end_ms);
    SingerPrefix::Line {
        role,
        text,
        words,
        explicit_last_word_end_ms,
    }
}

impl SingerState {
    fn role(&self) -> LineRole {
        match (&self.current, &self.primary) {
            (Some(current), Some(primary)) if current == primary => LineRole::Primary,
            (Some(_), Some(_)) => LineRole::DuetSecondary,
            _ => LineRole::Primary,
        }
    }

    fn register(&mut self, name: &str) -> LineRole {
        match &self.primary {
            None => {
                self.primary = Some(name.to_string());
                self.current = Some(name.to_string());
                LineRole::Primary
            }
            Some(primary) => {
                let role = if primary == name {
                    LineRole::Primary
                } else {
                    LineRole::DuetSecondary
                };
                self.current = Some(name.to_string());
                role
            }
        }
    }
}

const CREDIT_LABELS: [&str; 47] = [
    "词",
    "詞",
    "作词",
    "作詞",
    "曲",
    "作曲",
    "词曲",
    "詞曲",
    "作词作曲",
    "作詞作曲",
    "writtenby",
    "lyricsby",
    "composedby",
    "lyricist",
    "composer",
    "编曲",
    "編曲",
    "和声",
    "和聲",
    "混音",
    "吉他",
    "制作人",
    "製作人",
    "演唱",
    "原唱",
    "翻唱",
    "后期",
    "後期",
    "和音",
    "录音",
    "錄音",
    "策划",
    "策劃",
    "伴奏",
    "美工",
    "海报",
    "海報",
    "旁白",
    "producedby",
    "arrangedby",
    "mixing",
    "mastering",
    "vocal",
    "vocals",
    "guitar",
    "bass",
    "drums",
];

const CREDIT_SUFFIXES: [char; 6] = ['词', '詞', '曲', '声', '聲', '音'];

fn is_credit_label(label: &str) -> bool {
    let normalized = label
        .to_lowercase()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if CREDIT_LABELS.contains(&normalized.as_str()) {
        return true;
    }
    let parts = normalized
        .split(['/', '&', '、', ',', '，', '・', '·'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    !parts.is_empty()
        && parts.iter().all(|part| {
            part.chars().count() <= 4
                && CREDIT_SUFFIXES.iter().any(|suffix| part.ends_with(*suffix))
        })
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

    #[test]
    fn strips_singer_prefix_and_maps_duet_roles() {
        let track = parse_qrc(
            "[0,1000](0,400,0)Alice：hello (400,600,0)there\n\
             [2000,1000](2000,1000,0)still alice\n\
             [3000,1000](3000,400,0)Bob：hi (3400,600,0)you",
            None,
        )
        .unwrap();

        assert_eq!(track.lines.len(), 3);
        assert_eq!(track.lines[0].role, LineRole::Primary);
        assert_eq!(track.lines[0].text, "hello there");
        assert_eq!(track.lines[1].role, LineRole::Primary);
        assert_eq!(track.lines[1].text, "still alice");
        assert_eq!(track.lines[2].role, LineRole::DuetSecondary);
        assert_eq!(track.lines[2].text, "hi you");
    }

    #[test]
    fn prefix_only_line_names_the_singer_and_is_dropped() {
        let track = parse_qrc(
            "[0,500](0,500,0)Alice：\n\
             [1000,1000](1000,1000,0)first line\n\
             [3000,500](3000,500,0)Bob：\n\
             [4000,1000](4000,1000,0)second line",
            None,
        )
        .unwrap();

        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].text, "first line");
        assert_eq!(track.lines[0].role, LineRole::Primary);
        assert_eq!(track.lines[1].text, "second line");
        assert_eq!(track.lines[1].role, LineRole::DuetSecondary);
    }

    #[test]
    fn credit_labels_stay_as_lyrics() {
        let track = parse_qrc("[0,1000](0,1000,0)作词：Someone", None).unwrap();
        assert_eq!(track.lines[0].text, "作词：Someone");
        assert_eq!(track.lines[0].role, LineRole::Primary);
    }

    #[test]
    fn long_colon_prefixes_stay_as_lyrics() {
        let track = parse_qrc(
            "[0,1000](0,1000,0)A very long singer name here：hello",
            None,
        )
        .unwrap();
        assert_eq!(track.lines[0].text, "A very long singer name here：hello");
    }
}
