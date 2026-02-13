//! Audio utility functions.
//!
//! PCM format conversion, resampling (48 kHz ↔ 16 kHz),
//! and level metering helpers.  Uses `rubato` for high-quality
//! sample-rate conversion.

/// Resample 48 kHz stereo interleaved PCM to 16 kHz mono.
pub fn resample_48k_stereo_to_16k_mono(_input: &[f32]) -> Vec<f32> {
    // Stub — will use rubato SincFixedIn
    Vec::new()
}

/// Resample 24 kHz mono PCM to 48 kHz mono (for TTS output → Discord).
pub fn resample_24k_to_48k(_input: &[f32]) -> Vec<f32> {
    // Stub — will use rubato SincFixedIn
    Vec::new()
}

/// Convert i16 PCM samples to f32 (range -1.0 .. 1.0).
pub fn pcm_i16_to_f32(input: &[i16]) -> Vec<f32> {
    input.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// Convert f32 PCM samples to i16.
pub fn pcm_f32_to_i16(input: &[f32]) -> Vec<i16> {
    input
        .iter()
        .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
        .collect()
}

/// Compute RMS (root mean square) level of an f32 PCM buffer.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}
