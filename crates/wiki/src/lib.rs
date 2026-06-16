pub mod error;
pub mod page;
pub mod registry;
pub mod search;

pub use error::{WikiError, Result};
pub use page::{WikiPage, PageId, PageStatus};
pub use registry::WikiRegistry;
pub use search::{WikiSearch, SearchResult};
