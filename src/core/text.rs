use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedText {
    pub text: String,
    pub display_width: u16,
    pub source_range: (usize, usize),
}

pub fn display_width(text: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
}

pub fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

pub fn clamp_estimate(graphemes: usize, per_grapheme: u64, min: u64, max: u64) -> u64 {
    let estimate = (graphemes as u64).saturating_mul(per_grapheme);
    estimate.clamp(min, max)
}

pub fn trim_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    let start = start.min(text.len());
    let end = end.min(text.len()).max(start);
    let slice = &text[start..end];
    let trimmed = slice.trim();
    let leading = slice.len() - slice.trim_start().len();
    let trimmed_start = start + leading;
    let trimmed_end = trimmed_start + trimmed.len();
    (trimmed_start, trimmed_end)
}

pub fn truncate_to_width(text: &str, max_width: u16) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut width = 0usize;
    for (index, grapheme) in text.grapheme_indices(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > usize::from(max_width) {
            return &text[..index];
        }
        width += grapheme_width;
    }
    text
}

pub fn wrap_text(text: &str, max_width: u16) -> Vec<WrappedText> {
    let max_width = usize::from(max_width.max(1));
    if text.is_empty() {
        return vec![WrappedText {
            text: String::new(),
            display_width: 0,
            source_range: (0, 0),
        }];
    }

    let mut wrapped = Vec::new();
    let mut start = 0usize;

    while start < text.len() {
        while start < text.len() {
            let grapheme = text[start..].graphemes(true).next().unwrap_or("");
            if grapheme.is_empty() || !grapheme.chars().all(char::is_whitespace) {
                break;
            }
            start += grapheme.len();
        }
        if start >= text.len() {
            break;
        }

        let mut line_end = start;
        let mut width = 0usize;
        let mut last_space_end = None;

        for (offset, grapheme) in text[start..].grapheme_indices(true) {
            let grapheme_start = start + offset;
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if width > 0 && width + grapheme_width > max_width {
                break;
            }
            width += grapheme_width;
            line_end = grapheme_start + grapheme.len();
            if grapheme.chars().all(char::is_whitespace) {
                last_space_end = Some(line_end);
            }
        }

        let next_is_whitespace = text[line_end..]
            .graphemes(true)
            .next()
            .is_some_and(|grapheme| grapheme.chars().all(char::is_whitespace));

        if line_end == start {
            let grapheme = text[start..].graphemes(true).next().unwrap_or("");
            line_end = start + grapheme.len();
        } else if line_end < text.len()
            && !next_is_whitespace
            && let Some(space_end) = last_space_end
            && space_end > start
            && space_end < line_end
        {
            line_end = space_end;
        }

        let (trimmed_start, trimmed_end) = trim_range(text, start, line_end);
        if trimmed_start < trimmed_end {
            wrapped.push(WrappedText {
                text: text[trimmed_start..trimmed_end].to_string(),
                display_width: display_width(&text[trimmed_start..trimmed_end]),
                source_range: (trimmed_start, trimmed_end),
            });
        }

        start = line_end;
    }

    if wrapped.is_empty() {
        wrapped.push(WrappedText {
            text: String::new(),
            display_width: 0,
            source_range: (0, 0),
        });
    }

    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_wide_graphemes() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("世界"), 4);
        assert_eq!(grapheme_count("👩🏽‍💻"), 1);
    }

    #[test]
    fn wraps_without_splitting_graphemes() {
        let wrapped = wrap_text("hello world", 5);
        assert_eq!(
            wrapped
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<Vec<_>>(),
            vec!["hello", "world"]
        );

        let exact_fit = wrap_text("a full world", 6);
        assert_eq!(
            exact_fit
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a full", "world"]
        );
    }

    #[test]
    fn wraps_wide_text_by_width() {
        let wrapped = wrap_text("世界世界", 4);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].text, "世界");
        assert_eq!(wrapped[1].text, "世界");
    }

    #[test]
    fn keeps_source_ranges_valid() {
        let text = "  hello world  ";
        for segment in wrap_text(text, 5) {
            let (start, end) = segment.source_range;
            assert!(text.is_char_boundary(start));
            assert!(text.is_char_boundary(end));
            assert_eq!(&text[start..end], segment.text);
        }
    }
}
