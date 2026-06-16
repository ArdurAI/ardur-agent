pub mod error;
pub mod provider;
pub mod config;

pub use error::{OpenAiCompatError, Result};
pub use provider::{OpenAiCompatProvider, ProviderCapabilities};
pub use config::{OpenAiCompatConfig, ConfigBuilder};
