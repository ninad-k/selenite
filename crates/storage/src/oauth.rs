//! OAuth 2.0 authorization-code flow with PKCE (RFC 7636, `S256`) and a loopback redirect
//! (RFC 8252) for installed apps, plus token storage.
//!
//! Console requirements (the project owns these app registrations, see ADR-004):
//!
//! * **Google** (`GoogleDrive`): create an OAuth client of type *Desktop app*. No redirect URI needs
//!   registering; Google accepts any `http://127.0.0.1:<port>` loopback redirect for Desktop
//!   clients. Scope `https://www.googleapis.com/auth/drive.file` (non-sensitive: only files the app
//!   created). The Desktop client secret is not confidential and is sent at the token endpoint.
//! * **Microsoft** (`OneDrive`): register an app with the *Mobile and desktop applications*
//!   platform, redirect URI exactly `http://localhost` (Microsoft ignores the port for localhost
//!   loopback, and its portal does not accept `127.0.0.1`), "Allow public client flows" = yes, and
//!   supported accounts "personal Microsoft accounts" (or "any org and personal"). Delegated
//!   permissions `Files.ReadWrite.AppFolder` and `offline_access`. No client secret.
//!
//! The listener always binds `127.0.0.1` on an ephemeral port; only the host shown in the redirect
//! URI differs (`OAuthConfig::redirect_host`). Codes, verifiers, tokens and secrets are never
//! logged and `Debug` output redacts them.

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::util::{form_encode, http_client, lock, net_err, now_secs};
use crate::{Result, StorageError};

const REDACTED: &str = "<redacted>";

/// Static OAuth client configuration for one provider.
#[derive(Clone)]
pub struct OAuthConfig {
    /// Authorization endpoint (opened in the browser).
    pub auth_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Public client id.
    pub client_id: String,
    /// Client secret for providers that issue one to installed apps (Google Desktop clients). It is
    /// not confidential but is still never logged.
    pub client_secret: Option<String>,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Extra authorization-request parameters (e.g. `access_type=offline`).
    pub extra_auth_params: Vec<(String, String)>,
    /// Host shown in the redirect URI: `127.0.0.1` (Google) or `localhost` (Microsoft).
    pub redirect_host: String,
}

impl fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("auth_url", &self.auth_url)
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| REDACTED),
            )
            .field("scopes", &self.scopes)
            .field("redirect_host", &self.redirect_host)
            .finish_non_exhaustive()
    }
}

impl OAuthConfig {
    /// Google Drive, least-privilege `drive.file` scope, offline access (refresh token).
    pub fn google_drive(client_id: &str, client_secret: &str) -> Self {
        Self {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            client_id: client_id.into(),
            client_secret: Some(client_secret.into()),
            scopes: vec!["https://www.googleapis.com/auth/drive.file".into()],
            extra_auth_params: vec![
                ("access_type".into(), "offline".into()),
                ("prompt".into(), "consent".into()),
            ],
            redirect_host: "127.0.0.1".into(),
        }
    }

    /// OneDrive via Microsoft identity platform v2, app-folder scope, public client (no secret).
    pub fn onedrive(client_id: &str) -> Self {
        Self {
            auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
            token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
            client_id: client_id.into(),
            client_secret: None,
            scopes: vec!["Files.ReadWrite.AppFolder".into(), "offline_access".into()],
            extra_auth_params: Vec::new(),
            redirect_host: "localhost".into(),
        }
    }
}

/// An access/refresh token pair. `Debug` redacts both tokens.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    /// Short-lived bearer token (kept in memory only).
    pub access_token: String,
    /// Long-lived refresh token (persisted via a [`TokenStore`]).
    pub refresh_token: String,
    /// Unix seconds at which the access token expires.
    pub expires_at: i64,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &REDACTED)
            .field("refresh_token", &REDACTED)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

// ---------------------------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------------------------

/// `S256` code challenge for a verifier: `BASE64URL(SHA256(verifier))` without padding.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(buf)
}

fn build_auth_url(
    cfg: &OAuthConfig,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&cfg.auth_url)
        .map_err(|e| StorageError::Internal(format!("auth: bad authorization url: {e}")))?;
    {
        let scope = cfg.scopes.join(" ");
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &cfg.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &scope)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        for (k, v) in &cfg.extra_auth_params {
            q.append_pair(k, v);
        }
    }
    Ok(url)
}

// ---------------------------------------------------------------------------------------------
// Authorization flow
// ---------------------------------------------------------------------------------------------

/// Run the interactive sign-in: open the browser, wait for the loopback redirect, exchange the
/// code. `open_browser` receives the full authorization URL (see [`default_open_browser`]).
pub fn authorize(
    cfg: &OAuthConfig,
    open_browser: &dyn Fn(&str) -> std::result::Result<(), String>,
    timeout: Duration,
) -> Result<Tokens> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| StorageError::Io {
        path: "<loopback listener>".into(),
        source: e,
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| StorageError::Io {
            path: "<loopback listener>".into(),
            source: e,
        })?
        .port();
    let redirect_uri = format!("http://{}:{port}", cfg.redirect_host);
    let state = random_b64(32);
    let verifier = random_b64(64);
    let url = build_auth_url(cfg, &redirect_uri, &state, &pkce_challenge(&verifier))?;
    open_browser(url.as_str())
        .map_err(|e| StorageError::Internal(format!("auth: could not open browser: {e}")))?;
    let code = wait_for_code(&listener, &state, timeout)?;
    token_request(
        cfg,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
        ],
        None,
    )
}

const PAGE_OK: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Signed in</title></head><body style=\"font-family:sans-serif;margin:3em\"><h1>You can close this tab</h1><p>Sign-in finished. Return to the app to continue.</p></body></html>";
const PAGE_ERR: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Sign-in failed</title></head><body style=\"font-family:sans-serif;margin:3em\"><h1>Sign-in did not complete</h1><p>You can close this tab and try again from the app.</p></body></html>";

fn wait_for_code(listener: &TcpListener, state: &str, timeout: Duration) -> Result<String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| StorageError::Io {
            path: "<loopback listener>".into(),
            source: e,
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = handle_redirect(stream, state)? {
                    return Ok(code);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(StorageError::Internal(
                        "auth: timed out waiting for browser sign-in".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return Err(StorageError::Io {
                    path: "<loopback listener>".into(),
                    source: e,
                });
            }
        }
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Best effort: the browser may already have gone away.
    let _ = stream.write_all(msg.as_bytes());
    let _ = stream.flush();
}

/// Handle one request on the loopback listener. `Ok(Some(code))` on success, `Ok(None)` for an
/// unrelated request (favicon, probes), `Err` for a provider error or state mismatch.
fn handle_redirect(mut stream: TcpStream, expected_state: &str) -> Result<Option<String>> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while head.len() < 8192 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&head);
    let target = text
        .lines()
        .next()
        .and_then(|l| {
            let mut it = l.split_whitespace();
            (it.next() == Some("GET")).then(|| it.next())?
        })
        .unwrap_or("");
    let parsed = reqwest::Url::parse(&format!("http://loopback.invalid{target}")).ok();
    let (mut code, mut state, mut error) = (None, None, None);
    if let Some(u) = &parsed {
        for (k, v) in u.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                _ => {}
            }
        }
    }
    if let Some(err) = error {
        respond(&mut stream, "400 Bad Request", PAGE_ERR);
        // The error code is a short token such as `access_denied`; cap its length defensively.
        let err: String = err.chars().take(64).collect();
        return Err(StorageError::Internal(format!(
            "auth: provider returned error: {err}"
        )));
    }
    if code.is_none() && state.is_none() {
        respond(&mut stream, "404 Not Found", PAGE_ERR);
        return Ok(None);
    }
    if state.as_deref() != Some(expected_state) {
        respond(&mut stream, "400 Bad Request", PAGE_ERR);
        return Err(StorageError::Internal("auth: state mismatch".into()));
    }
    match code {
        Some(c) if !c.is_empty() => {
            respond(&mut stream, "200 OK", PAGE_OK);
            Ok(Some(c))
        }
        _ => {
            respond(&mut stream, "400 Bad Request", PAGE_ERR);
            Err(StorageError::Internal(
                "auth: redirect carried no code".into(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
}

fn token_request(
    cfg: &OAuthConfig,
    params: &[(&str, &str)],
    previous_refresh: Option<&str>,
) -> Result<Tokens> {
    let mut all: Vec<(&str, &str)> = params.to_vec();
    all.push(("client_id", &cfg.client_id));
    if let Some(secret) = &cfg.client_secret {
        all.push(("client_secret", secret));
    }
    let body = form_encode(&all);
    let resp = http_client()?
        .post(&cfg.token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .map_err(net_err)?;
    let status = resp.status();
    let parsed: TokenResponse = resp.json().map_err(|_| {
        StorageError::Internal(format!(
            "auth: token endpoint HTTP {status}, unreadable body"
        ))
    })?;
    if !status.is_success() {
        let code: String = parsed.error.unwrap_or_default().chars().take(64).collect();
        return Err(StorageError::Internal(format!(
            "auth: token endpoint HTTP {}: {code}",
            status.as_u16()
        )));
    }
    let access_token = parsed
        .access_token
        .ok_or_else(|| StorageError::Internal("auth: token response had no access token".into()))?;
    let refresh_token = parsed
        .refresh_token
        .or_else(|| previous_refresh.map(str::to_string))
        .ok_or_else(|| {
            StorageError::Internal(
                "auth: no refresh token returned (offline access was not granted)".into(),
            )
        })?;
    Ok(Tokens {
        access_token,
        refresh_token,
        expires_at: now_secs() + parsed.expires_in.unwrap_or(3600),
    })
}

/// Exchange a refresh token for a fresh access token. If the server does not rotate the refresh
/// token, the old one is kept in the result.
pub fn refresh(cfg: &OAuthConfig, refresh_token: &str) -> Result<Tokens> {
    token_request(
        cfg,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
        Some(refresh_token),
    )
}

/// Open a URL in the system browser without going through a shell.
pub fn default_open_browser(url: &str) -> std::result::Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refusing to open a non-http(s) URL".into());
    }
    let mut cmd;
    #[cfg(target_os = "windows")]
    {
        cmd = std::process::Command::new("rundll32");
        cmd.arg("url.dll,FileProtocolHandler").arg(url);
    }
    #[cfg(target_os = "macos")]
    {
        cmd = std::process::Command::new("open");
        cmd.arg(url);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------------------------

/// Persistent store for the refresh token (never the vault, never logs).
pub trait TokenStore: Send + Sync {
    /// The stored refresh token, if any.
    fn load(&self) -> Result<Option<String>>;
    /// Store (replace) the refresh token.
    fn save(&self, refresh_token: &str) -> Result<()>;
    /// Remove the stored token (sign out).
    fn clear(&self) -> Result<()>;
}

/// Refresh token in the OS keychain (Windows Credential Manager, macOS Keychain, Secret Service).
#[derive(Debug, Clone)]
pub struct KeyringTokenStore {
    service: String,
    account: String,
}

impl KeyringTokenStore {
    /// `service` should be app-specific per provider (e.g. `"<APP_NAME>-google"`), `account`
    /// identifies the vault or user.
    pub fn new(service: &str, account: &str) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| StorageError::Internal(format!("keychain: {e}")))
    }
}

impl TokenStore for KeyringTokenStore {
    fn load(&self) -> Result<Option<String>> {
        match self.entry()?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StorageError::Internal(format!("keychain: {e}"))),
        }
    }

    fn save(&self, refresh_token: &str) -> Result<()> {
        self.entry()?
            .set_password(refresh_token)
            .map_err(|e| StorageError::Internal(format!("keychain: {e}")))
    }

    fn clear(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StorageError::Internal(format!("keychain: {e}"))),
        }
    }
}

/// In-memory [`TokenStore`] for tests.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    token: Mutex<Option<String>>,
}

impl MemoryTokenStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> Result<Option<String>> {
        Ok(lock(&self.token)?.clone())
    }

    fn save(&self, refresh_token: &str) -> Result<()> {
        *lock(&self.token)? = Some(refresh_token.to_string());
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        *lock(&self.token)? = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Authenticator: bearer tokens for providers
// ---------------------------------------------------------------------------------------------

/// Supplies bearer tokens to a provider: keeps the access token in memory, refreshes it from the
/// stored refresh token when it expires or after a `401`, and persists rotated refresh tokens.
pub struct Authenticator {
    cfg: OAuthConfig,
    store: Arc<dyn TokenStore>,
    access: Mutex<Option<Tokens>>,
}

impl fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authenticator").finish_non_exhaustive()
    }
}

impl Authenticator {
    /// Use the refresh token already in `store` (access token is fetched on first use).
    pub fn new(cfg: OAuthConfig, store: Arc<dyn TokenStore>) -> Self {
        Self {
            cfg,
            store,
            access: Mutex::new(None),
        }
    }

    /// Adopt freshly obtained tokens (for example from [`authorize`]) and persist the refresh token.
    pub fn with_tokens(
        cfg: OAuthConfig,
        store: Arc<dyn TokenStore>,
        tokens: Tokens,
    ) -> Result<Self> {
        store.save(&tokens.refresh_token)?;
        Ok(Self {
            cfg,
            store,
            access: Mutex::new(Some(tokens)),
        })
    }

    /// A valid access token, refreshing if needed.
    pub fn access_token(&self) -> Result<String> {
        let mut guard = lock(&self.access)?;
        if let Some(t) = guard.as_ref()
            && t.expires_at > now_secs() + 60
        {
            return Ok(t.access_token.clone());
        }
        let t = self.refresh_locked()?;
        let out = t.access_token.clone();
        *guard = Some(t);
        Ok(out)
    }

    fn refresh_locked(&self) -> Result<Tokens> {
        let rt = self
            .store
            .load()?
            .ok_or_else(|| StorageError::Internal("auth: not signed in".into()))?;
        let t = refresh(&self.cfg, &rt)?;
        if t.refresh_token != rt {
            self.store.save(&t.refresh_token)?;
        }
        Ok(t)
    }

    /// Force a refresh after the server rejected `stale`; if another caller already replaced it,
    /// reuse the newer token.
    fn refresh_after_401(&self, stale: &str) -> Result<String> {
        let mut guard = lock(&self.access)?;
        if let Some(t) = guard.as_ref()
            && t.access_token != stale
        {
            return Ok(t.access_token.clone());
        }
        let t = self.refresh_locked()?;
        let out = t.access_token.clone();
        *guard = Some(t);
        Ok(out)
    }

    /// Send an authenticated request; on `401` refresh once and retry. The status of the final
    /// response is returned to the caller unchecked.
    pub(crate) fn send(
        &self,
        build: &dyn Fn(&str) -> reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response> {
        let token = self.access_token()?;
        let resp = build(&token).send().map_err(net_err)?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let fresh = self.refresh_after_401(&token)?;
        build(&fresh).send().map_err(net_err)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::testutil::{MockServer, Resp, query_of};

    #[test]
    fn pkce_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_values_have_expected_shape() {
        let v = random_b64(64);
        assert_eq!(v.len(), 86);
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(random_b64(32), random_b64(32));
    }

    #[test]
    fn debug_redacts_secrets() {
        let t = Tokens {
            access_token: "ACCESS-SECRET-123".into(),
            refresh_token: "REFRESH-SECRET-456".into(),
            expires_at: 5,
        };
        let s = format!("{t:?}");
        assert!(!s.contains("SECRET"), "{s}");
        let cfg = OAuthConfig::google_drive("cid", "CLIENT-SECRET-789");
        let s = format!("{cfg:?}");
        assert!(!s.contains("SECRET"), "{s}");
    }

    #[test]
    fn constructors_have_expected_scopes() {
        let g = OAuthConfig::google_drive("a", "b");
        assert_eq!(g.scopes, ["https://www.googleapis.com/auth/drive.file"]);
        assert!(
            g.extra_auth_params
                .contains(&("prompt".into(), "consent".into()))
        );
        let m = OAuthConfig::onedrive("a");
        assert_eq!(m.scopes, ["Files.ReadWrite.AppFolder", "offline_access"]);
        assert!(m.client_secret.is_none());
    }

    fn send_request(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    fn listener() -> (TcpListener, u16) {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        (l, p)
    }

    #[test]
    fn redirect_accepts_matching_state() {
        let (l, port) = listener();
        let h = std::thread::spawn(move || wait_for_code(&l, "st", Duration::from_secs(5)));
        // An unrelated probe first must not end the wait.
        assert!(send_request(port, "/favicon.ico").contains("404"));
        let page = send_request(port, "/?code=abc%2F1&state=st");
        assert!(page.contains("You can close this tab"));
        assert_eq!(h.join().unwrap().unwrap(), "abc/1");
    }

    #[test]
    fn redirect_rejects_state_mismatch() {
        let (l, port) = listener();
        let h = std::thread::spawn(move || wait_for_code(&l, "good", Duration::from_secs(5)));
        let page = send_request(port, "/?code=abc&state=evil");
        assert!(page.contains("400"));
        let err = h.join().unwrap().unwrap_err();
        assert!(err.to_string().contains("state mismatch"), "{err}");
    }

    #[test]
    fn redirect_surfaces_error_param() {
        let (l, port) = listener();
        let h = std::thread::spawn(move || wait_for_code(&l, "s", Duration::from_secs(5)));
        send_request(port, "/?error=access_denied&state=s");
        let err = h.join().unwrap().unwrap_err();
        assert!(err.to_string().contains("access_denied"), "{err}");
    }

    #[test]
    fn redirect_times_out() {
        let (l, _) = listener();
        let err = wait_for_code(&l, "s", Duration::from_millis(100)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    fn token_server() -> MockServer {
        MockServer::start(|req| {
            if req.path != "/token" {
                return Resp::status(404);
            }
            let form = query_of(&String::from_utf8_lossy(&req.body));
            let get = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
            match get("grant_type").as_deref() {
                Some("authorization_code") => {
                    if get("code").as_deref() == Some("the-code")
                        && get("code_verifier").is_some_and(|v| v.len() == 86)
                        && get("client_secret").as_deref() == Some("sec")
                    {
                        Resp::json(
                            200,
                            r#"{"access_token":"a1","refresh_token":"r1","expires_in":3600}"#,
                        )
                    } else {
                        Resp::json(400, r#"{"error":"invalid_grant"}"#)
                    }
                }
                Some("refresh_token") if get("refresh_token").as_deref() == Some("r1") => {
                    Resp::json(200, r#"{"access_token":"a2","expires_in":3600}"#)
                }
                _ => Resp::json(400, r#"{"error":"invalid_grant"}"#),
            }
        })
    }

    #[test]
    fn full_flow_with_simulated_browser() {
        let srv = token_server();
        let mut cfg = OAuthConfig::google_drive("cid", "sec");
        cfg.token_url = format!("{}/token", srv.url());
        cfg.auth_url = "http://auth.invalid/authorize".into();
        let open = |url: &str| -> std::result::Result<(), String> {
            let u = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
            let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
            assert_eq!(q["code_challenge_method"], "S256");
            assert_eq!(q["response_type"], "code");
            assert_eq!(q["access_type"], "offline");
            assert_eq!(q["scope"], "https://www.googleapis.com/auth/drive.file");
            assert!(q["redirect_uri"].starts_with("http://127.0.0.1:"));
            let redirect = reqwest::Url::parse(&q["redirect_uri"]).map_err(|e| e.to_string())?;
            let port = redirect.port().ok_or("no port")?;
            let state = q["state"].clone();
            std::thread::spawn(move || {
                send_request(port, &format!("/?code=the-code&state={state}"));
            });
            Ok(())
        };
        let t = authorize(&cfg, &open, Duration::from_secs(10)).unwrap();
        assert_eq!(t.access_token, "a1");
        assert_eq!(t.refresh_token, "r1");
        let r = refresh(&cfg, "r1").unwrap();
        assert_eq!(r.access_token, "a2");
        assert_eq!(r.refresh_token, "r1", "unrotated refresh token is kept");
        assert!(refresh(&cfg, "bad").is_err());
    }

    #[test]
    fn browser_failure_is_reported() {
        let cfg = OAuthConfig::onedrive("cid");
        let err =
            authorize(&cfg, &|_| Err("no browser".into()), Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("no browser"));
    }

    #[test]
    fn memory_store_roundtrip() {
        let s = MemoryTokenStore::new();
        assert_eq!(s.load().unwrap(), None);
        s.save("x").unwrap();
        assert_eq!(s.load().unwrap().as_deref(), Some("x"));
        s.clear().unwrap();
        assert_eq!(s.load().unwrap(), None);
    }

    #[test]
    fn authenticator_refreshes_and_persists() {
        let srv = token_server();
        let mut cfg = OAuthConfig::google_drive("cid", "sec");
        cfg.token_url = format!("{}/token", srv.url());
        let store = Arc::new(MemoryTokenStore::new());
        store.save("r1").unwrap();
        let auth = Authenticator::new(cfg, store);
        assert_eq!(auth.access_token().unwrap(), "a2");
        assert_eq!(auth.access_token().unwrap(), "a2");
    }
}
