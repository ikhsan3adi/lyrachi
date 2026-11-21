mod kugou;
mod netease;
mod qq_crypto;
mod qqmusic;

use std::io::Read;

use base64::Engine;

pub use kugou::KugouProvider;
pub use netease::NeteaseProvider;
pub use qqmusic::QqMusicProvider;

use crate::core::model::SyncedTrack;
use crate::core::normalize::normalize_track_checked;
use crate::core::parse::ParsedTrack;

use super::ProviderError;

pub(crate) const MAX_DECODED_BYTES: usize = 512 * 1024;

pub(crate) fn decode_base64_bounded(input: &str) -> Result<Vec<u8>, ProviderError> {
    let compact = input
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|error| ProviderError::Invalid(format!("invalid base64 payload: {error}")))?;
    if decoded.len() > MAX_DECODED_BYTES {
        return Err(ProviderError::PayloadTooLarge(decoded.len()));
    }
    Ok(decoded)
}

pub(crate) fn inflate_bounded(data: &[u8]) -> Result<Vec<u8>, ProviderError> {
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut output = Vec::new();
    decoder
        .by_ref()
        .take(MAX_DECODED_BYTES as u64 + 1)
        .read_to_end(&mut output)
        .map_err(|error| ProviderError::Invalid(format!("invalid zlib payload: {error}")))?;
    if output.len() > MAX_DECODED_BYTES {
        return Err(ProviderError::PayloadTooLarge(output.len()));
    }
    Ok(output)
}

pub(crate) fn decode_utf8(data: Vec<u8>) -> Result<String, ProviderError> {
    String::from_utf8(data)
        .map_err(|_| ProviderError::Invalid("lyric payload is not valid UTF-8".to_string()))
}

pub(crate) fn track_from_parsed(
    mut parsed: ParsedTrack,
    provider: &str,
    source_track_id: Option<String>,
    confidence: f32,
    duration_ms: Option<u64>,
    user_offset_ms: i64,
) -> Result<Option<SyncedTrack>, ProviderError> {
    drop_credit_lines(&mut parsed);
    if parsed.lines.is_empty() {
        return Ok(None);
    }
    let track = normalize_track_checked(
        parsed,
        provider,
        source_track_id,
        confidence,
        duration_ms,
        user_offset_ms,
    )
    .map_err(|error| ProviderError::Invalid(error.to_string()))?;
    Ok((!track.lines.is_empty()).then_some(track))
}

#[cfg(test)]
pub(crate) async fn mock_server(
    responses: Vec<(u16, String)>,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let _ = socket.read(&mut buffer).await;
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{address}"), handle)
}

fn drop_credit_lines(parsed: &mut ParsedTrack) {
    let title = parsed
        .metadata
        .title
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_lowercase();
    let artist = parsed
        .metadata
        .artist
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_lowercase();

    parsed.lines.retain(|line| {
        let text = line.text.trim();
        if !title.is_empty() && !artist.is_empty() {
            let lower = text.to_lowercase();
            if lower == format!("{title} - {artist}") || lower == format!("{artist} - {title}") {
                return false;
            }
        }
        !is_credit_line(text)
    });
}

fn is_credit_line(text: &str) -> bool {
    let Some((label, _)) = text.split_once([':', '：']) else {
        return false;
    };
    let label = label.trim().to_lowercase();
    if label.is_empty() || label.chars().count() > 24 {
        return false;
    }
    const LATIN: &[&str] = &[
        "lyrics",
        "composed",
        "arranged",
        "produced",
        "written",
        "mixed",
        "mastered",
        "engineer",
        "publisher",
        "copyright",
        "vocals",
        "guitar",
        "drums",
        "bass",
        "piano",
        "synthesizer",
    ];
    const CJK: &[&str] = &[
        "作词", "作曲", "编曲", "制作", "混音", "母带", "录音", "和声", "监制", "出品", "发行",
    ];
    LATIN.iter().any(|role| label.contains(role)) || CJK.iter().any(|role| label.contains(role))
}
