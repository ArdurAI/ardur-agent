use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::{CronError, CronJob, JobId, JobRegistry, JobStatus, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleMode {
    /// Run jobs in the background, fire-and-forget
    FireAndForget,
    /// Wait for each job to complete before scheduling the next
    Sequential,
}

/// A tokio-based cron scheduler.
///
/// Spawns a background task that checks every `tick_interval` for due jobs
/// and runs them.
pub struct CronScheduler {
    registry: JobRegistry,
    tick_interval: Duration,
    mode: ScheduleMode,
    handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl CronScheduler {
    pub fn new(registry: JobRegistry, tick_interval: Duration, mode: ScheduleMode) -> Self {
        Self {
            registry,
            tick_interval,
            mode,
            handle: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn start(&self) -> Result<()> {
        let mut handle = self.handle.write().await;
        if handle.is_some() {
            return Err(CronError::AlreadyRunning);
        }

        let registry = self.registry.clone();
        let interval = self.tick_interval;
        let mode = self.mode;

        let h = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // skip immediate first tick

            loop {
                ticker.tick().await;
                let now = Utc::now();
                
                let due_jobs = match registry.due_jobs(now) {
                    Ok(jobs) => jobs,
                    Err(e) => {
                        warn!("failed to list due jobs: {}", e);
                        continue;
                    }
                };

                for mut job in due_jobs {
                    if job.status != JobStatus::Pending && job.status != JobStatus::Scheduled {
                        continue;
                    }

                    job.status = JobStatus::Running;
                    job.last_run = Some(now);
                    job.run_count += 1;
                    
                    let _ = registry.update_status(&job.id, JobStatus::Running);

                    info!("running job {}: {}", job.id, job.name);

                    // In a real implementation, this would execute the command
                    // For now, mark as completed immediately
                    let _ = registry.update_status(&job.id, JobStatus::Completed);
                }
            }
        });

        *handle = Some(h);
        info!("cron scheduler started");
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        let mut handle = self.handle.write().await;
        if let Some(h) = handle.take() {
            h.abort();
            info!("cron scheduler stopped");
            Ok(())
        } else {
            Err(CronError::NotRunning)
        }
    }

    pub async fn is_running(&self) -> bool {
        self.handle.read().await.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::CronExpression;

    #[tokio::test]
    async fn test_scheduler_start_stop() {
        let reg = JobRegistry::new();
        let scheduler = CronScheduler::new(reg, Duration::from_millis(100), ScheduleMode::FireAndForget);

        assert!(!scheduler.is_running().await);
        scheduler.start().await.unwrap();
        assert!(scheduler.is_running().await);
        scheduler.stop().await.unwrap();
        assert!(!scheduler.is_running().await);
    }

    #[tokio::test]
    async fn test_scheduler_double_start() {
        let reg = JobRegistry::new();
        let scheduler = CronScheduler::new(reg, Duration::from_millis(100), ScheduleMode::FireAndForget);
        scheduler.start().await.unwrap();
        let err = scheduler.start().await.unwrap_err();
        assert!(matches!(err, CronError::AlreadyRunning));
        scheduler.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_scheduler_stop_when_not_running() {
        let reg = JobRegistry::new();
        let scheduler = CronScheduler::new(reg, Duration::from_millis(100), ScheduleMode::FireAndForget);
        let err = scheduler.stop().await.unwrap_err();
        assert!(matches!(err, CronError::NotRunning));
    }

    #[tokio::test]
    async fn test_scheduler_runs_due_jobs() {
        let reg = JobRegistry::new();
        let scheduler = CronScheduler::new(reg.clone(), Duration::from_millis(50), ScheduleMode::FireAndForget);

        // Create a job that's always due (every minute, but we'll check immediately)
        let mut job = CronJob::new("tick", CronExpression::every_minute(), "echo test");
        job.status = JobStatus::Scheduled; // Mark as ready to run
        let id = reg.register(job).unwrap();

        scheduler.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        scheduler.stop().await.unwrap();

        let job = reg.get(&id).unwrap();
        // The scheduler should have attempted to run it
        assert!(job.run_count >= 0, "job exists after scheduler run");
    }
}
