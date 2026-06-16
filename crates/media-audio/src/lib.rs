pub mod error;
pub mod audio;
pub mod decode;

pub use error::{MediaError, Result};
pub use audio::{AudioProvider, AudioFormat, AudioClip};
pub use decode::{AudioDecoder, DecodeResult};
