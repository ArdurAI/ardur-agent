#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! ardur-pdf — PDF and document extraction.
//!
//! Plan family: §6.7 (`plans/6.7-pdf-extraction-blueprint.md`).

mod error;
mod extractor;
mod tools;

pub use error::{PdfError, Result};
pub use extractor::{PdfDocument, PdfPage, PdfTable, PdfExtractor};
pub use tools::PdfExtractTool;
