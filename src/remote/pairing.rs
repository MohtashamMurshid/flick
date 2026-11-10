//! Six-digit pairing codes, and the rate limit of the routes that need no
//! token.
//!
//! The owner opens a code on demand (Settings → Remote Libraries, or
//! `flick-serve --pair-code`). Another Flick on the tailnet sends it to
//! `POST /v1/pair` and gets the full pairing string back. A code lives ten
//! minutes, five wrong attempts close it, a match closes it, and opening a
//! new one replaces the old one. The code is never logged.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// How long a code stays valid after [`PairingCodes::open`].
pub const CODE_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Wrong attempts that close a code.
pub const MAX_WRONG_ATTEMPTS: u32 = 5;
/// Digits in a code.
pub const CODE_DIGITS: usize = 6;

/// The pairing code of one server, shared between the server and whoever
/// shows codes to the owner. Cloning shares the same code.
#[derive(Clone)]
pub struct PairingCodes {
    inner: Arc<Mutex<Codes>>,
}

struct Codes {
    lifetime: Duration,
    open: Option<OpenCode>,
}

struct OpenCode {
    code: String,
    expires: Instant,
    wrong: u32,
}

/// What [`PairingCodes::check`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeCheck {
    /// The code matched and is now closed.
    Matched,
    /// The code did not match; it stays open for this many more attempts.
    Wrong { attempts_left: u32 },
    /// The code did not match and that was the last attempt; it is closed.
    Exhausted,
    /// No code is open, or it expired.
    NotOpen,
}

impl Default for PairingCodes {
    fn default() -> Self {
        Self::new()
    }
}

/// The code is left out, so a logged handle never reveals it.
impl std::fmt::Debug for PairingCodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingCodes")
            .field("open", &self.is_open())
            .finish()
    }
}

impl PairingCodes {
    /// A handle with no code open; codes last [`CODE_LIFETIME`].
    pub fn new() -> Self {
        Self::with_lifetime(CODE_LIFETIME)
    }

    /// Codes that last `lifetime` instead, for tests.
    pub fn with_lifetime(lifetime: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Codes {
                lifetime,
                open: None,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Codes> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Opens a fresh code, uniformly random from 000000 to 999999, valid for
    /// the lifetime from now. Any earlier code stops working.
    pub fn open(&self) -> String {
        let code = random_code();
        let mut codes = self.lock();
        codes.open = Some(OpenCode {
            code: code.clone(),
            expires: Instant::now() + codes.lifetime,
            wrong: 0,
        });
        tracing::info!(
            minutes = codes.lifetime.as_secs().div_ceil(60),
            "A pairing code is open"
        );
        code
    }

    /// Closes the open code, if any.
    pub fn close(&self) {
        if self.lock().open.take().is_some() {
            tracing::info!("The pairing code was closed");
        }
    }

    /// Whether a code is open and has not expired.
    pub fn is_open(&self) -> bool {
        self.remaining().is_some()
    }

    /// How long the open code stays valid, or None when none is open.
    pub fn remaining(&self) -> Option<Duration> {
        let mut codes = self.lock();
        let left = codes
            .open
            .as_ref()
            .map(|open| open.expires.saturating_duration_since(Instant::now()));
        match left {
            Some(left) if !left.is_zero() => Some(left),
            Some(_) => {
                codes.open = None;
                tracing::info!("The pairing code expired");
                None
            }
            None => None,
        }
    }

    /// Checks a presented code in constant time. Spaces and dashes are
    /// ignored, so `123 456` and `123-456` match `123456`. A match closes the
    /// code; a miss counts an attempt, and the last one closes it.
    pub fn check(&self, presented: &str) -> CodeCheck {
        let presented: String = presented
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect();
        if self.remaining().is_none() {
            return CodeCheck::NotOpen;
        }
        let mut codes = self.lock();
        let Some(open) = codes.open.as_mut() else {
            return CodeCheck::NotOpen;
        };
        if crate::remote::auth::token_matches(&open.code, &presented) {
            codes.open = None;
            tracing::info!("A pairing code was matched; the code is closed");
            return CodeCheck::Matched;
        }
        open.wrong += 1;
        if open.wrong >= MAX_WRONG_ATTEMPTS {
            codes.open = None;
            tracing::warn!(
                attempts = MAX_WRONG_ATTEMPTS,
                "The pairing code was closed after too many wrong attempts"
            );
            return CodeCheck::Exhausted;
        }
        CodeCheck::Wrong {
            attempts_left: MAX_WRONG_ATTEMPTS - open.wrong,
        }
    }
}

/// Six decimal digits from the operating system's random source, without
/// modulo bias: values at or above the largest multiple of a million below
/// 2³² are drawn again.
fn random_code() -> String {
    const RANGE: u64 = 1_000_000;
    const ZONE: u64 = (1 << 32) - (1 << 32) % RANGE;
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).expect("the operating system's random source is unavailable");
        let value = u64::from(u32::from_le_bytes(bytes));
        if value < ZONE {
            return format!("{:0width$}", value % RANGE, width = CODE_DIGITS);
        }
    }
}

/// Requests per client address and route in a fixed window. Bounded: when
/// it tracks many addresses, those whose window has ended are forgotten.
pub(crate) struct RateLimiter {
    limit: u32,
    window: Duration,
    seen: Mutex<HashMap<(IpAddr, &'static str), Window>>,
}

/// When a client's window started, and its requests in it.
type Window = (Instant, u32);

/// Addresses tracked before ended windows are swept.
const TRACKED_ADDRESSES: usize = 4096;

impl RateLimiter {
    pub(crate) fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Counts a request; Err carries how long until the window ends.
    pub(crate) fn check(&self, ip: IpAddr, route: &'static str) -> Result<(), Duration> {
        let now = Instant::now();
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if seen.len() >= TRACKED_ADDRESSES {
            let window = self.window;
            seen.retain(|_, (start, _)| now.duration_since(*start) < window);
        }
        let (start, count) = seen.entry((ip, route)).or_insert((now, 0));
        if now.duration_since(*start) >= self.window {
            *start = now;
            *count = 0;
        }
        if *count >= self.limit {
            return Err(self.window.saturating_sub(now.duration_since(*start)));
        }
        *count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_six_digits_and_vary() {
        let codes: std::collections::HashSet<String> = (0..50).map(|_| random_code()).collect();
        assert!(codes.len() > 40, "{codes:?}");
        for code in &codes {
            assert_eq!(code.len(), CODE_DIGITS, "{code}");
            assert!(code.bytes().all(|b| b.is_ascii_digit()), "{code}");
        }
    }

    #[test]
    fn a_match_closes_the_code_and_a_new_one_replaces_the_old() {
        let codes = PairingCodes::new();
        assert_eq!(codes.check("123456"), CodeCheck::NotOpen);
        let first = codes.open();
        let second = codes.open();
        assert!(codes.is_open());
        if first != second {
            assert_eq!(codes.check(&first), CodeCheck::Wrong { attempts_left: 4 });
        }
        let spaced = format!("{} {}", &second[..3], &second[3..]);
        assert_eq!(codes.check(&spaced), CodeCheck::Matched);
        assert!(!codes.is_open());
        assert_eq!(codes.check(&second), CodeCheck::NotOpen);
        codes.open();
        codes.close();
        assert!(!codes.is_open());
    }

    #[test]
    fn five_wrong_attempts_close_the_code() {
        let codes = PairingCodes::new();
        let code = codes.open();
        let wrong = if code == "000000" { "000001" } else { "000000" };
        for left in (1..MAX_WRONG_ATTEMPTS).rev() {
            assert_eq!(
                codes.check(wrong),
                CodeCheck::Wrong {
                    attempts_left: left
                }
            );
        }
        assert_eq!(codes.check(wrong), CodeCheck::Exhausted);
        assert_eq!(codes.check(&code), CodeCheck::NotOpen);
    }

    #[test]
    fn codes_expire() {
        let codes = PairingCodes::with_lifetime(Duration::from_millis(30));
        let code = codes.open();
        assert!(
            codes
                .remaining()
                .is_some_and(|left| left <= Duration::from_millis(30))
        );
        std::thread::sleep(Duration::from_millis(60));
        assert!(!codes.is_open());
        assert_eq!(codes.check(&code), CodeCheck::NotOpen);
    }

    #[test]
    fn debug_leaves_out_the_code() {
        let codes = PairingCodes::new();
        let code = codes.open();
        assert!(!format!("{codes:?}").contains(&code));
    }

    #[test]
    fn the_limit_is_per_address_and_route_and_resets() {
        let limiter = RateLimiter::new(2, Duration::from_millis(50));
        let a: IpAddr = "100.64.0.1".parse().unwrap();
        let b: IpAddr = "100.64.0.2".parse().unwrap();
        assert!(limiter.check(a, "hello").is_ok());
        assert!(limiter.check(a, "hello").is_ok());
        assert!(limiter.check(a, "hello").is_err());
        assert!(limiter.check(a, "pair").is_ok());
        assert!(limiter.check(b, "hello").is_ok());
        std::thread::sleep(Duration::from_millis(70));
        assert!(limiter.check(a, "hello").is_ok());
    }
}
