use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::backend::{BoxFuture, MediaBackend};
use super::composite::{ExponentialBackoff, jitter_seed, player_matches};
use super::config::is_valid_extension_origin;
use super::{
    BackendKind, MediaError, MediaEvent, MediaSnapshot, PlayerKind, TrackMeta, WebNowPlayingConfig,
};
use crate::core::clock::PlaybackStatus;

const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct WebNowPlayingBackend {
    config: WebNowPlayingConfig,
}

impl WebNowPlayingBackend {
    pub fn new(config: WebNowPlayingConfig) -> Self {
        Self { config }
    }
}

impl MediaBackend for WebNowPlayingBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::WebNowPlaying
    }

    fn probe<'a>(
        &'a self,
        _player_filter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<MediaSnapshot, MediaError>> {
        Box::pin(async {
            Ok(MediaSnapshot {
                backend: BackendKind::WebNowPlaying,
                ..MediaSnapshot::default()
            })
        })
    }

    fn start_monitor<'a>(
        &'a self,
        player_filter: Option<String>,
        tx: mpsc::Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<JoinHandle<()>, MediaError>> {
        let config = self.config.clone();
        Box::pin(async move {
            Ok(tokio::spawn(async move {
                run_server_loop(config, player_filter, tx, cancel).await;
            }))
        })
    }
}

async fn run_server_loop(
    config: WebNowPlayingConfig,
    player_filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) {
    let mut backoff = ExponentialBackoff::default();
    loop {
        if cancel.is_cancelled() {
            return;
        }

        match TcpListener::bind((config.bind_address, config.port)).await {
            Ok(listener) => {
                backoff.reset();
                let result = serve_listener(
                    listener,
                    config.clone(),
                    player_filter.clone(),
                    tx.clone(),
                    cancel.clone(),
                )
                .await;
                if let Err(error) = result {
                    warn!(%error, "webnowplaying listener stopped");
                }
            }
            Err(error) => {
                debug!(%error, "webnowplaying bind failed");
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff.next_delay_with_jitter(jitter_seed())) => {}
        }
    }
}

async fn serve_listener(
    listener: TcpListener,
    config: WebNowPlayingConfig,
    player_filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) -> Result<(), std::io::Error> {
    loop {
        let (stream, _) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = listener.accept() => result?,
        };
        let config = config.clone();
        let player_filter = player_filter.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            handle_connection(stream, config, player_filter, tx, cancel).await;
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    config: WebNowPlayingConfig,
    player_filter: Option<String>,
    tx: mpsc::Sender<MediaEvent>,
    cancel: CancellationToken,
) {
    let auth = HandshakeAuth {
        allowed_origins: config.allowed_origins.clone(),
        token: config.token.clone(),
    };

    let mut socket = match accept_hdr_async(stream, auth).await {
        Ok(socket) => socket,
        Err(error) => {
            debug!(%error, "webnowplaying handshake rejected");
            return;
        }
    };

    let mut previous: Option<WnpState> = None;
    loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => break,
            message = socket.next() => message,
        };
        let Some(message) = message else {
            break;
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                debug!(%error, "webnowplaying socket error");
                break;
            }
        };

        match message {
            Message::Text(text) if text.len() <= MAX_MESSAGE_BYTES => {
                match serde_json::from_str::<Value>(text.as_str()) {
                    Ok(value) => {
                        if let Some(update) = parse_message(&value, previous.as_ref())
                            && player_matches(&update.player_id, player_filter.as_deref())
                        {
                            emit_changes(previous.as_ref(), &update, &tx).await;
                            previous = Some(update);
                        }
                    }
                    Err(error) => debug!(%error, "webnowplaying invalid json"),
                }
            }
            Message::Text(_) => debug!("webnowplaying message exceeded size limit"),
            Message::Close(_) => break,
            _ => {}
        }
    }

    if previous.is_some() {
        let _ = tx.send(MediaEvent::NoPlayer).await;
        let _ = tx.send(MediaEvent::NoTrack).await;
        let _ = tx.send(MediaEvent::Status(PlaybackStatus::Stopped)).await;
    }
}

struct HandshakeAuth {
    allowed_origins: Vec<String>,
    token: Option<String>,
}

impl Callback for HandshakeAuth {
    #[allow(clippy::result_large_err)]
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let origin = request
            .headers()
            .get("origin")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !origin_allowed(origin, &self.allowed_origins) {
            return Err(reject(StatusCode::FORBIDDEN));
        }
        if !token_allowed(request, self.token.as_deref()) {
            return Err(reject(StatusCode::UNAUTHORIZED));
        }
        Ok(response)
    }
}

fn reject(status: StatusCode) -> ErrorResponse {
    Response::builder()
        .status(status)
        .body(Some(status.as_str().to_string()))
        .expect("static websocket error response")
}

fn token_allowed(request: &Request, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return true;
    };
    if request
        .headers()
        .get("x-lyrachi-token")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == token)
    {
        return true;
    }
    request.uri().query().is_some_and(|query| {
        query
            .split('&')
            .any(|pair| pair == format!("token={token}"))
    })
}

pub fn origin_allowed(origin: &str, allowed: &[String]) -> bool {
    is_valid_extension_origin(origin) && allowed.iter().any(|candidate| candidate == origin)
}

#[derive(Debug, Clone, PartialEq)]
struct WnpState {
    player_id: String,
    track: Option<TrackMeta>,
    playback: PlaybackStatus,
    position_ms: u64,
    rate: f64,
}

async fn emit_changes(
    previous: Option<&WnpState>,
    current: &WnpState,
    tx: &mpsc::Sender<MediaEvent>,
) {
    let player_changed = previous.is_none_or(|state| state.player_id != current.player_id);
    if player_changed {
        let _ = tx
            .send(MediaEvent::PlayerChanged {
                id: current.player_id.clone(),
                kind: PlayerKind::Browser,
                backend: BackendKind::WebNowPlaying,
            })
            .await;
    }

    if previous.and_then(|state| state.track.as_ref()) != current.track.as_ref() {
        match &current.track {
            Some(track) => {
                let _ = tx.send(MediaEvent::TrackChanged(track.clone())).await;
            }
            None => {
                let _ = tx.send(MediaEvent::NoTrack).await;
            }
        }
    }

    if previous.is_none_or(|state| state.playback != current.playback) {
        let _ = tx.send(MediaEvent::Status(current.playback)).await;
    }
    if previous.is_none_or(|state| (state.rate - current.rate).abs() > f64::EPSILON) {
        let _ = tx.send(MediaEvent::Rate(current.rate)).await;
    }
    if current.track.is_some() {
        let _ = tx
            .send(MediaEvent::PositionPoll {
                pos_ms: current.position_ms,
                rtt: Duration::ZERO,
                sample_age: None,
            })
            .await;
    }
}

fn parse_message(value: &Value, previous: Option<&WnpState>) -> Option<WnpState> {
    let payload = value
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(value);
    let event = text(value, &["event", "type", "action"]).unwrap_or_default();
    if matches!(
        event.to_ascii_lowercase().as_str(),
        "close" | "closed" | "stop" | "stopped" | "disconnect"
    ) && lookup(value, "title").is_none()
        && lookup(value, "track.title").is_none()
    {
        return Some(WnpState {
            player_id: previous
                .map(|state| state.player_id.clone())
                .unwrap_or_else(|| "webnowplaying".to_string()),
            track: None,
            playback: PlaybackStatus::Stopped,
            position_ms: 0,
            rate: 1.0,
        });
    }

    let player_id = text(payload, &["player.id", "player.name", "player", "playerId"])
        .or_else(|| text(value, &["player.id", "player.name", "player", "playerId"]))
        .or_else(|| previous.map(|state| state.player_id.clone()))
        .unwrap_or_else(|| "webnowplaying".to_string());
    let track_id = text(
        payload,
        &["track_id", "track.id", "id", "songid", "song_id"],
    );
    let title = text(payload, &["title", "track.title", "song", "name"]);
    let artist =
        text(payload, &["artist", "track.artist", "author", "creator"]).unwrap_or_default();
    let album = text(payload, &["album", "track.album"]);
    let duration_ms = number(payload, &["duration_ms", "track.duration_ms", "length_ms"])
        .map(|value| value.round() as u64)
        .or_else(|| number(payload, &["duration", "track.duration", "length"]).map(seconds_to_ms));
    let position_ms = number(
        payload,
        &[
            "position_ms",
            "progress_ms",
            "elapsed_ms",
            "track.position_ms",
        ],
    )
    .map(|value| value.round() as u64)
    .or_else(|| {
        number(
            payload,
            &["position", "progress", "elapsed", "track.position"],
        )
        .map(seconds_to_ms)
    })
    .or_else(|| previous.map(|state| state.position_ms))
    .unwrap_or(0);
    let playback = text(payload, &["state", "playback", "status"])
        .and_then(parse_playback)
        .or_else(|| {
            number(payload, &["playing", "isPlaying"]).map(|playing| {
                if playing > 0.0 {
                    PlaybackStatus::Playing
                } else {
                    PlaybackStatus::Paused
                }
            })
        })
        .or_else(|| previous.map(|state| state.playback))
        .unwrap_or(PlaybackStatus::Stopped);
    let rate = number(payload, &["rate", "playbackRate", "speed"])
        .filter(|value| value.is_finite() && *value > 0.0)
        .or_else(|| previous.map(|state| state.rate))
        .unwrap_or(1.0);

    let track = title
        .filter(|title| !title.trim().is_empty())
        .map(|title| TrackMeta {
            title,
            artist,
            album,
            duration_ms,
            track_id,
            url: None,
        });

    if track.is_none()
        && previous.is_none()
        && !matches!(playback, PlaybackStatus::Playing | PlaybackStatus::Paused)
    {
        return None;
    }

    Some(WnpState {
        player_id,
        track,
        playback,
        position_ms,
        rate,
    })
}

fn parse_playback(value: String) -> Option<PlaybackStatus> {
    match value.trim().to_ascii_lowercase().as_str() {
        "play" | "playing" | "1" => Some(PlaybackStatus::Playing),
        "pause" | "paused" | "2" => Some(PlaybackStatus::Paused),
        "stop" | "stopped" | "none" | "0" => Some(PlaybackStatus::Stopped),
        _ => None,
    }
}

fn seconds_to_ms(value: f64) -> u64 {
    (value * 1_000.0).round().max(0.0) as u64
}

fn lookup<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |current, key| current.get(key))
}

fn text(value: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|path| {
        lookup(value, path).and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
    })
}

fn number(value: &Value, paths: &[&str]) -> Option<f64> {
    paths.iter().find_map(|path| {
        lookup(value, path).and_then(|value| match value {
            Value::Number(value) => value.as_f64(),
            Value::String(value) => value.trim().parse::<f64>().ok(),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use futures_util::SinkExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    use super::*;

    fn config(port: u16) -> WebNowPlayingConfig {
        WebNowPlayingConfig {
            enabled: true,
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            allowed_origins: vec!["chrome-extension://abcdefghijklmnop".to_string()],
            token: None,
        }
    }

    fn request(url: &str, origin: &str) -> Request {
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("origin", HeaderValue::from_str(origin).unwrap());
        request
    }

    #[test]
    fn origin_validation_accepts_only_exact_extension_origins() {
        let allowed = vec!["chrome-extension://abcdefghijklmnop".to_string()];
        assert!(origin_allowed(
            "chrome-extension://abcdefghijklmnop",
            &allowed
        ));
        assert!(!origin_allowed("chrome-extension://other", &allowed));
        assert!(!origin_allowed(
            "chrome-extension://*",
            &["chrome-extension://*".to_string()]
        ));
        assert!(!origin_allowed("http://localhost", &allowed));
        assert!(!origin_allowed("https://example.com", &allowed));
    }

    #[tokio::test]
    async fn websocket_update_emits_browser_media_events() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve_listener(
            listener,
            config(address.port()),
            None,
            tx,
            cancel.clone(),
        ));

        let (mut socket, _) = connect_async(request(
            &format!("ws://{address}"),
            "chrome-extension://abcdefghijklmnop",
        ))
        .await
        .unwrap();
        socket
            .send(Message::text(
                r#"{"event":"update","data":{"player":"YouTube Music","title":"Yellow","artist":"Coldplay","album":"Parachutes","duration_ms":267000,"position_ms":4200,"state":"playing","rate":1.0}}"#,
            ))
            .await
            .unwrap();

        let player = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match rx.recv().await {
                    Some(MediaEvent::PlayerChanged { id, backend, .. }) => {
                        return (id, backend);
                    }
                    Some(_) => {}
                    None => panic!("event channel closed"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            player,
            ("YouTube Music".to_string(), BackendKind::WebNowPlaying)
        );

        let mut saw_track = false;
        let mut saw_status = false;
        let mut saw_position = false;
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
        {
            match event {
                MediaEvent::TrackChanged(track) => {
                    assert_eq!(track.title, "Yellow");
                    saw_track = true;
                }
                MediaEvent::Status(PlaybackStatus::Playing) => saw_status = true,
                MediaEvent::PositionPoll { pos_ms, .. } => {
                    assert_eq!(pos_ms, 4_200);
                    saw_position = true;
                }
                _ => {}
            }
        }
        assert!(saw_track && saw_status && saw_position);

        cancel.cancel();
        let _ = socket.close(None).await;
        let _ = server.await;
    }

    #[tokio::test]
    async fn websocket_rejects_http_and_unlisted_origins() {
        for origin in ["http://localhost", "chrome-extension://notallowed"] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let (tx, _rx) = mpsc::channel(8);
            let cancel = CancellationToken::new();
            let server = tokio::spawn(serve_listener(
                listener,
                config(address.port()),
                None,
                tx,
                cancel.clone(),
            ));

            let result = connect_async(request(&format!("ws://{address}"), origin)).await;
            assert!(result.is_err(), "{origin} must be rejected");

            cancel.cancel();
            let _ = server.await;
        }
    }

    #[tokio::test]
    async fn websocket_server_rebinds_after_transient_failure() {
        let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = blocker.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let backend = WebNowPlayingBackend::new(config(port));
        let handle = backend
            .start_monitor(None, tx, cancel.clone())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(700)).await;
        drop(blocker);

        let mut connected = None;
        for _ in 0..20 {
            match connect_async(request(
                &format!("ws://127.0.0.1:{port}"),
                "chrome-extension://abcdefghijklmnop",
            ))
            .await
            {
                Ok(socket) => {
                    connected = Some(socket);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let (mut socket, _) = connected.expect("server should rebind");
        socket
            .send(Message::text(
                r#"{"type":"update","player":"Spotify Web","title":"Yellow","artist":"Coldplay","state":"paused","position_ms":1000,"duration_ms":2000}"#,
            ))
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(event) = rx.recv().await
                    && matches!(event, MediaEvent::PlayerChanged { .. })
                {
                    return event;
                }
            }
        })
        .await
        .expect("player event after reconnect");
        assert!(matches!(event, MediaEvent::PlayerChanged { .. }));

        cancel.cancel();
        let _ = handle.await;
    }
}
