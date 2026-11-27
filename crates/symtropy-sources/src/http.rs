//! Shared blocking HTTP client for user-initiated source lookups.
//!
//! There is no scheduler here. A fetch only happens because a caller invoked
//! it, and every fetch passes through the same rate limiter, timeout, body
//! limit, and backoff policy. The UI must call this from a worker thread.

use crate::model::{FetchRequest, FetchResponse, Fetcher, Result, SourceSite};
use crate::policy::Backoff;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use ureq::ResponseExt;

pub const DEFAULT_USER_AGENT: &str =
    "Symtropy/0.1 (local-first gallery; user-initiated lookup; contact: local)";

const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Validate a URL that the user pasted or an adapter returned. This is a
/// local policy check only; it never opens a connection.
pub fn validate_source_url(url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("refusing non-http source URL: {url}"));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct RateLimiter {
    min_interval: Duration,
    next_allowed: Arc<Mutex<Instant>>,
}

impl RateLimiter {
    fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            next_allowed: Arc::new(Mutex::new(Instant::now())),
        }
    }

    fn wait_turn(&self) {
        if self.min_interval.is_zero() {
            return;
        }
        let mut next = self
            .next_allowed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        if *next > now {
            thread::sleep(*next - now);
        }
        let finished = Instant::now();
        *next = if *next < finished {
            finished + self.min_interval
        } else {
            *next + self.min_interval
        };
    }
}

/// Blocking HTTP implementation of the crate's injected `Fetcher` boundary.
pub struct HttpFetcher {
    agent: ureq::Agent,
    limiter: RateLimiter,
    user_agent: String,
    max_bytes: u64,
    max_attempts: u32,
    backoff: Backoff,
}

impl HttpFetcher {
    pub fn new(user_agent: impl Into<String>) -> Self {
        Self::with_min_interval(user_agent, DEFAULT_MIN_INTERVAL)
    }

    /// e621's public API asks for no more than roughly two requests per second.
    /// Other sites use a conservative one-per-second default until an adapter
    /// documents something better.
    pub fn for_site(site: SourceSite, user_agent: impl Into<String>) -> Self {
        let interval = match site {
            SourceSite::E621 => Duration::from_millis(500),
            _ => Duration::from_secs(1),
        };
        Self::with_min_interval(user_agent, interval)
    }

    pub fn with_min_interval(user_agent: impl Into<String>, min_interval: Duration) -> Self {
        let user_agent = user_agent.into();
        let config = ureq::Agent::config_builder()
            .user_agent(user_agent.clone())
            .timeout_global(Some(DEFAULT_TIMEOUT))
            .build();
        Self {
            agent: config.into(),
            limiter: RateLimiter::new(min_interval),
            user_agent,
            max_bytes: DEFAULT_MAX_BYTES,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff: Backoff::default(),
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    pub fn with_retries(mut self, max_attempts: u32, backoff: Backoff) -> Self {
        self.max_attempts = max_attempts.max(1);
        self.backoff = backoff;
        self
    }

    fn validate_url(url: &str) -> Result<()> {
        validate_source_url(url)
    }

    fn retryable(error: &ureq::Error) -> bool {
        match error {
            ureq::Error::StatusCode(code) => *code == 429 || (500..600).contains(code),
            ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::Protocol(_) => true,
            ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
            _ => false,
        }
    }
}

impl Fetcher for HttpFetcher {
    fn get(&self, request: &FetchRequest) -> Result<FetchResponse> {
        Self::validate_url(&request.url)?;
        let user_agent = if request.user_agent.trim().is_empty() {
            self.user_agent.as_str()
        } else {
            request.user_agent.as_str()
        };

        let mut attempt = 0;
        loop {
            self.limiter.wait_turn();
            match self
                .agent
                .get(&request.url)
                .header("user-agent", user_agent)
                .call()
            {
                Ok(mut response) => {
                    let final_url = response.get_uri().to_string();
                    let bytes = response
                        .body_mut()
                        .with_config()
                        .limit(self.max_bytes)
                        .read_to_vec()
                        .map_err(|error| format!("failed to read source response: {error}"))?;
                    return Ok(FetchResponse { bytes, final_url });
                }
                Err(error) => {
                    if !Self::retryable(&error) || attempt + 1 >= self.max_attempts {
                        return Err(format!("source request failed: {error}"));
                    }
                    let delay = self.backoff.delay_ms(attempt);
                    attempt += 1;
                    if delay > 0 {
                        thread::sleep(Duration::from_millis(delay));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_http_urls_are_rejected_before_any_network_call() {
        assert!(HttpFetcher::validate_url("ftp://example.invalid/file").is_err());
        assert!(HttpFetcher::validate_url("file:///etc/passwd").is_err());
        assert!(HttpFetcher::validate_url("https://example.invalid/file").is_ok());
    }

    #[test]
    fn only_transient_statuses_are_retried() {
        fn status(code: u16) -> ureq::Error {
            ureq::Error::StatusCode(code)
        }
        assert!(HttpFetcher::retryable(&status(429)));
        assert!(HttpFetcher::retryable(&status(503)));
        assert!(!HttpFetcher::retryable(&status(404)));
        assert!(!HttpFetcher::retryable(&status(403)));
    }

    #[test]
    fn rate_limiter_advances_even_after_a_late_request() {
        let limiter = RateLimiter::new(Duration::from_millis(1));
        {
            let mut next = limiter.next_allowed.lock().unwrap();
            *next = Instant::now() - Duration::from_secs(1);
        }
        limiter.wait_turn();
        let next = *limiter.next_allowed.lock().unwrap();
        assert!(next > Instant::now() - Duration::from_millis(500));
    }
}
