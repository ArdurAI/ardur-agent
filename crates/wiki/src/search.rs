use regex::Regex;

use crate::page::WikiPage;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub page: WikiPage,
    pub score: f64,
    pub matched_terms: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WikiSearch {
    // Simple in-memory search engine
}

impl Default for WikiSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl WikiSearch {
    pub fn new() -> Self {
        Self {}
    }

    pub fn search(&self, query: &str, pages: &[WikiPage]) -> Vec<SearchResult> {
        let query_lower = query.to_lowercase();
        let terms: Vec<&str> = query_lower.split_whitespace().collect();
        let mut results = Vec::new();

        for page in pages {
            let mut score = 0.0;
            let mut matched = Vec::new();
            let title_lower = page.title.to_lowercase();
            let content_lower = page.content.to_lowercase();

            for term in &terms {
                if title_lower.contains(term) {
                    score += 3.0;
                    matched.push(term.to_string());
                }
                if content_lower.contains(term) {
                    score += 1.0;
                    if !matched.contains(&term.to_string()) {
                        matched.push(term.to_string());
                    }
                }
                for tag in &page.tags {
                    if tag.to_lowercase().contains(term) {
                        score += 2.0;
                        if !matched.contains(&term.to_string()) {
                            matched.push(term.to_string());
                        }
                    }
                }
            }

            if score > 0.0 {
                results.push(SearchResult {
                    page: page.clone(),
                    score,
                    matched_terms: matched,
                });
            }
        }

        // Sort by score descending
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    pub fn search_by_tag(&self, tag: &str, pages: &[WikiPage]) -> Vec<WikiPage> {
        let tag_lower = tag.to_lowercase();
        pages
            .iter()
            .filter(|p| p.tags.iter().any(|t| t.to_lowercase() == tag_lower))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::WikiPage;

    #[test]
    fn test_search_by_title() {
        let search = WikiSearch::new();
        let pages = vec![
            WikiPage::new("Rust Guide", "/rust", "Learn Rust", "author"),
            WikiPage::new("Python Tips", "/python", "Python tricks", "author"),
        ];
        let results = search.search("rust", &pages);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page.title, "Rust Guide");
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn test_search_by_content() {
        let search = WikiSearch::new();
        let pages = vec![
            WikiPage::new("Page 1", "/p1", "content about rust programming", "author"),
            WikiPage::new("Page 2", "/p2", "content about python", "author"),
        ];
        let results = search.search("rust", &pages);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page.title, "Page 1");
    }

    #[test]
    fn test_search_by_tag() {
        let search = WikiSearch::new();
        let mut page = WikiPage::new("Tagged", "/tagged", "content", "author");
        page.add_tag("rust");
        let pages = vec![page];
        let results = search.search("rust", &pages);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page.title, "Tagged");
    }

    #[test]
    fn test_search_by_tag_filter() {
        let search = WikiSearch::new();
        let mut page1 = WikiPage::new("Rust", "/rust", "content", "author");
        page1.add_tag("programming");
        let mut page2 = WikiPage::new("Python", "/python", "content", "author");
        page2.add_tag("programming");
        let pages = vec![page1, page2];
        let results = search.search_by_tag("programming", &pages);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_no_results() {
        let search = WikiSearch::new();
        let pages = vec![WikiPage::new("Page", "/page", "content", "author")];
        let results = search.search("nonexistent", &pages);
        assert_eq!(results.len(), 0);
    }
}
