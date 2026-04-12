//! Server-Sent Events endpoint for real-time sensor/fan updates.
//!
//! `GET /events` returns an SSE stream that emits `sensors` and `fans` events
//! every second, plus a heartbeat every 5 seconds.
//!
//! Connection limit: at most `SSE_MAX_CLIENTS` concurrent SSE streams are
//! allowed. Additional connections receive HTTP 503.

use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use futures_util::stream::Stream;

use crate::constants;

use super::handlers::AppState;
use super::responses::API_VERSION;

/// GET /events — SSE stream of sensor and fan updates.
///
/// Returns 503 if the maximum number of concurrent SSE clients is reached.
pub async fn events_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    loop {
        let current = state.sse_clients.load(Ordering::SeqCst);
        if current >= constants::SSE_MAX_CLIENTS {
            log::warn!(
                "SSE connection rejected: {current} active clients (limit: {})",
                constants::SSE_MAX_CLIENTS,
            );
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": {
                    "code": "too_many_clients",
                    "message": "maximum SSE connections reached",
                    "retryable": true,
                    "source": "validation"
                }})),
            ));
        }
        if state
            .sse_clients
            .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            break;
        }
    }

    let started_at = Instant::now();
    let client_counter = state.sse_clients.clone();
    let stream =
        futures_util::stream::unfold((state, started_at), move |(state, started_at)| async move {
            tokio::time::sleep(constants::SSE_UPDATE_INTERVAL).await;

            // End stream after max lifetime — client must reconnect
            if started_at.elapsed() >= constants::SSE_MAX_LIFETIME {
                return None;
            }

            let snap = state.cache.snapshot();
            let now = Instant::now();

            let sensors = super::handlers::build_sensor_entries(&snap, now);
            let fans = super::handlers::build_fan_entries(&snap, now);

            // Wrap in an envelope with api_version
            let payload = serde_json::json!({
                "api_version": API_VERSION,
                "sensors": sensors,
                "fans": fans,
            });

            let event = Event::default().event("update").data(payload.to_string());

            Some((Ok::<_, Infallible>(event), (state, started_at)))
        });

    // Wrap in a stream that decrements the counter when dropped
    let guarded_stream = GuardedStream {
        inner: Box::pin(stream),
        counter: client_counter,
    };

    Ok(Sse::new(guarded_stream).keep_alive(
        KeepAlive::new()
            .interval(constants::SSE_HEARTBEAT_INTERVAL)
            .text("heartbeat"),
    ))
}

/// A wrapper stream that decrements the SSE client counter on drop,
/// ensuring the counter stays accurate even if clients disconnect.
struct GuardedStream<S> {
    inner: std::pin::Pin<Box<S>>,
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl<S> Drop for GuardedStream<S> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<S: Stream> Stream for GuardedStream<S> {
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use crate::constants::SSE_MAX_CLIENTS;

    /// Verify the CAS client-limiting pattern: a compare_exchange from 0->1
    /// succeeds, but once the counter reaches SSE_MAX_CLIENTS the next CAS
    /// attempt correctly fails.
    #[test]
    fn cas_client_limit_rejects_at_max() {
        let counter = AtomicUsize::new(0);

        // First CAS: 0 -> 1 should succeed
        let result = counter.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        assert!(result.is_ok(), "CAS from 0->1 should succeed");
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Set counter to the maximum
        counter.store(SSE_MAX_CLIENTS, Ordering::SeqCst);

        // The handler's admission check: current >= SSE_MAX_CLIENTS means reject.
        // This mirrors the `if current >= constants::SSE_MAX_CLIENTS` guard in
        // events_handler.
        let current = counter.load(Ordering::SeqCst);
        assert!(
            current >= SSE_MAX_CLIENTS,
            "counter at max should trigger rejection"
        );

        // A CAS with a stale `current` (e.g. 0) also fails — simulates a
        // concurrent increment race where the snapshot is outdated.
        let result = counter.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
        assert!(result.is_err(), "CAS with stale snapshot should fail");
    }

    /// Verify that GuardedStream decrements the client counter when dropped.
    #[test]
    fn guarded_stream_decrements_counter_on_drop() {
        let counter = Arc::new(AtomicUsize::new(1));

        // Wrap a no-op stream in GuardedStream
        let stream: GuardedStream<futures_util::stream::Empty<()>> = GuardedStream {
            inner: Box::pin(futures_util::stream::empty()),
            counter: counter.clone(),
        };

        assert_eq!(counter.load(Ordering::SeqCst), 1);
        drop(stream);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "counter must be 0 after GuardedStream is dropped"
        );
    }
}
