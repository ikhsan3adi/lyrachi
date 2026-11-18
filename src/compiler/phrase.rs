use unicode_segmentation::UnicodeSegmentation;

use crate::core::model::WordToken;
use crate::core::text::{cannot_end_line, cannot_start_line, is_cjk};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseTimingSource {
    SourceWords,
    SyntheticWords,
    PhraseInterpolation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhraseChunk {
    pub byte_range: (usize, usize),
    pub start_ms: u64,
    pub end_ms: u64,
}

impl PhraseChunk {
    pub fn slice<'a>(&self, line_text: &'a str) -> &'a str {
        &line_text[self.byte_range.0..self.byte_range.1]
    }

    pub fn trimmed_slice<'a>(&self, line_text: &'a str) -> (&'a str, (usize, usize)) {
        let raw = self.slice(line_text);
        let leading = raw.len() - raw.trim_start().len();
        let trimmed = raw.trim();
        let start = self.byte_range.0 + leading;
        let end = start + trimmed.len();
        (trimmed, (start, end))
    }
}

pub fn compile_line_phrases(
    line_text: &str,
    start_ms: u64,
    end_ms: u64,
    words: &[WordToken],
    max_chars: u16,
) -> Vec<PhraseChunk> {
    if line_text.is_empty() {
        return Vec::new();
    }

    let end_ms = end_ms.max(start_ms);
    let ranges = split_phrase_ranges(line_text, max_chars);
    if ranges.is_empty() {
        return Vec::new();
    }

    if words.is_empty() {
        assign_interpolated_timings(line_text, &ranges, start_ms, end_ms)
    } else {
        assign_word_backed_timings(line_text, &ranges, words, start_ms, end_ms)
    }
}

fn split_phrase_ranges(line_text: &str, max_chars: u16) -> Vec<(usize, usize)> {
    let graphemes: Vec<(usize, &str)> = line_text.grapheme_indices(true).collect();
    if graphemes.is_empty() {
        return Vec::new();
    }

    let max_chars = usize::from(max_chars.max(1));
    if graphemes.len() <= max_chars {
        return vec![(0, line_text.len())];
    }

    let mut ranges = Vec::new();
    let mut start_g = 0usize;

    while start_g < graphemes.len() {
        let chunk_start_byte = graphemes[start_g].0;
        let remaining = graphemes.len() - start_g;
        if remaining <= max_chars {
            ranges.push((chunk_start_byte, line_text.len()));
            break;
        }

        let limit_g = (start_g + max_chars).min(graphemes.len());
        let split_g = choose_split_grapheme(&graphemes, start_g, limit_g, max_chars);
        let chunk_end_byte = if split_g >= graphemes.len() {
            line_text.len()
        } else {
            graphemes[split_g].0
        };
        ranges.push((chunk_start_byte, chunk_end_byte));
        start_g = split_g;
    }

    ranges
}

fn choose_split_grapheme(
    graphemes: &[(usize, &str)],
    start_g: usize,
    limit_g: usize,
    max_chars: usize,
) -> usize {
    let mut best_punct = None;
    let mut best_space = None;
    let mut best_cjk = None;
    let min_punct_g = start_g + (max_chars / 3).max(1);

    for g_idx in (start_g + 1)..=limit_g {
        let prev = graphemes[g_idx - 1].1;
        let next = graphemes.get(g_idx).map(|(_, g)| *g);

        if is_phrase_pause_punct(prev) && !next.is_some_and(is_closing_or_pause_punct) {
            let split = advance_past_whitespace(graphemes, g_idx);
            if g_idx >= min_punct_g || best_punct.is_none() {
                best_punct = Some(split);
            }
        }

        if !is_whitespace_grapheme(prev) && next.is_some_and(is_whitespace_grapheme) {
            let split = advance_past_whitespace(graphemes, g_idx);
            best_space = Some(split);
        }

        if let Some(next_g) = next
            && (is_cjk_grapheme(prev) || is_cjk_grapheme(next_g))
            && !prev.chars().all(cannot_end_line)
            && !next_g.chars().any(cannot_start_line)
        {
            best_cjk = Some(g_idx);
        }
    }

    if let Some(split) = best_punct {
        return split;
    }
    if let Some(split) = best_space {
        return split;
    }
    if let Some(split) = best_cjk {
        return split;
    }

    advance_past_whitespace(graphemes, limit_g)
}

fn advance_past_whitespace(graphemes: &[(usize, &str)], mut idx: usize) -> usize {
    while idx < graphemes.len() && is_whitespace_grapheme(graphemes[idx].1) {
        idx += 1;
    }
    idx
}

fn is_whitespace_grapheme(grapheme: &str) -> bool {
    grapheme.chars().all(char::is_whitespace)
}

fn is_phrase_pause_punct(grapheme: &str) -> bool {
    matches!(
        grapheme,
        "," | ";" | ":" | "." | "!" | "?" | "…" | "、" | "，" | "；" | "：" | "。" | "！" | "？"
    )
}

fn is_closing_or_pause_punct(grapheme: &str) -> bool {
    is_phrase_pause_punct(grapheme)
        || matches!(
            grapheme,
            ")" | "]"
                | "}"
                | ">"
                | "\""
                | "'"
                | "”"
                | "’"
                | "）"
                | "］"
                | "｝"
                | "＞"
                | "」"
                | "』"
                | "】"
                | "〉"
                | "》"
                | "〕"
                | "〗"
                | "〙"
                | "〛"
        )
}

fn is_cjk_grapheme(grapheme: &str) -> bool {
    grapheme.chars().any(is_cjk)
}

fn assign_word_backed_timings(
    line_text: &str,
    ranges: &[(usize, usize)],
    words: &[WordToken],
    start_ms: u64,
    end_ms: u64,
) -> Vec<PhraseChunk> {
    let mut chunks = Vec::with_capacity(ranges.len());
    let fallback = assign_interpolated_timings(line_text, ranges, start_ms, end_ms);
    for (idx, &(c_start, c_end)) in ranges.iter().enumerate() {
        let chunk_start = if idx == 0 {
            start_ms
        } else {
            phrase_boundary_ms(
                line_text,
                c_start,
                words,
                fallback[idx].start_ms,
                start_ms,
                end_ms,
            )
        };
        let chunk_end = if idx + 1 == ranges.len() {
            end_ms
        } else {
            phrase_boundary_ms(
                line_text,
                c_end,
                words,
                fallback[idx].end_ms,
                start_ms,
                end_ms,
            )
        };
        chunks.push(PhraseChunk {
            byte_range: (c_start, c_end),
            start_ms: chunk_start,
            end_ms: chunk_end.max(chunk_start),
        });
    }
    let mut previous_end = start_ms;
    for chunk in &mut chunks {
        chunk.start_ms = chunk.start_ms.max(previous_end);
        chunk.end_ms = chunk.end_ms.max(chunk.start_ms);
        previous_end = chunk.end_ms;
    }
    if let Some(last) = chunks.last_mut() {
        last.end_ms = end_ms.max(last.start_ms);
    }
    chunks
}

fn phrase_boundary_ms(
    line_text: &str,
    byte_pos: usize,
    words: &[WordToken],
    fallback_ms: u64,
    start_ms: u64,
    end_ms: u64,
) -> u64 {
    if let Some(word) = words
        .iter()
        .find(|word| word.byte_range.0 < byte_pos && byte_pos < word.byte_range.1)
    {
        return word_time_at(line_text, word, byte_pos, start_ms, end_ms);
    }
    words
        .iter()
        .find(|word| word.byte_range.0 >= byte_pos)
        .map(|word| word.start_ms.clamp(start_ms, end_ms))
        .unwrap_or(fallback_ms)
}

fn word_time_at(
    line_text: &str,
    word: &WordToken,
    byte_pos: usize,
    start_ms: u64,
    end_ms: u64,
) -> u64 {
    let word_start = word.start_ms.clamp(start_ms, end_ms);
    let word_end = word.end_ms.clamp(word_start, end_ms);
    let Some(text) = line_text.get(word.byte_range.0..word.byte_range.1) else {
        return word_start;
    };
    let offset = byte_pos.saturating_sub(word.byte_range.0).min(text.len());
    let Some(prefix) = text.get(..offset) else {
        return word_start;
    };
    let total = text.graphemes(true).count().max(1) as u64;
    let before = prefix.graphemes(true).count() as u64;
    let elapsed = word_end.saturating_sub(word_start).saturating_mul(before) / total;
    word_start.saturating_add(elapsed).min(end_ms)
}

fn assign_interpolated_timings(
    line_text: &str,
    ranges: &[(usize, usize)],
    start_ms: u64,
    end_ms: u64,
) -> Vec<PhraseChunk> {
    let weights: Vec<f64> = ranges
        .iter()
        .map(|&(start, end)| {
            let trimmed = line_text[start..end].trim();
            trimmed.graphemes(true).count().max(1) as f64
        })
        .collect();
    let total_weight: f64 = weights.iter().sum();
    let total_ms = end_ms.saturating_sub(start_ms) as f64;

    let mut chunks = Vec::with_capacity(ranges.len());
    let mut cum_weight = 0.0f64;
    let mut prev_end = start_ms;

    for (idx, (&(c_start, c_end), weight)) in ranges.iter().zip(weights).enumerate() {
        let chunk_start = if idx == 0 { start_ms } else { prev_end };
        cum_weight += weight;
        let chunk_end = if idx + 1 == ranges.len() || total_weight <= 0.0 {
            end_ms
        } else {
            let fraction = (cum_weight / total_weight).clamp(0.0, 1.0);
            start_ms
                .saturating_add((fraction * total_ms).round() as u64)
                .clamp(chunk_start, end_ms)
        };
        chunks.push(PhraseChunk {
            byte_range: (c_start, c_end),
            start_ms: chunk_start,
            end_ms: chunk_end,
        });
        prev_end = chunk_end;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::text::{display_width, estimate_word_timings};

    fn assert_chunk_invariants(
        line_text: &str,
        start_ms: u64,
        end_ms: u64,
        chunks: &[PhraseChunk],
    ) {
        if line_text.is_empty() {
            assert!(chunks.is_empty());
            return;
        }
        assert!(!chunks.is_empty());
        assert_eq!(chunks.first().unwrap().byte_range.0, 0);
        assert_eq!(chunks.last().unwrap().byte_range.1, line_text.len());
        assert_eq!(chunks.first().unwrap().start_ms, start_ms);
        assert_eq!(chunks.last().unwrap().end_ms, end_ms);

        for (idx, chunk) in chunks.iter().enumerate() {
            let (bs, be) = chunk.byte_range;
            assert!(bs < be);
            assert!(line_text.is_char_boundary(bs));
            assert!(line_text.is_char_boundary(be));
            assert!(chunk.start_ms <= chunk.end_ms);
            if idx > 0 {
                let prev = &chunks[idx - 1];
                assert_eq!(prev.byte_range.1, bs, "byte range gap at chunk {idx}");
                assert_eq!(prev.end_ms, chunk.start_ms, "timing gap at chunk {idx}");
            }
        }

        let reconstructed: String = chunks.iter().map(|c| c.slice(line_text)).collect();
        assert_eq!(reconstructed, line_text);
    }

    #[test]
    fn phrase_chunks_cover_full_text_without_gaps_or_loss() {
        let text = "Hello world, how are you feeling tonight?";
        let chunks = compile_line_phrases(text, 1_000, 9_000, &[], 18);
        assert_chunk_invariants(text, 1_000, 9_000, &chunks);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].slice(text).trim(), "Hello world,");

        let cjk = "君がいるだけで、毎日がもっと輝くよ";
        let cjk_chunks = compile_line_phrases(cjk, 2_000, 8_000, &[], 8);
        assert_chunk_invariants(cjk, 2_000, 8_000, &cjk_chunks);
        assert!(cjk_chunks.len() >= 2);

        assert!(compile_line_phrases("", 0, 1_000, &[], 18).is_empty());
    }

    #[test]
    fn phrase_timing_follows_precedence_source_over_synthetic_over_interpolation() {
        let text = "aaaaaaaaaa b";
        // Under interpolation, the long first chunk gets most of the 10_000 ms duration.
        let interpolated = compile_line_phrases(text, 0, 10_000, &[], 10);
        assert_chunk_invariants(text, 0, 10_000, &interpolated);
        assert_eq!(interpolated.len(), 2);
        assert!(interpolated[0].end_ms > 7_000);

        // Synthetic word timing overrides raw phrase interpolation.
        let synthetic_words = estimate_word_timings(text, 0, 10_000);
        let synthetic = compile_line_phrases(text, 0, 10_000, &synthetic_words, 10);
        assert_chunk_invariants(text, 0, 10_000, &synthetic);
        assert_eq!(synthetic[0].end_ms, synthetic_words[1].start_ms);

        // Source word/syllable timing takes top precedence over everything else.
        let source_words = vec![
            WordToken {
                byte_range: (0, 10),
                start_ms: 0,
                end_ms: 1_500,
                display_width: display_width("aaaaaaaaaa"),
            },
            WordToken {
                byte_range: (11, 12),
                start_ms: 2_000,
                end_ms: 10_000,
                display_width: display_width("b"),
            },
        ];
        let from_source = compile_line_phrases(text, 0, 10_000, &source_words, 10);
        assert_chunk_invariants(text, 0, 10_000, &from_source);
        assert_eq!(from_source[0].start_ms, 0);
        assert_eq!(from_source[0].end_ms, 2_000);
        assert_eq!(from_source[1].start_ms, 2_000);
        assert_eq!(from_source[1].end_ms, 10_000);
    }

    #[test]
    fn handles_overlong_single_word_without_splitting_graphemes() {
        let text = "supercalifragilisticexpialidocious";
        let chunks = compile_line_phrases(text, 500, 3_500, &[], 12);
        assert_chunk_invariants(text, 500, 3_500, &chunks);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn source_word_spanning_chunks_still_yields_visible_durations() {
        let text = "supercalifragilisticexpialidocious";
        let words = vec![WordToken {
            byte_range: (0, text.len()),
            start_ms: 500,
            end_ms: 3_500,
            display_width: display_width(text),
        }];
        let chunks = compile_line_phrases(text, 500, 3_500, &words, 12);
        assert_chunk_invariants(text, 500, 3_500, &chunks);
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks.iter().all(|chunk| chunk.end_ms > chunk.start_ms),
            "{chunks:?}"
        );
    }

    #[test]
    fn whitespace_only_line_keeps_one_full_range_chunk() {
        let chunks = compile_line_phrases("   ", 100, 900, &[], 2);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].byte_range, (0, 3));
        assert_eq!((chunks[0].start_ms, chunks[0].end_ms), (100, 900));
    }
}
