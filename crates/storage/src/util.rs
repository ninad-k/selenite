//! Helpers shared by the HTTP-based providers. Nothing here logs tokens or file content.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{Result, StorageError};

/// Build the blocking HTTP client used by all cloud providers.
pub(crate) fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("selenite-storage/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| StorageError::Internal(format!("http client: {}", e.without_url())))
}

/// Map a transport-level failure. Connection and timeout failures become
/// `Internal("offline: ...")` so the sync engine can detect being offline; other failures keep the
/// `ErrorKind` of an underlying `io::Error` when one is found in the source chain.
pub(crate) fn net_err(e: reqwest::Error) -> StorageError {
    let e = e.without_url();
    if e.is_connect() || e.is_timeout() {
        return StorageError::Internal(format!("offline: {e}"));
    }
    let mut kind = std::io::ErrorKind::Other;
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            kind = io.kind();
            break;
        }
        cur = err.source();
    }
    StorageError::Io {
        path: "<network>".into(),
        source: std::io::Error::new(kind, e.to_string()),
    }
}

/// Map an unexpected HTTP status (never includes the response body).
pub(crate) fn status_error(status: reqwest::StatusCode, what: &str) -> StorageError {
    let s = status.as_u16();
    match s {
        401 | 403 => StorageError::Internal(format!("auth: HTTP {s} during {what}")),
        _ => StorageError::Internal(format!("remote: HTTP {s} during {what}")),
    }
}

/// Lock a mutex, mapping poisoning to `Internal`.
pub(crate) fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| StorageError::Internal("lock poisoned".into()))
}

/// Current Unix time in seconds.
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `application/x-www-form-urlencoded` body from pairs (reqwest's `form` feature is not enabled).
pub(crate) fn form_encode(pairs: &[(&str, &str)]) -> String {
    match reqwest::Url::parse("http://form.invalid/") {
        Ok(mut u) => {
            u.query_pairs_mut().extend_pairs(pairs.iter().copied());
            u.query().unwrap_or("").to_string()
        }
        Err(_) => String::new(),
    }
}

/// Parse an RFC 3339 timestamp such as `2026-11-16T12:34:56.789Z` (also `+hh:mm` offsets).
pub(crate) fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't') {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut i = 19;
    let mut nanos: u32 = 0;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let digits: String = s[start..i].chars().take(9).collect();
        nanos = format!("{digits:0<9}").parse().ok()?;
    }
    let offset = match b.get(i)? {
        b'Z' | b'z' => 0,
        sign @ (b'+' | b'-') => {
            let v = num(i + 1, i + 3)? * 3600 + num(i + 4, i + 6)? * 60;
            if *sign == b'+' { v } else { -v }
        }
        _ => return None,
    };
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::new(secs as u64, nanos))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parses() {
        let t = parse_rfc3339("2026-01-02T03:04:05.5Z").unwrap();
        let d = t.duration_since(UNIX_EPOCH).unwrap();
        assert_eq!(d.as_secs(), 1_767_323_045);
        assert_eq!(d.subsec_millis(), 500);
        let off = parse_rfc3339("2026-01-02T04:04:05+01:00").unwrap();
        assert_eq!(
            off.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_767_323_045
        );
        assert!(parse_rfc3339("garbage").is_none());
    }

    #[test]
    fn form_encoding_escapes() {
        assert_eq!(form_encode(&[("a", "b c"), ("d", "e&f")]), "a=b+c&d=e%26f");
    }
}
