//! WebSocket-based STT provider.
//!
//! Connects to an external STT server (e.g. Voxtral / mlx-whisper)
//! that performs VAD + speech recognition and returns [`SttEvent`]s.
//!
//! ## Protocol
//!
//! 1. Client connects and sends a JSON **config** frame.
//! 2. Client streams PCM s16le binary frames.
//! 3. Server streams JSON events (`speech_start`, `partial`, `final`, `speech_end`).
//! 4. Client sends `{"type":"end_of_stream"}` to signal completion.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

use crate::config::VoiceSttWsConfig;
use crate::voice::provider::{SttEvent, SttProvider, SttSession};

/// Initial config message sent to the STT server on connect.
#[derive(Debug, Serialize)]
struct WsConfigMessage {
    #[serde(rename = "type")]
    msg_type: &'static str,
    sample_rate: u32,
    channels: u8,
    encoding: &'static str,
    language: &'static str,
    interim_results: bool,
    temperature: f64,
}

/// Raw JSON message from the STT server.
///
/// Intermediate representation used to map `is_final` boolean protocol
/// to the typed [`SttEvent`] enum.
#[derive(Debug, Deserialize)]
struct WsServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    timestamp_ms: Option<u64>,
    #[serde(default)]
    duration_ms: Option<f64>,
    /// Some servers send a single `transcript` event with `is_final` flag
    /// instead of separate `partial` / `final` types.
    #[serde(default)]
    is_final: Option<bool>,
}

impl WsServerMessage {
    /// Convert the raw server message into a typed [`SttEvent`].
    fn into_stt_event(self) -> Option<SttEvent> {
        match self.msg_type.as_str() {
            "speech_start" => Some(SttEvent::SpeechStart {
                timestamp_ms: self.timestamp_ms.unwrap_or(0),
            }),
            "partial" => Some(SttEvent::Partial {
                text: self.text.unwrap_or_default(),
            }),
            "final" => Some(SttEvent::Final {
                text: self.text.unwrap_or_default(),
                language: self.language.unwrap_or_else(|| "ja".to_string()),
                confidence: self.confidence.unwrap_or(1.0),
                duration_ms: self.duration_ms.unwrap_or(0.0),
            }),
            "speech_end" => Some(SttEvent::SpeechEnd {
                timestamp_ms: self.timestamp_ms.unwrap_or(0),
                duration_ms: self.duration_ms.unwrap_or(0.0),
            }),
            // Handle `transcript` events with `is_final` flag.
            "transcript" => {
                let text = self.text.unwrap_or_default();
                if self.is_final.unwrap_or(false) {
                    Some(SttEvent::Final {
                        text,
                        language: self.language.unwrap_or_else(|| "ja".to_string()),
                        confidence: self.confidence.unwrap_or(1.0),
                        duration_ms: self.duration_ms.unwrap_or(0.0),
                    })
                } else {
                    Some(SttEvent::Partial { text })
                }
            }
            other => {
                debug!("ignoring unknown STT server message type: {other}");
                None
            }
        }
    }
}

// ── Provider ─────────────────────────────────────────────────────

/// WebSocket STT provider.
pub struct WsSttProvider {
    config: VoiceSttWsConfig,
}

impl WsSttProvider {
    pub fn new(config: VoiceSttWsConfig) -> Self {
        Self { config }
    }

    /// Connect to the STT WebSocket with retry.
    async fn connect_with_retry(
        &self,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let max_attempts = self.config.max_reconnect_attempts.max(1);
        let base_interval = Duration::from_millis(self.config.reconnect_interval_ms);

        for attempt in 0..max_attempts {
            match connect_async(&self.config.endpoint).await {
                Ok((ws_stream, _)) => {
                    if attempt > 0 {
                        info!(
                            "STT WebSocket connected after {} retries",
                            attempt
                        );
                    } else {
                        debug!("STT WebSocket connected to {}", self.config.endpoint);
                    }
                    return Ok(ws_stream);
                }
                Err(e) => {
                    let remaining = max_attempts - attempt - 1;
                    if remaining == 0 {
                        return Err(e).context(format!(
                            "failed to connect to STT server at {} after {max_attempts} attempts",
                            self.config.endpoint
                        ));
                    }
                    let backoff = base_interval * 2u32.saturating_pow(attempt);
                    warn!(
                        attempt = attempt + 1,
                        remaining,
                        backoff_ms = backoff.as_millis(),
                        "STT WebSocket connect failed: {e}, retrying…"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        }

        unreachable!()
    }
}

#[async_trait]
impl SttProvider for WsSttProvider {
    async fn connect(&self) -> Result<Box<dyn SttSession>> {
        let ws_stream = self.connect_with_retry().await?;
        let (mut sink, stream) = ws_stream.split();

        // Send initial config.
        let config_msg = WsConfigMessage {
            msg_type: "config",
            sample_rate: 48000,
            channels: 1,
            encoding: "pcm_s16le",
            language: "ja",
            interim_results: true,
            temperature: self.config.temperature,
        };
        let json = serde_json::to_string(&config_msg)?;
        sink.send(Message::Text(json)).await?;
        debug!("sent STT config: {:?}", config_msg);

        Ok(Box::new(WsSttSession { sink, stream }))
    }

    fn name(&self) -> &str {
        "ws"
    }
}

// ── Session ──────────────────────────────────────────────────────

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// A single WebSocket STT session.
struct WsSttSession {
    sink: WsSink,
    stream: WsStream,
}

/// Convert PCM f32 samples (range -1.0..1.0) to s16le bytes.
pub(crate) fn pcm_f32_to_s16le(samples: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let val = (clamped * 32767.0) as i16;
        buf.extend_from_slice(&val.to_le_bytes());
    }
    buf
}

#[async_trait]
impl SttSession for WsSttSession {
    async fn send_audio(&mut self, audio: &[f32]) -> Result<()> {
        let bytes = pcm_f32_to_s16le(audio);
        self.sink
            .send(Message::Binary(bytes))
            .await
            .context("failed to send audio to STT server")?;
        Ok(())
    }

    async fn recv_event(&mut self) -> Result<Option<SttEvent>> {
        loop {
            match self.stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    let msg: WsServerMessage = serde_json::from_str(&text)
                        .with_context(|| {
                            format!("failed to parse STT server message: {text}")
                        })?;
                    if let Some(event) = msg.into_stt_event() {
                        return Ok(Some(event));
                    }
                    // Unknown type — loop to next message.
                }
                Some(Ok(Message::Close(_))) => {
                    debug!("STT WebSocket closed by server");
                    return Ok(None);
                }
                Some(Ok(Message::Ping(data))) => {
                    // Respond to pings to keep connection alive.
                    let _ = self.sink.send(Message::Pong(data)).await;
                }
                Some(Ok(_)) => {
                    // Ignore other message types (Binary, Pong, Frame).
                }
                Some(Err(e)) => {
                    error!("STT WebSocket error: {e}");
                    return Err(e.into());
                }
                None => {
                    debug!("STT WebSocket stream ended");
                    return Ok(None);
                }
            }
        }
    }

    async fn close(&mut self) -> Result<()> {
        // Send end_of_stream signal.
        let eos = r#"{"type":"end_of_stream"}"#.to_string();
        if let Err(e) = self.sink.send(Message::Text(eos)).await {
            debug!("failed to send end_of_stream (connection may already be closed): {e}");
        }
        // Close the WebSocket.
        if let Err(e) = self.sink.close().await {
            debug!("failed to close STT WebSocket: {e}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── PCM f32 → s16le conversion ──────────────────────────────

    #[test]
    fn pcm_f32_to_s16le_silence() {
        let samples = vec![0.0f32; 4];
        let bytes = pcm_f32_to_s16le(&samples);
        assert_eq!(bytes.len(), 8); // 4 samples × 2 bytes
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn pcm_f32_to_s16le_max_positive() {
        let bytes = pcm_f32_to_s16le(&[1.0]);
        let val = i16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(val, 32767); // i16::MAX
    }

    #[test]
    fn pcm_f32_to_s16le_max_negative() {
        let bytes = pcm_f32_to_s16le(&[-1.0]);
        let val = i16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(val, -32767); // -(32767), not i16::MIN
    }

    #[test]
    fn pcm_f32_to_s16le_clamps_overflow() {
        let bytes_over = pcm_f32_to_s16le(&[2.0]);
        let bytes_max = pcm_f32_to_s16le(&[1.0]);
        assert_eq!(bytes_over, bytes_max);

        let bytes_under = pcm_f32_to_s16le(&[-2.0]);
        let bytes_min = pcm_f32_to_s16le(&[-1.0]);
        assert_eq!(bytes_under, bytes_min);
    }

    #[test]
    fn pcm_f32_to_s16le_half_value() {
        let bytes = pcm_f32_to_s16le(&[0.5]);
        let val = i16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(val, 16383); // (0.5 * 32767.0) as i16
    }

    // ── JSON parsing ────────────────────────────────────────────

    #[test]
    fn parse_speech_start() {
        let json = r#"{"type":"speech_start","timestamp_ms":100}"#;
        let msg: WsServerMessage = serde_json::from_str(json).unwrap();
        let event = msg.into_stt_event().unwrap();
        match event {
            SttEvent::SpeechStart { timestamp_ms } => assert_eq!(timestamp_ms, 100),
            _ => panic!("expected SpeechStart"),
        }
    }

    #[test]
    fn parse_partial() {
        let json = r#"{"type":"partial","text":"こんに"}"#;
        let msg: WsServerMessage = serde_json::from_str(json).unwrap();
        let event = msg.into_stt_event().unwrap();
        match event {
            SttEvent::Partial { text } => assert_eq!(text, "こんに"),
            _ => panic!("expected Partial"),
        }
    }

    #[test]
    fn parse_final() {
        let json = r#"{"type":"final","text":"こんにちは","language":"ja","confidence":0.98,"duration_ms":1500.0}"#;
        let msg: WsServerMessage = serde_json::from_str(json).unwrap();
        let event = msg.into_stt_event().unwrap();
        match event {
            SttEvent::Final {
                text,
                language,
                confidence,
                duration_ms,
            } => {
                assert_eq!(text, "こんにちは");
                assert_eq!(language, "ja");
                assert!((confidence - 0.98).abs() < f32::EPSILON);
                assert!((duration_ms - 1500.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected Final"),
        }
    }

    #[test]
    fn parse_speech_end() {
        let json = r#"{"type":"speech_end","timestamp_ms":2000,"duration_ms":1500.0}"#;
        let msg: WsServerMessage = serde_json::from_str(json).unwrap();
        let event = msg.into_stt_event().unwrap();
        match event {
            SttEvent::SpeechEnd {
                timestamp_ms,
                duration_ms,
            } => {
                assert_eq!(timestamp_ms, 2000);
                assert!((duration_ms - 1500.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected SpeechEnd"),
        }
    }

    #[test]
    fn parse_transcript_is_final_true() {
        let json = r#"{"type":"transcript","text":"hello","is_final":true,"language":"en","confidence":0.95,"duration_ms":800.0}"#;
        let msg: WsServerMessage = serde_json::from_str(json).unwrap();
        let event = msg.into_stt_event().unwrap();
        match event {
            SttEvent::Final {
                text, language, ..
            } => {
                assert_eq!(text, "hello");
                assert_eq!(language, "en");
            }
            _ => panic!("expected Final from transcript is_final=true"),
        }
    }

    #[test]
    fn parse_transcript_is_final_false() {
        let json = r#"{"type":"transcript","text":"hel","is_final":false}"#;
        let msg: WsServerMessage = serde_json::from_str(json).unwrap();
        let event = msg.into_stt_event().unwrap();
        match event {
            SttEvent::Partial { text } => assert_eq!(text, "hel"),
            _ => panic!("expected Partial from transcript is_final=false"),
        }
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        let json = r#"{"type":"ping","data":123}"#;
        let msg: WsServerMessage = serde_json::from_str(json).unwrap();
        assert!(msg.into_stt_event().is_none());
    }

    #[test]
    fn parse_final_with_defaults() {
        // Minimal final message — missing optional fields.
        let json = r#"{"type":"final","text":"ok"}"#;
        let msg: WsServerMessage = serde_json::from_str(json).unwrap();
        let event = msg.into_stt_event().unwrap();
        match event {
            SttEvent::Final {
                text,
                language,
                confidence,
                duration_ms,
            } => {
                assert_eq!(text, "ok");
                assert_eq!(language, "ja"); // default
                assert!((confidence - 1.0).abs() < f32::EPSILON); // default
                assert!((duration_ms - 0.0).abs() < f64::EPSILON); // default
            }
            _ => panic!("expected Final"),
        }
    }

    // ── Config message serialization ────────────────────────────

    #[test]
    fn config_message_serialization() {
        let msg = WsConfigMessage {
            msg_type: "config",
            sample_rate: 48000,
            channels: 1,
            encoding: "pcm_s16le",
            language: "ja",
            interim_results: true,
            temperature: 0.0,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "config");
        assert_eq!(parsed["sample_rate"], 48000);
        assert_eq!(parsed["channels"], 1);
        assert_eq!(parsed["encoding"], "pcm_s16le");
        assert_eq!(parsed["language"], "ja");
        assert_eq!(parsed["interim_results"], true);
        assert_eq!(parsed["temperature"], 0.0);
    }
}
