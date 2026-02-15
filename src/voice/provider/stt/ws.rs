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
use crate::voice::provider::{SttEvent, SttProvider, SttReceiver, SttSender};

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

fn parse_server_message(text: &str) -> Option<SttEvent> {
    let msg: WsServerMessage = serde_json::from_str(text).ok()?;
    msg.into_stt_event()
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
            // Handle `result` (same as reference: type === 'result', is_final, text, duration_ms).
            "result" => {
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
    async fn connect(&self) -> Result<(Box<dyn SttSender>, Box<dyn SttReceiver>)> {
        let ws_stream = self.connect_with_retry().await?;
        let (mut sink, stream) = ws_stream.split();

        // Send initial config. We always send 16 kHz PCM (from receiver resampler); the server
        // must receive this rate or recognition will fail or never return results.
        const PCM_SAMPLE_RATE: u32 = 16000;
        let config_msg = WsConfigMessage {
            msg_type: "config",
            sample_rate: PCM_SAMPLE_RATE,
            channels: 1,
            encoding: "pcm_s16le",
            language: "ja",
            interim_results: true,
            temperature: self.config.temperature,
        };
        let json = serde_json::to_string(&config_msg)?;
        sink.send(Message::Text(json)).await?;
        debug!("sent STT config: {:?}", config_msg);
        // Give server time to apply config (reference: onopen → send config then stream audio).
        tokio::time::sleep(Duration::from_millis(200)).await;

        let sender = Box::new(WsSttSender { sink }) as Box<dyn SttSender>;
        let receiver = Box::new(WsSttReceiver { stream }) as Box<dyn SttReceiver>;
        Ok((sender, receiver))
    }

    fn name(&self) -> &str {
        "ws"
    }
}

// ── Session ──────────────────────────────────────────────────────

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// WebSocket STT sender (audio only).
struct WsSttSender {
    sink: WsSink,
}

/// WebSocket STT receiver (events only).
struct WsSttReceiver {
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
impl SttSender for WsSttSender {
    async fn send_audio(&mut self, audio: &[f32]) -> Result<()> {
        let bytes = pcm_f32_to_s16le(audio);
        self.sink
            .send(Message::Binary(bytes))
            .await
            .context("failed to send audio to STT server")?;
        Ok(())
    }

    async fn send_end_of_stream(&mut self) -> Result<()> {
        let eos = r#"{"type":"end_of_stream"}"#.to_string();
        self.sink
            .send(Message::Text(eos))
            .await
            .context("failed to send end_of_stream")?;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        if let Err(e) = self.sink.close().await {
            debug!("failed to close STT WebSocket: {e}");
        }
        Ok(())
    }
}

fn log_stt_server_message(text: &str) {
    const MAX: usize = 300;
    let truncated: &str = if text.len() <= MAX { text } else { &text[..MAX] };
    info!(msg = %truncated, "STT server recv");
}

#[async_trait]
impl SttReceiver for WsSttReceiver {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>> {
        loop {
            match self.stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    #[cfg(test)]
                    eprintln!("[STT recv] text: {}", text);
                    log_stt_server_message(&text);
                    if let Some(ev) = parse_server_message(&text) {
                        return Ok(Some(ev));
                    }
                    if !text.contains("\"type\":\"config_ack\"") && !text.contains("\"type\": \"config_ack\"") {
                        let preview: String = text.chars().take(150).collect();
                        info!(msg = %preview, "STT server unparsed (ignored)");
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if let Ok(text) = std::str::from_utf8(&b) {
                        #[cfg(test)]
                        eprintln!("[STT recv] binary as text: {}", text);
                        log_stt_server_message(text);
                        if let Some(ev) = parse_server_message(text) {
                            return Ok(Some(ev));
                        }
                        if !text.contains("\"type\":\"config_ack\"") && !text.contains("\"type\": \"config_ack\"") {
                            let preview: String = text.chars().take(150).collect();
                            info!(msg = %preview, "STT server unparsed (ignored)");
                        }
                    } else {
                        #[cfg(test)]
                        eprintln!("[STT recv] binary (not UTF-8, {} bytes)", b.len());
                        info!(bytes = b.len(), "STT server recv binary (not UTF-8)");
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    #[cfg(test)]
                    eprintln!("[STT recv] server sent Close");
                    info!("STT WebSocket closed by server");
                    return Ok(None);
                }
                Some(Ok(Message::Ping(_))) => {
                    continue;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    #[cfg(test)]
                    eprintln!("[STT recv] error: {}", e);
                    error!("STT WebSocket error: {e}");
                    return Err(e.into());
                }
                None => {
                    #[cfg(test)]
                    eprintln!("[STT recv] stream ended (None)");
                    info!("STT WebSocket stream ended");
                    return Ok(None);
                }
            }
        }
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

// ── STT server integration tests (problem isolation) ───────────────────────
//
// Load pre-generated speech from tests/fixtures/stt_speech.wav (create once
// with curl; see tests/fixtures/README.md), send it to the STT server in 20ms
// or 100ms chunks. Only the STT server need be running for the test.
//
//   cargo test --features voice -- stt_server_ -- --ignored --nocapture
//
// Evidence from these tests (which chunk size gets events, timeouts, etc.)
// should guide any buffering or pipeline changes.

#[cfg(test)]
mod stt_server_tests {
    use super::*;
    use crate::config::{Config, VoiceSttWsConfig};
    use crate::voice::audio::{pcm_i16_to_f32, resample_mono};
    use std::path::Path;
    use std::time::Duration;

    /// Load STT WS config from config.toml (voice.stt.ws). Uses default if voice section missing.
    fn stt_ws_config_from_app_config() -> VoiceSttWsConfig {
        match Config::load() {
            Ok(c) => c
                .voice
                .as_ref()
                .map(|v| v.stt.ws.clone())
                .unwrap_or_else(VoiceSttWsConfig::default),
            Err(_) => VoiceSttWsConfig::default(),
        }
    }

    /// Load tests/fixtures/stt_speech.wav and return mono f32 PCM at the given sample rate.
    /// Create the file once with the curl command in tests/fixtures/README.md.
    fn load_speech_wav_at_rate(target_sample_rate: u32) -> Vec<f32> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stt_speech.wav");
        let reader = hound::WavReader::open(&path).unwrap_or_else(|e| {
            panic!(
                "open {}: {}. Generate the fixture once (see tests/fixtures/README.md)",
                path.display(),
                e
            )
        });
        let spec = reader.spec();
        let samples_i16: Vec<i16> = reader
            .into_samples::<i16>()
            .collect::<Result<_, _>>()
            .expect("read WAV samples");
        let pcm_f32 = pcm_i16_to_f32(&samples_i16);
        let mono: Vec<f32> = if spec.channels == 2 {
            pcm_f32
                .chunks_exact(2)
                .map(|lr| (lr[0] + lr[1]) / 2.0)
                .collect()
        } else {
            pcm_f32
        };
        resample_mono(&mono, spec.sample_rate, target_sample_rate)
            .expect("resample to target rate")
    }

    /// Split PCM into chunks of the given size.
    fn chunk_pcm(pcm: &[f32], samples_per_chunk: usize) -> Vec<Vec<f32>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < pcm.len() {
            let end = (i + samples_per_chunk).min(pcm.len());
            out.push(pcm[i..end].to_vec());
            i = end;
        }
        out
    }

    /// Send fixture PCM in chunks, close sender, collect STT events until stream end or timeout.
    /// Fails if no recognition event (Partial or Final) is received — i.e. asserts real STT.
    async fn send_fixture_and_assert_recognition(
        chunk_label: &str,
        chunks: &[Vec<f32>],
        config: VoiceSttWsConfig,
    ) {
        eprintln!("[STT test] endpoint={} sample_rate={}", config.endpoint, config.sample_rate);
        let provider = WsSttProvider::new(config);
        let (mut sender, mut receiver) = provider
            .connect()
            .await
            .expect("connect (is STT server running? check voice.stt.ws.endpoint in config)");

        for ch in chunks {
            sender.send_audio(ch).await.expect("send_audio");
        }
        sender.send_end_of_stream().await.expect("send_end_of_stream");
        eprintln!("[STT test] sent end_of_stream, waiting for results");

        let mut events = Vec::new();
        let recv_timeout = Duration::from_secs(10);
        eprintln!("[STT test] entering recv loop (timeout {:?})", recv_timeout);
        loop {
            match tokio::time::timeout(recv_timeout, receiver.recv_event()).await {
                Ok(Ok(Some(ev))) => {
                    eprintln!("[STT server] {}: {:?}", chunk_label, ev);
                    events.push(ev);
                }
                Ok(Ok(None)) => {
                    eprintln!("[STT test] recv loop exit: got None (stream end)");
                    break;
                }
                Ok(Err(e)) => panic!("recv_event error: {}", e),
                Err(_) => {
                    eprintln!("[STT test] recv loop exit: timeout after {:?}", recv_timeout);
                    break;
                }
            }
        }
        sender.close().await.expect("close");

        let has_recognition = events.iter().any(|e| {
            matches!(e, SttEvent::Partial { .. } | SttEvent::Final { .. })
        });
        assert!(
            has_recognition,
            "STT server returned no recognition result (got {} events: {:?}); \
             check server and voice.stt.ws.endpoint in config",
            events.len(),
            events
        );
    }

    #[tokio::test]
    #[ignore = "STT server + fixture required - run with: cargo test --features voice -- stt_server_ -- --ignored --nocapture"]
    async fn stt_server_responds_to_20ms_chunks() {
        let config = stt_ws_config_from_app_config();
        let sample_rate = config.sample_rate;
        let pcm = load_speech_wav_at_rate(sample_rate);
        assert!(!pcm.is_empty(), "fixture WAV is empty");

        let samples_20ms = (sample_rate as usize * 20) / 1000;
        let chunks = chunk_pcm(&pcm, samples_20ms);
        send_fixture_and_assert_recognition("20ms chunks", &chunks, config).await;
    }

    #[tokio::test]
    #[ignore = "STT server + fixture required - run with: cargo test --features voice -- stt_server_ -- --ignored --nocapture"]
    async fn stt_server_responds_to_100ms_chunks() {
        let config = stt_ws_config_from_app_config();
        let sample_rate = config.sample_rate;
        let pcm = load_speech_wav_at_rate(sample_rate);
        assert!(!pcm.is_empty(), "fixture WAV is empty");

        let samples_100ms = (sample_rate as usize * 100) / 1000;
        let chunks = chunk_pcm(&pcm, samples_100ms);
        send_fixture_and_assert_recognition("100ms chunks", &chunks, config).await;
    }
}
