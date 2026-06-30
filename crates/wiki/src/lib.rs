pub mod error;
pub mod page;
pub mod registry;
pub mod search;

pub use error::{Result, WikiError};
pub use page::{PageId, PageStatus, WikiPage};
pub use registry::WikiRegistry;
pub use search::{SearchResult, WikiSearch};
