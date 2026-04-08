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
use super::responses::{FanEntry, SensorEntry, API_VERSION};

/// GET /events — SSE stream of sensor and fan updates.
///
/// Returns 503 if the maximum number of concurrent SSE clients is reached.
pub async fn events_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let prev = state.sse_clients.fetch_add(1, Ordering::SeqCst);
    if prev >= constants::SSE_MAX_CLIENTS {
        state.sse_clients.fetch_sub(1, Ordering::SeqCst);
        log::warn!(
            "SSE connection rejected: {prev} active clients (limit: {})",
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

            // Build sensors payload
            let sensors: Vec<SensorEntry> = snap
                .sensors
                .values()
                .map(|s| {
                    let age_ms = now.duration_since(s.updated_at).as_millis() as u64;
                    SensorEntry {
                        id: s.id.clone(),
                        kind: s.kind.to_string(),
                        label: s.label.clone(),
                        value_c: s.value_c,
                        source: s.source.to_string(),
                        age_ms,
                        rate_c_per_s: s.rate_c_per_s,
                        session_min_c: s.session_min_c,
                        session_max_c: s.session_max_c,
                    }
                })
                .collect();

            // Build fans payload
            let mut fans: Vec<FanEntry> = Vec::new();
            for (ch, fan) in &snap.openfan_fans {
                let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
                let stall = fan
                    .last_commanded_pwm
                    .map(|pwm| fan.rpm == 0 && pwm > constants::STALL_PWM_THRESHOLD);
                fans.push(FanEntry {
                    id: format!("openfan:ch{ch:02}"),
                    source: "openfan".into(),
                    rpm: Some(fan.rpm),
                    last_commanded_pwm: fan.last_commanded_pwm,
                    age_ms,
                    stall_detected: stall,
                });
            }
            for (id, fan) in &snap.hwmon_fans {
                let age_ms = now.duration_since(fan.updated_at).as_millis() as u64;
                let stall = match (fan.rpm, fan.last_commanded_pwm) {
                    (Some(rpm), Some(pwm)) => {
                        Some(rpm == 0 && pwm > constants::STALL_PWM_THRESHOLD)
                    }
                    _ => None,
                };
                fans.push(FanEntry {
                    id: id.clone(),
                    source: "hwmon".into(),
                    rpm: fan.rpm,
                    last_commanded_pwm: fan.last_commanded_pwm,
                    age_ms,
                    stall_detected: stall,
                });
            }
            fans.sort_by(|a, b| a.id.cmp(&b.id));

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
