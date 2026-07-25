use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::{auth_file_path, AuthStore, OAuthCredential};
use crate::input::plain_read_line;

pub const PROVIDER: &str = "grok";
pub const INFERENCE_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
pub const TOKEN_AUTH_HEADER_VALUE: &str = "xai-grok-cli";
pub const CLIENT_VERSION_HEADER: &str = "x-grok-client-version";
pub const CLIENT_VERSION_FALLBACK: &str = "0.2.93";
pub const USER_AGENT_PREFIX: &str = "xai-grok-workspace";
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const ISSUER: &str = "https://auth.x.ai";
const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const REDIRECT_HOST: &str = "127.0.0.1";
const PREFERRED_REDIRECT_PORT: u16 = 20000;
const REDIRECT_PATH: &str = "/callback";
const CALLBACK_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(2);
const CALLBACK_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'";
const GROK_CLI_AUTH_SCOPE_KEY: &str = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";
const GROK_CLI_LEGACY_SCOPE_KEY: &str = "https://accounts.x.ai/sign-in";
/// Curated agent-ready Grok models, matching pi's built-in xAI catalog.
/// Kept static so `/model` does not hit `GET /models` on every open.
pub const GROK_MODEL_LIST: &[&str] = &["grok-4.5", "grok-4.3", "grok-build-0.1"];

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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

#[derive(Debug, Clone)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    device_authorization_endpoint: String,
}

fn validate_xai_endpoint(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url).context("parsing xAI OAuth endpoint")?;
    if parsed.scheme() != "https" {
        return Err(anyhow!(
            "xAI OAuth discovery returned non-https endpoint: {url}"
        ));
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if host != "x.ai" && !host.ends_with(".x.ai") {
        return Err(anyhow!(
            "xAI OAuth discovery returned unexpected host: {url}"
        ));
    }
    Ok(url.to_string())
}

async fn discover(client: &reqwest::Client) -> Result<Discovery> {
    let resp = client
        .get(DISCOVERY_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .context("fetching xAI OIDC discovery")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "xAI OAuth discovery failed ({status}): {}",
            oauth_error_detail(&body)
        ));
    }
    let data: Value = resp.json().await?;
    let auth = data
        .get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("xAI discovery missing authorization_endpoint"))?;
    let token = data
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("xAI discovery missing token_endpoint"))?;
    let device = data
        .get("device_authorization_endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("https://auth.x.ai/oauth2/device/code");
    Ok(Discovery {
        authorization_endpoint: validate_xai_endpoint(auth)?,
        token_endpoint: validate_xai_endpoint(token)?,
        device_authorization_endpoint: validate_xai_endpoint(device)?,
    })
}

fn authorization_url(
    discovery: &Discovery,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    nonce: &str,
) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("nonce", nonce),
    ];
    format!(
        "{}?{}",
        discovery.authorization_endpoint,
        form_urlencoded(&params)
    )
}

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn terminal_safe(input: &str) -> String {
    let mut output = String::new();
    let mut output_chars = 0;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.next_if_eq(&'[').is_some() {
                for sequence_ch in chars.by_ref() {
                    if ('@'..='~').contains(&sequence_ch) {
                        break;
                    }
                }
            } else {
                let _ = chars.next();
            }
            continue;
        }
        if !ch.is_control() && output_chars < 512 {
            output.push(ch);
            output_chars += 1;
        }
    }
    output
}

fn oauth_error_detail(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return "request rejected".into();
    };
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .map(terminal_safe)
        .unwrap_or_else(|| "request rejected".into());
    let description = value
        .get("error_description")
        .and_then(Value::as_str)
        .map(terminal_safe)
        .unwrap_or_default();
    if description.is_empty() {
        error
    } else {
        format!("{error}: {description}")
    }
}

fn callback_html(ok: bool, message: &str) -> String {
    let title = if ok {
        "Grok login complete"
    } else {
        "Grok login failed"
    };
    let message = escape_html(message);
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{}</title><body style=\"font-family: system-ui; margin: 3rem\"><h1>{}</h1><p>{}</p></body>",
        title, title, message
    )
}

fn write_callback_response(mut stream: TcpStream, ok: bool, message: &str, origin: Option<&str>) {
    let body = callback_html(ok, message);
    let status = if ok { "200 OK" } else { "400 Bad Request" };
    let mut headers = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncontent-security-policy: {CALLBACK_CSP}\r\nreferrer-policy: no-referrer\r\nx-content-type-options: nosniff\r\nconnection: close\r\n",
        body.len(),
    );
    if let Some(origin) = origin {
        headers.push_str(&format!(
            "access-control-allow-origin: {origin}\r\naccess-control-allow-methods: GET, OPTIONS\r\naccess-control-allow-headers: Content-Type\r\naccess-control-allow-private-network: true\r\nvary: Origin\r\n"
        ));
    }
    headers.push_str("\r\n");
    headers.push_str(&body);
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

fn cors_origin(headers: &str) -> Option<String> {
    for line in headers.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("Origin:")
            .or_else(|| line.strip_prefix("origin:"))
        {
            let origin = rest.trim();
            if origin == "https://accounts.x.ai" || origin == "https://auth.x.ai" {
                return Some(origin.to_string());
            }
        }
    }
    None
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
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        return Err(anyhow!("no browser open helper on this platform"));
    }
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        cmd.arg(url);
        cmd.spawn().context("opening browser")?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

async fn read_token_response(
    resp: reqwest::Response,
    operation: &str,
    fallback_refresh: Option<&str>,
) -> Result<OAuthCredential> {
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "xAI Grok token {operation} failed ({status}): {}",
            oauth_error_detail(&body)
        ));
    }
    let token: TokenResponse = resp.json().await?;
    let refresh = token
        .refresh_token
        .filter(|s| !s.is_empty())
        .or_else(|| fallback_refresh.map(str::to_string))
        .ok_or_else(|| anyhow!("xAI token response missing refresh_token"))?;
    Ok(OAuthCredential {
        credential_type: "oauth".into(),
        access: token.access_token,
        refresh,
        expires: now_secs() + token.expires_in.unwrap_or(3600),
        account_id: None,
    })
}

async fn exchange_authorization_code(
    client: &reqwest::Client,
    token_endpoint: &str,
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
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await?;
    read_token_response(resp, "exchange", None).await
}

pub async fn refresh_oauth(
    client: &reqwest::Client,
    refresh: &str,
    token_endpoint: Option<&str>,
) -> Result<OAuthCredential> {
    let endpoint = match token_endpoint {
        Some(url) => validate_xai_endpoint(url)?,
        None => discover(client).await?.token_endpoint,
    };
    let body = form_urlencoded(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", CLIENT_ID),
    ]);
    let resp = client
        .post(endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await?;
    read_token_response(resp, "refresh", Some(refresh)).await
}

fn save_oauth(credential: OAuthCredential) -> Result<PathBuf> {
    let mut store = AuthStore::load();
    store.set_oauth(PROVIDER, credential);
    store.save()?;
    let _ = capture_and_cache_client_version();
    auth_file_path().context("no auth file path")
}

fn client_version_cache_path() -> Option<PathBuf> {
    auth_file_path().map(|path| path.with_file_name("grok-client-version"))
}

fn grok_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok"))
}

fn normalize_client_version(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"(\d+\.\d+\.\d+)").expect("client version regex"));
    re.captures(trimmed)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

fn read_cached_client_version() -> Option<String> {
    let path = client_version_cache_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    normalize_client_version(&text)
}

fn write_cached_client_version(version: &str) -> Result<()> {
    let path = client_version_cache_path().context("no client version cache path")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("{version}\n"))?;
    Ok(())
}

fn read_grok_cli_version_json() -> Option<String> {
    let path = grok_home_dir()?.join("version.json");
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .or_else(|| value.get("stable_version").and_then(Value::as_str))?;
    normalize_client_version(version)
}

pub fn client_version() -> String {
    let version = read_cached_client_version()
        .or_else(read_grok_cli_version_json)
        .unwrap_or_else(|| CLIENT_VERSION_FALLBACK.to_string());
    let _ = write_cached_client_version(&version);
    version
}

pub fn capture_and_cache_client_version() -> String {
    client_version()
}

pub fn user_agent() -> String {
    format!("{USER_AGENT_PREFIX}/{}", client_version())
}

fn bind_callback_port() -> Result<(TcpListener, u16)> {
    match TcpListener::bind((REDIRECT_HOST, PREFERRED_REDIRECT_PORT)) {
        Ok(listener) => Ok((listener, PREFERRED_REDIRECT_PORT)),
        Err(_) => {
            let listener = TcpListener::bind((REDIRECT_HOST, 0))
                .context("binding ephemeral OAuth callback port")?;
            let port = listener.local_addr()?.port();
            Ok((listener, port))
        }
    }
}

fn wait_for_browser_callback_with_listener(listener: TcpListener, state: String) -> Result<String> {
    wait_for_browser_callback_with_timeout(listener, state, CALLBACK_WAIT_TIMEOUT)
}

fn wait_for_browser_callback_with_timeout(
    listener: TcpListener,
    state: String,
    timeout: Duration,
) -> Result<String> {
    listener
        .set_nonblocking(true)
        .context("configuring OAuth callback listener")?;
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!("OAuth callback timed out"));
        }
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(e) => return Err(e).context("waiting for OAuth callback"),
        };
        let _ = stream.set_read_timeout(Some(CALLBACK_READ_TIMEOUT));
        let mut buf = [0u8; 8192];
        let n = match stream.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(_) => continue,
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let first = request.lines().next().unwrap_or_default();
        let method = first.split_whitespace().next().unwrap_or_default();
        let path = first.split_whitespace().nth(1).unwrap_or_default();
        let origin = cors_origin(&request);

        if method.eq_ignore_ascii_case("OPTIONS") {
            let mut headers = "HTTP/1.1 204 No Content\r\nconnection: close\r\n".to_string();
            if let Some(ref o) = origin {
                headers.push_str(&format!(
                    "access-control-allow-origin: {o}\r\naccess-control-allow-methods: GET, OPTIONS\r\naccess-control-allow-headers: Content-Type\r\naccess-control-allow-private-network: true\r\nvary: Origin\r\n"
                ));
            }
            headers.push_str("\r\n");
            let _ = stream.write_all(headers.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
            continue;
        }

        if !method.eq_ignore_ascii_case("GET") {
            write_callback_response(stream, false, "Method not allowed.", origin.as_deref());
            continue;
        }
        let route = path.split_once('?').map(|(route, _)| route).unwrap_or(path);
        if route != REDIRECT_PATH {
            write_callback_response(
                stream,
                false,
                "Callback route not found.",
                origin.as_deref(),
            );
            continue;
        }
        let got_state = query_param(path, "state");
        if got_state.as_deref() != Some(state.as_str()) {
            write_callback_response(stream, false, "State mismatch.", origin.as_deref());
            continue;
        }
        if let Some(err) = query_param(path, "error") {
            let err = terminal_safe(&err);
            let desc = terminal_safe(
                query_param(path, "error_description")
                    .unwrap_or_default()
                    .as_str(),
            );
            let detail = if desc.is_empty() {
                err
            } else {
                format!("{err}: {desc}")
            };
            write_callback_response(
                stream,
                false,
                &format!("Authorization failed: {detail}"),
                origin.as_deref(),
            );
            return Err(anyhow!("xAI authorization failed: {detail}"));
        }
        let Some(code) = query_param(path, "code") else {
            write_callback_response(
                stream,
                false,
                "Authorization code missing.",
                origin.as_deref(),
            );
            continue;
        };
        write_callback_response(
            stream,
            true,
            "Grok authentication completed. You can close this window.",
            origin.as_deref(),
        );
        return Ok(code);
    }
}

fn parse_expiry_ms(value: &Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return Some(if n > 10_000_000_000 { n / 1000 } else { n });
    }
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.parse::<u64>() {
            return Some(if n > 10_000_000_000 { n / 1000 } else { n });
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return Some(dt.timestamp().max(0) as u64);
        }
    }
    None
}

pub fn load_grok_cli_credentials() -> Option<OAuthCredential> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".grok").join("auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;

    let try_entry = |entry: &Value| -> Option<OAuthCredential> {
        let access = entry
            .get("key")
            .or_else(|| entry.get("access_token"))
            .or_else(|| entry.get("token"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())?
            .to_string();
        let refresh = entry
            .get("refresh_token")
            .or_else(|| entry.get("refresh"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let expires = entry
            .get("expires_at")
            .or_else(|| entry.get("expires"))
            .and_then(parse_expiry_ms)
            .unwrap_or_else(|| now_secs() + 3600);
        Some(OAuthCredential {
            credential_type: "oauth".into(),
            access,
            refresh,
            expires,
            account_id: entry
                .get("user_id")
                .or_else(|| entry.get("principal_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
        })
    };

    if let Some(entry) = data.get(GROK_CLI_AUTH_SCOPE_KEY) {
        if let Some(cred) = try_entry(entry) {
            return Some(cred);
        }
    }
    if let Some(entry) = data.get(GROK_CLI_LEGACY_SCOPE_KEY) {
        if let Some(cred) = try_entry(entry) {
            return Some(cred);
        }
    }
    try_entry(&data)
}

async fn maybe_import_grok_cli(client: &reqwest::Client) -> Result<Option<OAuthCredential>> {
    let Some(existing) = load_grok_cli_credentials() else {
        return Ok(None);
    };
    println!("  Found existing Grok CLI credentials in ~/.grok/auth.json.");
    let pick = plain_read_line("  Use them instead of a new OAuth login? [Y/n]: ".into()).await?;
    let trimmed = pick.trim().to_lowercase();
    if trimmed.is_empty() || trimmed == "y" || trimmed == "yes" {
        if existing.expires <= now_secs() + 60 {
            if existing.refresh.is_empty() {
                println!("  Stored Grok CLI token is expired and has no refresh token.");
                return Ok(None);
            }
            match refresh_oauth(
                client,
                &existing.refresh,
                Some(&format!("{ISSUER}/oauth2/token")),
            )
            .await
            {
                Ok(refreshed) => return Ok(Some(refreshed)),
                Err(e) => {
                    println!(
                        "  Could not refresh Grok CLI credentials ({e}); starting fresh login."
                    );
                    return Ok(None);
                }
            }
        }
        return Ok(Some(existing));
    }
    Ok(None)
}

pub async fn login_browser(client: &reqwest::Client) -> Result<OAuthCredential> {
    if let Some(imported) = maybe_import_grok_cli(client).await? {
        return Ok(imported);
    }

    let discovery = discover(client).await?;
    let (verifier, challenge) = pkce_pair();
    let state = random_hex(16);
    let nonce = random_hex(16);
    let (listener, port) = bind_callback_port()?;
    let redirect_uri = format!("http://{REDIRECT_HOST}:{port}{REDIRECT_PATH}");
    let url = authorization_url(&discovery, &redirect_uri, &challenge, &state, &nonce);

    println!("  Open this URL to sign in with Grok / SuperGrok:\n\n  {url}\n");
    if let Err(e) = open_browser(&url) {
        println!("  Browser did not open automatically: {e}");
    }
    println!("  Waiting for callback on {redirect_uri} ...");
    println!(
        "  (If no callback arrives within 60 seconds, you can paste the redirect URL or code.)"
    );

    let state_for_thread = state.clone();
    let callback = tokio::task::spawn_blocking(move || {
        wait_for_browser_callback_with_listener(listener, state_for_thread)
    })
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

    exchange_authorization_code(
        client,
        &discovery.token_endpoint,
        &code,
        &verifier,
        &redirect_uri,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct DeviceStartResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

pub async fn login_device_code(client: &reqwest::Client) -> Result<OAuthCredential> {
    if let Some(imported) = maybe_import_grok_cli(client).await? {
        return Ok(imported);
    }

    let discovery = discover(client).await?;
    let body = form_urlencoded(&[("client_id", CLIENT_ID), ("scope", SCOPE)]);
    let resp = client
        .post(&discovery.device_authorization_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "xAI device-code request failed ({status}): {}",
            oauth_error_detail(&body)
        ));
    }
    let device: DeviceStartResponse = resp.json().await?;
    let interval = device.interval.unwrap_or(5).max(1);
    let expires_in = device.expires_in.unwrap_or(1800);
    let open_url = device
        .verification_uri_complete
        .as_deref()
        .unwrap_or(device.verification_uri.as_str());

    println!("  Open: {open_url}");
    println!("  Code: {}", device.user_code);
    if let Err(e) = open_browser(open_url) {
        println!("  Browser did not open automatically: {e}");
        println!(
            "  Open {} and enter code {}",
            device.verification_uri, device.user_code
        );
    }
    println!("  Waiting for approval ...");

    let deadline = now_secs() + expires_in;
    let mut sleep_secs = interval;
    loop {
        if now_secs() > deadline {
            return Err(anyhow!("device-code login timed out"));
        }
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
        let body = form_urlencoded(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", &device.device_code),
            ("client_id", CLIENT_ID),
        ]);
        let resp = client
            .post(&discovery.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await?;
        if resp.status().is_success() {
            return read_token_response(resp, "device", None).await;
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let err = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_default();
        match err.as_str() {
            "authorization_pending" | "" if status.as_u16() == 400 || status.as_u16() == 403 => {
                continue;
            }
            "slow_down" => {
                sleep_secs = sleep_secs.saturating_add(5);
                continue;
            }
            "expired_token" | "access_denied" => {
                return Err(anyhow!(
                    "xAI device-code login failed ({err}): {}",
                    oauth_error_detail(&body)
                ));
            }
            _ => {
                if body.contains("authorization_pending") {
                    continue;
                }
                return Err(anyhow!(
                    "xAI device-code polling failed ({status}): {}",
                    oauth_error_detail(&body)
                ));
            }
        }
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

pub async fn access_token(client: &reqwest::Client) -> Result<String> {
    let store = AuthStore::load();
    let credential = store
        .get_oauth(PROVIDER)
        .cloned()
        .ok_or_else(|| anyhow!("not logged in for grok; run `/login grok`"))?;
    let credential = if credential.expires <= now_secs() + 60 {
        if credential.refresh.is_empty() {
            return Err(anyhow!(
                "Grok OAuth token expired with no refresh token; run `/login grok`"
            ));
        }
        let refreshed = refresh_oauth(
            client,
            &credential.refresh,
            Some(&format!("{ISSUER}/oauth2/token")),
        )
        .await?;
        let mut store = AuthStore::load();
        store.set_oauth(PROVIDER, refreshed.clone());
        store.save()?;
        refreshed
    } else {
        credential
    };
    Ok(credential.access)
}

pub fn grok_model_list() -> Vec<String> {
    GROK_MODEL_LIST.iter().map(|s| (*s).to_string()).collect()
}

/// Canonical SuperGrok/xAI OAuth model ids from pi's current xAI catalog.
/// Accept a few shorthand / provider-prefixed aliases, but never send those
/// aliases over the wire.
pub fn canonical_grok_model(model: &str) -> Option<&'static str> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let bare = lower
        .rsplit_once('/')
        .map(|(_, id)| id)
        .unwrap_or(lower.as_str());
    match bare {
        "grok-4.5" | "grok-4.5-latest" | "4.5" => Some("grok-4.5"),
        "grok-4.3" | "grok-4.3-latest" | "grok-latest" | "4.3" => Some("grok-4.3"),
        "grok-build-0.1"
        | "grok-build-latest"
        | "grok-code-fast-1"
        | "grok-code-fast"
        | "grok-code-fast-1-0825"
        | "build-0.1" => Some("grok-build-0.1"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send_callback(port: u16, target: &str) {
        let mut stream = TcpStream::connect((REDIRECT_HOST, port)).unwrap();
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: {REDIRECT_HOST}:{port}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let _ = stream.shutdown(Shutdown::Write);
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
    }

    #[test]
    fn authorization_url_has_xai_oauth_params() {
        let discovery = Discovery {
            authorization_endpoint: "https://auth.x.ai/oauth2/authorize".into(),
            token_endpoint: "https://auth.x.ai/oauth2/token".into(),
            device_authorization_endpoint: "https://auth.x.ai/oauth2/device/code".into(),
        };
        let url = authorization_url(
            &discovery,
            "http://127.0.0.1:20000/callback",
            "challenge",
            "state",
            "nonce",
        );
        assert!(url.starts_with("https://auth.x.ai/oauth2/authorize?"));
        assert!(url.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope="));
        assert!(url.contains("nonce=nonce"));
    }

    #[test]
    fn parses_redirect_url_input() {
        let (code, state) =
            parse_authorization_input("http://127.0.0.1:20000/callback?code=abc%20123&state=st");
        assert_eq!(code.as_deref(), Some("abc 123"));
        assert_eq!(state.as_deref(), Some("st"));
    }

    #[test]
    fn callback_ignores_untrusted_requests_until_state_matches() {
        let listener = TcpListener::bind((REDIRECT_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            wait_for_browser_callback_with_timeout(
                listener,
                "expected-state".into(),
                Duration::from_secs(2),
            )
        });

        send_callback(port, "/callback-evil?code=bad&state=expected-state");
        send_callback(
            port,
            "/callback?error=denied&error_description=%3Cscript%3Ebad%3C%2Fscript%3E&state=wrong-state",
        );
        send_callback(port, "/callback?code=good-code&state=expected-state");
        assert_eq!(server.join().unwrap().unwrap(), "good-code");
    }

    #[test]
    fn callback_escapes_provider_errors_and_restricts_content() {
        let html = callback_html(false, "Authorization failed: <script>alert(1)</script>");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(CALLBACK_CSP.contains("default-src 'none'"));

        let listener = TcpListener::bind((REDIRECT_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            wait_for_browser_callback_with_timeout(
                listener,
                "expected-state".into(),
                Duration::from_secs(2),
            )
        });

        send_callback(
            port,
            "/callback?error=denied&error_description=%3Cscript%3Ealert(1)%3C%2Fscript%3E&state=expected-state",
        );
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn callback_wait_has_a_deadline() {
        let listener = TcpListener::bind((REDIRECT_HOST, 0)).unwrap();
        let started = Instant::now();
        let result = wait_for_browser_callback_with_timeout(
            listener,
            "state".into(),
            Duration::from_millis(40),
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn oauth_errors_do_not_echo_unstructured_response_bodies() {
        assert_eq!(
            oauth_error_detail("gateway exploded with a token"),
            "request rejected"
        );
        assert_eq!(
            oauth_error_detail(r#"{"error":"access_denied","error_description":"no\u001b[31mpe"}"#,),
            "access_denied: nope"
        );
    }

    #[test]
    fn validates_xai_hosts_only() {
        assert!(validate_xai_endpoint("https://auth.x.ai/oauth2/token").is_ok());
        assert!(validate_xai_endpoint("https://evil.example/oauth2/token").is_err());
        assert!(validate_xai_endpoint("http://auth.x.ai/oauth2/token").is_err());
    }

    #[test]
    fn canonical_model_accepts_pi_catalog_and_aliases() {
        assert_eq!(canonical_grok_model("grok-4.5"), Some("grok-4.5"));
        assert_eq!(canonical_grok_model("xai/grok-4.5"), Some("grok-4.5"));
        assert_eq!(canonical_grok_model("4.3"), Some("grok-4.3"));
        assert_eq!(
            canonical_grok_model("grok-code-fast-1"),
            Some("grok-build-0.1")
        );
        assert_eq!(canonical_grok_model("grok-4.20-0309-reasoning"), None);
        assert_eq!(canonical_grok_model("future-model"), None);
        assert_eq!(canonical_grok_model(""), None);
        assert_eq!(
            grok_model_list(),
            vec![
                "grok-4.5".to_string(),
                "grok-4.3".to_string(),
                "grok-build-0.1".to_string(),
            ]
        );
    }

    #[test]
    fn parse_expiry_accepts_rfc3339() {
        let v = Value::String("2026-07-15T18:03:10.175Z".into());
        let secs = parse_expiry_ms(&v).unwrap();
        assert!(secs > 1_700_000_000);
    }

    #[test]
    fn normalize_client_version_extracts_semver() {
        assert_eq!(
            normalize_client_version("grok 0.2.106 (bde89716f679) [stable]").as_deref(),
            Some("0.2.106")
        );
        assert_eq!(
            normalize_client_version("0.2.93").as_deref(),
            Some("0.2.93")
        );
        assert_eq!(normalize_client_version("  ").as_deref(), None);
        assert_eq!(normalize_client_version("no-version-here").as_deref(), None);
    }

    #[test]
    fn user_agent_matches_cli_proxy_convention() {
        let version = client_version();
        assert_eq!(user_agent(), format!("xai-grok-workspace/{version}"));
        assert!(
            version.split('.').count() == 3,
            "expected semver-like client version, got {version}"
        );
    }

    #[test]
    fn client_version_fallback_is_current_proxy_default() {
        assert_eq!(CLIENT_VERSION_FALLBACK, "0.2.93");
    }
}
