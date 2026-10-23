//! Server-Sent Events (SSE) stream for real-time event delivery.
//! Wraps the broadcast channel in AppState to deliver events to web dashboard clients.

use super::AppState;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use std::convert::Infallible;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

pub use zeroclaw_runtime::observability::broadcast::{BroadcastObserver, EventBuffer};
use zeroclaw_runtime::observability::broadcast::{history_events, is_public_event};

/// GET /api/events — SSE event stream.
///
/// Pairing credentials (QR payloads, one-shot pair codes) are **broadcast-only
/// and delivery-once**: they ride the live `event_tx` fan-out and are never
/// written to the history buffer or the persisted JSONL. A subscriber must be
/// connected *before* pairing to observe them; a client that connects late,
/// reconnects, or lags past the broadcast ring (the discarded
/// `BroadcastStreamRecvError` below) deliberately cannot recover the credential
/// — that is the non-persistent boundary, not a bug. Recovery would require
/// buffering the secret, which the credential boundary forbids.
pub async fn handle_sse_events(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Auth check. When pairing is enabled every subscriber that reaches the
    // stream below has passed the bearer check, so the stream is authenticated;
    // when it is disabled no subscriber is authenticated. That posture decides
    // whether broadcast-only pairing secrets may ride the stream (see
    // `sse_frame_for_stream`).
    let auth_enforced = state.pairing.require_pairing();
    if auth_enforced {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|auth| auth.strip_prefix("Bearer "))
            .unwrap_or("");

        if !state.pairing.is_authenticated(token) {
            return (
                StatusCode::UNAUTHORIZED,
                "Unauthorized — provide Authorization: Bearer <token>",
            )
                .into_response();
        }
    }

    let rx = state.event_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(
        move |result: Result<
            serde_json::Value,
            tokio_stream::wrappers::errors::BroadcastStreamRecvError,
        >| {
            match result {
                Ok(value) => sse_frame_for_stream(value, auth_enforced)
                    .map(|v| Ok::<_, Infallible>(Event::default().data(v.to_string()))),
                Err(_) => None, // Skip lagged messages
            }
        },
    );

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Decide the deliverable form of a broadcast frame for an SSE stream with the
/// given authentication posture. Returns `None` to withhold the frame.
///
/// Fail-closed contract: a frame carrying broadcast-only pairing secrets
/// (stamped [`zeroclaw_log::EPHEMERAL_BROADCAST_MARKER`] by the log layer when
/// it merges `ephemeral_attributes` — QR payloads, pair codes) is withheld
/// entirely unless `auth_enforced` is true. This keeps the credential off any
/// unauthenticated `/api/events` stream even though the pre-existing handler
/// skips the bearer check when pairing is disabled. The internal marker is
/// stripped before delivery so the public event shape is unchanged.
fn sse_frame_for_stream(
    mut value: serde_json::Value,
    auth_enforced: bool,
) -> Option<serde_json::Value> {
    if !is_public_sse_event(&value) {
        return None;
    }
    if zeroclaw_log::frame_carries_ephemeral_credentials(&value) && !auth_enforced {
        return None;
    }
    // Strip the internal marker so the delivered public shape is unchanged.
    // Shared with every other broadcast consumer (RPC `logs/subscribe`) so the
    // credential boundary is enforced identically across the bus.
    zeroclaw_log::strip_ephemeral_broadcast_marker(&mut value);
    Some(value)
}

/// GET /api/events/history — return buffered recent events as JSON.
pub async fn handle_events_history(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = super::api::require_auth(&state, &headers) {
        return e.into_response();
    }
    Json(history_events_payload(&state.event_buffer)).into_response()
}

fn history_events_payload(buffer: &EventBuffer) -> serde_json::Value {
    serde_json::json!({ "events": history_events(buffer) })
}

fn is_public_sse_event(event: &serde_json::Value) -> bool {
    is_public_event(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use zeroclaw_runtime::observability::{Observer, ObserverEvent};

    #[test]
    fn session_scoped_events_are_not_public_sse_events() {
        let session_event = serde_json::json!({
            "type": "message",
            "session_id": "operator-1",
            "content": "private session notification"
        });
        let global_event = serde_json::json!({
            "type": "tool_call",
            "tool": "shell"
        });

        assert!(!is_public_sse_event(&session_event));
        assert!(is_public_sse_event(&global_event));
    }

    #[test]
    fn history_payload_returns_only_public_events() {
        let buffer = EventBuffer::new(8);
        buffer.push(serde_json::json!({
            "type": "message",
            "session_id": "operator-1",
            "content": "private session notification"
        }));
        buffer.push(serde_json::json!({
            "type": "agent_start",
            "source": "observability",
            "model_provider": "test",
            "model": "test-model"
        }));
        buffer.push(serde_json::json!({
            "type": "gateway_lifecycle",
            "phase": "ready"
        }));

        let payload = history_events_payload(&buffer);
        let events = payload["events"].as_array().expect("events array");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "agent_start");
        assert_eq!(events[1]["type"], "gateway_lifecycle");
    }

    /// Build a broadcast frame stamped the way `zeroclaw_log::record_event`
    /// stamps a credential-bearing login event (ephemeral attrs merged into
    /// `attributes.login`, plus the fail-closed marker).
    fn credential_login_frame() -> serde_json::Value {
        serde_json::json!({
            "source": "observability",
            "attributes": { "login": { "state": "qr", "qr_payload": "SECRET-QR-PAYLOAD" } },
            zeroclaw_log::EPHEMERAL_BROADCAST_MARKER: true,
        })
    }

    #[test]
    fn ephemeral_credential_frame_is_withheld_from_unauthenticated_stream() {
        // Pairing disabled ⇒ the `/api/events` handler skips the bearer check,
        // so the stream is unauthenticated. The credential frame must be
        // withheld entirely rather than fanned out to an anonymous subscriber.
        let frame = credential_login_frame();
        assert!(
            sse_frame_for_stream(frame, /* auth_enforced */ false).is_none(),
            "pairing secret must never ride an unauthenticated /api/events stream"
        );
    }

    #[test]
    fn ephemeral_credential_frame_reaches_authenticated_stream_without_marker() {
        // Pairing enabled ⇒ every subscriber passed the bearer check, so the
        // credential may be delivered; the internal marker is stripped first.
        let delivered =
            sse_frame_for_stream(credential_login_frame(), /* auth_enforced */ true)
                .expect("authenticated stream should receive the credential frame");
        assert_eq!(
            delivered["attributes"]["login"]["qr_payload"], "SECRET-QR-PAYLOAD",
            "authenticated stream still renders the QR payload"
        );
        assert!(
            delivered
                .get(zeroclaw_log::EPHEMERAL_BROADCAST_MARKER)
                .is_none(),
            "internal fail-closed marker must be stripped before delivery"
        );
    }

    #[test]
    fn credential_free_frame_flows_on_unauthenticated_stream() {
        // A lifecycle frame with no ephemeral secret is unmarked and still
        // flows when auth is disabled (unchanged behavior for non-secret data).
        let frame = serde_json::json!({
            "source": "observability",
            "attributes": { "login": { "state": "connected" } },
        });
        assert!(
            sse_frame_for_stream(frame, /* auth_enforced */ false).is_some(),
            "credential-free lifecycle frames are unaffected"
        );
    }

    #[test]
    fn session_scoped_frame_is_withheld_regardless_of_auth() {
        let frame = serde_json::json!({ "type": "message", "session_id": "operator-1" });
        assert!(sse_frame_for_stream(frame.clone(), true).is_none());
        assert!(sse_frame_for_stream(frame, false).is_none());
    }

    #[test]
    fn observability_tagged_events_are_public_even_without_session_id() {
        // After observability frames keep the SSE pathway open even
        // though they would not otherwise carry a session_id discriminator.
        let obs = serde_json::json!({
            "type": "tool_call",
            "source": "observability",
            "tool": "shell",
        });
        assert!(is_public_sse_event(&obs));
    }

    /// Warning follow-up (non-persistent boundary): even if a credential-marked
    /// login frame reached the replay buffer, `/api/events/history` must
    /// withhold it. Pairing secrets are delivery-once — a client that connects
    /// after pairing cannot recover them from history.
    #[test]
    fn history_payload_never_recovers_pairing_credentials() {
        let buffer = EventBuffer::new(8);
        buffer.push(credential_login_frame());
        buffer.push(serde_json::json!({
            "type": "agent_start",
            "source": "observability",
            "model_provider": "test",
            "model": "test-model",
        }));

        let payload = history_events_payload(&buffer);
        let events = payload["events"].as_array().expect("events array");
        assert_eq!(
            events.len(),
            1,
            "credential-bearing frame must be withheld from history: {events:?}"
        );
        assert_eq!(events[0]["type"], "agent_start");
        let dump = payload.to_string();
        assert!(
            !dump.contains("SECRET-QR-PAYLOAD"),
            "history must not expose a pairing secret: {dump}"
        );
        assert!(!dump.contains(zeroclaw_log::EPHEMERAL_BROADCAST_MARKER));
    }

    // ── Route-level `/api/events` credential-boundary regressions ─────────
    //
    // These drive the real axum handler through the router — PairingGuard,
    // bearer parsing, the early 401, subscription wiring, and the routed SSE
    // response — rather than calling `sse_frame_for_stream` in isolation, so
    // they catch the auth check being removed or disconnected from the filter.

    fn events_app(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/api/events", axum::routing::get(handle_sse_events))
            .with_state(state)
    }

    /// Drive the SSE response body, accumulating delivered bytes until `needle`
    /// is seen or the budget elapses (avoids blocking on the keep-alive).
    async fn read_stream_until(
        body: axum::body::Body,
        needle: &str,
        budget: std::time::Duration,
    ) -> String {
        use http_body_util::BodyExt;
        let mut body = body;
        let mut acc = String::new();
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, body.frame()).await {
                Ok(Some(Ok(frame))) => {
                    if let Ok(bytes) = frame.into_data() {
                        acc.push_str(&String::from_utf8_lossy(&bytes));
                        if acc.contains(needle) {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        acc
    }

    fn pairing_guard(
        require: bool,
        tokens: &[&str],
    ) -> Arc<zeroclaw_runtime::security::pairing::PairingGuard> {
        let owned: Vec<String> = tokens.iter().map(|t| (*t).to_string()).collect();
        Arc::new(zeroclaw_runtime::security::pairing::PairingGuard::new(
            require,
            &owned,
            zeroclaw_config::pairing::PairingCodePolicy::default(),
        ))
    }

    /// Pairing disabled ⇒ the handler skips the bearer check, so the stream is
    /// unauthenticated. A credential frame must be withheld end-to-end while a
    /// credential-free frame still flows (proving the stream is live).
    #[tokio::test]
    async fn route_withholds_credential_frame_on_unauthenticated_events_stream() {
        use tower::ServiceExt as _;

        let mut state = crate::api::test_state(zeroclaw_config::schema::Config::default());
        state.pairing = pairing_guard(false, &[]);
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        state.event_tx = tx.clone();

        let response = events_app(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/events")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let _ = tx.send(credential_login_frame());
        let _ = tx.send(serde_json::json!({
            "source": "observability",
            "type": "tool_call",
            "tool": "SENTINEL-LIVE",
        }));

        let body = read_stream_until(
            response.into_body(),
            "SENTINEL-LIVE",
            std::time::Duration::from_secs(2),
        )
        .await;
        assert!(
            body.contains("SENTINEL-LIVE"),
            "the stream must stay live for non-secret frames: {body:?}"
        );
        assert!(
            !body.contains("SECRET-QR-PAYLOAD"),
            "pairing secret must never reach an unauthenticated /api/events client: {body:?}"
        );
    }

    /// Pairing enabled ⇒ the handler enforces the bearer check and returns 401
    /// before any subscription for a missing or bad token.
    #[tokio::test]
    async fn route_returns_401_when_pairing_enabled_without_valid_token() {
        use tower::ServiceExt as _;

        // No token.
        let mut state = crate::api::test_state(zeroclaw_config::schema::Config::default());
        state.pairing = pairing_guard(true, &["valid-token"]);
        let response = events_app(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/events")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);

        // Bad token.
        let mut state = crate::api::test_state(zeroclaw_config::schema::Config::default());
        state.pairing = pairing_guard(true, &["valid-token"]);
        let response = events_app(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/events")
                    .header(axum::http::header::AUTHORIZATION, "Bearer wrong-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    /// Pairing enabled + valid bearer ⇒ every subscriber is authenticated, so
    /// the credential is delivered — with the internal marker stripped.
    #[tokio::test]
    async fn route_delivers_credential_without_marker_to_authenticated_client() {
        use tower::ServiceExt as _;

        let mut state = crate::api::test_state(zeroclaw_config::schema::Config::default());
        state.pairing = pairing_guard(true, &["valid-token"]);
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        state.event_tx = tx.clone();

        let response = events_app(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/events")
                    .header(axum::http::header::AUTHORIZATION, "Bearer valid-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let _ = tx.send(credential_login_frame());

        let body = read_stream_until(
            response.into_body(),
            "SECRET-QR-PAYLOAD",
            std::time::Duration::from_secs(2),
        )
        .await;
        assert!(
            body.contains("SECRET-QR-PAYLOAD"),
            "an authenticated client should receive the QR payload: {body:?}"
        );
        assert!(
            !body.contains(zeroclaw_log::EPHEMERAL_BROADCAST_MARKER),
            "the internal fail-closed marker must be stripped before delivery: {body:?}"
        );
    }

    #[test]
    fn the_chat_turn_boundary_is_not_a_public_sse_event() {
        // The frame is built by the production constructor, not by hand: a
        // hand-written fixture would keep passing while the gateway broadcast
        // a sessionless frame that leaks to every observer.
        let chat_turn_boundary = crate::ws::agent_start_frame("sess-1", "test", "test-model");
        let observability_bracket = serde_json::json!({
            "type": "agent_start",
            "source": "observability",
            "model_provider": "test",
            "model": "test-model",
        });

        assert!(
            !is_public_sse_event(&chat_turn_boundary),
            "the turn boundary must not reach the public event stream: {chat_turn_boundary}"
        );
        assert!(is_public_sse_event(&observability_bracket));
    }
    fn make_broadcast() -> (
        Arc<BroadcastObserver>,
        tokio::sync::broadcast::Receiver<serde_json::Value>,
        Arc<EventBuffer>,
    ) {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let buffer = Arc::new(EventBuffer::new(16));
        let obs = Arc::new(BroadcastObserver::new(tx, buffer.clone()));
        (obs, rx, buffer)
    }

    static HOOK_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn agent_lifecycle_events_reach_history_payload_via_broadcast_hook() {
        let _guard = HOOK_TEST_LOCK.lock();

        zeroclaw_runtime::observability::clear_broadcast_hook();

        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let buffer = Arc::new(EventBuffer::new(16));
        let bo: Arc<dyn Observer> = Arc::new(BroadcastObserver::new(tx, buffer.clone()));
        zeroclaw_runtime::observability::set_broadcast_hook(bo);

        // Same factory call site as the channel orchestrator and agent loop.
        let cfg = zeroclaw_config::schema::ObservabilityConfig {
            backend: zeroclaw_config::schema::ObservabilityBackend::None,
            ..Default::default()
        };
        let observer = zeroclaw_runtime::observability::create_observer(&cfg);

        observer.record_event(&ObserverEvent::AgentStart {
            model_provider: "p".into(),
            model: "m".into(),
            channel: Some("telegram".into()),
            agent_alias: Some("default".into()),
            turn_id: Some("turn-1".into()),
        });
        observer.record_event(&ObserverEvent::AgentEnd {
            model_provider: "p".into(),
            model: "m".into(),
            duration: std::time::Duration::from_millis(5),
            tokens_used: None,
            cost_usd: None,
            channel: Some("telegram".into()),
            agent_alias: Some("default".into()),
            turn_id: Some("turn-1".into()),
        });

        let payload = history_events_payload(&buffer);
        let events = payload["events"].as_array().expect("events array");
        assert_eq!(
            events.len(),
            2,
            "both brackets must be retained: {events:?}"
        );
        assert_eq!(events[0]["type"], "agent_start");
        assert_eq!(events[1]["type"], "agent_end");
        for event in events {
            assert_eq!(event["source"], "observability");
            assert!(
                event["timestamp"].is_string(),
                "history frames must carry a timestamp: {event}"
            );
            assert_eq!(event["turn_id"], "turn-1");
            assert_eq!(event["channel"], "telegram");
        }

        zeroclaw_runtime::observability::clear_broadcast_hook();
    }

    #[test]
    fn factory_observer_events_reach_broadcast_hook() {
        let _guard = HOOK_TEST_LOCK.lock();

        zeroclaw_runtime::observability::clear_broadcast_hook();

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let buffer = Arc::new(EventBuffer::new(16));
        let bo: Arc<dyn Observer> = Arc::new(BroadcastObserver::new(tx, buffer.clone()));
        zeroclaw_runtime::observability::set_broadcast_hook(bo);

        // Same factory call site as `process_message` in the agent loop.
        let cfg = zeroclaw_config::schema::ObservabilityConfig {
            backend: zeroclaw_config::schema::ObservabilityBackend::None,
            ..Default::default()
        };
        let observer = zeroclaw_runtime::observability::create_observer(&cfg);

        observer.record_event(&ObserverEvent::ToolCall {
            parent_agent_alias: None,
            tool: "shell".into(),
            tool_call_id: None,
            duration: std::time::Duration::from_millis(7),
            success: true,
            arguments: None,
            result: None,
            channel: None,
            agent_alias: None,
            turn_id: None,
        });

        let value = rx
            .try_recv()
            .expect("factory-built observer event must reach the SSE broadcast channel");
        assert_eq!(value["type"], "tool_call");
        assert_eq!(value["tool"], "shell");
        assert_eq!(value["success"], true);

        let snap = buffer.snapshot();
        assert_eq!(
            snap.len(),
            1,
            "broadcast events must also land in the buffer"
        );

        zeroclaw_runtime::observability::clear_broadcast_hook();
    }

    #[test]
    fn broadcast_observer_tags_every_event_with_observability_source() {
        // The chat-WS filter relies on this tag as a defense-in-depth check
        // (any future emitter that forgets to set session_id still gets
        // routed correctly). Cover every variant the observer broadcasts.
        let (obs, mut rx, _buffer) = make_broadcast();

        let cases: Vec<ObserverEvent> = vec![
            ObserverEvent::LlmRequest {
                parent_agent_alias: None,
                model_provider: "p".into(),
                model: "m".into(),
                messages_count: 0,
                channel: None,
                agent_alias: None,
                turn_id: None,
            },
            ObserverEvent::ToolCall {
                parent_agent_alias: None,
                tool: "shell".into(),
                tool_call_id: None,
                duration: std::time::Duration::from_millis(1),
                success: true,
                arguments: None,
                result: None,
                channel: None,
                agent_alias: None,
                turn_id: None,
            },
            ObserverEvent::ToolCallStart {
                parent_agent_alias: None,
                tool: "shell".into(),
                tool_call_id: None,
                arguments: None,
                channel: None,
                agent_alias: None,
                turn_id: None,
            },
            ObserverEvent::Error {
                component: "any".into(),
                message: "boom".into(),
            },
            ObserverEvent::AgentStart {
                model_provider: "p".into(),
                model: "m".into(),
                channel: None,
                agent_alias: None,
                turn_id: None,
            },
            ObserverEvent::AgentEnd {
                model_provider: "p".into(),
                model: "m".into(),
                duration: std::time::Duration::from_millis(1),
                tokens_used: None,
                cost_usd: None,
                channel: None,
                agent_alias: None,
                turn_id: None,
            },
        ];
        for ev in cases {
            obs.record_event(&ev);
            let v = rx.try_recv().expect("event must broadcast");
            assert_eq!(
                v["source"], "observability",
                "every BroadcastObserver event must be tagged source=observability: {v}"
            );
        }
    }

    #[test]
    fn broadcast_agent_end_includes_turn_metadata_and_token_total() {
        let (obs, mut rx, _buffer) = make_broadcast();

        obs.record_event(&ObserverEvent::AgentEnd {
            model_provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            duration: std::time::Duration::from_millis(42),
            tokens_used: Some(zeroclaw_api::observability_traits::TurnTokenUsage {
                input_tokens: 12,
                output_tokens: 34,
            }),
            cost_usd: Some(0.001),
            channel: Some("wss".into()),
            agent_alias: Some("default".into()),
            turn_id: Some("turn-1".into()),
        });

        let value = rx.try_recv().expect("event should be broadcast");
        assert_eq!(value["type"], "agent_end");
        assert_eq!(value["source"], "observability");
        assert_eq!(value["tokens_used"], 46);
        assert_eq!(value["input_tokens"], 12);
        assert_eq!(value["output_tokens"], 34);
        assert_eq!(value["channel"], "wss");
        assert_eq!(value["agent_alias"], "default");
        assert_eq!(value["turn_id"], "turn-1");
    }

    #[test]
    fn unmapped_events_are_skipped() {
        let (obs, mut rx, buffer) = make_broadcast();

        obs.record_event(&ObserverEvent::HeartbeatTick);

        assert!(rx.try_recv().is_err(), "heartbeat should not broadcast");
        assert!(buffer.snapshot().is_empty());
    }

    #[test]
    fn history_trimmed_event_is_broadcast_with_cut_accounting() {
        let (obs, mut rx, _buffer) = make_broadcast();

        obs.record_event(&ObserverEvent::HistoryTrimmed {
            dropped_messages: 12,
            kept_turns: 1,
            reason: "context token budget exceeded".into(),
            channel: Some("wss".into()),
            agent_alias: Some("trimtest".into()),
            turn_id: Some("turn-1".into()),
            // Token accounting is optional; this fixture exercises the cut
            // accounting that is always on the frame.
            token_budget: None,
            tokens_before: None,
            tokens_after: None,
            tokens_before_source: None,
            tokens_after_source: None,
            unsatisfiable_floor: None,
        });

        let value = rx.try_recv().expect("history_trimmed must broadcast");
        assert_eq!(value["type"], "history_trimmed");
        assert_eq!(value["source"], "observability");
        assert_eq!(value["dropped_messages"], 12);
        assert_eq!(value["kept_turns"], 1);
        assert_eq!(value["reason"], "context token budget exceeded");
        assert_eq!(value["channel"], "wss");
        assert_eq!(value["agent_alias"], "trimtest");
        assert_eq!(value["turn_id"], "turn-1");
        assert!(is_public_sse_event(&value));
    }

    #[test]
    fn tool_call_start_event_is_broadcast() {
        let (obs, mut rx, _buffer) = make_broadcast();

        obs.record_event(&ObserverEvent::ToolCallStart {
            parent_agent_alias: None,
            tool: "mcp_filesystem__read_file".into(),
            tool_call_id: None,
            arguments: None,
            channel: None,
            agent_alias: None,
            turn_id: None,
        });

        let value = rx.try_recv().expect("event should be broadcast");
        assert_eq!(value["type"], "tool_call_start");
        assert_eq!(value["tool"], "mcp_filesystem__read_file");
    }

    #[test]
    fn tool_call_event_is_broadcast_and_buffered() {
        let (obs, mut rx, buffer) = make_broadcast();

        obs.record_event(&ObserverEvent::ToolCall {
            parent_agent_alias: None,
            tool: "shell".into(),
            tool_call_id: None,
            duration: std::time::Duration::from_millis(42),
            success: true,
            arguments: None,
            result: None,
            channel: None,
            agent_alias: None,
            turn_id: None,
        });

        let value = rx.try_recv().expect("event should be broadcast");
        assert_eq!(value["type"], "tool_call");
        assert_eq!(value["tool"], "shell");
        assert_eq!(value["success"], true);

        let snap = buffer.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0]["type"], "tool_call");
    }

    #[test]
    fn event_buffer_recovers_after_a_poisoned_writer() {
        let buffer = Arc::new(EventBuffer::new(4));
        let poisoned = buffer.clone();
        let result = std::thread::spawn(move || {
            // Panic while the buffer's lock is held, poisoning it.
            poisoned.with_locked_queue(|queue| {
                queue.push_back(serde_json::json!({"sequence": 1}));
                panic!("poison event buffer");
            });
        })
        .join();
        assert!(result.is_err());

        buffer.push(serde_json::json!({"sequence": 2}));
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0]["sequence"], 1);
        assert_eq!(snapshot[1]["sequence"], 2);
    }

}
