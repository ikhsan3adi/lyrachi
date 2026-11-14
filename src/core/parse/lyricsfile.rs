use serde::Deserialize;
use tracing::warn;

use super::{
    ParseError, ParsedLine, ParsedMetadata, ParsedTrack, ParsedWord, trim_line_with_words,
};

#[derive(Debug, Deserialize)]
struct LyricsFile {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    metadata: LyricsMetadata,
    #[serde(default)]
    lines: Vec<LyricsFileLine>,
}

#[derive(Debug, Default, Deserialize)]
struct LyricsMetadata {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    instrumental: bool,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LyricsFileLine {
    #[serde(default)]
    text: Option<String>,
    start_ms: u64,
    #[serde(default)]
    end_ms: Option<u64>,
    #[serde(default)]
    words: Vec<LyricsFileWord>,
}

#[derive(Debug, Deserialize)]
struct LyricsFileWord {
    text: String,
    start_ms: u64,
    #[serde(default)]
    end_ms: Option<u64>,
}

pub fn parse_lyricsfile(input: &str) -> Result<ParsedTrack, ParseError> {
    let file = serde_saphyr::from_str::<LyricsFile>(input)
        .map_err(|error| ParseError::InvalidLyricsFile(error.to_string()))?;

    if file
        .version
        .as_deref()
        .is_some_and(|version| version != "1.0")
    {
        warn!(version = ?file.version, "unknown lyricsfile version");
    }

    let mut track = ParsedTrack {
        track_duration_ms: file.metadata.duration_ms,
        is_instrumental: file.metadata.instrumental,
        metadata: ParsedMetadata {
            title: file.metadata.title.and_then(non_empty),
            artist: file.metadata.artist.and_then(non_empty),
            album: file.metadata.album.and_then(non_empty),
            language: file.metadata.language.and_then(non_empty),
        },
        ..ParsedTrack::default()
    };

    for line in file.lines {
        let (text, words) = build_line(line.text.as_deref(), &line.words);
        track.lines.push(ParsedLine {
            start_ms: line.start_ms,
            end_ms: line.end_ms,
            text,
            words,
            explicit_last_word_end_ms: None,
        });
    }

    track.lines.sort_by_key(|line| line.start_ms);

    if track.lines.is_empty() && !track.is_instrumental {
        return Err(ParseError::Empty);
    }

    Ok(track)
}

fn build_line(text: Option<&str>, words: &[LyricsFileWord]) -> (String, Vec<ParsedWord>) {
    if words.is_empty() {
        return (text.unwrap_or("").trim().to_string(), Vec::new());
    }

    if let Some(text) = text
        && let Some(mapped) = map_words(text, words)
    {
        return trim_line_with_words(text.to_string(), mapped);
    }

    let joined = words
        .iter()
        .map(|word| word.text.as_str())
        .collect::<String>();
    let mapped = sequential_words(&joined, words);
    if text.is_some() {
        warn!("lyricsfile word text did not match line text; using joined words");
    }
    trim_line_with_words(joined, mapped)
}

fn map_words(text: &str, words: &[LyricsFileWord]) -> Option<Vec<ParsedWord>> {
    let mut cursor = 0usize;
    let mut mapped = Vec::with_capacity(words.len());

    for word in words {
        if word.text.is_empty() {
            continue;
        }
        let relative = text[cursor..].find(&word.text)?;
        let start = cursor + relative;
        let end = start + word.text.len();
        mapped.push(ParsedWord {
            byte_range: (start, end),
            start_ms: word.start_ms,
            end_ms: word.end_ms,
        });
        cursor = end;
    }

    Some(mapped)
}

fn sequential_words(text: &str, words: &[LyricsFileWord]) -> Vec<ParsedWord> {
    let mut cursor = 0usize;
    let mut mapped = Vec::with_capacity(words.len());

    for word in words {
        let start = cursor;
        let end = start + word.text.len();
        if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            break;
        }
        mapped.push(ParsedWord {
            byte_range: (start, end),
            start_ms: word.start_ms,
            end_ms: word.end_ms,
        });
        cursor = end;
    }

    mapped
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_words_and_provided_end() {
        let input = r#"
version: '1.0'
metadata:
  title: Test
  artist: Artist
  duration_ms: 120000
lines:
- text: Look at stars
  start_ms: 1000
  end_ms: 2500
  words:
  - text: 'Look '
    start_ms: 1000
  - text: 'at '
    start_ms: 1200
  - text: stars
    start_ms: 1400
"#;

        let track = parse_lyricsfile(input).unwrap();
        assert_eq!(track.metadata.title.as_deref(), Some("Test"));
        assert_eq!(track.track_duration_ms, Some(120_000));
        assert_eq!(track.lines[0].end_ms, Some(2_500));
        assert_eq!(track.lines[0].words.len(), 3);
        assert_eq!(track.lines[0].words[2].byte_range, (8, 13));
    }

    #[test]
    fn falls_back_to_joined_words() {
        let input = r#"
lines:
- text: mismatched
  start_ms: 0
  words:
  - text: 'Look '
    start_ms: 0
  - text: stars
    start_ms: 100
"#;

        let track = parse_lyricsfile(input).unwrap();
        assert_eq!(track.lines[0].text, "Look stars");
        assert_eq!(track.lines[0].words[1].byte_range, (5, 10));
    }
}
