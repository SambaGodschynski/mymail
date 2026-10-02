use std::time::Duration;

use serde_json::Value;

use crate::Error;

const MAX_RATE_LIMIT_WAITS: u32 = 5;
// A request that gets no answer has to fail rather than wait for ever. Without
// this a stalled connection — a sleeping laptop, a VPN that dropped, a proxy
// that accepted and then went quiet — reads as the program having frozen, with
// nothing on screen to say otherwise.
const TIMEOUT: Duration = Duration::from_secs(30);
// A stall is usually a blip, and a run of several hundred requests shouldn't
// end because one of them met it.
const STALL_RETRIES: u32 = 3;
const STALL_BACKOFF: Duration = Duration::from_secs(5);
const FALLBACK_RETRY_AFTER: u64 = 10;
const MAX_RETRY_AFTER: u64 = 120;

// What an expired token looks like, which for this tool is a matter of when
// rather than if: a bearer token copied out of a browser lasts about an hour.
const EXPIRED: &str = "the token was rejected — a browser token lasts about an hour, so it has \
probably expired. Copy a fresh one (see the README).";

pub fn client() -> reqwest::Client {
    client_with_timeout(TIMEOUT)
}

pub fn client_with_timeout(timeout: Duration) -> reqwest::Client {
    // Outlook returns calendar times in whatever zone it feels is right and
    // says which in a field beside them; asking for UTC once means every
    // timestamp that arrives can be read the same way. It does nothing to mail,
    // whose receivedDateTime is UTC regardless.
    // IdType: a message's default id changes when it moves between folders, so
    // anything that stored one would break the first time a rule filed
    // something away. The immutable one survives that — and is 68 characters
    // rather than 152.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Prefer",
        "outlook.timezone=\"UTC\", IdType=\"ImmutableId\"".parse().expect("a literal header value"),
    );

    reqwest::Client::builder()
        .timeout(timeout)
        .default_headers(headers)
        // A redirect would be Microsoft sending us to a sign-in page, which is
        // not an answer and should not be followed and parsed as one.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a default reqwest client always builds")
}

pub struct Session {
    client: reqwest::Client,
    token: String,
    stall_retries: u32,
    stall_backoff: Duration,
}

impl Session {
    pub fn new(client: &reqwest::Client, token: String) -> Self {
        Self {
            client: client.clone(),
            token,
            stall_retries: STALL_RETRIES,
            stall_backoff: STALL_BACKOFF,
        }
    }

    // For tests, which cannot wait out the real backoff.
    #[cfg(test)]
    fn impatient(mut self, retries: u32, backoff: Duration) -> Self {
        self.stall_retries = retries;
        self.stall_backoff = backoff;
        self
    }

    pub async fn get(&self, url: &str) -> Result<Value, Error> {
        let mut waits = 0u32;
        let mut stalls = 0u32;
        loop {
            let sent = self
                .client
                .get(url)
                .bearer_auth(&self.token)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await;

            let response = match sent {
                Ok(response) => response,
                // Timed out, or never connected: nothing was answered, so
                // asking again is safe — this is a GET either way.
                Err(e) if (e.is_timeout() || e.is_connect()) && stalls < self.stall_retries => {
                    stalls += 1;
                    eprintln!(
                        "no answer ({}) — trying again ({stalls}/{})",
                        if e.is_timeout() { "timed out" } else { "could not connect" },
                        self.stall_retries
                    );
                    tokio::time::sleep(self.stall_backoff).await;
                    continue;
                }
                Err(e) => return Err(with_cause(e)),
            };

            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
                if status == reqwest::StatusCode::UNAUTHORIZED || status.is_redirection() {
                    return Err(EXPIRED.into());
                }
                let body = response.text().await?;
                if !status.is_success() {
                    return Err(format!("{} (HTTP {})", detail(&body), status.as_u16()).into());
                }
                return serde_json::from_str(&body).map_err(|e| {
                    if body.trim_start().starts_with('<') {
                        EXPIRED.into()
                    } else {
                        Error::from(format!("could not parse the response as JSON: {e} — {}", snippet(&body)))
                    }
                });
            }

            waits += 1;
            if waits > MAX_RATE_LIMIT_WAITS {
                return Err(format!("rate limited (gave up after {MAX_RATE_LIMIT_WAITS} waits)").into());
            }
            let secs = retry_after.unwrap_or(FALLBACK_RETRY_AFTER).clamp(1, MAX_RETRY_AFTER);
            eprintln!("Rate limited — waiting {secs}s ({waits}/{MAX_RATE_LIMIT_WAITS})...");
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        }
    }

    // Graph pages with an @odata.nextLink holding the whole next URL, query
    // string and all, so there is no offset or token to assemble here.
    pub async fn get_all(&self, url: &str, verbose: bool) -> Result<Vec<Value>, Error> {
        const MAX_PAGES: u32 = 500;
        let mut all = Vec::new();
        let mut next = Some(url.to_string());

        for _ in 0..MAX_PAGES {
            let Some(url) = next else { break };
            if verbose {
                eprintln!("GET {url}");
            }
            let resp = self.get(&url).await?;
            if let Some(values) = resp.get("value").and_then(Value::as_array) {
                all.extend(values.iter().cloned());
            }
            next = resp.get("@odata.nextLink").and_then(Value::as_str).map(str::to_string);
        }
        Ok(all)
    }
}

// Graph reports what went wrong inside an "error" object. Digging the sentence
// out of it is the difference between an error someone can act on and one that
// only says a request failed.
fn detail(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let error = v.get("error");
        if let Some(message) = error
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let code = error.and_then(|e| e.get("code")).and_then(Value::as_str).unwrap_or_default();
            return if code.is_empty() { message.to_string() } else { format!("{code}: {message}") };
        }
    }
    snippet(body)
}

// reqwest reports "error sending request for url (…)" and keeps the reason —
// a DNS failure, a refused connection, an expired certificate — one level down
// in the source chain, which is usually the whole answer.
fn with_cause(e: reqwest::Error) -> Error {
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(&format!(": {text}"));
        }
        source = cause.source();
    }
    message.into()
}

fn snippet(body: &str) -> String {
    let line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let mut out: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        out.push('…');
    }
    out
}

// Percent-encodes everything but the unreserved set, so an OData filter — full
// of spaces, colons and quotes — survives a query string intact.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn response(head: &str, json: &str) -> String {
        format!(
            "HTTP/1.1 {head}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{json}",
            json.len()
        )
    }

    fn rate_limited() -> String {
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string()
    }

    async fn serve(responses: Vec<String>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in responses {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{addr}/v1.0/me/messages")
    }

    fn session() -> Session {
        Session::new(&client(), "eyJ-token".to_string())
    }

    #[tokio::test]
    async fn the_token_is_sent_as_a_bearer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
            let _ = sock.write_all(response("200 OK", r#"{"value":[]}"#).as_bytes()).await;
        });

        session().get(&format!("http://{addr}/x")).await.unwrap();

        let request = rx.await.unwrap().to_lowercase();
        assert!(request.contains("authorization: bearer eyj-token"), "got:\n{request}");
    }

    // The failure this exists for: a server that accepts the connection and
    // then says nothing. Without a timeout the program waits for ever and looks
    // frozen; with one it gives up, says so, and tries again.
    #[tokio::test]
    async fn a_server_that_never_answers_times_out_and_is_retried() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            // Accept and hold, answering nothing.
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                held.push(sock);
            }
        });

        let session = Session::new(&client_with_timeout(Duration::from_millis(200)), "t".to_string())
            .impatient(2, Duration::from_millis(50));
        let started = std::time::Instant::now();

        let err = session.get(&format!("http://{addr}/")).await.expect_err("a silent server is a failure");

        assert!(err.to_string().contains("error sending request"), "got: {err}");
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 3, "one try plus two retries");
        assert!(started.elapsed() < Duration::from_secs(5), "it must not have waited for ever");
    }

    #[tokio::test]
    async fn an_unauthorized_answer_says_the_token_expired() {
        let url = serve(vec![response("401 Unauthorized", r#"{"error":{"code":"InvalidAuthenticationToken"}}"#)]).await;

        let err = session().get(&url).await.expect_err("401 is not an answer").to_string();
        assert!(err.contains("expired"), "got: {err}");
    }

    // The other shape a dead session takes: a redirect to a sign-in page, which
    // would otherwise be followed and come back as HTML.
    #[tokio::test]
    async fn a_redirect_to_a_sign_in_page_says_the_same() {
        let url = serve(vec!["HTTP/1.1 302 Found\r\nLocation: https://login.microsoftonline.com/\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string()]).await;

        let err = session().get(&url).await.expect_err("a redirect is not an answer").to_string();
        assert!(err.contains("expired"), "got: {err}");
    }

    #[tokio::test]
    async fn a_graph_error_message_is_lifted_out_of_its_envelope() {
        let body = r#"{"error":{"code":"ErrorInvalidProperty","message":"The property is not supported."}}"#;
        let url = serve(vec![response("400 Bad Request", body)]).await;

        let err = session().get(&url).await.expect_err("400 is an error").to_string();
        assert!(err.starts_with("ErrorInvalidProperty: The property is not supported."), "got: {err}");
        assert!(err.contains("HTTP 400"), "the status should still be there, got: {err}");
    }

    #[tokio::test]
    async fn a_rate_limit_is_waited_out_rather_than_returned_as_an_error() {
        let url = serve(vec![rate_limited(), response("200 OK", r#"{"value":[{"id":"1"}]}"#)]).await;
        let started = std::time::Instant::now();

        let all = session().get_all(&url, false).await.expect("should succeed on the retry");

        assert_eq!(all.len(), 1);
        assert!(started.elapsed() >= std::time::Duration::from_secs(1), "Retry-After should be honoured");
    }

    #[tokio::test]
    async fn the_retry_budget_is_bounded_so_a_persistent_limit_cannot_hang_the_run() {
        let url = serve(vec![rate_limited(); MAX_RATE_LIMIT_WAITS as usize + 1]).await;

        let err = session().get(&url).await.expect_err("should give up").to_string();
        assert!(err.contains("rate limited"), "got: {err}");
    }

    // Graph hands back the whole next URL rather than an offset to assemble.
    #[tokio::test]
    async fn pages_are_followed_by_their_next_link() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let first = format!(
            r#"{{"value":[{{"id":"1"}}],"@odata.nextLink":"http://{addr}/page2"}}"#
        );
        tokio::spawn(async move {
            for body in [response("200 OK", &first), response("200 OK", r#"{"value":[{"id":"2"}]}"#)] {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(body.as_bytes()).await;
            }
        });

        let all = session().get_all(&format!("http://{addr}/page1"), false).await.unwrap();
        let ids: Vec<&str> = all.iter().filter_map(|v| v.get("id").and_then(Value::as_str)).collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn encoding_leaves_the_unreserved_set_alone_and_escapes_the_rest() {
        assert_eq!(encode("receivedDateTime"), "receivedDateTime");
        assert_eq!(encode("a ge 2026-09-22T00:00:00Z"), "a%20ge%202026-09-22T00%3A00%3A00Z");
    }
}
