use crate::core::model::SyncedTrack;
use crate::providers::ProviderError;

#[derive(Debug)]
pub enum AppEvent {
    Resolved {
        epoch: u64,
        result: Result<Option<SyncedTrack>, ProviderError>,
    },
}
