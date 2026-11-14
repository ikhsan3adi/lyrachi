use std::collections::HashMap;

use zbus::{OwnedObjectPath, OwnedVariant};

use crate::media::{PlayerKind, TrackMeta};

const MAX_DURATION_MS: u64 = 3 * 60 * 60 * 1_000;

pub fn extract_track_meta(
    metadata: &HashMap<String, OwnedVariant>,
    kind: PlayerKind,
) -> Option<TrackMeta> {
    let mut title = metadata
        .get("xesam:title")
        .and_then(value_string)
        .unwrap_or_default();
    if title.is_empty() {
        return None;
    }

    let raw_track_id = metadata.get("mpris:trackid").and_then(value_object_path);
    if raw_track_id.as_deref().is_some_and(is_ad_track) {
        return None;
    }
    let track_id = raw_track_id.filter(|track_id| !is_constant_track_id(track_id));

    let mut artist = metadata
        .get("xesam:artist")
        .map(value_artists)
        .unwrap_or_default();

    if kind.is_browser() {
        let topic = artist.ends_with(" - Topic");
        if topic {
            artist = artist.trim_end_matches(" - Topic").trim().to_string();
        }
        if let Some((left, right)) = title.split_once(" - ") {
            let left = left.trim();
            let right = right.trim();
            if !left.is_empty()
                && !right.is_empty()
                && (artist.is_empty() || topic || left.eq_ignore_ascii_case(&artist))
            {
                artist = left.to_string();
                title = right.to_string();
            }
        }
    }

    if title.is_empty() {
        return None;
    }

    let album = metadata
        .get("xesam:album")
        .and_then(value_string)
        .filter(|album| !album.is_empty());
    let duration_ms = metadata.get("mpris:length").and_then(value_duration_ms);

    Some(TrackMeta {
        title,
        artist,
        album,
        duration_ms,
        track_id,
    })
}

fn value_string(value: &OwnedVariant) -> Option<String> {
    String::try_from(value.clone())
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn value_artists(value: &OwnedVariant) -> String {
    if let Ok(artists) = Vec::<String>::try_from(value.clone()) {
        return artists
            .into_iter()
            .map(|artist| artist.trim().to_string())
            .filter(|artist| !artist.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
    }

    value_string(value).unwrap_or_default()
}

fn value_object_path(value: &OwnedVariant) -> Option<String> {
    OwnedObjectPath::try_from(value.clone())
        .map(|path| path.to_string())
        .ok()
        .or_else(|| value_string(value))
}

fn value_duration_ms(value: &OwnedVariant) -> Option<u64> {
    let micros = i64::try_from(value)
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .or_else(|| u64::try_from(value).ok())
        .or_else(|| {
            i32::try_from(value)
                .ok()
                .and_then(|value| u64::try_from(value).ok())
        })
        .or_else(|| u32::try_from(value).ok().map(u64::from))?;
    let duration_ms = micros / 1_000;

    (duration_ms > 0 && duration_ms <= MAX_DURATION_MS).then_some(duration_ms)
}

pub fn is_constant_track_id(track_id: &str) -> bool {
    matches!(
        track_id.trim(),
        "" | "/"
            | "/org/mpris/MediaPlayer2"
            | "/org/mpris/MediaPlayer2/TrackList/NoTrack"
            | "/org/mpris/MediaPlayer2/CurrentTrack"
    )
}

fn is_ad_track(track_id: &str) -> bool {
    track_id.starts_with("spotify:ad:") || track_id.starts_with("/com/spotify/ad/")
}

#[cfg(test)]
mod tests {
    use zbus::Variant;

    use super::*;

    fn metadata(entries: Vec<(&str, OwnedVariant)>) -> HashMap<String, OwnedVariant> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect()
    }

    fn text(value: &str) -> OwnedVariant {
        OwnedVariant::try_from(Variant::from(value)).unwrap()
    }

    fn object_path(value: &str) -> OwnedVariant {
        OwnedVariant::from(zbus::ObjectPath::try_from(value).unwrap())
    }

    #[test]
    fn extracts_standard_metadata() {
        let metadata = metadata(vec![
            ("xesam:title", text("Song")),
            (
                "xesam:artist",
                OwnedVariant::try_from(Variant::from(vec!["Artist", "Feature"])).unwrap(),
            ),
            ("xesam:album", text("Album")),
            ("mpris:length", OwnedVariant::from(240_000_000_i64)),
            (
                "mpris:trackid",
                object_path("/org/mpris/MediaPlayer2/Track/1"),
            ),
        ]);

        let track = extract_track_meta(&metadata, PlayerKind::Native).unwrap();

        assert_eq!(track.title, "Song");
        assert_eq!(track.artist, "Artist, Feature");
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.duration_ms, Some(240_000));
        assert_eq!(
            track.track_id.as_deref(),
            Some("/org/mpris/MediaPlayer2/Track/1")
        );
    }

    #[test]
    fn rejects_spotify_ads() {
        let metadata = metadata(vec![
            ("xesam:title", text("Advertisement")),
            ("mpris:trackid", text("spotify:ad:123")),
        ]);

        assert!(extract_track_meta(&metadata, PlayerKind::Native).is_none());
    }

    #[test]
    fn splits_browser_titles_when_artist_is_missing() {
        let metadata = metadata(vec![("xesam:title", text("Artist - Song"))]);

        let track = extract_track_meta(&metadata, PlayerKind::Browser).unwrap();

        assert_eq!(track.artist, "Artist");
        assert_eq!(track.title, "Song");
    }

    #[test]
    fn splits_topic_browser_titles() {
        let metadata = metadata(vec![
            ("xesam:title", text("Artist - Song")),
            (
                "xesam:artist",
                OwnedVariant::try_from(Variant::from(vec!["Artist - Topic"])).unwrap(),
            ),
        ]);

        let track = extract_track_meta(&metadata, PlayerKind::Browser).unwrap();

        assert_eq!(track.artist, "Artist");
        assert_eq!(track.title, "Song");
    }

    #[test]
    fn does_not_split_native_titles() {
        let metadata = metadata(vec![("xesam:title", text("Artist - Song"))]);

        let track = extract_track_meta(&metadata, PlayerKind::Native).unwrap();

        assert_eq!(track.title, "Artist - Song");
        assert_eq!(track.artist, "");
    }

    #[test]
    fn rejects_invalid_durations() {
        for micros in [-1_i64, 0, 10_800_001_000] {
            let metadata = metadata(vec![
                ("xesam:title", text("Song")),
                ("mpris:length", OwnedVariant::from(micros)),
            ]);

            let track = extract_track_meta(&metadata, PlayerKind::Native).unwrap();

            assert_eq!(track.duration_ms, None, "micros={micros}");
        }
    }

    #[test]
    fn rejects_empty_titles() {
        let metadata = metadata(vec![("xesam:title", text("   "))]);

        assert!(extract_track_meta(&metadata, PlayerKind::Native).is_none());
    }

    #[test]
    fn ignores_constant_track_ids() {
        for track_id in [
            "/",
            "/org/mpris/MediaPlayer2",
            "/org/mpris/MediaPlayer2/TrackList/NoTrack",
            "/org/mpris/MediaPlayer2/CurrentTrack",
        ] {
            let metadata = metadata(vec![
                ("xesam:title", text("Song")),
                ("mpris:trackid", object_path(track_id)),
            ]);

            let track = extract_track_meta(&metadata, PlayerKind::Native).unwrap();
            assert_eq!(track.track_id, None, "track_id={track_id}");
        }
    }

    #[test]
    fn splits_browser_title_when_artist_duplicates_prefix() {
        let metadata = metadata(vec![
            ("xesam:title", text("Coldplay - Yellow")),
            (
                "xesam:artist",
                OwnedVariant::try_from(Variant::from(vec!["coldplay"])).unwrap(),
            ),
        ]);

        let track = extract_track_meta(&metadata, PlayerKind::Browser).unwrap();

        assert_eq!(track.artist, "Coldplay");
        assert_eq!(track.title, "Yellow");
    }

    #[test]
    fn extracts_32bit_durations() {
        let metadata_i32 = metadata(vec![
            ("xesam:title", text("Song")),
            ("mpris:length", OwnedVariant::from(180_000_000_i32)),
        ]);
        assert_eq!(
            extract_track_meta(&metadata_i32, PlayerKind::Native)
                .unwrap()
                .duration_ms,
            Some(180_000)
        );

        let metadata_u32 = metadata(vec![
            ("xesam:title", text("Song")),
            ("mpris:length", OwnedVariant::from(180_000_000_u32)),
        ]);
        assert_eq!(
            extract_track_meta(&metadata_u32, PlayerKind::Native)
                .unwrap()
                .duration_ms,
            Some(180_000)
        );
    }
}
