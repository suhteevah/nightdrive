// stage: 0
// expect: retry::with_backoff recovers from a 500-500-200 sequence and returns the
//         final 200 response body, having made exactly 3 attempts
// requires: nothing — owns its mock TCP listener
//
// Proves nightdrive_core::retry::with_backoff against a controlled-sequence HTTP
// server. **Mock-server exception** per tests/witnesses/README.md §74: retry tests
// can't be reliably reproduced against a real endpoint (transient 500s in a known
// order) so we spin up a minimal in-process TCP listener that serves a scripted
// response sequence. No external mocking library — the listener is 30 lines of
// tokio::net::TcpListener + raw HTTP, scoped to this test's lifetime.

use nightdrive_core::retry::{RetryPolicy, with_backoff};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_recovers_from_500_500_200_sequence() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://127.0.0.1:{}/", addr.port());

    let request_count = Arc::new(AtomicU32::new(0));

    // Drive the mock server in the background. It serves up to 3 requests
    // (matching the retry policy's max_attempts) then drops the listener so
    // any extra connections fail fast.
    let server_counter = request_count.clone();
    let server_handle = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            // Drain just enough to satisfy the client's request flush.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;

            let n = server_counter.fetch_add(1, Ordering::SeqCst);
            let response = if n < 2 {
                "HTTP/1.1 500 Internal Server Error\r\n\
                 Content-Length: 9\r\n\
                 Connection: close\r\n\
                 \r\n\
                 try later"
            } else {
                "HTTP/1.1 200 OK\r\n\
                 Content-Length: 5\r\n\
                 Connection: close\r\n\
                 \r\n\
                 hello"
            };
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("http client");

    // Short backoffs so the witness completes in <100ms total even with retries.
    let policy = RetryPolicy {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(20),
        jitter: 0.0,
    };

    let result: Result<String, String> = with_backoff(
        policy,
        || {
            let http = http.clone();
            let url = url.clone();
            async move {
                let resp = http.get(&url).send().await.map_err(|e| e.to_string())?;
                let status = resp.status();
                if !status.is_success() {
                    // Surface the status in the err string so should_retry can
                    // match on it.
                    return Err(format!("status {status}"));
                }
                resp.text().await.map_err(|e| e.to_string())
            }
        },
        |e| {
            // Retry only on 5xx; treat anything else (incl. network errors) as
            // retryable too since the mock listener might still be coming up
            // on the very first attempt — the point of with_backoff is to
            // absorb that warm-up flake.
            e.starts_with("status 5") || e.contains("error")
        },
    )
    .await;

    let body = result.expect("retry must succeed within budget");
    assert_eq!(body, "hello", "final response body must be the 200's payload");

    assert_eq!(
        request_count.load(Ordering::SeqCst),
        3,
        "must have hit the mock server exactly 3 times (500, 500, 200)"
    );

    // Drain the server task so it doesn't show up as a leaked task in test
    // output. The server already exited its 3-iteration loop on its own —
    // this just synchronizes.
    let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;
}
