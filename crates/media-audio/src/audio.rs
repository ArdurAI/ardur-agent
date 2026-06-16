use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    Wav,
    Ogg,
    Flac,
    Aac,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioClip {
    pub id: String,
    pub format: AudioFormat,
    pub duration_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioClip {
    pub fn new(format: AudioFormat, duration_ms: u64) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            format,
            duration_ms,
            sample_rate: 44100,
            channels: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AudioProvider {
    clips: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, AudioClip>>>,
}

impl Default for AudioProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioProvider {
    pub fn new() -> Self {
        Self {
            clips: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    pub fn add_clip(&self, clip: AudioClip) -> crate::error::Result<()> {
        let mut clips = self.clips.write().map_err(|_| {
            crate::error::MediaError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        clips.insert(clip.id.clone(), clip);
        Ok(())
    }

    pub fn get_clip(&self, id: &str) -> crate::error::Result<AudioClip> {
        let clips = self.clips.read().map_err(|_| {
            crate::error::MediaError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        clips
            .get(id)
            .cloned()
            .ok_or_else(|| crate::error::MediaError::ProcessingFailed(format!("clip not found: {id}")))
    }

    pub fn list_clips(&self) -> crate::error::Result<Vec<AudioClip>> {
        let clips = self.clips.read().map_err(|_| {
            crate::error::MediaError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(clips.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clip_creation() {
        let clip = AudioClip::new(AudioFormat::Mp3, 5000);
        assert_eq!(clip.format, AudioFormat::Mp3);
        assert_eq!(clip.duration_ms, 5000);
    }

    #[test]
    fn test_provider_add_and_get() {
        let provider = AudioProvider::new();
        let clip = AudioClip::new(AudioFormat::Wav, 3000);
        let id = clip.id.clone();
        provider.add_clip(clip).unwrap();
        let retrieved = provider.get_clip(&id).unwrap();
        assert_eq!(retrieved.format, AudioFormat::Wav);
    }

    #[test]
    fn test_provider_list() {
        let provider = AudioProvider::new();
        provider.add_clip(AudioClip::new(AudioFormat::Mp3, 1000)).unwrap();
        provider.add_clip(AudioClip::new(AudioFormat::Wav, 2000)).unwrap();
        let list = provider.list_clips().unwrap();
        assert_eq!(list.len(), 2);
    }
}
