use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::auth::{auth_file_path, AuthStore, OAuthCredential};
use crate::input::plain_read_line;

const PROVIDER: &str = "openai-codex";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cryptographically-secure random bytes from the OS CSPRNG.
///
/// PKCE verifiers and the OAuth `state` nonce MUST be unpredictable, so this
/// pulls from the platform RNG via `getrandom` (which uses `/dev/urandom` on
/// Unix, `BCryptGenRandom` on Windows, `getentropy` on macOS, etc.). A failing
/// OS RNG is a fatal environment problem and unrecoverable here, so we panic
/// rather than fall back to a weak, predictable seed.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::getrandom(&mut out).expect("OS CSPRNG (getrandom) unavailable");
    out
}

fn random_hex(bytes: usize) -> String {
    random_bytes::<32>()[..bytes]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn pkce_pair() -> (String, String) {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes::<32>());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn percent_encode(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form_urlencoded(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn authorization_url(state: &str, challenge: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        // Deliberately still the pre-rename name: this identifies the client to
        // the Codex backend, so it changes only alongside a live auth test.
        ("originator", "small-harness"),
    ];
    format!("{AUTHORIZE_URL}?{}", form_urlencoded(&params))
}

fn query_param(path: &str, name: &str) -> Option<String> {
    let raw_query = path.split_once('?').map(|(_, q)| q).unwrap_or(path);
    let query = raw_query
        .split_once('#')
        .map(|(q, _)| q)
        .unwrap_or(raw_query);
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == name {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.contains("code=") || value.contains("state=") {
        (query_param(value, "code"), query_param(value, "state"))
    } else if let Some((code, state)) = value.split_once('#') {
        (Some(code.to_string()), Some(state.to_string()))
    } else if value.is_empty() {
        (None, None)
    } else {
        (Some(value.to_string()), None)
    }
}

fn write_callback_response(mut stream: TcpStream, ok: bool, message: &str) {
    let title = if ok {
        "OpenAI login complete"
    } else {
        "OpenAI login failed"
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{}</title><body style=\"font-family: system-ui; margin: 3rem\"><h1>{}</h1><p>{}</p></body>",
        title, title, message
    );
    let status = if ok { "200 OK" } else { "400 Bad Request" };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn wait_for_browser_callback(state: String) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:1455")
        .context("binding OAuth callback server on 127.0.0.1:1455")?;
    let (mut stream, _) = listener.accept().context("waiting for OAuth callback")?;
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    let first = request.lines().next().unwrap_or_default();
    let path = first.split_whitespace().nth(1).unwrap_or_default();
    if !path.starts_with("/auth/callback") {
        write_callback_response(stream, false, "Callback route not found.");
        return Err(anyhow!("unexpected OAuth callback path"));
    }
    let got_state = query_param(path, "state");
    if got_state.as_deref() != Some(state.as_str()) {
        write_callback_response(stream, false, "State mismatch.");
        return Err(anyhow!("OAuth state mismatch"));
    }
    let code = query_param(path, "code").ok_or_else(|| anyhow!("OAuth callback missing code"))?;
    write_callback_response(
        stream,
        true,
        "OpenAI authentication completed. You can close this window.",
    );
    Ok(code)
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut cmd = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    cmd.arg(url);
    cmd.spawn().context("opening browser")?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

async fn read_token_response(resp: reqwest::Response, operation: &str) -> Result<OAuthCredential> {
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "OpenAI Codex token {operation} failed ({status}): {}",
            body.trim()
        ));
    }
    let token: TokenResponse = resp.json().await?;
    let account_id = account_id_from_access_token(&token.access_token);
    Ok(OAuthCredential {
        credential_type: "oauth".into(),
        access: token.access_token,
        refresh: token.refresh_token,
        expires: now_secs() + token.expires_in,
        account_id,
    })
}

async fn exchange_authorization_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredential> {
    let body = form_urlencoded(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ]);
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    read_token_response(resp, "exchange").await
}

pub async fn refresh_oauth(client: &reqwest::Client, refresh: &str) -> Result<OAuthCredential> {
    let body = form_urlencoded(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", CLIENT_ID),
    ]);
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    read_token_response(resp, "refresh").await
}

fn save_oauth(credential: OAuthCredential) -> Result<PathBuf> {
    let mut store = AuthStore::load();
    store.set_oauth(PROVIDER, credential);
    store.save()?;
    auth_file_path().context("no auth file path")
}

pub async fn login_browser(client: &reqwest::Client) -> Result<OAuthCredential> {
    let (verifier, challenge) = pkce_pair();
    let state = random_hex(16);
    let url = authorization_url(&state, &challenge);
    println!("  Open this URL to sign in with ChatGPT/Codex:\n\n  {url}\n");
    if let Err(e) = open_browser(&url) {
        println!("  Browser did not open automatically: {e}");
    }
    println!("  Waiting for callback on {REDIRECT_URI} ...");
    let state_for_thread = state.clone();
    let callback = tokio::task::spawn_blocking(move || wait_for_browser_callback(state_for_thread))
        .await
        .context("joining OAuth callback task")?;
    let code = match callback {
        Ok(code) => code,
        Err(e) => {
            println!("  Callback failed: {e}");
            let input = plain_read_line(
                "  Paste the authorization code or full redirect URL (blank to cancel): ".into(),
            )
            .await?;
            let (code, got_state) = parse_authorization_input(&input);
            if let Some(got_state) = got_state {
                if got_state != state {
                    return Err(anyhow!("OAuth state mismatch"));
                }
            }
            code.ok_or_else(|| anyhow!("missing authorization code"))?
        }
    };
    exchange_authorization_code(client, &code, &verifier, REDIRECT_URI).await
}

#[derive(Debug, Deserialize)]
struct DeviceStartResponse {
    device_auth_id: String,
    user_code: String,
    interval: Value,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

pub async fn login_device_code(client: &reqwest::Client) -> Result<OAuthCredential> {
    let resp = client
        .post(DEVICE_USER_CODE_URL)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "OpenAI Codex device-code request failed ({status}): {}",
            body.trim()
        ));
    }
    let device: DeviceStartResponse = resp.json().await?;
    let interval = device
        .interval
        .as_u64()
        .or_else(|| device.interval.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(5)
        .max(1);
    println!("  Open: {DEVICE_VERIFICATION_URI}");
    println!("  Code: {}", device.user_code);
    println!("  Waiting for approval ...");

    let deadline = now_secs() + 15 * 60;
    loop {
        if now_secs() > deadline {
            return Err(anyhow!("device-code login timed out"));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let resp = client
            .post(DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()
            .await?;
        if resp.status().is_success() {
            let complete: DeviceTokenResponse = resp.json().await?;
            return exchange_authorization_code(
                client,
                &complete.authorization_code,
                &complete.code_verifier,
                DEVICE_REDIRECT_URI,
            )
            .await;
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let pending = status.as_u16() == 403
            || status.as_u16() == 404
            || body.contains("deviceauth_authorization_pending")
            || body.contains("authorization_pending");
        if pending {
            continue;
        }
        if body.contains("slow_down") {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            continue;
        }
        return Err(anyhow!(
            "OpenAI Codex device-code polling failed ({status}): {}",
            body.trim()
        ));
    }
}

pub async fn login_and_save_browser(client: &reqwest::Client) -> Result<PathBuf> {
    let credential = login_browser(client).await?;
    save_oauth(credential)
}

pub async fn login_and_save_device_code(client: &reqwest::Client) -> Result<PathBuf> {
    let credential = login_device_code(client).await?;
    save_oauth(credential)
}

pub async fn access_token(client: &reqwest::Client) -> Result<(String, String)> {
    let store = AuthStore::load();
    let credential = store
        .get_oauth(PROVIDER)
        .cloned()
        .ok_or_else(|| anyhow!("not logged in for openai-codex; run `/login openai-codex`"))?;
    let credential = if credential.expires <= now_secs() + 60 {
        let refreshed = refresh_oauth(client, &credential.refresh).await?;
        let mut store = AuthStore::load();
        store.set_oauth(PROVIDER, refreshed.clone());
        store.save()?;
        refreshed
    } else {
        credential
    };
    let account_id = credential
        .account_id
        .clone()
        .or_else(|| account_id_from_access_token(&credential.access))
        .ok_or_else(|| anyhow!("failed to extract ChatGPT account ID from Codex access token"))?;
    Ok((credential.access, account_id))
}

pub fn account_id_from_access_token(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: Value = serde_json::from_slice(&decoded).ok()?;
    json.get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_url_has_codex_oauth_params() {
        let url = authorization_url("state", "challenge");
        assert!(url.starts_with("https://auth.openai.com"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
    }

    #[test]
    fn parses_redirect_url_input() {
        let (code, state) = parse_authorization_input(
            "http://localhost:1455/auth/callback?code=abc%20123&state=st",
        );
        assert_eq!(code.as_deref(), Some("abc 123"));
        assert_eq!(state.as_deref(), Some("st"));
    }
}
