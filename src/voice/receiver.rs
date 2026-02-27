//! Voice receive handler.
//!
//! Receives decoded PCM from songbird's VoiceTick events
//! (configured with `DecodeMode::Decode`). The decoded 48 kHz stereo PCM
//! is then downmixed to 48 kHz mono before forwarding
//! [`AudioChunk`]s to the dispatcher via an mpsc channel.
//!
//! Also handles `SpeakingStateUpdate` events to maintain the SSRC → UserId
//! mapping, and `ClientDisconnect` to clean up user state.

use chrono::Local;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use songbird::events::{Event, EventContext, EventHandler as VoiceEventHandler};
use std::collections::HashMap;
use std::sync::{atomic::AtomicBool, atomic::AtomicU32, atomic::Ordering, Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::ssrc_map::SsrcUserMap;

/// Global instance counter for VoiceReceiveHandler instances
static INSTANCE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Silence length per VoiceTick (20 ms at 16 kHz) so STT receives a continuous stream
/// and can detect end-of-speech. Matches the resampled chunk size we send for speech.
const SILENCE_SAMPLES_PER_TICK_16K: usize = 16000 * 20 / 1000;

/// A chunk of decoded audio from a single speaker.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Synchronization source — identifies the speaker.
    pub ssrc: u32,
    /// Discord user ID resolved from the SSRC map (None if not yet mapped).
    pub user_id: Option<u64>,
    /// Discord username resolved from the SSRC map (None if not yet mapped).
    pub user_name: Option<String>,
    /// 16 kHz mono f32 PCM samples (resampled from 48 kHz).
    pub pcm: Vec<f32>,
}

/// Shared inner state for the voice receive handler.
///
/// Wrapped in `Arc` so that a single `VoiceReceiveHandler` can be cloned
/// and registered for multiple event types while sharing state.
struct InnerReceiver {
    /// Channel to send audio chunks to the dispatcher
    audio_tx: mpsc::UnboundedSender<AudioChunk>,
    /// SSRC → (UserId, username) mapping, updated by SpeakingStateUpdate events.
    /// Shared as Arc so VoiceGateway can also update it (e.g. on VOICE_STATE_UPDATE).
    ssrc_map: Arc<SsrcUserMap>,
    /// WAV writer for debug logging (16 kHz mono 16-bit PCM, resampled from 48 kHz)
    wav_writer: Mutex<Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>>,
    /// WAV writer for 48 kHz stereo debug logging
    wav_writer_48k_stereo: Mutex<Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>>,
    /// Per-SSRC resamplers for 48 kHz → 16 kHz mono conversion.
    /// Each speaker gets their own resampler to avoid state contamination between users.
    resamplers: Mutex<HashMap<u32, SincFixedIn<f32>>>,
    /// Flag to track if we're currently recording (once per session)
    recording: AtomicBool,
    /// SSRC of the current speaker being recorded
    recording_ssrc: Mutex<Option<u32>>,
    /// Count of consecutive silent ticks (at 20ms/tick, 100 = 2 seconds of silence)
    silent_tick_count: AtomicU32,
    /// Unique instance identifier
    instance_id: u32,
    /// Counter to limit act() logging (only logs first 10 calls per instance)
    log_count: AtomicU32,
}

/// songbird `EventHandler` implementation for receiving voice packets.
///
/// Handles multiple event types via a single shared instance:
/// - `VoiceTick`: Processes decoded PCM (downmix stereo→mono, resample 48→16 kHz)
/// - `SpeakingStateUpdate`: Maintains SSRC → UserId mapping
/// - `ClientDisconnect`: Cleans up user state from SSRC map
///
/// Clone this handler and register the same instance for all needed event types.
#[derive(Clone)]
pub struct VoiceReceiveHandler {
    inner: Arc<InnerReceiver>,
}

impl VoiceReceiveHandler {
    /// Create a new receive handler with a shared SSRC map.
    ///
    /// The `ssrc_map` is shared with the VoiceGateway so that gateway-level
    /// events (e.g. VOICE_STATE_UPDATE for other users) can also update the mapping.
    pub fn new(audio_tx: mpsc::UnboundedSender<AudioChunk>, ssrc_map: Arc<SsrcUserMap>) -> Self {
        Self {
            inner: Arc::new(InnerReceiver {
                audio_tx,
                ssrc_map,
                wav_writer: Mutex::new(None),
                wav_writer_48k_stereo: Mutex::new(None),
                resamplers: Mutex::new(HashMap::new()),
                recording: AtomicBool::new(false),
                recording_ssrc: Mutex::new(None),
                silent_tick_count: AtomicU32::new(0),
                instance_id: INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed),
                log_count: AtomicU32::new(0),
            }),
        }
    }
}

impl InnerReceiver {
    /// Create a new SincFixedIn resampler for 48 kHz → 16 kHz conversion.
    fn make_resampler() -> SincFixedIn<f32> {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        SincFixedIn::<f32>::new(16000.0 / 48000.0, 2.0, params, 960, 1)
            .expect("Failed to create resampler")
    }

    /// Resample mono 48 kHz PCM to 16 kHz using a per-SSRC resampler.
    ///
    /// Each SSRC (speaker) has its own resampler instance to prevent state
    /// contamination when multiple users are speaking simultaneously.
    fn process_with_resampler(&self, ssrc: u32, mono: Vec<f32>) -> Vec<f32> {
        let mut resamplers = self.resamplers.lock().unwrap();
        let resampler = resamplers.entry(ssrc).or_insert_with(Self::make_resampler);
        let waves_in = vec![mono.clone()];
        match resampler.process(&waves_in, None) {
            Ok(mut result) => result.remove(0),
            Err(e) => {
                warn!(ssrc, "Resample failed: {}", e);
                mono
            }
        }
    }

    /// Remove per-SSRC resampler state when a user disconnects.
    fn remove_resampler(&self, ssrc: u32) {
        let mut resamplers = self.resamplers.lock().unwrap();
        resamplers.remove(&ssrc);
    }

    /// Initialize WAV writers for debug logging.
    /// Returns tuple of (48kHz stereo, 16kHz mono) writers sharing the same timestamp.
    fn init_wav_writers(
        &self,
    ) -> Result<
        (
            hound::WavWriter<std::io::BufWriter<std::fs::File>>,
            hound::WavWriter<std::io::BufWriter<std::fs::File>>,
        ),
        Box<dyn std::error::Error>,
    > {
        info!(instance_id = self.instance_id, "init_wav_writers");
        let timestamp = Local::now().format("%Y%m%d_%H%M%S");

        std::fs::create_dir_all("/Users/kojira/.localgpt/logs")?;

        // 48kHz stereo writer
        let filename_48k_stereo = format!("voice_48k_stereo_{}.wav", timestamp);
        let wav_path_48k_stereo = std::path::PathBuf::from(format!(
            "/Users/kojira/.localgpt/logs/{}",
            filename_48k_stereo
        ));
        let spec_48k_stereo = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let file_48k_stereo = std::fs::File::create(&wav_path_48k_stereo)?;
        let writer_48k_stereo =
            hound::WavWriter::new(std::io::BufWriter::new(file_48k_stereo), spec_48k_stereo)?;

        // 16kHz mono writer
        let filename_debug = format!("voice_debug_{}.wav", timestamp);
        let wav_path_debug = std::path::PathBuf::from(format!(
            "/Users/kojira/.localgpt/logs/{}",
            filename_debug
        ));
        let spec_debug = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let file_debug = std::fs::File::create(&wav_path_debug)?;
        let writer_debug =
            hound::WavWriter::new(std::io::BufWriter::new(file_debug), spec_debug)?;

        Ok((writer_48k_stereo, writer_debug))
    }

    /// Handle a VoiceTick event: decode, downmix, resample, and send audio chunks.
    fn handle_voice_tick(&self, tick: &songbird::events::context_data::VoiceTick) {
        // Reset silent tick counter when speaking users are detected
        if !tick.speaking.is_empty() {
            self.silent_tick_count.store(0, Ordering::Relaxed);
        }

        // Handle silent users: increment silence counter and finalize if threshold reached
        if !tick.silent.is_empty() && self.recording.load(Ordering::Relaxed) {
            let new_count = self.silent_tick_count.fetch_add(1, Ordering::Relaxed) + 1;

            // Finalize WAV after 100 consecutive silent ticks (2 seconds at 20ms/tick)
            if new_count >= 100 {
                // Finalize both WAV writers
                let mut finalized_count = 0;

                if let Ok(mut writer_guard) = self.wav_writer_48k_stereo.lock() {
                    if let Some(writer) = writer_guard.take() {
                        match writer.finalize() {
                            Ok(_) => finalized_count += 1,
                            Err(e) => warn!("Failed to finalize 48k stereo WAV file: {}", e),
                        }
                    }
                }

                if let Ok(mut writer_guard) = self.wav_writer.lock() {
                    if let Some(writer) = writer_guard.take() {
                        match writer.finalize() {
                            Ok(_) => {
                                finalized_count += 1;
                                info!(
                                    instance_id = self.instance_id,
                                    finalized_count, "finalized wav files"
                                );
                                self.recording.store(false, Ordering::Relaxed);
                                self.silent_tick_count.store(0, Ordering::Relaxed);
                                if let Ok(mut ssrc_guard) = self.recording_ssrc.lock() {
                                    *ssrc_guard = None;
                                }
                            }
                            Err(e) => {
                                warn!("Failed to finalize 48k mono WAV file: {}", e);
                            }
                        }
                    }
                }
            }
        }

        for (&ssrc, data) in &tick.speaking {
            // Get decoded PCM from songbird (48 kHz stereo i16)
            let pcm_i16 = match data.decoded_voice.as_ref() {
                Some(v) => v,
                None => continue,
            };

            if pcm_i16.is_empty() {
                continue;
            }

            // Write 48kHz stereo to debug file if recording
            if self.recording.load(Ordering::Relaxed) {
                let recording_ssrc = self.recording_ssrc.lock().ok().and_then(|g| *g);
                if recording_ssrc == Some(ssrc) {
                    if let Ok(mut writer_guard) = self.wav_writer_48k_stereo.lock() {
                        if let Some(ref mut writer) = *writer_guard {
                            for &sample_i16 in pcm_i16 {
                                if let Err(e) = writer.write_sample(sample_i16) {
                                    warn!("Failed to write 48k stereo WAV sample: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Convert i16 → f32 (range −1.0 … 1.0)
            let pcm_f32: Vec<f32> = pcm_i16.iter().map(|&s| s as f32 / 32768.0).collect();

            // Downmix stereo → mono (average L and R channels)
            let mono = stereo_to_mono(&pcm_f32);

            // Resample mono from 48 kHz to 16 kHz using per-SSRC resampler.
            // Each SSRC has its own SincFixedIn instance to avoid state contamination
            // between multiple simultaneous speakers.
            let rms_before = calculate_rms(&mono);
            let mono_16k = self.process_with_resampler(ssrc, mono.clone());

            let rms = calculate_rms(&mono_16k);

            // Diagnostic: compare RMS before/after resampling for audible chunks
            if rms_before > 0.01 || rms > 0.01 {
                info!(
                    ssrc,
                    pcm_i16_len = pcm_i16.len(),
                    mono_len = mono.len(),
                    mono_16k_len = mono_16k.len(),
                    rms_before_resample = format!("{:.6}", rms_before),
                    rms_after_resample = format!("{:.6}", rms),
                    "[DIAG] Resample comparison (audible)"
                );
            }

            debug!(
                ssrc,
                pcm_bytes = pcm_i16.len() * 2,
                decoded_samples = pcm_i16.len(),
                out_samples = mono_16k.len(),
                rms = format!("{:.4}", rms),
                "Decoded 48kHz stereo → 16kHz mono (resampled)"
            );

            // Write 16kHz mono to debug file if recording
            if self.recording.load(Ordering::Relaxed) {
                let recording_ssrc = self.recording_ssrc.lock().ok().and_then(|g| *g);
                if recording_ssrc == Some(ssrc) {
                    if let Ok(mut writer_guard) = self.wav_writer.lock() {
                        if let Some(ref mut writer) = *writer_guard {
                            for &sample_f32 in &mono_16k {
                                let sample_i16 = (sample_f32 * 32767.0) as i16;
                                if let Err(e) = writer.write_sample(sample_i16) {
                                    warn!("Failed to write 16k WAV sample: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }
            } else if !self.recording.load(Ordering::Relaxed) {
                // Start recording on first speaking user
                match self.init_wav_writers() {
                    Ok((writer_48k_stereo, writer_debug)) => {
                        if let Ok(mut writer_guard) = self.wav_writer_48k_stereo.lock() {
                            *writer_guard = Some(writer_48k_stereo);
                        }
                        if let Ok(mut writer_guard) = self.wav_writer.lock() {
                            *writer_guard = Some(writer_debug);
                        }
                        if let Ok(mut ssrc_guard) = self.recording_ssrc.lock() {
                            *ssrc_guard = Some(ssrc);
                        }
                        self.recording.store(true, Ordering::Relaxed);
                        debug!(ssrc, "Started WAV recording (48k stereo + 16k mono)");

                        // Write this chunk to both new files
                        // 48kHz stereo
                        if let Ok(mut writer_guard) = self.wav_writer_48k_stereo.lock() {
                            if let Some(ref mut writer) = *writer_guard {
                                for &sample_i16 in pcm_i16 {
                                    if let Err(e) = writer.write_sample(sample_i16) {
                                        warn!("Failed to write 48k stereo WAV sample: {}", e);
                                        break;
                                    }
                                }
                            }
                        }

                        // 16kHz mono
                        if let Ok(mut writer_guard) = self.wav_writer.lock() {
                            if let Some(ref mut writer) = *writer_guard {
                                for &sample_f32 in &mono_16k {
                                    let sample_i16 = (sample_f32 * 32767.0) as i16;
                                    if let Err(e) = writer.write_sample(sample_i16) {
                                        warn!("Failed to write 16k WAV sample: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to initialize WAV writers: {}", e);
                    }
                }
            }

            // Resolve SSRC → (user_id, user_name) from the shared map
            let (user_id, user_name) = match self.ssrc_map.get_user(ssrc) {
                Some((uid, uname)) => (Some(uid), Some(uname)),
                None => (None, None),
            };

            let samples = mono_16k.len();
            let chunk = AudioChunk {
                ssrc,
                user_id,
                user_name,
                pcm: mono_16k,
            };
            // Diagnostic: log chunks with audible audio (RMS > 0.01)
            if rms > 0.01 {
                info!(
                    ssrc,
                    ?user_id,
                    samples,
                    rms = format!("{:.4}", rms),
                    "[A] AudioChunk with audio sending to dispatcher"
                );
            }
            if let Err(e) = self.audio_tx.send(chunk) {
                warn!("Failed to send audio chunk: {}", e);
            }
        }

        // Send silence for users in tick.silent so STT gets a continuous stream and can
        // detect end-of-speech (emit final when it sees enough silence).
        for &ssrc in &tick.silent {
            let (user_id, user_name) = match self.ssrc_map.get_user(ssrc) {
                Some((uid, uname)) => (Some(uid), Some(uname)),
                None => continue,
            };
            let silence = vec![0.0f32; SILENCE_SAMPLES_PER_TICK_16K];
            let chunk = AudioChunk {
                ssrc,
                user_id,
                user_name,
                pcm: silence,
            };
            if let Err(e) = self.audio_tx.send(chunk) {
                warn!("Failed to send silence chunk: {}", e);
            }
        }
    }

    /// Handle a SpeakingStateUpdate event: update SSRC → UserId mapping.
    fn handle_speaking_state_update(
        &self,
        speaking: &songbird::model::payload::Speaking,
    ) {
        if let Some(user_id) = speaking.user_id {
            let uid = user_id.0;
            // We don't have the username from this event; use a placeholder
            // that will be refined when the dispatcher receives it.
            let username = format!("user_{}", uid);
            self.ssrc_map
                .update_from_speaking(speaking.ssrc, uid, username);
            info!(
                ssrc = speaking.ssrc,
                user_id = uid,
                "SSRC → UserId mapping updated from SpeakingStateUpdate"
            );
        }
    }

    /// Handle a ClientDisconnect event: remove user from SSRC map and clean up resampler.
    fn handle_client_disconnect(
        &self,
        disconnect: &songbird::model::payload::ClientDisconnect,
    ) {
        let uid = disconnect.user_id.0;
        // Look up the SSRC before removing the user, so we can clean up the resampler
        if let Some(ssrc) = self.ssrc_map.get_ssrc_for_user(uid) {
            self.remove_resampler(ssrc);
        }
        self.ssrc_map.remove_user(uid);
        info!(
            user_id = uid,
            "User removed from SSRC map (ClientDisconnect)"
        );
    }
}

#[async_trait::async_trait]
impl VoiceEventHandler for VoiceReceiveHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let count = self.inner.log_count.fetch_add(1, Ordering::Relaxed);
        if count < 10 {
            info!(
                instance_id = self.inner.instance_id,
                count,
                event = ?std::mem::discriminant(ctx),
                "act() called"
            );
        }

        match ctx {
            EventContext::VoiceTick(tick) => {
                self.inner.handle_voice_tick(tick);
            }
            EventContext::SpeakingStateUpdate(speaking) => {
                self.inner.handle_speaking_state_update(speaking);
            }
            EventContext::ClientDisconnect(disconnect) => {
                self.inner.handle_client_disconnect(disconnect);
            }
            _ => {
                // RtpPacket, RtcpPacket, etc. — not needed for our pipeline
            }
        }

        None
    }
}

/// Downmix interleaved stereo to mono by averaging L and R channels.
fn stereo_to_mono(interleaved: &[f32]) -> Vec<f32> {
    interleaved
        .chunks_exact(2)
        .map(|pair| (pair[0] + pair[1]) * 0.5)
        .collect()
}

/// Calculate RMS (root mean square) of audio samples.
fn calculate_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_chunk_fields() {
        let chunk = AudioChunk {
            ssrc: 12345,
            user_id: Some(99999),
            user_name: Some("Alice".to_string()),
            pcm: vec![0.1, 0.2, 0.3],
        };
        assert_eq!(chunk.ssrc, 12345);
        assert_eq!(chunk.user_id, Some(99999));
        assert_eq!(chunk.user_name.as_deref(), Some("Alice"));
        assert_eq!(chunk.pcm.len(), 3);
    }

    #[test]
    fn audio_chunk_without_user_info() {
        let chunk = AudioChunk {
            ssrc: 12345,
            user_id: None,
            user_name: None,
            pcm: vec![0.1, 0.2, 0.3],
        };
        assert_eq!(chunk.ssrc, 12345);
        assert!(chunk.user_id.is_none());
        assert!(chunk.user_name.is_none());
    }

    #[test]
    fn calculate_rms_zero() {
        let samples = vec![0.0; 100];
        let rms = calculate_rms(&samples);
        assert!(rms < 0.001);
    }

    #[test]
    fn calculate_rms_nonzero() {
        let samples = vec![0.5; 100];
        let rms = calculate_rms(&samples);
        assert!((rms - 0.5).abs() < 0.001);
    }

    #[test]
    fn calculate_rms_empty() {
        let samples: Vec<f32> = vec![];
        let rms = calculate_rms(&samples);
        assert_eq!(rms, 0.0);
    }

    #[test]
    fn voice_receive_handler_new() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let ssrc_map = Arc::new(SsrcUserMap::new());
        let handler = VoiceReceiveHandler::new(tx, ssrc_map);
        // Verify construction succeeds
        let _ = handler;
    }

    #[test]
    fn voice_receive_handler_is_clone() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let ssrc_map = Arc::new(SsrcUserMap::new());
        let handler = VoiceReceiveHandler::new(tx, ssrc_map);
        let _cloned = handler.clone();
        // Both share the same inner state (Arc)
    }

    #[test]
    fn i16_to_f32_conversion() {
        // Max positive i16 → ~1.0
        let max_val = i16::MAX as f32 / 32768.0;
        assert!((max_val - 0.999969).abs() < 0.001);

        // Min i16 → −1.0
        let min_val = i16::MIN as f32 / 32768.0;
        assert!((min_val - (-1.0)).abs() < 0.001);

        // Zero
        let zero_val = 0i16 as f32 / 32768.0;
        assert!((zero_val - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn stereo_to_mono_downmix() {
        // L=1.0, R=0.0 → 0.5
        let stereo = vec![1.0f32, 0.0, 0.6, 0.4, -0.5, 0.5];
        let mono = stereo_to_mono(&stereo);
        assert_eq!(mono.len(), 3);
        assert!((mono[0] - 0.5).abs() < f32::EPSILON);
        assert!((mono[1] - 0.5).abs() < f32::EPSILON);
        assert!((mono[2] - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn stereo_to_mono_empty() {
        let mono = stereo_to_mono(&[]);
        assert!(mono.is_empty());
    }

    #[test]
    fn sample_counts_per_tick() {
        // 20ms @ 48kHz stereo = 960 samples/ch × 2 = 1920 interleaved
        let interleaved_per_tick = 48000 * 20 / 1000 * 2;
        assert_eq!(interleaved_per_tick, 1920);

        // After stereo→mono: 960 mono samples @ 48kHz
        let mono_per_tick = interleaved_per_tick / 2;
        assert_eq!(mono_per_tick, 960);
    }
}
