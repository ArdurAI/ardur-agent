use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{WikiError, Result};
use crate::page::{PageId, PageStatus, WikiPage};

#[derive(Debug, Clone)]
pub struct WikiRegistry {
    pages: Arc<RwLock<HashMap<PageId, WikiPage>>>,
    path_index: Arc<RwLock<HashMap<String, PageId>>>,
}

impl Default for WikiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WikiRegistry {
    pub fn new() -> Self {
        Self {
            pages: Arc::new(RwLock::new(HashMap::new())),
            path_index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn create(&self, page: WikiPage) -> Result<PageId> {
        let mut pages = self.pages.write().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        let mut path_index = self.path_index.write().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        
        if path_index.contains_key(&page.path) {
            return Err(WikiError::PageAlreadyExists(page.path.clone()));
        }
        
        let id = page.id.clone();
        path_index.insert(page.path.clone(), id.clone());
        pages.insert(id.clone(), page);
        Ok(id)
    }

    pub fn get(&self, id: &PageId) -> Result<WikiPage> {
        let pages = self.pages.read().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        pages.get(id).cloned().ok_or_else(|| WikiError::PageNotFound(id.clone()))
    }

    pub fn get_by_path(&self, path: &str) -> Result<WikiPage> {
        let path_index = self.path_index.read().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        let id = path_index.get(path).ok_or_else(|| WikiError::PageNotFound(path.to_string()))?;
        self.get(id)
    }

    pub fn list(&self) -> Result<Vec<WikiPage>> {
        let pages = self.pages.read().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        Ok(pages.values().cloned().collect())
    }

    pub fn list_by_status(&self, status: PageStatus) -> Result<Vec<WikiPage>> {
        let pages = self.pages.read().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        Ok(pages.values().filter(|p| p.status == status).cloned().collect())
    }

    pub fn update(&self, page: WikiPage) -> Result<()> {
        let mut pages = self.pages.write().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        if !pages.contains_key(&page.id) {
            return Err(WikiError::PageNotFound(page.id.clone()));
        }
        pages.insert(page.id.clone(), page);
        Ok(())
    }

    pub fn delete(&self, id: &PageId) -> Result<()> {
        let mut pages = self.pages.write().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        let mut path_index = self.path_index.write().map_err(|_| WikiError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        
        let page = pages.remove(id).ok_or_else(|| WikiError::PageNotFound(id.clone()))?;
        path_index.remove(&page.path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_create_and_get() {
        let registry = WikiRegistry::new();
        let page = WikiPage::new("Test", "/test", "content", "author");
        let id = registry.create(page.clone()).unwrap();
        let retrieved = registry.get(&id).unwrap();
        assert_eq!(retrieved.title, "Test");
    }

    #[test]
    fn test_registry_get_by_path() {
        let registry = WikiRegistry::new();
        let page = WikiPage::new("Test", "/test", "content", "author");
        registry.create(page).unwrap();
        let retrieved = registry.get_by_path("/test").unwrap();
        assert_eq!(retrieved.title, "Test");
    }

    #[test]
    fn test_registry_duplicate_path() {
        let registry = WikiRegistry::new();
        let page1 = WikiPage::new("Test1", "/test", "content1", "author1");
        let page2 = WikiPage::new("Test2", "/test", "content2", "author2");
        registry.create(page1).unwrap();
        assert!(registry.create(page2).is_err());
    }

    #[test]
    fn test_registry_list_by_status() {
        let registry = WikiRegistry::new();
        let mut page1 = WikiPage::new("Draft", "/draft", "content", "author");
        let mut page2 = WikiPage::new("Published", "/published", "content", "author");
        page2.publish();
        registry.create(page1).unwrap();
        registry.create(page2).unwrap();
        
        let published = registry.list_by_status(PageStatus::Published).unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].title, "Published");
    }

    #[test]
    fn test_registry_delete() {
        let registry = WikiRegistry::new();
        let page = WikiPage::new("Test", "/test", "content", "author");
        let id = registry.create(page).unwrap();
        registry.delete(&id).unwrap();
        assert!(registry.get(&id).is_err());
        assert!(registry.get_by_path("/test").is_err());
    }
}
