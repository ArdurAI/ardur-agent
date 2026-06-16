use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{CronError, Result};
use crate::job::{CronJob, JobId, JobStatus};

#[derive(Debug, Clone)]
pub struct JobRegistry {
    jobs: Arc<RwLock<HashMap<JobId, CronJob>>>,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register(&self, job: CronJob) -> Result<JobId> {
        let mut jobs = self.jobs.write().map_err(|_| CronError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        let id = job.id.clone();
        jobs.insert(id.clone(), job);
        Ok(id)
    }

    pub fn get(&self, id: &JobId) -> Result<CronJob> {
        let jobs = self.jobs.read().map_err(|_| CronError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        jobs.get(id).cloned().ok_or_else(|| CronError::JobNotFound(id.clone()))
    }

    pub fn list(&self) -> Result<Vec<CronJob>> {
        let jobs = self.jobs.read().map_err(|_| CronError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        Ok(jobs.values().cloned().collect())
    }

    pub fn update_status(&self, id: &JobId, status: JobStatus) -> Result<()> {
        let mut jobs = self.jobs.write().map_err(|_| CronError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        let job = jobs.get_mut(id).ok_or_else(|| CronError::JobNotFound(id.clone()))?;
        job.status = status;
        Ok(())
    }

    pub fn remove(&self, id: &JobId) -> Result<()> {
        let mut jobs = self.jobs.write().map_err(|_| CronError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        jobs.remove(id).ok_or_else(|| CronError::JobNotFound(id.clone()))?;
        Ok(())
    }

    pub fn due_jobs(&self, now: chrono::DateTime<chrono::Utc>) -> Result<Vec<CronJob>> {
        let jobs = self.jobs.read().map_err(|_| CronError::Io(std::io::Error::new(std::io::ErrorKind::Other, "poisoned lock")))?;
        Ok(jobs.values().filter(|j| j.is_due(now)).cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::CronExpression;

    #[test]
    fn test_registry_register_and_get() {
        let registry = JobRegistry::new();
        let job = CronJob::new("test", CronExpression::hourly(), "echo hello");
        let id = registry.register(job.clone()).unwrap();
        let retrieved = registry.get(&id).unwrap();
        assert_eq!(retrieved.name, "test");
    }

    #[test]
    fn test_registry_list() {
        let registry = JobRegistry::new();
        registry.register(CronJob::new("job1", CronExpression::hourly(), "cmd1")).unwrap();
        registry.register(CronJob::new("job2", CronExpression::daily(), "cmd2")).unwrap();
        let list = registry.list().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_registry_remove() {
        let registry = JobRegistry::new();
        let id = registry.register(CronJob::new("test", CronExpression::hourly(), "cmd")).unwrap();
        registry.remove(&id).unwrap();
        assert!(registry.get(&id).is_err());
    }

    #[test]
    fn test_registry_due_jobs() {
        let registry = JobRegistry::new();
        let job = CronJob::new("test", CronExpression::every_minute(), "cmd");
        let id = registry.register(job).unwrap();
        let due = registry.due_jobs(chrono::Utc::now()).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, id);
    }
}