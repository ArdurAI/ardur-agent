//! PDF extraction logic.

use serde::{Deserialize, Serialize};

/// A table extracted from a PDF.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfTable {
    pub rows: Vec<Vec<String>>,
    pub page: u32,
}

/// A page in a PDF document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfPage {
    pub number: u32,
    pub text: String,
    pub tables: Vec<PdfTable>,
}

/// A PDF document with metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfDocument {
    pub title: Option<String>,
    pub author: Option<String>,
    pub pages: Vec<PdfPage>,
    pub metadata: serde_json::Value,
}

/// Extractor for PDF documents.
pub struct PdfExtractor;

impl PdfExtractor {
    pub fn new() -> Self { Self }

    pub fn extract_text(&self, _data: &[u8]) -> Result<String, String> {
        Ok("Mock PDF text extraction".to_string())
    }

    pub fn extract_tables(&self, _data: &[u8]) -> Result<Vec<PdfTable>, String> {
        Ok(vec![PdfTable {
            rows: vec![vec!["A".to_string(), "B".to_string()], vec!["1".to_string(), "2".to_string()]],
            page: 1,
        }])
    }

    pub fn extract_metadata(&self, _data: &[u8]) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "title": "Mock PDF",
            "pages": 3,
        }))
    }

    pub fn parse(&self, data: &[u8]) -> Result<PdfDocument, String> {
        Ok(PdfDocument {
            title: Some("Mock PDF".to_string()),
            author: None,
            pages: vec![PdfPage {
                number: 1,
                text: self.extract_text(data)?,
                tables: self.extract_tables(data)?,
            }],
            metadata: self.extract_metadata(data)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_mock() {
        let extractor = PdfExtractor::new();
        let text = extractor.extract_text(b"fake pdf data").unwrap();
        assert!(text.contains("Mock PDF"));
    }

    #[test]
    fn extract_tables_mock() {
        let extractor = PdfExtractor::new();
        let tables = extractor.extract_tables(b"fake pdf data").unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows.len(), 2);
    }

    #[test]
    fn parse_document() {
        let extractor = PdfExtractor::new();
        let doc = extractor.parse(b"fake pdf data").unwrap();
        assert_eq!(doc.pages.len(), 1);
        assert!(doc.title.is_some());
    }
}
