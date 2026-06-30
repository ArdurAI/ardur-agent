//! Shared media decode value types for audio-capable plans.
//!
//! This crate is the Phase 1 boundary for §10.3 audio transcription and later
//! media plans that need sandboxed decode/normalization. It does not invoke
//! FFmpeg yet; later phases will place subprocess policy and waveform handling
//! behind these request/result shapes.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Audio container/codec formats accepted by the transcription surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    /// MPEG layer 3 audio.
    Mp3,
    /// RIFF/WAVE PCM audio.
    Wav,
    /// Opus audio, either standalone or in a supported container.
    Opus,
    /// Free Lossless Audio Codec.
    Flac,
    /// MPEG-4 audio container.
    M4a,
    /// WebM container with audio tracks.
    WebmAudio,
}

impl AudioFormat {
    /// Return the canonical lowercase extension used for persisted metadata.
    #[must_use]
    pub fn canonical_extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Opus => "opus",
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::WebmAudio => "webm",
        }
    }

    /// Parse a common file extension into an [`AudioFormat`].
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "mp3" => Some(Self::Mp3),
            "wav" | "wave" => Some(Self::Wav),
            "opus" | "ogg" => Some(Self::Opus),
            "flac" => Some(Self::Flac),
            "m4a" | "mp4" => Some(Self::M4a),
            "webm" => Some(Self::WebmAudio),
            _ => None,
        }
    }
}

/// A request to decode or normalize an audio file under the future sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDecodeRequest {
    /// Workspace-scoped path to the source audio file.
    pub input_path: PathBuf,
    /// Declared format after preflight sniffing.
    pub format: AudioFormat,
    /// Target sample rate for normalized PCM output.
    pub target_sample_rate_hz: u32,
    /// Target channel count for normalized PCM output.
    pub target_channel_count: u8,
}

/// Metadata returned with decoded audio frames.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodedAudio {
    /// PCM samples, interleaved by channel.
    pub pcm_i16: Vec<i16>,
    /// Sample rate of the PCM frame stream.
    pub sample_rate_hz: u32,
    /// Number of channels in the PCM frame stream.
    pub channel_count: u8,
    /// Duration represented by the decoded frames.
    pub duration_seconds: u32,
}

/// Failure modes for the future sandboxed decoder.
#[derive(Clone, Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum DecodeError {
    /// The caller supplied an unsupported audio format.
    #[error("unsupported audio format: {format:?}")]
    UnsupportedFormat {
        /// The unsupported format.
        format: AudioFormat,
    },
    /// The sandbox policy needed to run the decoder is not configured.
    #[error("decode sandbox policy is missing")]
    MissingSandboxPolicy,
    /// The requested output shape is invalid.
    #[error("invalid decode target: {reason}")]
    InvalidTarget {
        /// Human-readable validation failure.
        reason: String,
    },
}

/// Validate a decode request without reading or decoding bytes.
pub fn validate_decode_request(request: &AudioDecodeRequest) -> Result<(), DecodeError> {
    if request.target_sample_rate_hz == 0 {
        return Err(DecodeError::InvalidTarget {
            reason: "target sample rate must be non-zero".to_string(),
        });
    }
    if request.target_channel_count == 0 {
        return Err(DecodeError::InvalidTarget {
            reason: "target channel count must be non-zero".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_audio_extensions() {
        assert_eq!(AudioFormat::from_extension(".MP3"), Some(AudioFormat::Mp3));
        assert_eq!(AudioFormat::from_extension("wave"), Some(AudioFormat::Wav));
        assert_eq!(AudioFormat::from_extension("ogg"), Some(AudioFormat::Opus));
        assert_eq!(AudioFormat::from_extension("txt"), None);
    }

    #[test]
    fn decode_request_rejects_zero_targets() {
        let request = AudioDecodeRequest {
            input_path: PathBuf::from("voice.wav"),
            format: AudioFormat::Wav,
            target_sample_rate_hz: 0,
            target_channel_count: 1,
        };

        assert!(matches!(
            validate_decode_request(&request),
            Err(DecodeError::InvalidTarget { .. })
        ));
    }
}
