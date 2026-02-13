//! Audio utility functions.
//!
//! PCM format conversion, resampling (48 kHz ↔ other rates),
//! and level metering helpers.  Uses `rubato` for high-quality
//! sample-rate conversion when needed for TTS output → Discord playback.

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Resample mono PCM from `from_hz` to `to_hz`.
///
/// Returns the resampled f32 samples or an error if the resampler
/// cannot be created or processing fails.
pub fn resample_mono(input: &[f32], from_hz: u32, to_hz: u32) -> Result<Vec<f32>, String> {
    if from_hz == to_hz || input.is_empty() {
        return Ok(input.to_vec());
    }

    let ratio = to_hz as f64 / from_hz as f64;

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };

    let chunk_size = input.len();
    let mut resampler = SincFixedIn::<f32>::new(ratio, 2.0, params, chunk_size, 1)
        .map_err(|e| format!("Failed to create resampler: {}", e))?;

    let waves_in = vec![input.to_vec()];
    let result = resampler
        .process(&waves_in, None)
        .map_err(|e| format!("Resample failed: {}", e))?;

    Ok(result.into_iter().next().unwrap_or_default())
}

/// Resample 24 kHz mono PCM to 48 kHz mono (for TTS output → Discord).
pub fn resample_24k_to_48k(input: &[f32]) -> Result<Vec<f32>, String> {
    resample_mono(input, 24000, 48000)
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

/// Convert mono f32 PCM samples (48 kHz, range -1.0..1.0) into WAV bytes.
///
/// The resulting WAV can be passed directly to songbird via `Into<Input>`.
pub fn pcm_f32_to_wav_bytes(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, String> {
    use std::io::Cursor;

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut buf = Vec::new();
    {
        let cursor = Cursor::new(&mut buf);
        let mut writer =
            hound::WavWriter::new(cursor, spec).map_err(|e| format!("WAV writer: {}", e))?;
        for &s in samples {
            let i16_sample = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            writer
                .write_sample(i16_sample)
                .map_err(|e| format!("WAV write: {}", e))?;
        }
        writer
            .finalize()
            .map_err(|e| format!("WAV finalize: {}", e))?;
    }

    Ok(buf)
}

/// Compute RMS (root mean square) level of an f32 PCM buffer.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_i16_to_f32_range() {
        let input = vec![i16::MAX, 0, i16::MIN];
        let output = pcm_i16_to_f32(&input);
        assert!((output[0] - 0.999969).abs() < 0.001);
        assert!((output[1] - 0.0).abs() < f32::EPSILON);
        assert!((output[2] - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn pcm_f32_to_i16_range() {
        let input = vec![1.0, 0.0, -1.0];
        let output = pcm_f32_to_i16(&input);
        assert_eq!(output[0], 32767);
        assert_eq!(output[1], 0);
        assert_eq!(output[2], -32767);
    }

    #[test]
    fn pcm_f32_to_i16_clamp() {
        let input = vec![2.0, -2.0]; // out of range
        let output = pcm_f32_to_i16(&input);
        assert_eq!(output[0], 32767);
        assert_eq!(output[1], -32768);
    }

    #[test]
    fn rms_zero() {
        assert!(rms(&vec![0.0; 100]) < 0.001);
    }

    #[test]
    fn rms_constant() {
        let rms_val = rms(&vec![0.5; 100]);
        assert!((rms_val - 0.5).abs() < 0.001);
    }

    #[test]
    fn rms_empty() {
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn resample_mono_same_rate() {
        let input = vec![0.1, 0.2, 0.3];
        let output = resample_mono(&input, 48000, 48000).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn resample_mono_empty() {
        let output = resample_mono(&[], 24000, 48000).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn pcm_f32_to_wav_bytes_roundtrip() {
        let samples = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];
        let wav = pcm_f32_to_wav_bytes(&samples, 48000).unwrap();
        // Should start with RIFF header
        assert_eq!(&wav[0..4], b"RIFF");
        // Parse back with hound
        let reader =
            hound::WavReader::new(std::io::Cursor::new(&wav)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 48000);
        let read_samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(read_samples.len(), 5);
        // 0.5 * 32767 ≈ 16383
        assert!((read_samples[1] - 16383).abs() <= 1);
    }

    #[test]
    fn pcm_f32_to_wav_bytes_empty() {
        let wav = pcm_f32_to_wav_bytes(&[], 48000).unwrap();
        let reader = hound::WavReader::new(std::io::Cursor::new(&wav)).unwrap();
        let count = reader.into_samples::<i16>().count();
        assert_eq!(count, 0);
    }

    #[test]
    fn resample_24k_to_48k_doubles_length() {
        // 480 samples @ 24kHz = 20ms → should yield ~960 samples @ 48kHz
        let input: Vec<f32> = (0..480).map(|i| (i as f32 / 480.0).sin()).collect();
        let output = resample_24k_to_48k(&input).unwrap();
        // rubato SincFixedIn may produce fewer samples due to filter delay;
        // just verify output is non-empty and roughly in the right ballpark
        assert!(
            output.len() > 400 && output.len() <= 1100,
            "expected 400..1100 samples, got {}",
            output.len()
        );
    }
}
