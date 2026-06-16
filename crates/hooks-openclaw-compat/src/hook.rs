use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct OpenClawHook {
    pub name: String,
    pub action: String,
    pub enabled: bool,
}

impl OpenClawHook {
    pub fn new(name: &str, action: &str) -> Self {
        Self {
            name: name.to_string(),
            action: action.to_string(),
            enabled: true,
        }
    }

    pub fn disable(&mut self) {
        self.enabled = false;
    }

    pub fn enable(&mut self) {
        self.enabled = true;
    }
}

#[derive(Debug, Clone)]
pub struct HookRegistry {
    hooks: std::sync::Arc<std::sync::RwLock<HashMap<String, OpenClawHook>>>,
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            hooks: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    pub fn register(&self, hook: OpenClawHook) -> crate::error::Result<()> {
        let mut hooks = self.hooks.write().map_err(|_| {
            crate::error::HookCompatError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        hooks.insert(hook.name.clone(), hook);
        Ok(())
    }

    pub fn get(&self, name: &str) -> crate::error::Result<OpenClawHook> {
        let hooks = self.hooks.read().map_err(|_| {
            crate::error::HookCompatError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        hooks
            .get(name)
            .cloned()
            .ok_or_else(|| crate::error::HookCompatError::NotFound(name.to_string()))
    }

    pub fn list(&self) -> crate::error::Result<Vec<OpenClawHook>> {
        let hooks = self.hooks.read().map_err(|_| {
            crate::error::HookCompatError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(hooks.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_creation() {
        let hook = OpenClawHook::new("test", "echo hello");
        assert_eq!(hook.name, "test");
        assert!(hook.enabled);
    }

    #[test]
    fn test_registry_register_and_get() {
        let registry = HookRegistry::new();
        registry.register(OpenClawHook::new("h1", "action1")).unwrap();
        let hook = registry.get("h1").unwrap();
        assert_eq!(hook.action, "action1");
    }

    #[test]
    fn test_registry_list() {
        let registry = HookRegistry::new();
        registry.register(OpenClawHook::new("h1", "a1")).unwrap();
        registry.register(OpenClawHook::new("h2", "a2")).unwrap();
        let list = registry.list().unwrap();
        assert_eq!(list.len(), 2);
    }
}
