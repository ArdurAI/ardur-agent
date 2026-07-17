//! [`ToolRegistry`] — the id→tool lookup the runtime resolves a [`ToolId`]
//! against before dispatching an invocation.

use std::collections::HashMap;

use crate::capability::Capability;
use crate::error::RegistryError;
use crate::tool::{Tool, ToolId};

/// An in-process registry of tools, keyed by [`ToolId`].
///
/// Registration is append-only: a [`ToolId`] already present is rejected with
/// [`RegistryError::DuplicateId`] rather than silently replaced, so a tool's
/// identity is stable for the registry's lifetime.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<ToolId, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tool` under its own [`Tool::id`].
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::DuplicateId`] if a tool is already registered
    /// under the same id; the existing entry is left untouched.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), RegistryError> {
        let id = tool.id();
        if self.tools.contains_key(&id) {
            return Err(RegistryError::DuplicateId(id));
        }
        self.tools.insert(id, tool);
        Ok(())
    }

    /// Resolve a tool by id.
    #[must_use]
    pub fn get(&self, id: &ToolId) -> Option<&dyn Tool> {
        self.tools.get(id).map(AsRef::as_ref)
    }

    /// Every registered tool, in unspecified order.
    #[must_use]
    pub fn list(&self) -> Vec<&dyn Tool> {
        self.tools.values().map(AsRef::as_ref).collect()
    }

    /// Every registered tool that declares `cap` among its
    /// [`Tool::required_capabilities`], in unspecified order.
    #[must_use]
    pub fn find_by_capability(&self, cap: &Capability) -> Vec<&dyn Tool> {
        self.tools
            .values()
            .filter(|tool| tool.required_capabilities().contains(cap))
            .map(AsRef::as_ref)
            .collect()
    }
}
