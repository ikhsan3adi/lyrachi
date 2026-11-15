use std::net::IpAddr;

use reqwest::Url;
use reqwest::redirect::Policy;

use super::ProviderError;

pub const MAX_HTTP_BODY_BYTES: usize = 512 * 1024;
const MAX_REDIRECTS: usize = 5;

pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().redirect(redirect_policy())
}

pub fn redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }

        let Some(previous) = attempt.previous().last() else {
            return attempt.error("redirect chain has no previous URL");
        };

        let next = attempt.url();
        if next.host_str() != previous.host_str()
            || next.scheme() != previous.scheme()
            || !allowed_url(next)
        {
            return attempt.error("redirect blocked by network policy");
        }

        attempt.follow()
    })
}

fn allowed_url(url: &Url) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http" && url.host_str().is_some_and(is_loopback_host))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

pub async fn read_limited_text(mut response: reqwest::Response) -> Result<String, ProviderError> {
    if let Some(length) = response.content_length()
        && length > MAX_HTTP_BODY_BYTES as u64
    {
        return Err(body_limit_error(length));
    }

    let mut body = Vec::new();

    while let Some(chunk) = response.chunk().await? {
        let next_len = body.len().saturating_add(chunk.len());
        if next_len > MAX_HTTP_BODY_BYTES {
            return Err(body_limit_error(next_len as u64));
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body)
        .map_err(|_| ProviderError::Invalid("HTTP body is not valid UTF-8".to_string()))
}

fn body_limit_error(length: u64) -> ProviderError {
    ProviderError::Invalid(format!(
        "HTTP body is {length} bytes, limit is {MAX_HTTP_BODY_BYTES}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    fn client() -> reqwest::Client {
        client_builder().build().unwrap()
    }

    async fn listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        (listener, format!("http://{address}"))
    }

    fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let headers = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn serve(listener: TcpListener, responses: Vec<Vec<u8>>) -> JoinHandle<()> {
        tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                socket.write_all(&response).await.unwrap();
            }
        })
    }

    #[tokio::test]
    async fn rejects_body_over_limit() {
        let (listener, base) = listener().await;
        let body = vec![b'x'; MAX_HTTP_BODY_BYTES + 1];
        let server = serve(listener, vec![response("200 OK", &[], &body)]);

        let response = client().get(format!("{base}/")).send().await.unwrap();
        let error = read_limited_text(response).await.unwrap_err();

        assert!(matches!(error, ProviderError::Invalid(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_oversized_content_length_before_body() {
        let (listener, base) = listener().await;
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_HTTP_BODY_BYTES + 1
        )
        .into_bytes();
        let server = serve(listener, vec![raw]);

        let response = client().get(format!("{base}/")).send().await.unwrap();
        let error = read_limited_text(response).await.unwrap_err();

        assert!(matches!(error, ProviderError::Invalid(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn follows_same_origin_redirects() {
        let (listener, base) = listener().await;
        let location = format!("{base}/final");
        let server = serve(
            listener,
            vec![
                response("302 Found", &[("Location", &location)], b""),
                response("200 OK", &[], b"ok"),
            ],
        );

        let response = client().get(format!("{base}/start")).send().await.unwrap();
        let text = read_limited_text(response).await.unwrap();

        assert_eq!(text, "ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn blocks_cross_origin_redirects() {
        let (listener, base) = listener().await;
        let port = listener.local_addr().unwrap().port();
        let location = format!("http://localhost:{port}/final");
        let server = serve(
            listener,
            vec![response("302 Found", &[("Location", &location)], b"")],
        );

        let result = client().get(format!("{base}/start")).send().await;

        assert!(result.is_err());
        server.await.unwrap();
    }
}
