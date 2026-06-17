use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub type PageId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PageStatus {
    Draft,
    Published,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub id: PageId,
    pub title: String,
    pub path: String,
    pub content: String,
    pub status: PageStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub author: String,
    pub tags: Vec<String>,
    pub metadata: HashMap<String, String>,
}

impl WikiPage {
    pub fn new(title: &str, path: &str, content: &str, author: &str) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7().to_string(),
            title: title.to_string(),
            path: path.to_string(),
            content: content.to_string(),
            status: PageStatus::Draft,
            created_at: now,
            updated_at: now,
            author: author.to_string(),
            tags: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    pub fn publish(&mut self) {
        self.status = PageStatus::Published;
        self.updated_at = Utc::now();
    }

    pub fn archive(&mut self) {
        self.status = PageStatus::Archived;
        self.updated_at = Utc::now();
    }

    pub fn update_content(&mut self, content: &str) {
        self.content = content.to_string();
        self.updated_at = Utc::now();
    }

    pub fn add_tag(&mut self, tag: &str) {
        if !self.tags.contains(&tag.to_string()) {
            self.tags.push(tag.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_creation() {
        let page = WikiPage::new("Test Page", "/test", "Hello world", "gnani");
        assert_eq!(page.title, "Test Page");
        assert_eq!(page.path, "/test");
        assert_eq!(page.status, PageStatus::Draft);
        assert_eq!(page.author, "gnani");
    }

    #[test]
    fn test_page_publish() {
        let mut page = WikiPage::new("Test", "/test", "content", "author");
        page.publish();
        assert_eq!(page.status, PageStatus::Published);
    }

    #[test]
    fn test_page_archive() {
        let mut page = WikiPage::new("Test", "/test", "content", "author");
        page.archive();
        assert_eq!(page.status, PageStatus::Archived);
    }

    #[test]
    fn test_page_update_content() {
        let mut page = WikiPage::new("Test", "/test", "old", "author");
        page.update_content("new");
        assert_eq!(page.content, "new");
    }

    #[test]
    fn test_page_add_tag() {
        let mut page = WikiPage::new("Test", "/test", "content", "author");
        page.add_tag("rust");
        page.add_tag("rust"); // duplicate should be ignored
        assert_eq!(page.tags.len(), 1);
        assert_eq!(page.tags[0], "rust");
    }
}
