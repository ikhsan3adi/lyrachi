use std::sync::LazyLock;

use regex::Regex;

use crate::core::model::CanonicalTrackKey;

static NOISE_BRACKETS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s*[\(\[]\s*(?:official\s*(?:music\s*)?video|official\s*audio|official\s*lyric\s*video|lyric\s*video|lyrics?|visuali[sz]er|audio|video|mv|m/v|hd|hq|4k|8k|remaster(?:ed)?(?:\s*\d{4})?|live(?:\s*at\s+[^)\]]+)?|performance|acoustic)\s*[\)\]]",
    )
    .expect("valid noise regex")
});
static UNICODE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*【[^】]*】").expect("valid unicode bracket regex"));
static FEATURED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*(?:[\(\[]\s*)?(?:feat\.?|ft\.?|featuring)\s+[^\)\]]+(?:[\)\]]|$)")
        .expect("valid featured regex")
});
static DASH_QUALIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s+-\s+(?:official\s*(?:music\s*)?video|official\s*audio|lyric\s*video|lyrics?|visuali[sz]er|audio|video|mv|hd|4k).*$",
    )
    .expect("valid dash qualifier regex")
});
static VEVO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*-?\s*vevo\s*$").expect("valid vevo regex"));
static TOPIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*-\s*topic\s*$").expect("valid topic regex"));

pub fn clean_text(input: &str) -> String {
    let input = input.replace(['\u{2014}', '\u{2013}'], "-");
    let mut text = NOISE_BRACKETS.replace_all(&input, " ").into_owned();
    text = UNICODE_BRACKETS.replace_all(&text, " ").into_owned();
    text = FEATURED.replace_all(&text, " ").into_owned();
    text = DASH_QUALIFIER.replace_all(&text, " ").into_owned();
    text = VEVO.replace_all(&text, " ").into_owned();
    collapse_text(&text)
}

pub fn clean_artist(input: &str) -> String {
    let text = clean_text(input);
    collapse_text(&TOPIC.replace_all(&text, " "))
}

pub fn clean_title(input: &str) -> String {
    clean_text(input)
}

pub fn split_artist_title(title: &str) -> Option<(String, String)> {
    let (artist, title) = title.split_once(" - ")?;
    let artist = artist.trim();
    let title = title.trim();

    if artist.is_empty() || title.is_empty() {
        return None;
    }

    Some((artist.to_string(), title.to_string()))
}

pub fn normalize_for_match(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn canonical_key(
    artist: &str,
    title: &str,
    album: Option<&str>,
    duration_ms: Option<u64>,
) -> CanonicalTrackKey {
    CanonicalTrackKey {
        norm_artist: normalize_for_match(artist),
        norm_title: normalize_for_match(title),
        norm_album: album
            .map(normalize_for_match)
            .filter(|album| !album.is_empty()),
        duration_bucket_sec: duration_ms.map(duration_bucket),
    }
}

fn collapse_text(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|character: char| {
            character.is_whitespace()
                || matches!(character, '-' | '|' | ':' | '\u{2014}' | '\u{2013}')
        })
        .to_string()
}

fn duration_bucket(duration_ms: u64) -> u32 {
    let bucket = ((duration_ms.saturating_add(1_000)) / 2_000).saturating_mul(2);
    u32::try_from(bucket).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_release_noise() {
        assert_eq!(clean_title("Yellow (Official Video) [4K]"), "Yellow");
        assert_eq!(clean_title("Song - Official Audio"), "Song");
        assert_eq!(clean_title("Song (Live at Wembley)"), "Song");
        assert_eq!(clean_title("Song 【MV】"), "Song");
    }

    #[test]
    fn strips_featured_artists() {
        assert_eq!(clean_title("Song feat. Guest"), "Song");
        assert_eq!(clean_title("Song (ft. Guest)"), "Song");
    }

    #[test]
    fn cleans_channel_style_artists() {
        assert_eq!(clean_artist("Coldplay - Topic"), "Coldplay");
        assert_eq!(clean_artist("ColdplayVEVO"), "Coldplay");
    }

    #[test]
    fn splits_artist_and_title() {
        assert_eq!(
            split_artist_title("Coldplay - Yellow"),
            Some(("Coldplay".to_string(), "Yellow".to_string()))
        );
        assert_eq!(split_artist_title("Yellow"), None);
    }

    #[test]
    fn normalizes_unicode_punctuation_for_matching() {
        assert_eq!(normalize_for_match("  Beyoncé — Halo!! "), "beyoncé halo");
        assert_eq!(normalize_for_match("夜空 / Hoshi"), "夜空 hoshi");
    }

    #[test]
    fn canonical_key_buckets_duration_and_normalizes_fields() {
        let key = canonical_key("Coldplay", "Yellow", Some("Parachutes"), Some(267_000));

        assert_eq!(key.norm_artist, "coldplay");
        assert_eq!(key.norm_title, "yellow");
        assert_eq!(key.norm_album.as_deref(), Some("parachutes"));
        assert_eq!(key.duration_bucket_sec, Some(268));
    }
}
