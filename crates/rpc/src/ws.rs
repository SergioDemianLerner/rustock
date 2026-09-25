//! The WebSocket JSON-RPC transport.
//!
//! rskj serves this on its own port (`rpc.providers.web.ws.port`, default
//! 4445) and leaves it **disabled by default**; rustock matches both, because
//! a subscription socket is a resource an operator should opt into.
//!
//! Everything a plain HTTP request can do works here too — the same
//! `dispatch` handles it — plus `eth_subscribe` and `eth_unsubscribe`, which
//! only mean anything on a connection that stays open.

use crate::server::{dispatch_rpc, RpcState};
use crate::subscribe::{notifications_for, parse_subscription, ChainEvent, Subscription};
use crate::types::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use tracing::{debug, info, warn};

/// Start the WebSocket server. Returns when the listener fails.
pub async fn start_ws_server(host: &str, port: u16, state: RpcState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", any(upgrade))
        .route("/websocket", any(upgrade))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    info!("RPC WebSocket server listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
pub(crate) async fn upgrade_for_test(
    ws: WebSocketUpgrade,
    state: State<RpcState>,
) -> Response {
    upgrade(ws, state).await
}

async fn upgrade(ws: WebSocketUpgrade, State(state): State<RpcState>) -> Response {
    ws.on_upgrade(move |socket| connection(socket, state))
}

/// One client connection: its own subscriptions, its own view of the event
/// stream.
async fn connection(socket: WebSocket, state: RpcState) {
    let (mut sink, mut stream) = socket.split();
    let mut subscriptions: HashMap<String, Subscription> = HashMap::new();
    let mut next_id: u64 = 1;

    // A receiver is taken even when the node publishes nothing, so that
    // `eth_subscribe` works on a node without an event source and simply
    // never fires.
    let mut events = match &state.events {
        Some(tx) => Some(tx.subscribe()),
        None => None,
    };

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                let text = match message {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => match String::from_utf8(b.to_vec()) {
                        Ok(t) => t,
                        Err(_) => continue,
                    },
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => break,
                };
                let reply =
                    handle_text(&text, &state, &mut subscriptions, &mut next_id).await;
                if let Some(reply) = reply {
                    if sink.send(Message::text(reply)).await.is_err() {
                        break;
                    }
                }
            }

            event = async {
                match events.as_mut() {
                    Some(rx) => rx.recv().await,
                    // No event source: park forever rather than spinning.
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    Ok(event) => {
                        if !fan_out(&mut sink, &subscriptions, &event).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // The subscriber could not keep up. Dropping the
                        // connection is the honest outcome: a subscriber that
                        // silently missed `n` events would carry a wrong view
                        // of the chain and never know. It can reconnect and
                        // resynchronise from the RPC.
                        warn!(
                            target: "rustock::rpc",
                            "Disconnecting a WebSocket subscriber that fell {n} events behind"
                        );
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    debug!(target: "rustock::rpc", "WebSocket connection closed");
}

/// Send every notification this event produces for this connection.
/// Returns false when the socket is gone.
async fn fan_out(
    sink: &mut futures::stream::SplitSink<WebSocket, Message>,
    subscriptions: &HashMap<String, Subscription>,
    event: &ChainEvent,
) -> bool {
    for (id, subscription) in subscriptions {
        for note in notifications_for(subscription, id, event) {
            if sink.send(Message::text(note.to_string())).await.is_err() {
                return false;
            }
        }
    }
    true
}

async fn handle_text(
    text: &str,
    state: &RpcState,
    subscriptions: &mut HashMap<String, Subscription>,
    next_id: &mut u64,
) -> Option<String> {
    let request: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return Some(
                JsonRpcResponse::error(Value::Null, PARSE_ERROR, format!("Parse error: {e}"))
                    .to_text(),
            )
        }
    };

    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Array(vec![]));

    match method {
        "eth_subscribe" => Some(match parse_subscription(&params) {
            Ok(subscription) => {
                // rskj's `SubscriptionId` is 16 random bytes; the id only has
                // to be unique on the connection, and a counter makes a
                // failing test readable.
                let sid = format!("0x{:016x}", *next_id);
                *next_id += 1;
                subscriptions.insert(sid.clone(), subscription);
                JsonRpcResponse::success(id, json!(sid)).to_text()
            }
            Err(why) => JsonRpcResponse::error(id, INVALID_PARAMS, why).to_text(),
        }),
        "eth_unsubscribe" => {
            let removed = params
                .get(0)
                .and_then(|v| v.as_str())
                .map(|s| subscriptions.remove(s).is_some())
                .unwrap_or(false);
            Some(JsonRpcResponse::success(id, json!(removed)).to_text())
        }
        // Everything else is an ordinary request on a long-lived socket.
        _ => {
            let response = dispatch_rpc(state, parse_request(&request)).await;
            Some(response.to_text())
        }
    }
}

fn parse_request(value: &Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: value.get("jsonrpc").and_then(|v| v.as_str()).map(str::to_string),
        method: value.get("method").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        params: value.get("params").cloned().unwrap_or(Value::Array(vec![])),
        id: value.get("id").cloned(),
    }
}
