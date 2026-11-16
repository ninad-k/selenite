//! Bearer-token and `Origin` checks, and the server token file.
//!
//! The token never appears in logs, in the vault or in error messages. It comes from the
//! `SELENITE_SERVER_TOKEN` environment variable, else from a file in the OS config directory
//! (outside the vault) that is created on first run.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::ServerError;

/// Environment variable that supplies the token.
pub const TOKEN_ENV: &str = "SELENITE_SERVER_TOKEN";

/// Shortest accepted token (characters). Generated tokens are 43 characters.
pub const MIN_TOKEN_LEN: usize = 16;

/// Checks `Authorization` headers against the configured token in constant time.
#[derive(Clone)]
pub struct TokenGuard {
    digest: [u8; 32],
}

impl std::fmt::Debug for TokenGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenGuard(..)")
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Constant-time equality of two digests.
fn digests_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl TokenGuard {
    /// A guard for `token`.
    pub fn new(token: &str) -> Self {
        Self {
            digest: sha256(token.as_bytes()),
        }
    }

    /// Whether `header` (the raw `Authorization` value) is `Bearer <the token>`.
    ///
    /// Both sides are hashed first, so the comparison time does not depend on how much of the
    /// token a caller guessed or on its length.
    pub fn check(&self, header: Option<&str>) -> bool {
        let (scheme, value) = header
            .and_then(|h| h.trim().split_once(' '))
            .unwrap_or(("", ""));
        let presented = sha256(value.trim().as_bytes());
        digests_equal(&self.digest, &presented) & scheme.eq_ignore_ascii_case("bearer")
    }
}

/// Which `Origin` values are accepted: loopback origins plus any configured ones.
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    allowed: Vec<String>,
}

fn normalize_origin(o: &str) -> String {
    o.trim().trim_end_matches('/').to_ascii_lowercase()
}

impl OriginPolicy {
    /// A policy accepting loopback origins and `allowed` (full origins such as
    /// `https://notes.example.com`).
    pub fn new(allowed: &[String]) -> Self {
        Self {
            allowed: allowed.iter().map(|o| normalize_origin(o)).collect(),
        }
    }

    /// Whether a request carrying this `Origin` header value may proceed.
    pub fn permits(&self, origin: &str) -> bool {
        let origin = normalize_origin(origin);
        if self.allowed.contains(&origin) {
            return true;
        }
        let Some(rest) = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
        else {
            return false;
        };
        let authority = rest.split('/').next().unwrap_or("");
        let host = if let Some(v6) = authority.strip_prefix('[') {
            v6.split(']').next().map(|h| format!("[{h}]"))
        } else {
            authority.split(':').next().map(str::to_string)
        };
        matches!(host.as_deref(), Some("localhost" | "127.0.0.1" | "[::1]"))
    }
}

/// The per-user config directory for this app, outside any vault.
///
/// Windows: `%APPDATA%\<APP_NAME>`. Elsewhere: `$XDG_CONFIG_HOME/<app>` or `~/.config/<app>`.
pub fn default_config_dir() -> Option<PathBuf> {
    let name = selenite_core::APP_NAME;
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join(name))
    } else {
        let name = name.to_lowercase();
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|d| d.join(name))
    }
}

/// Path of the token file inside `config_dir`.
pub fn token_file(config_dir: &Path) -> PathBuf {
    config_dir.join("server-token")
}

/// Where the token came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// The `SELENITE_SERVER_TOKEN` environment variable.
    Env,
    /// An existing token file.
    File(PathBuf),
    /// A token file created just now.
    Created(PathBuf),
}

fn validate(token: &str) -> Result<(), ServerError> {
    if token.len() < MIN_TOKEN_LEN {
        return Err(ServerError::Token(format!(
            "the token must be at least {MIN_TOKEN_LEN} characters"
        )));
    }
    if token.chars().any(char::is_whitespace) {
        return Err(ServerError::Token(
            "the token must not contain whitespace".into(),
        ));
    }
    Ok(())
}

fn generate() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    URL_SAFE_NO_PAD.encode(bytes)
}

fn create_exclusive(path: &Path, token: &str) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// The token to use: `env_token` if set, else the file in `config_dir` (created with a fresh
/// random token, mode 0600 on Unix, if it does not exist yet).
pub fn resolve_token(
    env_token: Option<String>,
    config_dir: &Path,
) -> Result<(String, TokenSource), ServerError> {
    if let Some(t) = env_token.filter(|t| !t.trim().is_empty()) {
        let t = t.trim().to_string();
        validate(&t)?;
        return Ok((t, TokenSource::Env));
    }
    let path = token_file(config_dir);
    let io = |e: std::io::Error| ServerError::Token(format!("{}: {e}", path.display()));
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let t = text.trim().to_string();
            validate(&t)?;
            return Ok((t, TokenSource::File(path)));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    std::fs::create_dir_all(config_dir).map_err(io)?;
    let token = generate();
    match create_exclusive(&path, &token) {
        Ok(()) => Ok((token, TokenSource::Created(path))),
        // Another process created it first: use theirs.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let text = std::fs::read_to_string(&path).map_err(io)?;
            let t = text.trim().to_string();
            validate(&t)?;
            Ok((t, TokenSource::File(path)))
        }
        Err(e) => Err(io(e)),
    }
}

/// [`resolve_token`] using the environment and the default config directory.
pub fn resolve_default_token() -> Result<(String, TokenSource), ServerError> {
    let env = std::env::var(TOKEN_ENV).ok();
    let dir = match default_config_dir() {
        Some(d) => d,
        None if env.is_some() => PathBuf::new(),
        None => {
            return Err(ServerError::Token(format!(
                "cannot find a config directory; set {TOKEN_ENV} instead"
            )));
        }
    };
    resolve_token(env, &dir)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn guard_accepts_only_the_exact_bearer_token() {
        let g = TokenGuard::new("correct-horse-battery");
        assert!(g.check(Some("Bearer correct-horse-battery")));
        assert!(g.check(Some("bearer correct-horse-battery")));
        assert!(!g.check(Some("Bearer correct-horse-batterx")));
        assert!(!g.check(Some("Basic correct-horse-battery")));
        assert!(!g.check(Some("correct-horse-battery")));
        assert!(!g.check(Some("Bearer ")));
        assert!(!g.check(None));
    }

    #[test]
    fn origin_policy() {
        let p = OriginPolicy::new(&["https://notes.example.com/".into()]);
        assert!(p.permits("http://localhost:3000"));
        assert!(p.permits("http://127.0.0.1"));
        assert!(p.permits("http://[::1]:8787"));
        assert!(p.permits("https://notes.example.com"));
        assert!(!p.permits("https://evil.example"));
        assert!(!p.permits("http://localhost.evil.example"));
        assert!(!p.permits("null"));
    }

    #[test]
    fn token_file_is_created_once_and_reused() {
        let dir = tempfile::tempdir().expect("tmp");
        let cfg = dir.path().join("cfg");
        let (t1, src1) = resolve_token(None, &cfg).expect("create");
        assert_eq!(t1.len(), 43);
        assert_eq!(src1, TokenSource::Created(token_file(&cfg)));
        let (t2, src2) = resolve_token(None, &cfg).expect("reuse");
        assert_eq!(t1, t2);
        assert_eq!(src2, TokenSource::File(token_file(&cfg)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_file(&cfg))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn env_token_wins_and_short_tokens_are_refused() {
        let dir = tempfile::tempdir().expect("tmp");
        let (t, src) = resolve_token(Some("0123456789abcdef".into()), dir.path()).expect("env");
        assert_eq!((t.as_str(), src), ("0123456789abcdef", TokenSource::Env));
        assert!(!token_file(dir.path()).exists());
        assert!(resolve_token(Some("short".into()), dir.path()).is_err());
    }
}
