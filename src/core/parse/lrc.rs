use super::{ParseError, ParsedLine, ParsedTrack, ParsedWord, trim_line_with_words};
use tracing::warn;

pub fn parse_lrc(input: &str, track_duration_ms: Option<u64>) -> Result<ParsedTrack, ParseError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut track = ParsedTrack {
        track_duration_ms,
        ..ParsedTrack::default()
    };

    for raw_line in input.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut rest = line;
        let mut times = Vec::new();

        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else {
                break;
            };
            let tag = &rest[1..end];
            let after = &rest[end + 1..];

            if let Some(time_ms) = parse_time_tag(tag) {
                times.push(time_ms);
                rest = after;
                continue;
            }

            if let Some((key, value)) = tag.split_once(':') {
                apply_metadata(&mut track, key, value);
                rest = after;
                continue;
            }

            rest = after;
        }

        rest = rest.trim_start();

        if times.is_empty() {
            let first_word_ms = first_word_tag_ms(rest);
            let Some(line_start_ms) = first_word_ms else {
                continue;
            };
            let (text, words, explicit_last_word_end_ms) = parse_line_text(rest, line_start_ms);
            if words.is_empty() {
                continue;
            }
            track.lines.push(ParsedLine {
                start_ms: line_start_ms,
                end_ms: None,
                text,
                words,
                explicit_last_word_end_ms,
            });
            continue;
        }

        times.sort_unstable();
        times.dedup();

        let base_start = first_word_tag_ms(rest).map_or(times[0], |word_ms| times[0].min(word_ms));
        let (text, words, explicit_last_word_end_ms) = parse_line_text(rest, base_start);

        for start_ms in times {
            let shift = start_ms.saturating_sub(base_start);
            let shifted_words = words
                .iter()
                .map(|word| ParsedWord {
                    byte_range: word.byte_range,
                    start_ms: word.start_ms.saturating_add(shift),
                    end_ms: word.end_ms.map(|end_ms| end_ms.saturating_add(shift)),
                })
                .collect();
            let shifted_explicit =
                explicit_last_word_end_ms.map(|end_ms| end_ms.saturating_add(shift));

            track.lines.push(ParsedLine {
                start_ms,
                end_ms: None,
                text: text.clone(),
                words: shifted_words,
                explicit_last_word_end_ms: shifted_explicit,
            });
        }
    }

    track.lines.sort_by_key(|line| line.start_ms);

    if track.lines.is_empty() {
        return Err(ParseError::Empty);
    }

    Ok(track)
}

fn apply_metadata(track: &mut ParsedTrack, key: &str, value: &str) {
    let value = value.trim();

    match key.trim().to_ascii_lowercase().as_str() {
        "ti" => track.metadata.title = non_empty(value),
        "ar" => track.metadata.artist = non_empty(value),
        "al" => track.metadata.album = non_empty(value),
        "la" | "language" => track.metadata.language = non_empty(value),
        "offset" => match value.parse::<i64>() {
            Ok(offset_ms) => track.intrinsic_offset_ms = offset_ms,
            Err(_) => warn!(value, "ignoring invalid LRC offset"),
        },
        "length" if track.track_duration_ms.is_none() => {
            track.track_duration_ms = parse_time_tag(value)
                .or_else(|| value.parse::<u64>().ok().map(|seconds| seconds * 1000));
        }
        _ => {}
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn first_word_tag_ms(text: &str) -> Option<u64> {
    let mut cursor = 0usize;
    while let Some(relative_open) = text[cursor..].find('<') {
        let open = cursor + relative_open;
        let relative_close = text[open..].find('>')?;
        let close = open + relative_close;
        if let Some(time_ms) = parse_time_tag(&text[open + 1..close]) {
            return Some(time_ms);
        }
        cursor = open + 1;
    }
    None
}

fn parse_line_text(text: &str, line_start_ms: u64) -> (String, Vec<ParsedWord>, Option<u64>) {
    let tags = word_tags(text);
    if tags.is_empty() {
        return (text.trim().to_string(), Vec::new(), None);
    }

    let mut output = String::new();
    let mut words = Vec::new();
    let mut explicit_last_word_end_ms = None;
    let mut previous: Option<(u64, usize)> = None;

    for (open, after, start_ms) in &tags {
        let segment_start = previous.map(|(_, end)| end).unwrap_or(0);
        let segment = &text[segment_start..*open];
        let range = append_segment(&mut output, segment);

        if let Some((previous_start_ms, _)) = previous {
            if !segment.trim().is_empty() {
                words.push(ParsedWord {
                    byte_range: range,
                    start_ms: previous_start_ms,
                    end_ms: None,
                });
            } else if let Some(last_word) = words.last_mut()
                && last_word.end_ms.is_none()
            {
                last_word.end_ms = Some(previous_start_ms.max(last_word.start_ms));
            }
        } else if !segment.trim().is_empty() {
            words.push(ParsedWord {
                byte_range: range,
                start_ms: line_start_ms,
                end_ms: None,
            });
        }

        previous = Some((*start_ms, *after));
    }

    let (last_start_ms, last_tag_end) = previous.expect("word tags are non-empty");
    let tail = &text[last_tag_end..];
    let tail_range = append_segment(&mut output, tail);

    if tail.trim().is_empty() {
        explicit_last_word_end_ms = Some(last_start_ms);
    } else {
        words.push(ParsedWord {
            byte_range: tail_range,
            start_ms: last_start_ms,
            end_ms: None,
        });
    }

    let (output, words) = trim_line_with_words(output, words);
    (output, words, explicit_last_word_end_ms)
}

fn word_tags(text: &str) -> Vec<(usize, usize, u64)> {
    let mut tags = Vec::new();
    let mut cursor = 0usize;

    while let Some(relative_open) = text[cursor..].find('<') {
        let open = cursor + relative_open;
        let Some(relative_close) = text[open..].find('>') else {
            break;
        };
        let close = open + relative_close;

        if let Some(time_ms) = parse_time_tag(&text[open + 1..close]) {
            tags.push((open, close + 1, time_ms));
            cursor = close + 1;
        } else {
            cursor = open + 1;
        }
    }

    tags
}

fn append_segment(output: &mut String, segment: &str) -> (usize, usize) {
    let start = output.len();
    output.push_str(segment);
    (start, output.len())
}

pub(crate) fn parse_time_tag(tag: &str) -> Option<u64> {
    let parts: Vec<&str> = tag.trim().split(':').collect();

    match parts.as_slice() {
        [minutes, seconds] => {
            let minutes = parse_digits(minutes)?;
            let (seconds, fraction) = parse_seconds(seconds)?;
            minutes
                .checked_mul(60_000)?
                .checked_add(seconds.checked_mul(1_000)?)?
                .checked_add(fraction)
        }
        [hours, minutes, seconds] => {
            let hours = parse_digits(hours)?;
            let minutes = parse_digits(minutes)?;
            let (seconds, fraction) = parse_seconds(seconds)?;
            hours
                .checked_mul(3_600_000)?
                .checked_add(minutes.checked_mul(60_000)?)?
                .checked_add(seconds.checked_mul(1_000)?)?
                .checked_add(fraction)
        }
        _ => None,
    }
}

fn parse_digits(value: &str) -> Option<u64> {
    if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_seconds(value: &str) -> Option<(u64, u64)> {
    let (seconds, fraction) = value.split_once(['.', ',']).unwrap_or((value, ""));
    let seconds = parse_digits(seconds)?;

    if fraction.is_empty() {
        return Some((seconds, 0));
    }
    if !fraction.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }

    let digits = &fraction[..fraction.len().min(3)];
    let value = digits.parse::<u64>().ok()?;
    let milliseconds = match digits.len() {
        1 => value * 100,
        2 => value * 10,
        _ => value,
    };

    Some((seconds, milliseconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_time_tag("00:35.66"), Some(35_660));
        assert_eq!(parse_time_tag("00:35,66"), Some(35_660));
        assert_eq!(parse_time_tag("00:35.6"), Some(35_600));
        assert_eq!(parse_time_tag("01:02:03.45"), Some(3_723_450));
        assert_eq!(parse_time_tag("offset:-300"), None);
    }

    #[test]
    fn parses_mid_line_pause_tags_and_unsorted_multi_timestamps() {
        let input =
            "[00:20.00][00:10.00][00:10.00]<00:10.00>Look <00:10.30> <00:10.60>stars <00:11.00>\n";
        let track = parse_lrc(input, None).unwrap();

        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].start_ms, 10_000);
        assert_eq!(track.lines[0].words.len(), 2);
        assert_eq!(track.lines[0].words[0].start_ms, 10_000);
        assert_eq!(track.lines[0].words[0].end_ms, Some(10_300));
        assert_eq!(track.lines[0].words[1].start_ms, 10_600);
        assert_eq!(track.lines[0].explicit_last_word_end_ms, Some(11_000));

        assert_eq!(track.lines[1].start_ms, 20_000);
        assert_eq!(track.lines[1].words[0].start_ms, 20_000);
        assert_eq!(track.lines[1].words[0].end_ms, Some(20_300));
        assert_eq!(track.lines[1].words[1].start_ms, 20_600);
        assert_eq!(track.lines[1].explicit_last_word_end_ms, Some(21_000));
    }

    #[test]
    fn parses_metadata_multi_timestamps_and_markers() {
        let input = "\u{feff}[ti:Test]\r\n[ar:Artist]\r\n[offset:-300]\r\n[00:01.00]One\r\n[00:02.00]\r\n[00:03.00][00:04.00]Again\r\n";
        let track = parse_lrc(input, None).unwrap();

        assert_eq!(track.metadata.title.as_deref(), Some("Test"));
        assert_eq!(track.metadata.artist.as_deref(), Some("Artist"));
        assert_eq!(track.intrinsic_offset_ms, -300);
        assert_eq!(track.lines.len(), 4);
        assert_eq!(track.lines[0].start_ms, 1_000);
        assert_eq!(track.lines[1].start_ms, 2_000);
        assert_eq!(track.lines[2].start_ms, 3_000);
        assert_eq!(track.lines[3].start_ms, 4_000);
        assert_eq!(track.lines[2].text, "Again");
        assert_eq!(track.lines[3].text, "Again");
    }

    #[test]
    fn parses_word_tags_with_and_without_trailing_end() {
        let input = "[00:10.00]<00:10.00>Look <00:10.40>at <00:10.60>stars <00:12.10>\n[00:13.00]<00:13.00>No <00:13.50>trailing\n";
        let track = parse_lrc(input, None).unwrap();

        assert_eq!(track.lines[0].text, "Look at stars");
        assert_eq!(track.lines[0].words.len(), 3);
        assert_eq!(track.lines[0].words[0].byte_range, (0, 4));
        assert_eq!(track.lines[0].words[1].byte_range, (5, 7));
        assert_eq!(track.lines[0].words[2].byte_range, (8, 13));
        assert_eq!(track.lines[0].explicit_last_word_end_ms, Some(12_100));
        assert_eq!(track.lines[1].text, "No trailing");
        assert_eq!(track.lines[1].explicit_last_word_end_ms, None);
        assert_eq!(track.lines[1].words[1].start_ms, 13_500);
    }

    #[test]
    fn preserves_adjacent_syllable_ranges_without_trailing_spaces() {
        let input = "[00:01.00]<00:01.00>hel<00:01.20>lo <00:01.50>world\n";
        let track = parse_lrc(input, None).unwrap();
        let line = &track.lines[0];

        assert_eq!(line.text, "hello world");
        assert_eq!(line.words.len(), 3);
        assert_eq!(line.words[0].byte_range, (0, 3));
        assert_eq!(line.words[1].byte_range, (3, 5));
        assert_eq!(line.words[2].byte_range, (6, 11));
    }

    #[test]
    fn ignores_malformed_lines_without_panicking() {
        let input = "not a lyric\n[bad:tag]\n[99:99.999]Still parses\n";
        let track = parse_lrc(input, None).unwrap();
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].start_ms, 6_039_999);
    }
}
