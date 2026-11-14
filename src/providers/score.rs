use super::TrackQuery;
use super::lrclib::LrclibRecord;
use super::sanitize::normalize_for_match;

const MIN_CONFIDENCE: f32 = 0.65;
const TIE_WINDOW: f32 = 0.05;

pub fn score_candidate(query: &TrackQuery, candidate: &LrclibRecord) -> f32 {
    let title_similarity = similarity(
        &query.canonical.norm_title,
        &normalize_for_match(candidate.track_name.as_deref().unwrap_or_default()),
    );
    let artist_similarity = if query.canonical.norm_artist.is_empty() {
        0.8
    } else {
        similarity(
            &query.canonical.norm_artist,
            &normalize_for_match(candidate.artist_name.as_deref().unwrap_or_default()),
        )
    };
    let duration_score = duration_score(query.duration_sec(), candidate.duration);

    (title_similarity * 0.45 + artist_similarity * 0.35 + duration_score * 0.20).clamp(0.0, 1.0)
}

pub fn duration_score(query_sec: Option<f64>, candidate_sec: Option<f64>) -> f32 {
    let Some(query_sec) = query_sec else {
        return 0.75;
    };
    let Some(candidate_sec) = candidate_sec.filter(|duration| *duration > 0.0) else {
        return 0.75;
    };

    let difference = (query_sec - candidate_sec).abs();

    if difference <= 2.0 {
        1.0
    } else if difference >= 20.0 {
        0.0
    } else {
        (1.0 - ((difference - 2.0) / 18.0)) as f32
    }
}

pub fn rank_candidates<'a>(
    query: &TrackQuery,
    candidates: &'a [LrclibRecord],
) -> Vec<(&'a LrclibRecord, f32)> {
    let mut scored = candidates
        .iter()
        .map(|candidate| (candidate, score_candidate(query, candidate)))
        .filter(|(_, score)| *score >= MIN_CONFIDENCE)
        .collect::<Vec<_>>();

    let Some(max_score) = scored.iter().map(|(_, score)| *score).reduce(f32::max) else {
        return Vec::new();
    };

    let (mut top, mut rest): (Vec<_>, Vec<_>) = scored
        .drain(..)
        .partition(|(_, score)| max_score - *score < TIE_WINDOW);

    top.sort_by(|(left, left_score), (right, right_score)| {
        quality_rank(left)
            .cmp(&quality_rank(right))
            .then_with(|| right_score.total_cmp(left_score))
            .then_with(|| left.id.cmp(&right.id))
    });
    rest.sort_by(|(left, left_score), (right, right_score)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| quality_rank(left).cmp(&quality_rank(right)))
            .then_with(|| left.id.cmp(&right.id))
    });

    top.into_iter().chain(rest).collect()
}

pub fn select_best<'a>(
    query: &TrackQuery,
    candidates: &'a [LrclibRecord],
) -> Option<(&'a LrclibRecord, f32)> {
    rank_candidates(query, candidates).into_iter().next()
}

fn similarity(left: &str, right: &str) -> f32 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }

    strsim::normalized_damerau_levenshtein(left, right) as f32
}

fn quality_rank(candidate: &LrclibRecord) -> u8 {
    if candidate.has_word_sync {
        0
    } else if candidate
        .synced_lyrics
        .as_deref()
        .is_some_and(|lyrics| !lyrics.trim().is_empty())
    {
        1
    } else if candidate.instrumental {
        2
    } else if candidate
        .plain_lyrics
        .as_deref()
        .is_some_and(|lyrics| !lyrics.trim().is_empty())
    {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::TrackMeta;

    fn query() -> TrackQuery {
        TrackQuery::from_meta(
            &TrackMeta {
                title: "Yellow".to_string(),
                artist: "Coldplay".to_string(),
                album: None,
                duration_ms: Some(267_000),
                track_id: None,
            },
            0,
        )
    }

    fn record(id: u64, title: &str, artist: &str, duration: f64) -> LrclibRecord {
        LrclibRecord {
            id,
            track_name: Some(title.to_string()),
            artist_name: Some(artist.to_string()),
            duration: Some(duration),
            ..LrclibRecord::default()
        }
    }

    #[test]
    fn exact_match_scores_one() {
        let candidate = record(1, "Yellow", "Coldplay", 267.0);

        assert!((score_candidate(&query(), &candidate) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn duration_score_uses_expected_boundaries() {
        assert_eq!(duration_score(Some(200.0), Some(202.0)), 1.0);
        assert_eq!(duration_score(Some(200.0), Some(220.0)), 0.0);
        assert_eq!(duration_score(Some(200.0), None), 0.75);
        assert_eq!(duration_score(None, Some(200.0)), 0.75);

        let mid = duration_score(Some(200.0), Some(211.0));
        assert!((mid - 0.5).abs() < 0.001);
    }

    #[test]
    fn drops_low_confidence_candidates() {
        let candidates = [record(1, "Completely Different", "Someone Else", 100.0)];

        assert!(select_best(&query(), &candidates).is_none());
    }

    #[test]
    fn prefers_word_sync_within_tie_window() {
        let mut word_sync = record(2, "Yellow", "Coldplay", 267.0);
        word_sync.has_word_sync = true;
        word_sync.synced_lyrics = Some("[00:01.00]Yellow".to_string());

        let mut instrumental = record(1, "Yellow", "Coldplay", 267.0);
        instrumental.instrumental = true;

        let candidates = [instrumental, word_sync];
        let (selected, _) = select_best(&query(), &candidates).unwrap();

        assert_eq!(selected.id, 2);
    }
}
