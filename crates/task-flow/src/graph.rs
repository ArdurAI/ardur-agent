use crate::error::TaskFlowError;
use crate::task::{Task, TaskId};
use std::collections::{HashMap, HashSet, VecDeque};

/// A directed acyclic graph (DAG) of tasks.
#[derive(Debug, Clone, Default)]
pub struct TaskGraph {
    nodes: HashMap<TaskId, Task>,
    edges: HashMap<TaskId, Vec<TaskId>>,         // from -> to
    reverse_edges: HashMap<TaskId, Vec<TaskId>>, // to -> from
}

impl TaskGraph {
    /// Create an empty task graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a task node to the graph.
    pub fn add_node(&mut self, task: Task) -> TaskId {
        let id = task.id;
        self.nodes.insert(id, task);
        self.edges.entry(id).or_default();
        self.reverse_edges.entry(id).or_default();
        id
    }

    /// Add a dependency edge: `from` must complete before `to` can start.
    pub fn add_edge(&mut self, from: TaskId, to: TaskId) -> Result<(), TaskFlowError> {
        if !self.nodes.contains_key(&from) {
            return Err(TaskFlowError::TaskNotFound(from.to_string()));
        }
        if !self.nodes.contains_key(&to) {
            return Err(TaskFlowError::TaskNotFound(to.to_string()));
        }

        self.edges.entry(from).or_default().push(to);
        self.reverse_edges.entry(to).or_default().push(from);

        if self.has_cycle() {
            // Rollback
            self.edges.get_mut(&from).unwrap().retain(|&id| id != to);
            self.reverse_edges
                .get_mut(&to)
                .unwrap()
                .retain(|&id| id != from);
            return Err(TaskFlowError::CycleDetected);
        }

        Ok(())
    }

    /// Get a task by ID.
    pub fn get(&self, id: TaskId) -> Option<&Task> {
        self.nodes.get(&id)
    }

    /// Get a mutable task by ID.
    pub fn get_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.nodes.get_mut(&id)
    }

    /// Return all task IDs.
    pub fn task_ids(&self) -> Vec<TaskId> {
        self.nodes.keys().copied().collect()
    }

    /// Return the direct dependents of a task (tasks that depend on it).
    pub fn dependents(&self, id: TaskId) -> Vec<TaskId> {
        self.edges.get(&id).cloned().unwrap_or_default()
    }

    /// Return the direct dependencies of a task.
    pub fn dependencies(&self, id: TaskId) -> Vec<TaskId> {
        self.reverse_edges.get(&id).cloned().unwrap_or_default()
    }

    /// Perform a topological sort of the task graph.
    /// Returns IDs in an order that respects dependencies.
    pub fn topological_sort(&self) -> Result<Vec<TaskId>, TaskFlowError> {
        let mut in_degree: HashMap<TaskId, usize> = HashMap::new();
        for id in self.nodes.keys() {
            in_degree.insert(*id, 0);
        }
        // self.edges maps from -> to (dependents)
        // For each edge from -> to, to has an incoming edge, so to's in-degree increases
        for (_from, tos) in &self.edges {
            for to in tos {
                *in_degree.entry(*to).or_insert(0) += 1;
            }
        }

        let mut queue: VecDeque<TaskId> = in_degree
            .iter()
            .filter(|&(_, deg)| *deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut sorted = Vec::new();

        while let Some(id) = queue.pop_front() {
            sorted.push(id);
            for &dep in self.edges.get(&id).unwrap_or(&Vec::new()) {
                let deg = in_degree.get_mut(&dep).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(dep);
                }
            }
        }

        if sorted.len() != self.nodes.len() {
            return Err(TaskFlowError::CycleDetected);
        }

        Ok(sorted)
    }

    /// Detect whether the graph contains a cycle.
    pub fn has_cycle(&self) -> bool {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for &id in self.nodes.keys() {
            if !visited.contains(&id) {
                if self.dfs_cycle(id, &mut visited, &mut rec_stack) {
                    return true;
                }
            }
        }
        false
    }

    fn dfs_cycle(
        &self,
        id: TaskId,
        visited: &mut HashSet<TaskId>,
        rec_stack: &mut HashSet<TaskId>,
    ) -> bool {
        visited.insert(id);
        rec_stack.insert(id);

        for &dep in self.edges.get(&id).unwrap_or(&Vec::new()) {
            if !visited.contains(&dep) {
                if self.dfs_cycle(dep, visited, rec_stack) {
                    return true;
                }
            } else if rec_stack.contains(&dep) {
                return true;
            }
        }

        rec_stack.remove(&id);
        false
    }

    /// Return tasks with no dependencies (in-degree 0).
    pub fn roots(&self) -> Vec<TaskId> {
        self.nodes
            .keys()
            .copied()
            .filter(|id| {
                self.reverse_edges
                    .get(id)
                    .map(|v| v.is_empty())
                    .unwrap_or(true)
            })
            .collect()
    }

    /// Return the number of tasks in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Check if the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_node() {
        let mut graph = TaskGraph::new();
        let task = Task::new("A");
        let id = graph.add_node(task.clone());
        assert_eq!(graph.len(), 1);
        assert!(graph.get(id).is_some());
    }

    #[test]
    fn test_topological_sort_linear() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        let c = graph.add_node(Task::new("C"));
        graph.add_edge(a, b).unwrap();
        graph.add_edge(b, c).unwrap();

        let sorted = graph.topological_sort().unwrap();
        assert_eq!(sorted, vec![a, b, c]);
    }

    #[test]
    fn test_topological_sort_diamond() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        let c = graph.add_node(Task::new("C"));
        let d = graph.add_node(Task::new("D"));
        graph.add_edge(a, b).unwrap();
        graph.add_edge(a, c).unwrap();
        graph.add_edge(b, d).unwrap();
        graph.add_edge(c, d).unwrap();

        let sorted = graph.topological_sort().unwrap();
        assert_eq!(sorted[0], a);
        assert_eq!(sorted[3], d);
        // b and c can be in either order
        assert!(sorted[1] == b || sorted[1] == c);
        assert!(sorted[2] == b || sorted[2] == c);
    }

    #[test]
    fn test_cycle_detection() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        let c = graph.add_node(Task::new("C"));
        graph.add_edge(a, b).unwrap();
        graph.add_edge(b, c).unwrap();
        let res = graph.add_edge(c, a);
        assert!(matches!(res, Err(TaskFlowError::CycleDetected)));
    }

    #[test]
    fn test_self_cycle() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let res = graph.add_edge(a, a);
        assert!(matches!(res, Err(TaskFlowError::CycleDetected)));
    }

    #[test]
    fn test_roots() {
        let mut graph = TaskGraph::new();
        let a = graph.add_node(Task::new("A"));
        let b = graph.add_node(Task::new("B"));
        let c = graph.add_node(Task::new("C"));
        graph.add_edge(a, b).unwrap();
        graph.add_edge(a, c).unwrap();

        let roots = graph.roots();
        assert_eq!(roots, vec![a]);
    }
}
