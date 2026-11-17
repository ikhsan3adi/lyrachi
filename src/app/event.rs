use crate::core::model::CanonicalTrackKey;
use crate::providers::{CandidateSummary, ProviderAnswer, ProviderError};

#[derive(Debug)]
pub enum AppEvent {
    Lyrics {
        epoch: u64,
        result: Result<Option<ProviderAnswer>, ProviderError>,
        is_final: bool,
    },
    TrackPreferenceLoaded {
        epoch: u64,
        preference_revision: u64,
        key: CanonicalTrackKey,
        pinned_source_label: Option<String>,
        pinned_style: Option<String>,
    },
    SearchCandidates {
        request_id: u64,
        key: CanonicalTrackKey,
        result: Result<Vec<CandidateSummary>, ProviderError>,
    },
    CandidateHydrated {
        request_id: u64,
        key: CanonicalTrackKey,
        summary: Box<CandidateSummary>,
        result: Result<Option<ProviderAnswer>, ProviderError>,
    },
    StyleReloadRequested,
}
