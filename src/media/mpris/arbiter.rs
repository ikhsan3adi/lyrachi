use std::collections::HashMap;
use std::time::{Duration, Instant};

use color_eyre::Result;
use futures_util::StreamExt;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use zbus::proxy::CacheProperties;
use zbus::{Connection, OwnedVariant};

use super::metadata::extract_track_meta;
use super::proxy::{DBusProxy, PlayerProxy, micros_to_ms, parse_playback_status};
use crate::core::clock::PlaybackStatus;
use crate::media::{MPRIS_BUS_PREFIX, MediaEvent, PlayerKind, TrackMeta};

const ARBITER_TICK_INTERVAL: Duration = Duration::from_secs(2);
const POSITION_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CALL_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerCandidate {
    pub bus: String,
    pub status: PlaybackStatus,
    pub kind: PlayerKind,
}

struct ActivePlayer {
    bus: String,
    token: CancellationToken,
    handle: JoinHandle<()>,
}

impl ActivePlayer {
    fn stop(self) {
        self.token.cancel();
        self.handle.abort();
    }
}

#[derive(PartialEq, Eq)]
struct TrackIdentity {
    track_id: Option<String>,
    url: Option<String>,
    title: String,
    artist: String,
    album: Option<String>,
    duration_ms: Option<u64>,
}

impl From<&TrackMeta> for TrackIdentity {
    fn from(track: &TrackMeta) -> Self {
        Self {
            track_id: track.track_id.clone(),
            url: track.url.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            duration_ms: track.duration_ms,
        }
    }
}

pub async fn run(
    player_filter: Option<String>,
    conn: Connection,
    tx: Sender<MediaEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    MprisSessionArbiter::new(player_filter)
        .run(conn, tx, cancel)
        .await
}

pub struct MprisSessionArbiter {
    player_filter: Option<String>,
}

impl MprisSessionArbiter {
    pub fn new(player_filter: Option<String>) -> Self {
        Self { player_filter }
    }

    pub async fn run(
        self,
        conn: Connection,
        tx: Sender<MediaEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let dbus = DBusProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let mut name_owner_changed = dbus.receive_name_owner_changed().await?;
        let mut ticker = tokio::time::interval(ARBITER_TICK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        let mut active: Option<ActivePlayer> = None;

        if let Err(error) = reselect(&conn, self.player_filter.as_deref(), &tx, &mut active).await {
            warn!(%error, "MPRIS arbitration failed");
        }

        loop {
            let should_reselect = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tx.closed() => break,
                changed = name_owner_changed.next() => {
                    let Some(changed) = changed else {
                        break;
                    };
                    changed
                        .args()
                        .is_ok_and(|args| args.name().as_str().starts_with(MPRIS_BUS_PREFIX))
                }
                _ = ticker.tick() => true,
            };

            if should_reselect
                && let Err(error) =
                    reselect(&conn, self.player_filter.as_deref(), &tx, &mut active).await
            {
                warn!(%error, "MPRIS arbitration failed");
            }
        }

        if let Some(player) = active.take() {
            player.stop();
        }

        Ok(())
    }
}

async fn reselect(
    conn: &Connection,
    player_filter: Option<&str>,
    tx: &Sender<MediaEvent>,
    active: &mut Option<ActivePlayer>,
) -> Result<()> {
    let had_active = active.is_some();

    if active
        .as_ref()
        .is_some_and(|player| player.handle.is_finished())
        && let Some(player) = active.take()
    {
        player.stop();
    }

    let candidates = discover_candidates(conn, player_filter).await;
    let selected = select_player(&candidates, player_filter).cloned();
    let selected_bus = selected.as_ref().map(|player| player.bus.as_str());

    if active.as_ref().map(|player| player.bus.as_str()) == selected_bus {
        return Ok(());
    }

    if let Some(player) = active.take() {
        player.stop();
    }

    let Some(selected) = selected else {
        if had_active {
            let _ = tx.send(MediaEvent::NoPlayer).await;
        }
        return Ok(());
    };

    let bus = selected.bus;
    let kind = selected.kind;
    let token = CancellationToken::new();
    let child_token = token.clone();
    let child_tx = tx.clone();
    let child_conn = conn.clone();
    let child_bus = bus.clone();
    let handle = tokio::spawn(async move {
        if let Err(error) = watch_player(child_conn, child_bus, kind, child_tx, child_token).await {
            warn!(%error, "MPRIS player watch stopped");
        }
    });

    active.replace(ActivePlayer {
        bus: bus.clone(),
        token,
        handle,
    });

    let _ = tx.send(MediaEvent::PlayerChanged { id: bus, kind }).await;

    Ok(())
}

pub async fn discover_candidates(
    conn: &Connection,
    player_filter: Option<&str>,
) -> Vec<PlayerCandidate> {
    let dbus = match DBusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
    {
        Ok(dbus) => dbus,
        Err(error) => {
            warn!(%error, "failed to connect to D-Bus");
            return Vec::new();
        }
    };
    let names = match tokio::time::timeout(CALL_TIMEOUT, dbus.list_names()).await {
        Ok(Ok(names)) => names,
        Ok(Err(error)) => {
            warn!(%error, "failed to list D-Bus names");
            return Vec::new();
        }
        Err(_) => {
            warn!("listing D-Bus names timed out");
            return Vec::new();
        }
    };

    let mut candidates = Vec::new();

    for name in names {
        let bus = name.as_str();

        if !bus.starts_with(MPRIS_BUS_PREFIX) {
            continue;
        }
        match player_filter {
            Some(filter) if !matches_filter(bus, filter) => continue,
            None if is_ignored_bus(bus) => continue,
            _ => {}
        }

        let kind = PlayerKind::from_id(bus);
        let status = match PlayerProxy::builder(conn)
            .destination(bus)
            .cache_properties(CacheProperties::No)
            .build()
            .await
        {
            Ok(proxy) => match tokio::time::timeout(CALL_TIMEOUT, proxy.playback_status()).await {
                Ok(Ok(status)) => parse_playback_status(&status),
                Ok(Err(error)) => {
                    debug!(%error, %bus, "failed to read playback status");
                    PlaybackStatus::Stopped
                }
                Err(_) => {
                    debug!(%bus, "playback status timed out");
                    PlaybackStatus::Stopped
                }
            },
            Err(error) => {
                debug!(%error, %bus, "failed to build MPRIS proxy");
                PlaybackStatus::Stopped
            }
        };

        candidates.push(PlayerCandidate {
            bus: bus.to_string(),
            status,
            kind,
        });
    }

    candidates
}

pub fn select_player<'a>(
    candidates: &'a [PlayerCandidate],
    player_filter: Option<&str>,
) -> Option<&'a PlayerCandidate> {
    candidates
        .iter()
        .filter(|candidate| match player_filter {
            Some(filter) => matches_filter(&candidate.bus, filter),
            None => !is_ignored_bus(&candidate.bus),
        })
        .min_by(|left, right| {
            status_rank(left.status)
                .cmp(&status_rank(right.status))
                .then_with(|| left.kind.priority_rank().cmp(&right.kind.priority_rank()))
                .then_with(|| left.bus.cmp(&right.bus))
        })
}

pub fn is_ignored_bus(bus: &str) -> bool {
    let short = bus.strip_prefix(MPRIS_BUS_PREFIX).unwrap_or(bus);
    let base = short.split('.').next().unwrap_or(short);
    base.eq_ignore_ascii_case("playerctld") || base.eq_ignore_ascii_case("kdeconnect")
}

pub fn matches_filter(bus: &str, filter: &str) -> bool {
    if bus == filter {
        return true;
    }

    let short = bus.strip_prefix(MPRIS_BUS_PREFIX).unwrap_or(bus);

    short == filter
        || short
            .strip_prefix(filter)
            .is_some_and(|rest| rest.starts_with('.'))
}

fn status_rank(status: PlaybackStatus) -> u8 {
    match status {
        PlaybackStatus::Playing => 0,
        PlaybackStatus::Paused => 1,
        PlaybackStatus::Stopped => 2,
    }
}

async fn watch_player(
    conn: Connection,
    bus: String,
    kind: PlayerKind,
    tx: Sender<MediaEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    let proxy = PlayerProxy::builder(&conn)
        .destination(bus.as_str())
        .build()
        .await?;
    let mut status_stream = proxy.receive_playback_status_changed().await;
    let mut metadata_stream = proxy.receive_metadata_changed().await;
    let mut rate_stream = proxy.receive_rate_changed().await;
    let mut seeked_stream = proxy.receive_seeked().await?;
    let mut owner_stream = proxy.inner().receive_owner_changed().await?;
    let mut poll = tokio::time::interval(POSITION_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;

    let mut track_identity: Option<TrackIdentity> = None;
    let mut no_track_sent = false;
    let mut current_rate: Option<f64> = None;

    if let Ok(Ok(metadata)) = tokio::time::timeout(CALL_TIMEOUT, proxy.metadata()).await {
        update_track(
            &tx,
            &metadata,
            kind,
            &mut track_identity,
            &mut no_track_sent,
        )
        .await;
    }

    let mut status = match tokio::time::timeout(CALL_TIMEOUT, proxy.playback_status()).await {
        Ok(Ok(value)) => parse_playback_status(&value),
        _ => PlaybackStatus::Stopped,
    };
    let _ = tx.send(MediaEvent::Status(status)).await;

    if let Ok(Ok(rate)) = tokio::time::timeout(CALL_TIMEOUT, proxy.rate()).await {
        current_rate = Some(rate);
        let _ = tx.send(MediaEvent::Rate(rate)).await;
    }

    if status == PlaybackStatus::Playing {
        poll_position(&proxy, &tx).await;
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tx.closed() => break,
            _ = owner_stream.next() => break,
            _ = poll.tick() => {
                if status == PlaybackStatus::Playing {
                    poll_position(&proxy, &tx).await;
                }
            }
            seeked = seeked_stream.next() => {
                let Some(seeked) = seeked else {
                    break;
                };
                match seeked.args() {
                    Ok(args) => {
                        let _ = tx.send(MediaEvent::Seeked {
                            pos_ms: micros_to_ms(args.position),
                        }).await;
                        if status == PlaybackStatus::Playing {
                            poll_position(&proxy, &tx).await;
                        }
                    }
                    Err(error) => warn!(%error, "failed to decode seeked signal"),
                }
            }
            status_change = status_stream.next() => {
                let Some(status_change) = status_change else {
                    break;
                };
                match tokio::time::timeout(CALL_TIMEOUT, status_change.get()).await {
                    Ok(Ok(value)) => {
                        let new_status = parse_playback_status(&value);
                        if new_status != status {
                            status = new_status;
                            let _ = tx.send(MediaEvent::Status(status)).await;
                            if status == PlaybackStatus::Playing {
                                poll_position(&proxy, &tx).await;
                            }
                        }
                    }
                    Ok(Err(error)) => warn!(%error, "failed to read playback status"),
                    Err(_) => debug!(%bus, "playback status change timed out"),
                }
            }
            rate_change = rate_stream.next() => {
                let Some(rate_change) = rate_change else {
                    break;
                };
                match tokio::time::timeout(CALL_TIMEOUT, rate_change.get()).await {
                    Ok(Ok(rate)) => {
                        if current_rate.is_none_or(|prev| (prev - rate).abs() > f64::EPSILON) {
                            current_rate = Some(rate);
                            let _ = tx.send(MediaEvent::Rate(rate)).await;
                        }
                    }
                    Ok(Err(error)) => debug!(%error, "failed to read playback rate"),
                    Err(_) => debug!(%bus, "playback rate change timed out"),
                }
            }
            metadata_change = metadata_stream.next() => {
                let Some(metadata_change) = metadata_change else {
                    break;
                };
                match tokio::time::timeout(CALL_TIMEOUT, metadata_change.get()).await {
                    Ok(Ok(metadata)) => {
                        let changed = update_track(
                            &tx,
                            &metadata,
                            kind,
                            &mut track_identity,
                            &mut no_track_sent,
                        ).await;
                        if changed && status == PlaybackStatus::Playing {
                            poll_position(&proxy, &tx).await;
                        }
                    }
                    Ok(Err(error)) => warn!(%error, "failed to read metadata"),
                    Err(_) => debug!(%bus, "metadata change timed out"),
                }
            }
        }
    }

    Ok(())
}

async fn update_track(
    tx: &Sender<MediaEvent>,
    metadata: &HashMap<String, OwnedVariant>,
    kind: PlayerKind,
    identity: &mut Option<TrackIdentity>,
    no_track_sent: &mut bool,
) -> bool {
    match extract_track_meta(metadata, kind) {
        Some(track) => {
            let new_identity = TrackIdentity::from(&track);
            if identity.as_ref() != Some(&new_identity) {
                *identity = Some(new_identity);
                *no_track_sent = false;
                let _ = tx.send(MediaEvent::TrackChanged(track)).await;
                return true;
            }
            false
        }
        None => {
            *identity = None;
            if !*no_track_sent {
                *no_track_sent = true;
                let _ = tx.send(MediaEvent::NoTrack).await;
            }
            false
        }
    }
}

async fn poll_position(proxy: &PlayerProxy<'_>, tx: &Sender<MediaEvent>) {
    let started = Instant::now();

    match tokio::time::timeout(CALL_TIMEOUT, proxy.position()).await {
        Ok(Ok(position)) => {
            let _ = tx
                .send(MediaEvent::PositionPoll {
                    pos_ms: micros_to_ms(position),
                    rtt: started.elapsed(),
                })
                .await;
        }
        Ok(Err(error)) => warn!(%error, "failed to read playback position"),
        Err(_) => debug!("reading playback position timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(bus: &str, status: PlaybackStatus, kind: PlayerKind) -> PlayerCandidate {
        PlayerCandidate {
            bus: bus.to_string(),
            status,
            kind,
        }
    }

    #[test]
    fn prefers_playing_players() {
        let candidates = [
            candidate(
                "org.mpris.MediaPlayer2.spotify",
                PlaybackStatus::Paused,
                PlayerKind::Native,
            ),
            candidate(
                "org.mpris.MediaPlayer2.firefox",
                PlaybackStatus::Playing,
                PlayerKind::Browser,
            ),
        ];

        let selected = select_player(&candidates, None).unwrap();

        assert_eq!(selected.bus, "org.mpris.MediaPlayer2.firefox");
    }

    #[test]
    fn prefers_native_players_over_browsers_when_both_play() {
        let candidates = [
            candidate(
                "org.mpris.MediaPlayer2.firefox",
                PlaybackStatus::Playing,
                PlayerKind::Browser,
            ),
            candidate(
                "org.mpris.MediaPlayer2.spotify",
                PlaybackStatus::Playing,
                PlayerKind::Native,
            ),
        ];

        let selected = select_player(&candidates, None).unwrap();

        assert_eq!(selected.bus, "org.mpris.MediaPlayer2.spotify");
    }

    #[test]
    fn uses_bus_name_as_a_deterministic_tie_breaker() {
        let candidates = [
            candidate(
                "org.mpris.MediaPlayer2.zed",
                PlaybackStatus::Playing,
                PlayerKind::Native,
            ),
            candidate(
                "org.mpris.MediaPlayer2.alpha",
                PlaybackStatus::Playing,
                PlayerKind::Native,
            ),
        ];

        let selected = select_player(&candidates, None).unwrap();

        assert_eq!(selected.bus, "org.mpris.MediaPlayer2.alpha");
    }

    #[test]
    fn filters_by_exact_and_suffix_names() {
        assert!(matches_filter("org.mpris.MediaPlayer2.spotify", "spotify"));
        assert!(matches_filter(
            "org.mpris.MediaPlayer2.spotify.instance1",
            "spotify"
        ));
        assert!(matches_filter(
            "org.mpris.MediaPlayer2.spotify",
            "org.mpris.MediaPlayer2.spotify"
        ));
        assert!(!matches_filter(
            "org.mpris.MediaPlayer2.spotifyd",
            "spotify"
        ));

        let candidates = [
            candidate(
                "org.mpris.MediaPlayer2.spotify",
                PlaybackStatus::Playing,
                PlayerKind::Native,
            ),
            candidate(
                "org.mpris.MediaPlayer2.firefox",
                PlaybackStatus::Playing,
                PlayerKind::Browser,
            ),
        ];

        assert_eq!(
            select_player(&candidates, Some("firefox")).unwrap().bus,
            "org.mpris.MediaPlayer2.firefox"
        );
    }

    #[test]
    fn identifies_player_kinds() {
        assert_eq!(
            PlayerKind::from_id("org.mpris.MediaPlayer2.firefox.instance1"),
            PlayerKind::Browser
        );
        assert_eq!(
            PlayerKind::from_id("org.mpris.MediaPlayer2.spotify"),
            PlayerKind::Native
        );
        assert_eq!(
            PlayerKind::from_id("org.mpris.MediaPlayer2.limusic"),
            PlayerKind::Native
        );
        assert_eq!(
            PlayerKind::from_id("org.mpris.MediaPlayer2.mpv.instance42"),
            PlayerKind::Native
        );
        assert_eq!(
            PlayerKind::from_id("org.mpris.MediaPlayer2.mystery"),
            PlayerKind::Unknown
        );
    }

    #[test]
    fn ignores_proxy_buses_unless_explicitly_filtered() {
        assert!(is_ignored_bus("org.mpris.MediaPlayer2.playerctld"));
        assert!(is_ignored_bus("org.mpris.MediaPlayer2.kdeconnect"));
        assert!(!is_ignored_bus("org.mpris.MediaPlayer2.limusic"));

        let candidates = [
            candidate(
                "org.mpris.MediaPlayer2.playerctld",
                PlaybackStatus::Playing,
                PlayerKind::Unknown,
            ),
            candidate(
                "org.mpris.MediaPlayer2.firefox",
                PlaybackStatus::Playing,
                PlayerKind::Browser,
            ),
        ];

        assert_eq!(
            select_player(&candidates, None).unwrap().bus,
            "org.mpris.MediaPlayer2.firefox"
        );
        assert_eq!(
            select_player(&candidates, Some("playerctld")).unwrap().bus,
            "org.mpris.MediaPlayer2.playerctld"
        );
    }

    #[test]
    fn prioritizes_native_over_unknown_over_browser() {
        let candidates = [
            candidate(
                "org.mpris.MediaPlayer2.firefox",
                PlaybackStatus::Playing,
                PlayerKind::Browser,
            ),
            candidate(
                "org.mpris.MediaPlayer2.mystery",
                PlaybackStatus::Playing,
                PlayerKind::Unknown,
            ),
        ];
        assert_eq!(
            select_player(&candidates, None).unwrap().bus,
            "org.mpris.MediaPlayer2.mystery"
        );

        let with_native = [
            candidates[0].clone(),
            candidates[1].clone(),
            candidate(
                "org.mpris.MediaPlayer2.mpv",
                PlaybackStatus::Playing,
                PlayerKind::Native,
            ),
        ];
        assert_eq!(
            select_player(&with_native, None).unwrap().bus,
            "org.mpris.MediaPlayer2.mpv"
        );
    }

    #[tokio::test]
    async fn late_duration_and_url_updates_emit_track_changed() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "xesam:title".to_string(),
            OwnedVariant::try_from(zbus::Variant::from("Song")).unwrap(),
        );
        metadata.insert(
            "xesam:url".to_string(),
            OwnedVariant::try_from(zbus::Variant::from("file:///music/song.flac")).unwrap(),
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut identity = None;
        let mut no_track_sent = false;

        assert!(
            update_track(
                &tx,
                &metadata,
                PlayerKind::Native,
                &mut identity,
                &mut no_track_sent,
            )
            .await
        );
        assert!(matches!(rx.recv().await, Some(MediaEvent::TrackChanged(_))));

        metadata.insert(
            "mpris:length".to_string(),
            OwnedVariant::from(240_000_000_i64),
        );
        assert!(
            update_track(
                &tx,
                &metadata,
                PlayerKind::Native,
                &mut identity,
                &mut no_track_sent,
            )
            .await
        );
        assert!(matches!(rx.recv().await, Some(MediaEvent::TrackChanged(_))));

        metadata.insert(
            "xesam:url".to_string(),
            OwnedVariant::try_from(zbus::Variant::from("file:///music/other.flac")).unwrap(),
        );
        assert!(
            update_track(
                &tx,
                &metadata,
                PlayerKind::Native,
                &mut identity,
                &mut no_track_sent,
            )
            .await
        );
        assert!(matches!(rx.recv().await, Some(MediaEvent::TrackChanged(_))));
    }
}
