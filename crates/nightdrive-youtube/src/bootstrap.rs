//! YouTube OAuth Desktop bootstrap.
//!
//! One-time flow for obtaining a refresh token from a Google OAuth Desktop client.
//! The refresh token then lives in the `NIGHTDRIVE_YT_REFRESH_TOKEN` env var and
//! is exchanged for short-lived access tokens by [`crate::YoutubeClient`].
//!
//! ## Flow
//!
//! 1. Bind a TcpListener on `127.0.0.1:0` (OS-picked free port).
//! 2. Build the Google OAuth consent URL with `redirect_uri=http://127.0.0.1:<port>/callback`
//!    and `access_type=offline` + `prompt=consent` so we're guaranteed a refresh token
//!    even on a re-auth.
//! 3. Print the URL to stdout (we don't try to auto-open the browser — that adds a
//!    dep with no real value when the user is already at a shell).
//! 4. Wait for the browser's `GET /callback?code=...` request.
//! 5. Exchange the auth code for `refresh_token` + `access_token` via
//!    `https://oauth2.googleapis.com/token`.
//! 6. Return the refresh token.
//!
//! ## Scope
//!
//! We request `https://www.googleapis.com/auth/youtube`. That's the full account-
//! management scope, which covers: `videos.insert` (upload), `videos.update`
//! (snippet/status patches), `videos.delete` (witness cleanup), `thumbnails.set`,
//! and `liveBroadcasts` (for the eventual N1 livestream rotation). The narrower
//! `youtube.upload` scope was tried first but rejects `videos.update` and
//! `videos.delete` with `ACCESS_TOKEN_SCOPE_INSUFFICIENT`, which made
//! upload-then-cleanup witnesses impossible.

use crate::{NightdriveError, NightdriveResult};
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, instrument};

const SCOPE: &str = "https://www.googleapis.com/auth/youtube";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Run the OAuth Desktop flow against `client_id` / `client_secret`. Blocks
/// waiting for the user to complete the browser consent step, then exchanges
/// the auth code for a refresh token. Returns the refresh token; caller is
/// expected to write it to `NIGHTDRIVE_YT_REFRESH_TOKEN` (in `.env` or shell).
///
/// The function prints a single URL to stdout for the user to open. The local
/// callback server only handles one request and then exits, so the function
/// is fire-and-forget.
#[instrument(skip_all)]
pub async fn bootstrap_refresh_token(
    client_id: &str,
    client_secret: &str,
) -> NightdriveResult<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| NightdriveError::Youtube(format!("bind loopback: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| NightdriveError::Youtube(format!("read local addr: {e}")))?;
    let redirect_uri = format!("http://127.0.0.1:{}/callback", local_addr.port());

    let auth_url = build_consent_url(client_id, &redirect_uri);

    println!("\nOpen this URL in a browser to grant nightdrive YouTube upload access:\n");
    println!("    {auth_url}\n");
    println!("Waiting for callback on {redirect_uri} (timeout: 5 minutes)...\n");

    let code = tokio::time::timeout(Duration::from_secs(300), wait_for_callback(&listener))
        .await
        .map_err(|_| NightdriveError::Youtube("oauth callback timeout (5 min)".into()))??;

    info!("auth code received, exchanging for refresh token");

    exchange_code(client_id, client_secret, &code, &redirect_uri).await
}

fn build_consent_url(client_id: &str, redirect_uri: &str) -> String {
    let mut url = url::Url::parse(AUTH_URL).expect("AUTH_URL must parse");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        // access_type=offline + prompt=consent guarantees we get a refresh_token in
        // the token exchange, even if the user has previously consented.
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    url.to_string()
}

async fn wait_for_callback(listener: &tokio::net::TcpListener) -> NightdriveResult<String> {
    let (mut stream, peer) = listener
        .accept()
        .await
        .map_err(|e| NightdriveError::Youtube(format!("accept callback: {e}")))?;
    debug!(%peer, "callback connection received");

    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| NightdriveError::Youtube(format!("read request: {e}")))?;
    let request = std::str::from_utf8(&buf[..n])
        .map_err(|e| NightdriveError::Youtube(format!("non-utf8 request: {e}")))?;

    // The first line is `GET /callback?code=...&scope=... HTTP/1.1`.
    let first_line = request
        .lines()
        .next()
        .ok_or_else(|| NightdriveError::Youtube("empty request".into()))?;
    let path = first_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| NightdriveError::Youtube("malformed request line".into()))?;

    // Parse query params. We accept the auth code from `code=...` and surface any
    // `error=...` Google might have sent (user-denied-consent etc).
    let url = url::Url::parse(&format!("http://127.0.0.1{path}"))
        .map_err(|e| NightdriveError::Youtube(format!("parse callback path: {e}")))?;

    let mut code: Option<String> = None;
    let mut err_msg: Option<String> = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.to_string()),
            "error" => err_msg = Some(v.to_string()),
            _ => {}
        }
    }

    // Always respond to the browser so the user sees a friendly page, then close.
    let body = match (&code, &err_msg) {
        (Some(_), _) => {
            "<!doctype html><html><body style='font-family:sans-serif;padding:2em'>\
             <h2>nightdrive — auth complete</h2>\
             <p>You can close this tab and return to the terminal.</p>\
             </body></html>"
        }
        (None, Some(_)) => {
            "<!doctype html><html><body style='font-family:sans-serif;padding:2em'>\
             <h2>nightdrive — auth failed</h2>\
             <p>Check the terminal for details.</p>\
             </body></html>"
        }
        _ => "<!doctype html><html><body>nightdrive: unexpected callback shape</body></html>",
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;

    match (code, err_msg) {
        (Some(c), _) => Ok(c),
        (None, Some(e)) => Err(NightdriveError::Youtube(format!("oauth denied: {e}"))),
        (None, None) => Err(NightdriveError::Youtube("callback had no code or error".into())),
    }
}

async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> NightdriveResult<String> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| NightdriveError::Youtube(format!("http client: {e}")))?;

    let params = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
    ];

    let resp = http
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .map_err(|e| NightdriveError::Youtube(format!("token POST: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(NightdriveError::Youtube(format!(
            "token exchange failed {status}: {text}"
        )));
    }

    #[derive(Deserialize)]
    struct ExchangeResp {
        refresh_token: Option<String>,
        access_token: String,
        expires_in: u64,
    }
    let tok: ExchangeResp = resp
        .json()
        .await
        .map_err(|e| NightdriveError::Youtube(format!("decode token resp: {e}")))?;

    let _ = tok.access_token;
    debug!(expires_in = tok.expires_in, "access token received");

    tok.refresh_token.ok_or_else(|| {
        NightdriveError::Youtube(
            "google didn't return a refresh_token — re-check OAuth client is 'Desktop app' \
             and that access_type=offline + prompt=consent are in the consent URL"
                .into(),
        )
    })
}
