//! Outbound rate governor for all Tuta API traffic.
//!
//! Every SDK request is routed through one [`GovernedRestClient`] so the bridge
//! cannot flood the Tuta API. It enforces two things neither the raw HTTP
//! client nor the SDK's own suspension layer provide:
//!
//! 1. **Bounded concurrency.** At most [`MAX_IN_FLIGHT`] requests run at once,
//!    capping the HTTP/2 stream fan-out at the source (attachment sub-loops,
//!    on-demand IMAP fetches across several client connections, and the syncer
//!    can otherwise stampede in parallel).
//! 2. **Throttle backoff.** On an HTTP 429/503 the governor suspends *all*
//!    outbound traffic. It honors the server's `retry-after` / `suspension-time`
//!    header when present and otherwise applies an escalating default backoff.
//!    The SDK's built-in `SuspendableRestClient` arms nothing when a 429 carries
//!    no header, which let the bridge keep hammering a rate-limited server.
//!
//! Because a 429 is returned to the caller *and* arms the gate, the existing
//! retry layers (`sync::retry`, attachment retries) become self-correcting:
//! their next attempt blocks on the gate instead of amplifying the flood.
//!
//! Installed in [`crate::tuta`] via `Sdk::new_without_suspension`, replacing the
//! SDK suspension layer so there is exactly one authoritative choke point.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{RwLock, Semaphore};
use tokio::time::Instant;
use tutasdk::bindings::rest_client::{
    HttpMethod, RestClient, RestClientError, RestClientOptions, RestResponse,
};

/// Maximum number of Tuta API requests in flight at once.
const MAX_IN_FLIGHT: usize = 4;
/// First step of the default backoff, used when the server throttles without
/// telling us how long to wait.
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(2);
/// Ceiling for any single backoff (server-provided or default), so one throttle
/// can never stall the bridge for minutes.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Response headers Tuta uses to advertise a throttle duration, in seconds.
/// Header names reach us lowercased (see [`RestResponse`]).
const RETRY_AFTER_HEADER: &str = "retry-after";
const SUSPENSION_TIME_HEADER: &str = "suspension-time";

/// A [`RestClient`] decorator enforcing bounded concurrency and 429/503 backoff.
pub struct GovernedRestClient {
    inner: Arc<dyn RestClient>,
    permits: Arc<Semaphore>,
    /// When `Some`, no request proceeds until this instant has passed.
    suspended_until: Arc<RwLock<Option<Instant>>>,
    /// Count of consecutive header-less throttles, for escalating the default
    /// backoff. Reset on any 2xx response or any server-provided delay.
    consecutive_throttles: AtomicU32,
}

impl GovernedRestClient {
    pub fn new(inner: Arc<dyn RestClient>) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            suspended_until: Arc::new(RwLock::new(None)),
            consecutive_throttles: AtomicU32::new(0),
        }
    }

    /// Block until any active suspension has elapsed. Re-checks after each sleep
    /// because a concurrent request may have extended the suspension meanwhile.
    async fn wait_out_suspension(&self) {
        loop {
            let until = *self.suspended_until.read().await;
            match until {
                Some(t) => {
                    let now = Instant::now();
                    if t > now {
                        tokio::time::sleep(t - now).await;
                    } else {
                        return;
                    }
                }
                None => return,
            }
        }
    }

    /// Arm (or extend) the suspension so it lasts at least `backoff` from now.
    /// The longer of the existing and the new deadline wins.
    async fn suspend_for(&self, backoff: Duration) {
        let until = Instant::now() + backoff;
        let mut guard = self.suspended_until.write().await;
        match *guard {
            Some(existing) if existing >= until => {}
            _ => *guard = Some(until),
        }
    }

    /// Backoff for a throttle response: the server's requested seconds when
    /// present (clamped to a sane window, escalation reset), otherwise an
    /// escalating default (2s, 4s, 8s, ...) capped at [`MAX_BACKOFF`].
    fn backoff_for(&self, server_secs: Option<u64>) -> Duration {
        if let Some(secs) = server_secs {
            self.consecutive_throttles.store(0, Ordering::Relaxed);
            return Duration::from_secs(secs).clamp(DEFAULT_BACKOFF_BASE, MAX_BACKOFF);
        }
        let n = self.consecutive_throttles.fetch_add(1, Ordering::Relaxed);
        let factor = 1u32 << n.min(5); // 1, 2, 4, 8, 16, 32
        DEFAULT_BACKOFF_BASE.saturating_mul(factor).min(MAX_BACKOFF)
    }

    fn note_success(&self) {
        self.consecutive_throttles.store(0, Ordering::Relaxed);
    }
}

/// If `resp` is a throttle (429/503), return `Some(server_secs)` where the inner
/// option is the server-advertised delay in seconds, if any. A non-throttle
/// response returns `None`.
fn throttle_delay(resp: &RestResponse) -> Option<Option<u64>> {
    if resp.status == 429 || resp.status == 503 {
        let secs = resp
            .headers
            .get(RETRY_AFTER_HEADER)
            .or_else(|| resp.headers.get(SUSPENSION_TIME_HEADER))
            .and_then(|v| v.trim().parse::<u64>().ok());
        Some(secs)
    } else {
        None
    }
}

#[async_trait]
impl RestClient for GovernedRestClient {
    async fn request_binary(
        &self,
        url: String,
        method: HttpMethod,
        options: RestClientOptions,
    ) -> Result<RestResponse, RestClientError> {
        // Take a concurrency permit first, then wait out any suspension while
        // holding it. This bounds both the number of in-flight requests and the
        // number that fire the instant a suspension lifts.
        let _permit = self
            .permits
            .acquire()
            .await
            .expect("rate governor semaphore is never closed");
        self.wait_out_suspension().await;

        let result = self.inner.request_binary(url, method, options).await;

        if let Ok(ref resp) = result {
            if let Some(server_secs) = throttle_delay(resp) {
                let backoff = self.backoff_for(server_secs);
                log::warn!(
                    "Tuta throttled us (HTTP {}); pausing all API traffic for {:?}",
                    resp.status,
                    backoff
                );
                self.suspend_for(backoff).await;
            } else if (200..300).contains(&resp.status) {
                self.note_success();
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    fn opts() -> RestClientOptions {
        RestClientOptions {
            headers: HashMap::new(),
            body: None,
            suspension_behavior: None,
        }
    }

    fn resp(status: u32, headers: &[(&str, &str)]) -> RestResponse {
        RestResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: None,
        }
    }

    /// A backend that records the peak number of simultaneous in-flight calls
    /// and replies with a configurable status.
    struct CountingClient {
        in_flight: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
        status: u32,
    }

    #[async_trait]
    impl RestClient for CountingClient {
        async fn request_binary(
            &self,
            _url: String,
            _method: HttpMethod,
            _options: RestClientOptions,
        ) -> Result<RestResponse, RestClientError> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(resp(self.status, &[]))
        }
    }

    #[test]
    fn throttle_delay_classifies_responses() {
        assert_eq!(throttle_delay(&resp(200, &[])), None);
        assert_eq!(throttle_delay(&resp(404, &[])), None);
        assert_eq!(throttle_delay(&resp(429, &[])), Some(None));
        assert_eq!(
            throttle_delay(&resp(429, &[("retry-after", "30")])),
            Some(Some(30))
        );
        assert_eq!(
            throttle_delay(&resp(429, &[("suspension-time", "12")])),
            Some(Some(12))
        );
        assert_eq!(
            throttle_delay(&resp(503, &[("retry-after", "5")])),
            Some(Some(5))
        );
        // garbage header value -> treated as "no hint"
        assert_eq!(
            throttle_delay(&resp(429, &[("retry-after", "soon")])),
            Some(None)
        );
    }

    #[test]
    fn backoff_honors_and_clamps_server_seconds() {
        let g = GovernedRestClient::new(Arc::new(CountingClient {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
            status: 200,
        }));
        // Below the floor clamps up to the base; above the ceiling clamps down.
        assert_eq!(g.backoff_for(Some(1)), DEFAULT_BACKOFF_BASE);
        assert_eq!(g.backoff_for(Some(9999)), MAX_BACKOFF);
        assert_eq!(g.backoff_for(Some(10)), Duration::from_secs(10));
    }

    #[test]
    fn backoff_escalates_then_caps_without_header() {
        let g = GovernedRestClient::new(Arc::new(CountingClient {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
            status: 200,
        }));
        assert_eq!(g.backoff_for(None), Duration::from_secs(2));
        assert_eq!(g.backoff_for(None), Duration::from_secs(4));
        assert_eq!(g.backoff_for(None), Duration::from_secs(8));
        assert_eq!(g.backoff_for(None), Duration::from_secs(16));
        assert_eq!(g.backoff_for(None), Duration::from_secs(32));
        assert_eq!(g.backoff_for(None), MAX_BACKOFF); // 64 -> capped at 60
        assert_eq!(g.backoff_for(None), MAX_BACKOFF);
    }

    #[test]
    fn backoff_escalation_resets_on_server_value() {
        let g = GovernedRestClient::new(Arc::new(CountingClient {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
            status: 200,
        }));
        assert_eq!(g.backoff_for(None), Duration::from_secs(2));
        assert_eq!(g.backoff_for(None), Duration::from_secs(4));
        let _ = g.backoff_for(Some(10)); // server value resets the escalation
        assert_eq!(g.backoff_for(None), Duration::from_secs(2));
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let client = Arc::new(GovernedRestClient::new(Arc::new(CountingClient {
            in_flight: in_flight.clone(),
            max_seen: max_seen.clone(),
            status: 200,
        })));

        let mut handles = Vec::new();
        for _ in 0..20 {
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                c.request_binary("u".into(), HttpMethod::GET, opts()).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        let peak = max_seen.load(Ordering::SeqCst);
        assert!(peak >= 1, "at least one request ran");
        assert!(
            peak <= MAX_IN_FLIGHT,
            "peak in-flight {peak} exceeded cap {MAX_IN_FLIGHT}"
        );
    }

    #[tokio::test]
    async fn throttle_response_arms_suspension() {
        let g = GovernedRestClient::new(Arc::new(CountingClient {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
            status: 429, // every response is a throttle
        }));
        // No suspension before the first request.
        assert!(g.suspended_until.read().await.is_none());
        let _ = g
            .request_binary("u".into(), HttpMethod::GET, opts())
            .await
            .unwrap();
        // A 429 (no header) must arm a future suspension.
        let until = *g.suspended_until.read().await;
        assert!(until.is_some());
        assert!(until.unwrap() > Instant::now());
    }

    #[tokio::test]
    async fn wait_out_suspension_blocks_until_elapsed() {
        let g = GovernedRestClient::new(Arc::new(CountingClient {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_seen: Arc::new(AtomicUsize::new(0)),
            status: 200,
        }));
        g.suspend_for(Duration::from_millis(60)).await;
        let start = Instant::now();
        g.wait_out_suspension().await;
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "wait returned too early: {:?}",
            start.elapsed()
        );
    }
}
