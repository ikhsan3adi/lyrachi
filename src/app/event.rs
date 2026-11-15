use crate::providers::{ProviderAnswer, ProviderError};

#[derive(Debug)]
pub enum AppEvent {
    Lyrics {
        epoch: u64,
        result: Result<Option<ProviderAnswer>, ProviderError>,
        is_final: bool,
    },
}
