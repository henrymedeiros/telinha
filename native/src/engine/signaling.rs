//! Client for the PeerJS signaling server (the same one the site uses).
//!
//! Protocol: WebSocket at `wss://SERVER/peer/peerjs?key=peerjs&id=..&token=..`,
//! JSON messages `{type, src|dst, payload}`. Heartbeat every 5 s.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Ws;

#[derive(Debug, Clone)]
pub enum Signal {
    Open,
    IdTaken,
    Error(String),
    Offer { src: String, payload: Value },
    Answer { src: String, payload: Value },
    Candidate { src: String, payload: Value },
    /// The destination does not exist or left (PeerJS calls it EXPIRE/LEAVE).
    Gone { peer: String },
    /// The connection dropped and the first try to get it back failed; more follow.
    Reconnecting,
    /// The connection is gone for good (after `GIVE_UP` of trying).
    Closed,
}

#[derive(Clone)]
pub struct Signaling {
    out: mpsc::UnboundedSender<String>,
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const HEARTBEAT: Duration = Duration::from_secs(5);
/// Nothing read for this long, not even the pong to our own ping every
/// `HEARTBEAT`, means the socket died without saying so.
const READ_IDLE: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Reconnect waits: 0.25 s, doubling up to 8 s, for a minute in all. A server
/// restart takes seconds; past a minute the channel is better ended honestly.
const RETRY_FIRST: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(8);
const GIVE_UP: Duration = Duration::from_secs(60);
/// Messages kept while the connection is down (the newest ones).
const HELD_MAX: usize = 64;
/// Largest signaling message read. An offer with all its candidates is a few
/// kilobytes; the library's default (64 MiB) lets anyone who can reach this id
/// make the app parse and copy that much.
const MAX_MESSAGE: usize = 1 << 20;

/// How long to wait before reconnect try number `attempt` (0 is the first),
/// or `None` once the connection has been down too long to keep trying.
fn retry_delay(attempt: u32, down_for: Duration) -> Option<Duration> {
    if down_for >= GIVE_UP {
        return None;
    }
    Some(RETRY_FIRST.saturating_mul(1 << attempt.min(16)).min(RETRY_MAX))
}

/// One server message as a `Signal` (`None`: not for us, or not valid).
fn parse(text: &str) -> Option<Signal> {
    let v = serde_json::from_str::<Value>(text).ok()?;
    let src = v["src"].as_str().unwrap_or_default().to_owned();
    let payload = v["payload"].clone();
    Some(match v["type"].as_str().unwrap_or_default() {
        "OPEN" => Signal::Open,
        "ID-TAKEN" => Signal::IdTaken,
        "ERROR" | "INVALID-KEY" => Signal::Error(payload["msg"].as_str().unwrap_or("erro no servidor").to_owned()),
        "OFFER" => Signal::Offer { src, payload },
        "ANSWER" => Signal::Answer { src, payload },
        "CANDIDATE" => Signal::Candidate { src, payload },
        "EXPIRE" | "LEAVE" => Signal::Gone { peer: src },
        _ => return None,
    })
}

async fn open(url: &str) -> Result<Socket, String> {
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async_with_config(url, Some(config), false)).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(e)) => Err(format!("sinalização: {e}")),
        Err(_) => Err("sinalização: o servidor demorou demais para responder".into()),
    }
}

/// How one socket's life ended.
enum End {
    /// Nobody holds the `Signaling` any more (the session ended).
    Dropped,
    /// The server gave the id to someone else: reconnecting cannot help.
    Taken,
    /// The connection broke or went silent.
    Lost,
}

async fn run_socket(ws: Socket, rx: &mut mpsc::UnboundedReceiver<String>, held: &mut VecDeque<String>, events: &mpsc::Sender<Signal>) -> End {
    let (mut write, mut read) = ws.split();
    // What was asked for while the connection was down goes first.
    while let Some(text) = held.pop_front() {
        if write.send(Ws::Text(text.into())).await.is_err() {
            return End::Lost;
        }
    }
    let mut beat = tokio::time::interval(HEARTBEAT);
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(text) => if write.send(Ws::Text(text.into())).await.is_err() { return End::Lost },
                None => { let _ = write.close().await; return End::Dropped }
            },
            _ = beat.tick() => {
                if heard.elapsed() > READ_IDLE {
                    return End::Lost;
                }
                // HEARTBEAT keeps the server from dropping us; it is never answered.
                // The ping is: any WebSocket server sends a pong back, which `heard` sees.
                if write.send(Ws::Text(r#"{"type":"HEARTBEAT"}"#.into())).await.is_err() || write.send(Ws::Ping(Default::default())).await.is_err() {
                    return End::Lost;
                }
            }
            frame = read.next() => {
                let Some(Ok(msg)) = frame else { return End::Lost };
                heard = Instant::now();
                let Ws::Text(text) = msg else { continue };
                let Some(signal) = parse(&text) else { continue };
                let taken = matches!(signal, Signal::IdTaken);
                if events.send(signal).await.is_err() {
                    return End::Dropped;
                }
                if taken {
                    return End::Taken;
                }
            }
        }
    }
}

/// Waits `wait`, keeping what the session asks to send meanwhile. `false`: the session ended.
async fn hold(rx: &mut mpsc::UnboundedReceiver<String>, held: &mut VecDeque<String>, wait: Duration) -> bool {
    let until = tokio::time::Instant::now() + wait;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(until) => return true,
            msg = rx.recv() => match msg {
                Some(text) => {
                    if held.len() >= HELD_MAX {
                        held.pop_front();
                    }
                    held.push_back(text);
                }
                None => return false,
            },
        }
    }
}

/// Owns the connection for as long as the session holds the `Signaling`:
/// when it breaks, opens it again with the same id and token.
async fn supervise(url: String, mut ws: Socket, mut rx: mpsc::UnboundedReceiver<String>, events: mpsc::Sender<Signal>) {
    let mut held = VecDeque::new();
    loop {
        match run_socket(ws, &mut rx, &mut held, &events).await {
            End::Dropped | End::Taken => return,
            End::Lost => {}
        }
        tracing::warn!("signaling connection lost; reconnecting");
        let lost = Instant::now();
        let mut attempt = 0u32;
        ws = loop {
            let Some(wait) = retry_delay(attempt, lost.elapsed()) else {
                tracing::warn!("signaling did not come back; giving up");
                let _ = events.send(Signal::Closed).await;
                return;
            };
            if !hold(&mut rx, &mut held, wait).await {
                return;
            }
            match open(&url).await {
                Ok(ws) => break ws,
                Err(e) => {
                    tracing::warn!(attempt, "{e}");
                    if attempt == 0 {
                        let _ = events.send(Signal::Reconnecting).await;
                    }
                    attempt += 1;
                }
            }
        };
        tracing::info!(after_ms = lost.elapsed().as_millis() as u64, "signaling connection is back");
    }
}

impl Signaling {
    /// Connects and forwards each message to `events`. If the connection later
    /// breaks it is opened again with the same id and token (see `supervise`);
    /// `Signal::Closed` only comes when that failed for `GIVE_UP`.
    pub async fn connect(server: &str, id: &str, events: mpsc::Sender<Signal>) -> Result<Self, String> {
        let token: String = (0..10).map(|_| char::from(b'a' + rand::random_range(0..26u8))).collect();
        let scheme = if is_local(server) { "ws" } else { "wss" };
        let url = format!("{scheme}://{server}/peer/peerjs?key=peerjs&id={id}&token={token}&version=1.5.5");
        let ws = open(&url).await?;
        let (out, rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(supervise(url, ws, rx, events));
        Ok(Self { out })
    }

    pub fn send(&self, kind: &str, dst: &str, payload: Value) {
        let _ = self.out.send(json!({ "type": kind, "dst": dst, "payload": payload }).to_string());
    }
}

/// A signaling server on this machine (tools/latency/sinal.mjs, for tests)
/// speaks plain HTTP and WebSocket; every real one is TLS.
fn is_local(server: &str) -> bool {
    let host = server.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map_or(server, |(h, _)| h);
    host == "localhost" || host == "127.0.0.1"
}

/// ICE servers (STUN and TURN with temporary credentials) from our own server.
pub async fn ice_servers(server: &str) -> Result<Vec<webrtc::peer_connection::RTCIceServer>, String> {
    let url = format!("{}://{server}/api/ice", if is_local(server) { "http" } else { "https" });
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(8)).build().map_err(|e| e.to_string())?;
    let mut last = String::new();
    for _ in 0..2 {
        match client.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => {
                let v: Value = r.json().await.map_err(|e| e.to_string())?;
                let list = v["iceServers"].as_array().cloned().unwrap_or_default();
                return Ok(list
                    .into_iter()
                    .map(|s| webrtc::peer_connection::RTCIceServer {
                        urls: match &s["urls"] {
                            Value::String(u) => vec![u.clone()],
                            Value::Array(a) => a.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect(),
                            _ => vec![],
                        },
                        username: s["username"].as_str().unwrap_or_default().to_owned(),
                        credential: s["credential"].as_str().unwrap_or_default().to_owned(),
                        ..Default::default()
                    })
                    .collect());
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("não consegui falar com o servidor do Telinha ({last})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnects_quickly_then_every_eight_seconds_for_a_minute() {
        let s = Duration::from_secs;
        assert_eq!(retry_delay(0, s(0)), Some(Duration::from_millis(250)));
        assert_eq!(retry_delay(1, s(1)), Some(Duration::from_millis(500)));
        assert_eq!(retry_delay(4, s(4)), Some(s(4)));
        assert_eq!(retry_delay(5, s(8)), Some(s(8)));
        // Never longer than that, however many tries.
        assert_eq!(retry_delay(40, s(59)), Some(s(8)));
        // A minute down: give up, even on what would be an early try.
        assert_eq!(retry_delay(0, s(60)), None);
        assert_eq!(retry_delay(9, s(75)), None);
    }

    #[test]
    fn reads_the_server_messages_the_session_uses() {
        assert!(matches!(parse(r#"{"type":"OPEN"}"#), Some(Signal::Open)));
        assert!(matches!(parse(r#"{"type":"ID-TAKEN","payload":{"msg":"ID is taken"}}"#), Some(Signal::IdTaken)));
        assert!(matches!(parse(r#"{"type":"EXPIRE","src":"telinha-canal-v1-4821"}"#), Some(Signal::Gone { peer }) if peer == "telinha-canal-v1-4821"));
        assert!(matches!(parse(r#"{"type":"OFFER","src":"abc","payload":{"connectionId":"dc_1"}}"#), Some(Signal::Offer { src, payload }) if src == "abc" && payload["connectionId"] == "dc_1"));
        assert!(matches!(parse(r#"{"type":"ERROR","payload":{"msg":"x"}}"#), Some(Signal::Error(m)) if m == "x"));
        // Unknown types, heartbeats echoed back and broken JSON are skipped.
        assert!(parse(r#"{"type":"HEARTBEAT"}"#).is_none());
        assert!(parse("not json").is_none());
    }
}
