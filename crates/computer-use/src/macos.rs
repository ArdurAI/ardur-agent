//! macOS UI automation via Accessibility API.
//!
//! Phase 1 provides a mock implementation for testing. Phase 2 will
//! integrate with the macOS Accessibility API via `accessibility` crate
//! or FFI bindings.

use serde::{Deserialize, Serialize};

/// A UI element on macOS.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiElement {
    pub role: String,
    pub title: Option<String>,
    pub identifier: Option<String>,
    pub bounds: Option<(i32, i32, i32, i32)>,
}

/// A UI action to perform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiAction {
    Click { x: i32, y: i32 },
    Type { text: String },
    PressKey { key: String },
    Scroll { x: i32, y: i32, delta: i32 },
    Focus { element_id: String },
}

/// macOS automation interface.
pub struct MacOsAutomation;

impl MacOsAutomation {
    pub fn new() -> Self { Self }

    pub fn list_elements(&self, _app: &str) -> Result<Vec<UiElement>, String> {
        Ok(vec![
            UiElement {
                role: "button".to_string(),
                title: Some("OK".to_string()),
                identifier: Some("ok-button".to_string()),
                bounds: Some((100, 100, 50, 30)),
            },
            UiElement {
                role: "textfield".to_string(),
                title: None,
                identifier: Some("input-field".to_string()),
                bounds: Some((100, 150, 200, 30)),
            },
        ])
    }

    pub fn perform_action(&self, action: &UiAction) -> Result<bool, String> {
        match action {
            UiAction::Click { x, y } => {
                println!("[mock] Click at ({x}, {y})");
                Ok(true)
            }
            UiAction::Type { text } => {
                println!("[mock] Type: {text}");
                Ok(true)
            }
            UiAction::PressKey { key } => {
                println!("[mock] Press key: {key}");
                Ok(true)
            }
            UiAction::Scroll { x, y, delta } => {
                println!("[mock] Scroll at ({x}, {y}) by {delta}");
                Ok(true)
            }
            UiAction::Focus { element_id } => {
                println!("[mock] Focus element: {element_id}");
                Ok(true)
            }
        }
    }

    pub fn screenshot(&self) -> Result<Vec<u8>, String> {
        Ok(vec![0x89, 0x50, 0x4E, 0x47]) // PNG header mock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_elements_mock() {
        let auto = MacOsAutomation::new();
        let elements = auto.list_elements("Safari").unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].role, "button");
    }

    #[test]
    fn perform_click() {
        let auto = MacOsAutomation::new();
        let action = UiAction::Click { x: 10, y: 20 };
        assert!(auto.perform_action(&action).unwrap());
    }

    #[test]
    fn screenshot_mock() {
        let auto = MacOsAutomation::new();
        let data = auto.screenshot().unwrap();
        assert_eq!(&data[..4], &[0x89, 0x50, 0x4E, 0x47]);
    }
}
