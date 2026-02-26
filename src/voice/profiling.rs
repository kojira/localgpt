//! Voice pipeline profiling logger.
//!
//! Records latency at each step of the STT → LLM → TTS pipeline
//! to `~/.localgpt/logs/voice-profiling.log`.
//!
//! # Session lifecycle
//! ```text
//! SpeechStart → STT_PARTIAL (repeated, deduped) → STT_FINAL
//!             → LLM_START → LLM_DONE
//!             → TTS_START → TTS_DONE → AUDIO_PLAY
//! ```

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use uuid::Uuid;

/// Shared handle to the profiling log file.
/// Cheap to clone — backed by `Arc<Mutex<File>>`.
#[derive(Clone)]
pub struct VoiceProfilerWriter {
    inner: Arc<Mutex<Option<std::fs::File>>>,
    path: PathBuf,
}

impl VoiceProfilerWriter {
    /// Open (or create) the profiling log file.
    ///
    /// The file is named `voice-profiling-YYYY-MM-DD_HHMMSS.log` where the timestamp
    /// is the **current wall-clock time** (UTC) at the moment `open()` is called —
    /// i.e. the daemon startup time.  This gives one file per daemon invocation so
    /// that separate runs and test executions never share a log file.
    ///
    /// Example path: `~/.localgpt/logs/voice-profiling-2026-02-26_013800.log`
    pub fn open() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let dir = PathBuf::from(&home).join(".localgpt").join("logs");
        let filename = format!("voice-profiling-{}.log", startup_datetime_str());
        let path = dir.join(&filename);

        let file = if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("voice profiler: failed to create log dir {:?}: {}", dir, e);
            None
        } else {
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => {
                    tracing::info!("voice profiler: logging to {:?}", path);
                    Some(f)
                }
                Err(e) => {
                    tracing::warn!("voice profiler: failed to open {:?}: {}", path, e);
                    None
                }
            }
        };

        Self {
            inner: Arc::new(Mutex::new(file)),
            path,
        }
    }

    /// Create a no-op (null) writer that discards all writes.
    ///
    /// Intended for unit tests: calling `write_line` is a no-op, so test runs
    /// never create or pollute real log files on the filesystem.
    pub fn null() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            path: PathBuf::from("/dev/null"),
        }
    }

    /// Write a single log line (appends newline).
    pub fn write_line(&self, line: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(ref mut f) = *guard {
                if let Err(e) = writeln!(f, "{}", line) {
                    tracing::warn!("voice profiler write error: {}", e);
                }
            }
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

/// Returns a compact UTC datetime string suitable for log file names.
///
/// Format: `YYYY-MM-DD_HHMMSS`  (e.g. `2026-02-26_013800`)
fn startup_datetime_str() -> String {
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();

    let sec_of_day = secs % 86400;
    let days = secs / 86400;
    let (year, month, day) = days_to_ymd(days);
    let h = (sec_of_day / 3600) as u32;
    let m = ((sec_of_day % 3600) / 60) as u32;
    let s = (sec_of_day % 60) as u32;

    format!(
        "{:04}-{:02}-{:02}_{:02}{:02}{:02}",
        year, month, day, h, m, s
    )
}

/// ISO 8601 UTC timestamp string with milliseconds, e.g. `2026-02-26T01:40:02.941Z`.
fn iso_timestamp() -> String {
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();

    let sec_of_day = secs % 86400;
    let days = secs / 86400;
    let (year, month, day) = days_to_ymd(days);
    let h = (sec_of_day / 3600) as u32;
    let m = ((sec_of_day % 3600) / 60) as u32;
    let s = (sec_of_day % 60) as u32;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, h, m, s, millis
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32)
}

/// Tracks profiling timing for one complete voice pipeline session.
///
/// Created at `SpeechStart`; lives until `AUDIO_PLAY` (or barge-in / cancel).
pub struct ProfileSession {
    /// Short UUID prefix (8 hex chars) to correlate log lines for one turn.
    pub session_id: String,
    /// When SpeechStart was received — base reference for STT_PARTIAL elapsed.
    speech_start_at: Instant,
    /// Last partial text seen; used to skip duplicate partials.
    last_partial_text: String,
    /// When STT_FINAL was received — base reference for LLM elapsed.
    stt_final_at: Option<Instant>,
    /// When LLM call started.
    llm_start_at: Option<Instant>,
    /// When LLM call finished.
    llm_done_at: Option<Instant>,
    /// When TTS synthesis started.
    tts_start_at: Option<Instant>,
    writer: VoiceProfilerWriter,
}

impl ProfileSession {
    /// Create a new session, recording now as the speech-start anchor.
    pub fn new(writer: VoiceProfilerWriter) -> Self {
        Self::new_with_start(writer, Instant::now())
    }

    /// Create a new session with an explicit speech-start anchor.
    ///
    /// Use this when the start time was captured before the session object was created
    /// (e.g., from the first audible PCM chunk arriving before the STT `SpeechStart` event).
    /// Passing a pre-captured `Instant` ensures `elapsed_from_speech_start` reflects the
    /// true moment the user began speaking rather than the STT event latency.
    pub fn new_with_start(writer: VoiceProfilerWriter, start: Instant) -> Self {
        let session_id = Uuid::new_v4()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>();
        tracing::debug!(session = %session_id, "PROFILE session created");
        Self {
            session_id,
            speech_start_at: start,
            last_partial_text: String::new(),
            stt_final_at: None,
            llm_start_at: None,
            llm_done_at: None,
            tts_start_at: None,
            writer,
        }
    }

    // ── STT phase ──────────────────────────────────────────────────────────

    /// Log a STT partial result.
    ///
    /// Only writes if `text` differs from the last logged partial (deduplication).
    /// `elapsed_from_speech_start` shows how long after SpeechStart this partial arrived.
    pub fn log_stt_partial(&mut self, text: &str) {
        if text == self.last_partial_text {
            return; // no change — skip
        }
        self.last_partial_text = text.to_string();

        let elapsed_ms = self.speech_start_at.elapsed().as_millis() as u64;
        let short_text = text.chars().take(30).collect::<String>();
        let line = format!(
            "{} [PROFILE] session={} step=STT_PARTIAL text=\"{}\" elapsed_from_speech_start={}ms",
            iso_timestamp(),
            self.session_id,
            short_text,
            elapsed_ms
        );
        self.writer.write_line(&line);
        tracing::debug!(
            session = %self.session_id,
            elapsed_ms,
            text = %short_text,
            "PROFILE STT_PARTIAL"
        );
    }

    /// Log STT_FINAL.
    ///
    /// Records elapsed from speech-start and anchors the STT→LLM elapsed reference.
    pub fn log_stt_final(&mut self, text: &str) {
        self.stt_final_at = Some(Instant::now());
        let elapsed_ms = self.speech_start_at.elapsed().as_millis() as u64;
        let short_text = text.chars().take(30).collect::<String>();
        let line = format!(
            "{} [PROFILE] session={} step=STT_FINAL text=\"{}\" elapsed_from_speech_start={}ms",
            iso_timestamp(),
            self.session_id,
            short_text,
            elapsed_ms
        );
        self.writer.write_line(&line);
        tracing::debug!(
            session = %self.session_id,
            elapsed_ms,
            "PROFILE STT_FINAL"
        );
    }

    // ── LLM phase ──────────────────────────────────────────────────────────

    /// Log LLM_START.
    pub fn log_llm_start(&mut self) {
        self.llm_start_at = Some(Instant::now());
        let elapsed_from_stt_ms = self
            .stt_final_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let line = format!(
            "{} [PROFILE] session={} step=LLM_START elapsed_from_stt={}ms",
            iso_timestamp(),
            self.session_id,
            elapsed_from_stt_ms
        );
        self.writer.write_line(&line);
        tracing::debug!(
            session = %self.session_id,
            elapsed_from_stt_ms,
            "PROFILE LLM_START"
        );
    }

    /// Log LLM_FIRST_TOKEN (for streaming providers).
    #[allow(dead_code)]
    pub fn log_llm_first_token(&mut self) {
        let elapsed_from_llm_start_ms = self
            .llm_start_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let line = format!(
            "{} [PROFILE] session={} step=LLM_FIRST_TOKEN elapsed_from_llm_start={}ms",
            iso_timestamp(),
            self.session_id,
            elapsed_from_llm_start_ms
        );
        self.writer.write_line(&line);
        tracing::debug!(
            session = %self.session_id,
            elapsed_from_llm_start_ms,
            "PROFILE LLM_FIRST_TOKEN"
        );
    }

    /// Expose the speech-start anchor so callers can compute relative timestamps.
    pub fn speech_start_at(&self) -> std::time::Instant {
        self.speech_start_at
    }

    /// Log LLM_SEGMENT — a sentence segment was produced by SentenceSplitter and handed to TTS.
    ///
    /// `synthesis_started_at` is the `Instant` captured just before the TTS call began,
    /// which equals the moment the LLM segment was ready.  Using it (rather than `now()`)
    /// gives the correct elapsed even when this method is called after TTS completes.
    ///
    /// Full text is logged without truncation so split boundaries are clearly visible.
    pub fn log_llm_segment(&mut self, index: usize, text: &str, synthesis_started_at: std::time::Instant) {
        let elapsed_from_speech_ms = synthesis_started_at
            .checked_duration_since(self.speech_start_at)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_else(|| self.speech_start_at.elapsed().as_millis() as u64);
        let elapsed_from_llm_start_ms = self
            .llm_start_at
            .and_then(|t| synthesis_started_at.checked_duration_since(t))
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let line = format!(
            "{} [PROFILE] session={} step=LLM_SEGMENT segment={} text=\"{}\" elapsed_from_speech_ms={} elapsed_from_llm_start_ms={}",
            iso_timestamp(),
            self.session_id,
            index,
            text,
            elapsed_from_speech_ms,
            elapsed_from_llm_start_ms,
        );
        self.writer.write_line(&line);
        tracing::info!(
            session = %self.session_id,
            segment = index,
            text = %text,
            elapsed_from_speech_ms,
            elapsed_from_llm_start_ms,
            "PROFILE LLM_SEGMENT"
        );
    }

    /// Log TTS_SEGMENT_DONE — TTS synthesis for one segment completed.
    ///
    /// `synthesis_started_at` is when TTS began (= LLM segment ready time).
    /// `tts_duration_ms` is how long synthesis took.
    /// Logs both durations so the log shows the full picture:
    ///   - `tts_ready_from_speech_ms`  : speech_start → LLM produced the segment
    ///   - `tts_duration_ms`           : time spent in TTS synthesis
    ///   - `tts_done_from_speech_ms`   : speech_start → segment audio ready
    pub fn log_tts_segment_done(
        &mut self,
        index: usize,
        tts_duration_ms: u64,
        synthesis_started_at: std::time::Instant,
    ) {
        let tts_ready_from_speech_ms = synthesis_started_at
            .checked_duration_since(self.speech_start_at)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let tts_done_from_speech_ms = tts_ready_from_speech_ms + tts_duration_ms;
        let line = format!(
            "{} [PROFILE] session={} step=TTS_SEGMENT_DONE segment={} tts_duration_ms={} tts_ready_from_speech_ms={} tts_done_from_speech_ms={}",
            iso_timestamp(),
            self.session_id,
            index,
            tts_duration_ms,
            tts_ready_from_speech_ms,
            tts_done_from_speech_ms,
        );
        self.writer.write_line(&line);
        tracing::info!(
            session = %self.session_id,
            segment = index,
            tts_duration_ms,
            tts_ready_from_speech_ms,
            tts_done_from_speech_ms,
            "PROFILE TTS_SEGMENT_DONE"
        );
    }

    /// Log LLM_FIRST_SEGMENT — first TTS segment completed (streaming pipeline).
    ///
    /// This is the key latency metric for the streaming pipeline:
    /// elapsed from speech-start and from LLM-start until the first
    /// synthesised audio segment is ready to play.
    pub fn log_llm_first_segment(&mut self) {
        let elapsed_from_speech_ms = self.speech_start_at.elapsed().as_millis() as u64;
        let elapsed_from_llm_start_ms = self
            .llm_start_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let line = format!(
            "{} [PROFILE] session={} step=LLM_FIRST_SEGMENT elapsed_from_speech_ms={} elapsed_from_llm_start={}ms",
            iso_timestamp(),
            self.session_id,
            elapsed_from_speech_ms,
            elapsed_from_llm_start_ms
        );
        self.writer.write_line(&line);
        tracing::info!(
            session = %self.session_id,
            elapsed_from_speech_ms,
            elapsed_from_llm_start_ms,
            "PROFILE LLM_FIRST_SEGMENT"
        );
    }

    /// Log LLM_DONE.
    pub fn log_llm_done(&mut self) {
        self.llm_done_at = Some(Instant::now());
        let elapsed_from_llm_start_ms = self
            .llm_start_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let line = format!(
            "{} [PROFILE] session={} step=LLM_DONE elapsed_from_llm_start={}ms",
            iso_timestamp(),
            self.session_id,
            elapsed_from_llm_start_ms
        );
        self.writer.write_line(&line);
        tracing::debug!(
            session = %self.session_id,
            elapsed_from_llm_start_ms,
            "PROFILE LLM_DONE"
        );
    }

    // ── TTS phase ──────────────────────────────────────────────────────────

    /// Log TTS_START.
    pub fn log_tts_start(&mut self) {
        self.tts_start_at = Some(Instant::now());
        let elapsed_from_llm_done_ms = self
            .llm_done_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let line = format!(
            "{} [PROFILE] session={} step=TTS_START elapsed_from_llm_done={}ms",
            iso_timestamp(),
            self.session_id,
            elapsed_from_llm_done_ms
        );
        self.writer.write_line(&line);
        tracing::debug!(
            session = %self.session_id,
            elapsed_from_llm_done_ms,
            "PROFILE TTS_START"
        );
    }

    /// Log TTS_DONE.
    pub fn log_tts_done(&mut self) {
        let elapsed_from_tts_start_ms = self
            .tts_start_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let line = format!(
            "{} [PROFILE] session={} step=TTS_DONE elapsed_from_tts_start={}ms",
            iso_timestamp(),
            self.session_id,
            elapsed_from_tts_start_ms
        );
        self.writer.write_line(&line);
        tracing::debug!(
            session = %self.session_id,
            elapsed_from_tts_start_ms,
            "PROFILE TTS_DONE"
        );
    }

    // ── Playback phase ─────────────────────────────────────────────────────

    /// Log AUDIO_PLAY — final step; computes total latency from both SpeechStart and STT_FINAL.
    pub fn log_audio_play(&mut self) {
        let total_from_speech_ms = self.speech_start_at.elapsed().as_millis() as u64;
        let total_from_stt_ms = self
            .stt_final_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let line = format!(
            "{} [PROFILE] session={} step=AUDIO_PLAY total_latency_from_speech_ms={} total_latency_from_stt_ms={}",
            iso_timestamp(),
            self.session_id,
            total_from_speech_ms,
            total_from_stt_ms
        );
        self.writer.write_line(&line);
        tracing::info!(
            session = %self.session_id,
            total_from_speech_ms,
            total_from_stt_ms,
            "PROFILE AUDIO_PLAY (pipeline complete)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null_writer() -> VoiceProfilerWriter {
        VoiceProfilerWriter::null()
    }

    #[test]
    fn iso_timestamp_format() {
        let ts = iso_timestamp();
        assert!(ts.ends_with('Z'), "timestamp should end with Z: {}", ts);
        assert!(ts.contains('T'), "timestamp should contain T: {}", ts);
        assert_eq!(ts.len(), 24, "timestamp length: {}", ts);
    }

    #[test]
    fn startup_datetime_str_format() {
        let s = startup_datetime_str();
        // Expected: "YYYY-MM-DD_HHMMSS" — 17 chars, contains '_', no colons.
        assert_eq!(s.len(), 17, "startup_datetime_str length: {}", s);
        assert!(s.contains('_'), "must contain underscore: {}", s);
        assert!(!s.contains(':'), "must not contain colons: {}", s);
        // Basic sanity: year prefix "20"
        assert!(s.starts_with("20"), "year should start with 20: {}", s);
    }

    #[test]
    fn null_writer_does_not_panic() {
        let w = VoiceProfilerWriter::null();
        // Writing to null writer must be a no-op and never panic.
        w.write_line("test line 1");
        w.write_line("test line 2");
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn profile_session_creates_unique_ids() {
        let w = null_writer();
        let s1 = ProfileSession::new(w.clone());
        let s2 = ProfileSession::new(w.clone());
        assert_ne!(s1.session_id, s2.session_id);
    }

    #[test]
    fn stt_partial_deduplication() {
        let w = null_writer();
        let mut session = ProfileSession::new(w);
        // First call: new text, recorded.
        session.log_stt_partial("hello");
        assert_eq!(session.last_partial_text, "hello");
        // Same text: skipped.
        session.log_stt_partial("hello");
        assert_eq!(session.last_partial_text, "hello");
        // Different text: recorded.
        session.log_stt_partial("hello world");
        assert_eq!(session.last_partial_text, "hello world");
    }

    #[test]
    fn stt_final_sets_anchor() {
        let w = null_writer();
        let mut session = ProfileSession::new(w);
        assert!(session.stt_final_at.is_none());
        session.log_stt_final("test text");
        assert!(session.stt_final_at.is_some());
    }

    #[test]
    fn full_pipeline_sequence() {
        let w = null_writer();
        let mut session = ProfileSession::new(w);
        session.log_stt_partial("hel");
        session.log_stt_partial("hello");
        session.log_stt_partial("hello"); // deduped — no write
        session.log_stt_final("hello world");
        session.log_llm_start();
        session.log_llm_done();
        session.log_tts_start();
        session.log_tts_done();
        session.log_audio_play();
        // No panics — all steps completed successfully.
    }
}
