use crate::context::TaskContext;
use crate::error::TaskFlowError;
use crate::graph::TaskGraph;
use crate::task::{Task, TaskId, TaskResult, TaskStatus};
use async_trait::async_trait;
use futures::future::join_all;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{error, info};

/// Trait for task executors. Implementors define how a single task is run.
#[async_trait]
pub trait TaskExecutor: Send + Sync {
    /// Execute a single task given a shared context.
    async fn execute(&self, task: &Task, context: &TaskContext) -> TaskResult;
}

/// The execution engine that runs tasks respecting dependencies.
#[derive(Debug)]
pub struct ExecutionEngine {
    graph: Arc<Mutex<TaskGraph>>,
}

impl ExecutionEngine {
    /// Create a new execution engine from a task graph.
    pub fn new(graph: TaskGraph) -> Self {
        Self {
            graph: Arc::new(Mutex::new(graph)),
        }
    }

    /// Run all tasks in the graph using the provided executor and context.
    /// Independent tasks are executed in parallel.
    pub async fn run<E: TaskExecutor>(
        &self,
        executor: &E,
        context: &TaskContext,
    ) -> Result<HashMap<TaskId, TaskResult>, TaskFlowError> {
        let graph = self.graph.lock();
        let sorted = graph.topological_sort()?;
        drop(graph);

        let mut completed: HashMap<TaskId, TaskResult> = HashMap::new();
        let mut failed: HashSet<TaskId> = HashSet::new();

        // Group tasks by "levels" — tasks whose dependencies are all satisfied.
        let mut pending: HashSet<TaskId> = sorted.into_iter().collect();

        while !pending.is_empty() {
            let graph = self.graph.lock();
            let ready: Vec<TaskId> = pending
                .iter()
                .copied()
                .filter(|id| {
                    let deps = graph.dependencies(*id);
                    deps.iter()
                        .all(|d| completed.contains_key(d) || failed.contains(d))
                        && deps.iter().all(|d| !failed.contains(d))
                })
                .collect();

            // Tasks whose dependencies have failed should be skipped
            let skipped: Vec<TaskId> = pending
                .iter()
                .copied()
                .filter(|id| {
                    let deps = graph.dependencies(*id);
                    deps.iter().any(|d| failed.contains(d))
                })
                .collect();
            drop(graph);

            for id in skipped {
                pending.remove(&id);
                let skip_result = TaskResult::Failure("dependency failed".into());
                failed.insert(id);
                completed.insert(id, skip_result.clone());
                let mut graph = self.graph.lock();
                if let Some(task) = graph.get_mut(id) {
                    task.result = Some(skip_result);
                    task.status = TaskStatus::Failed;
                }
            }

            if ready.is_empty() && pending.is_empty() {
                break;
            }

            if ready.is_empty() && !pending.is_empty() {
                // Deadlock — shouldn't happen in a DAG
                return Err(TaskFlowError::CycleDetected);
            }

            let mut futures = Vec::new();
            for id in ready {
                pending.remove(&id);
                let graph = self.graph.lock();
                let task = graph.get(id).cloned();
                drop(graph);

                if let Some(task) = task {
                    let executor_ref = executor;
                    let context_ref = context.clone();
                    futures.push(async move {
                        info!(task_id = %id, task_name = %task.name, "starting task");
                        let result = executor_ref.execute(&task, &context_ref).await;
                        (id, result)
                    });
                }
            }

            let results = join_all(futures).await;
            for (id, result) in results {
                match &result {
                    TaskResult::Success(_) => {
                        info!(task_id = %id, "task completed");
                        completed.insert(id, result);
                    }
                    TaskResult::Failure(err) => {
                        error!(task_id = %id, error = %err, "task failed");
                        failed.insert(id);
                        completed.insert(id, result);
                    }
                }
                let mut graph = self.graph.lock();
                if let Some(task) = graph.get_mut(id) {
                    task.result = completed.get(&id).cloned();
                    task.status = if failed.contains(&id) {
                        TaskStatus::Failed
                    } else {
                        TaskStatus::Completed
                    };
                }
            }
        }

        Ok(completed)
    }

    /// Run a single task by ID, respecting its dependencies.
    pub async fn run_task<E: TaskExecutor>(
        &self,
        task_id: TaskId,
        executor: &E,
        context: &TaskContext,
    ) -> Result<TaskResult, TaskFlowError> {
        let graph = self.graph.lock();
        let deps = graph.dependencies(task_id);
        let task = graph.get(task_id).cloned();
        drop(graph);

        let task = task.ok_or_else(|| TaskFlowError::TaskNotFound(task_id.to_string()))?;

        // Ensure dependencies are completed first
        for dep in deps {
            let dep_result = Box::pin(self.run_task(dep, executor, context)).await?;
            if let TaskResult::Failure(_) = dep_result {
                return Err(TaskFlowError::DependencyFailed(task_id.to_string()));
            }
        }

        let result = executor.execute(&task, context).await;
        let mut graph = self.graph.lock();
        if let Some(t) = graph.get_mut(task_id) {
            t.result = Some(result.clone());
            t.status = match &result {
                TaskResult::Success(_) => TaskStatus::Completed,
                TaskResult::Failure(_) => TaskStatus::Failed,
            };
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct CountingExecutor {
        counter: AtomicUsize,
    }

    #[async_trait]
    impl TaskExecutor for CountingExecutor {
        async fn execute(&self, task: &Task, _ctx: &TaskContext) -> TaskResult {
            let count = self.counter.fetch_add(1, Ordering::SeqCst);
            TaskResult::Success(Some(serde_json::json!({
                "task": task.name,
                "count": count,
            })))
        }
    }

    #[derive(Debug)]
    struct FailingExecutor {
        fail_at: String,
    }

    #[async_trait]
    impl TaskExecutor for FailingExecutor {
        async fn execute(&self, task: &Task, _ctx: &TaskContext) -> TaskResult {
            if task.name == self.fail_at {
                TaskResult::Failure("intentional failure".into())
            } else {
                TaskResult::Success(None)
            }
        }
    }

    #[tokio::test]
    async fn test_linear_flow() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        let c = graph.add_node(Task::new("C"));
        graph.add_edge(a, b).unwrap();
        graph.add_edge(b, c).unwrap();

        let engine = ExecutionEngine::new(graph);
        let executor = CountingExecutor {
            counter: AtomicUsize::new(0),
        };
        let ctx = TaskContext::new();
        let results = engine.run(&executor, &ctx).await.unwrap();

        assert_eq!(results.len(), 3);
        assert!(matches!(results[&a], TaskResult::Success(_)));
        assert!(matches!(results[&b], TaskResult::Success(_)));
        assert!(matches!(results[&c], TaskResult::Success(_)));
    }

    #[tokio::test]
    async fn test_diamond_flow() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        let c = graph.add_node(Task::new("C"));
        let d = graph.add_node(Task::new("D"));
        graph.add_edge(a, b).unwrap();
        graph.add_edge(a, c).unwrap();
        graph.add_edge(b, d).unwrap();
        graph.add_edge(c, d).unwrap();

        let engine = ExecutionEngine::new(graph);
        let executor = CountingExecutor {
            counter: AtomicUsize::new(0),
        };
        let ctx = TaskContext::new();
        let results = engine.run(&executor, &ctx).await.unwrap();

        assert_eq!(results.len(), 4);
        assert!(matches!(results[&a], TaskResult::Success(_)));
        assert!(matches!(results[&b], TaskResult::Success(_)));
        assert!(matches!(results[&c], TaskResult::Success(_)));
        assert!(matches!(results[&d], TaskResult::Success(_)));
    }

    #[tokio::test]
    async fn test_error_propagation() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        let c = graph.add_node(Task::new("C"));
        graph.add_edge(a, b).unwrap();
        graph.add_edge(b, c).unwrap();

        let engine = ExecutionEngine::new(graph);
        let executor = FailingExecutor {
            fail_at: "B".into(),
        };
        let ctx = TaskContext::new();
        let results = engine.run(&executor, &ctx).await.unwrap();

        // A should succeed, B should fail, C should be skipped due to dep failure
        assert!(matches!(results[&a], TaskResult::Success(_)));
        assert!(matches!(results[&b], TaskResult::Failure(_)));
        // C is skipped because its dependency B failed
        let graph = engine.graph.lock();
        let task_c = graph.get(c).unwrap();
        assert_eq!(task_c.status, TaskStatus::Failed);
        assert!(matches!(task_c.result, Some(TaskResult::Failure(_))));
    }

    #[tokio::test]
    async fn test_parallel_execution() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Instant;

        #[derive(Debug)]
        struct SlowExecutor {
            delay_ms: u64,
            counter: AtomicUsize,
        }

        #[async_trait]
        impl TaskExecutor for SlowExecutor {
            async fn execute(&self, _task: &Task, _ctx: &TaskContext) -> TaskResult {
                tokio::time::sleep(tokio::time::Duration::from_millis(self.delay_ms)).await;
                self.counter.fetch_add(1, Ordering::SeqCst);
                TaskResult::Success(None)
            }
        }

        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        let c = graph.add_node(Task::new("C"));
        // A -> B and A -> C, but B and C are independent
        graph.add_edge(a, b).unwrap();
        graph.add_edge(a, c).unwrap();

        let engine = ExecutionEngine::new(graph);
        let executor = SlowExecutor {
            delay_ms: 100,
            counter: AtomicUsize::new(0),
        };
        let ctx = TaskContext::new();
        let start = Instant::now();
        let results = engine.run(&executor, &ctx).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 3);
        // B and C should run in parallel, so total time should be ~200ms, not ~300ms
        assert!(
            elapsed.as_millis() < 250,
            "parallel execution took too long: {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_run_task_single() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        graph.add_edge(a, b).unwrap();

        let engine = ExecutionEngine::new(graph);
        let executor = CountingExecutor {
            counter: AtomicUsize::new(0),
        };
        let ctx = TaskContext::new();
        let result = engine.run_task(b, &executor, &ctx).await.unwrap();
        assert!(matches!(result, TaskResult::Success(_)));
    }
}
