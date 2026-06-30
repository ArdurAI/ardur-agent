#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! ardur-search — Web search suite with 5+ providers and egress controls.
//!
//! Plan family: §6.6 (`plans/6.6-web-search-blueprint.md`).

mod error;
mod providers;
mod policy;
mod tools;

pub use error::{SearchError, Result};
pub use providers::{SearchProvider, SearchResult, BraveProvider, DuckDuckGoProvider, SearxngProvider, TavilyProvider, FirecrawlProvider};
pub use policy::{SearchPolicy, DomainRule};
pub use tools::{WebSearchTool};
