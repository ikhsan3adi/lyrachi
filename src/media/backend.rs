use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::media::composite::CompositeBackend;
#[cfg(target_os = "macos")]
use crate::media::macos::MacosBackend;
#[cfg(target_os = "macos")]
use crate::media::mediaremote::MediaRemoteBackend;
use crate::media::mpd::MpdBackend;
#[cfg(target_os = "windows")]
use crate::media::smtc::SmtcBackend;
use crate::media::webnowplaying::WebNowPlayingBackend;
use crate::media::{BackendKind, MediaConfig, MediaError, MediaEvent, MediaSnapshot};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait MediaBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn probe<'a>(
        &'a self,
        player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>>;
    fn start_monitor<'a>(
        &'a self,
        player_filter: Option<String>,
        tx: mpsc::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>>;
}

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
#[derive(Debug, Default, Clone)]
pub struct MprisBackend;

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
impl MediaBackend for MprisBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Mpris
    }

    fn probe<'a>(
        &'a self,
        player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        Box::pin(crate::media::mpris::probe::probe(player_filter))
    }

    fn start_monitor<'a>(
        &'a self,
        player_filter: Option<String>,
        tx: mpsc::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        Box::pin(async move {
            let connection = zbus::Connection::session()
                .await
                .map_err(|error| MediaError::SessionUnavailable(error.to_string()))?;
            Ok(tokio::spawn(async move {
                if let Err(error) =
                    crate::media::mpris::arbiter::run(player_filter, connection, tx, cancel).await
                {
                    tracing::warn!(%error, "media monitor stopped");
                }
            }))
        })
    }
}

#[derive(Debug, Default, Clone)]
pub struct UnsupportedBackend;

impl MediaBackend for UnsupportedBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Unsupported
    }

    fn probe<'a>(
        &'a self,
        _player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported) })
    }

    fn start_monitor<'a>(
        &'a self,
        _player_filter: Option<String>,
        _tx: mpsc::Sender<MediaEvent>,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported) })
    }
}

pub fn default_backend(media: &MediaConfig) -> Arc<dyn MediaBackend> {
    let mut backends: Vec<Arc<dyn MediaBackend>> = Vec::new();
    let selected = |kind: BackendKind| media.backends.is_empty() || media.backends.contains(&kind);

    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    if selected(BackendKind::Mpris) {
        backends.push(Arc::new(MprisBackend));
    }

    #[cfg(target_os = "macos")]
    if selected(BackendKind::MediaRemote) {
        backends.push(Arc::new(MediaRemoteBackend));
    }

    #[cfg(target_os = "macos")]
    if selected(BackendKind::AppleScript) {
        backends.push(Arc::new(MacosBackend));
    }

    #[cfg(target_os = "windows")]
    if selected(BackendKind::Smtc) {
        backends.push(Arc::new(SmtcBackend));
    }

    if selected(BackendKind::Mpd) && media.mpd.enabled {
        backends.push(Arc::new(MpdBackend::new(media.mpd.clone())));
    }
    if selected(BackendKind::WebNowPlaying) && media.webnowplaying.enabled {
        backends.push(Arc::new(WebNowPlayingBackend::new(
            media.webnowplaying.clone(),
        )));
    }
    if backends.is_empty() {
        backends.push(Arc::new(UnsupportedBackend));
    }

    Arc::new(CompositeBackend::new(backends))
}

pub async fn start_monitor(
    media: &MediaConfig,
    player_filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>, MediaError> {
    default_backend(media)
        .start_monitor(player_filter, tx, cancel)
        .await
}

pub async fn probe(
    media: &MediaConfig,
    player_filter: Option<&str>,
) -> Result<MediaSnapshot, MediaError> {
    default_backend(media).probe(player_filter).await
}
