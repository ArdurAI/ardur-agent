use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub max_concurrent_tasks: usize,
    pub task_timeout_secs: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: 10,
            task_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutomationEngine {
    config: EngineConfig,
    tasks: std::sync::Arc<std::sync::RwLock<HashMap<String, crate::task::AutomationTask>>>,
}

impl Default for AutomationEngine {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

impl AutomationEngine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            tasks: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    pub fn create_task(&self, name: &str) -> crate::error::Result<String> {
        let mut tasks = self.tasks.write().map_err(|_| {
            crate::error::AutomationError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let task = crate::task::AutomationTask::new(name);
        let id = task.id.clone();
        tasks.insert(id.clone(), task);
        Ok(id)
    }

    pub fn get_task(&self, id: &str) -> crate::error::Result<crate::task::AutomationTask> {
        let tasks = self.tasks.read().map_err(|_| {
            crate::error::AutomationError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        tasks
            .get(id)
            .cloned()
            .ok_or_else(|| crate::error::AutomationError::TaskNotFound(id.to_string()))
    }

    pub fn start_task(&self, id: &str) -> crate::error::Result<()> {
        let mut tasks = self.tasks.write().map_err(|_| {
            crate::error::AutomationError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| crate::error::AutomationError::TaskNotFound(id.to_string()))?;
        task.start();
        Ok(())
    }

    pub fn complete_task(
        &self,
        id: &str,
        result: crate::task::TaskResult,
    ) -> crate::error::Result<()> {
        let mut tasks = self.tasks.write().map_err(|_| {
            crate::error::AutomationError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| crate::error::AutomationError::TaskNotFound(id.to_string()))?;
        task.complete(result);
        Ok(())
    }

    pub fn list_tasks(&self) -> crate::error::Result<Vec<crate::task::AutomationTask>> {
        let tasks = self.tasks.read().map_err(|_| {
            crate::error::AutomationError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(tasks.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_create_and_get() {
        let engine = AutomationEngine::default();
        let id = engine.create_task("test").unwrap();
        let task = engine.get_task(&id).unwrap();
        assert_eq!(task.name, "test");
    }

    #[test]
    fn test_engine_start_and_complete() {
        let engine = AutomationEngine::default();
        let id = engine.create_task("test").unwrap();
        engine.start_task(&id).unwrap();
        engine
            .complete_task(
                &id,
                crate::task::TaskResult {
                    success: true,
                    output: "done".to_string(),
                    metadata: std::collections::HashMap::new(),
                },
            )
            .unwrap();
        let task = engine.get_task(&id).unwrap();
        assert_eq!(task.status, crate::task::TaskStatus::Completed);
    }

    #[test]
    fn test_engine_list() {
        let engine = AutomationEngine::default();
        engine.create_task("task1").unwrap();
        engine.create_task("task2").unwrap();
        let list = engine.list_tasks().unwrap();
        assert_eq!(list.len(), 2);
    }
}
