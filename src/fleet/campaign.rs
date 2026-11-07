use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskKey(String);

impl TaskKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TaskKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for TaskKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Skipped,
}

impl TaskStatus {
    fn satisfies_dependency(self) -> bool {
        matches!(self, Self::Completed | Self::Skipped)
    }

    fn blocks_dependency(self) -> bool {
        self == Self::Failed
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CampaignTask {
    pub id: TaskKey,
    pub title: String,
    pub status: TaskStatus,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CampaignSummary {
    pub total: usize,
    pub ready: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub blocked: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CampaignError {
    DuplicateTask(TaskKey),
    UnknownDependency(TaskKey),
    UnknownTask(TaskKey),
}

impl fmt::Display for CampaignError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTask(task) => write!(formatter, "campaign task already exists: {task}"),
            Self::UnknownDependency(task) => {
                write!(formatter, "campaign dependency does not exist: {task}")
            }
            Self::UnknownTask(task) => write!(formatter, "campaign task does not exist: {task}"),
        }
    }
}

impl std::error::Error for CampaignError {}

#[derive(Debug)]
struct TaskNode {
    task: CampaignTask,
    dependencies: BTreeSet<TaskKey>,
    dependents: BTreeSet<TaskKey>,
    unmet_dependencies: usize,
    failed_dependencies: usize,
}

/// Incremental dependency graph whose ready queue changes only on mutations.
#[derive(Debug, Default)]
pub struct CampaignGraph {
    nodes: BTreeMap<TaskKey, TaskNode>,
    ready: BTreeSet<TaskKey>,
}

impl CampaignGraph {
    pub fn add_task(
        &mut self,
        id: TaskKey,
        title: impl Into<String>,
        dependencies: impl IntoIterator<Item = TaskKey>,
    ) -> Result<(), CampaignError> {
        if self.nodes.contains_key(&id) {
            return Err(CampaignError::DuplicateTask(id));
        }
        let dependencies: BTreeSet<_> = dependencies.into_iter().collect();
        for dependency in &dependencies {
            if !self.nodes.contains_key(dependency) {
                return Err(CampaignError::UnknownDependency(dependency.clone()));
            }
        }

        let unmet_dependencies = dependencies
            .iter()
            .filter(|dependency| !self.nodes[dependency].task.status.satisfies_dependency())
            .count();
        let failed_dependencies = dependencies
            .iter()
            .filter(|dependency| self.nodes[dependency].task.status.blocks_dependency())
            .count();
        let task = CampaignTask {
            id: id.clone(),
            title: title.into(),
            status: TaskStatus::Pending,
        };
        self.nodes.insert(
            id.clone(),
            TaskNode {
                task,
                dependencies: dependencies.clone(),
                dependents: BTreeSet::new(),
                unmet_dependencies,
                failed_dependencies,
            },
        );
        for dependency in dependencies {
            self.nodes
                .get_mut(&dependency)
                .expect("dependency was validated")
                .dependents
                .insert(id.clone());
        }
        self.sync_ready(&id);
        Ok(())
    }

    pub fn set_status(&mut self, id: &TaskKey, status: TaskStatus) -> Result<(), CampaignError> {
        let Some(node) = self.nodes.get_mut(id) else {
            return Err(CampaignError::UnknownTask(id.clone()));
        };
        let previous = node.task.status;
        if previous == status {
            return Ok(());
        }
        node.task.status = status;
        let dependents = node.dependents.clone();
        self.sync_ready(id);

        for dependent in dependents {
            let node = self
                .nodes
                .get_mut(&dependent)
                .expect("dependent index must reference a task");
            match (
                previous.satisfies_dependency(),
                status.satisfies_dependency(),
            ) {
                (false, true) => node.unmet_dependencies -= 1,
                (true, false) => node.unmet_dependencies += 1,
                _ => {}
            }
            match (previous.blocks_dependency(), status.blocks_dependency()) {
                (false, true) => node.failed_dependencies += 1,
                (true, false) => node.failed_dependencies -= 1,
                _ => {}
            }
            self.sync_ready(&dependent);
        }
        Ok(())
    }

    pub fn task(&self, id: &TaskKey) -> Option<&CampaignTask> {
        self.nodes.get(id).map(|node| &node.task)
    }

    pub fn dependencies(&self, id: &TaskKey) -> Option<impl Iterator<Item = &TaskKey>> {
        Some(self.nodes.get(id)?.dependencies.iter())
    }

    pub fn next_ready(&self, limit: usize) -> impl Iterator<Item = &CampaignTask> {
        self.ready
            .iter()
            .take(limit)
            .filter_map(|id| self.nodes.get(id).map(|node| &node.task))
    }

    pub fn summary(&self) -> CampaignSummary {
        let mut summary = CampaignSummary {
            total: self.nodes.len(),
            ready: self.ready.len(),
            ..CampaignSummary::default()
        };
        for node in self.nodes.values() {
            match node.task.status {
                TaskStatus::Pending if node.failed_dependencies > 0 => summary.blocked += 1,
                TaskStatus::Pending => {}
                TaskStatus::Running => summary.running += 1,
                TaskStatus::Completed | TaskStatus::Skipped => summary.completed += 1,
                TaskStatus::Failed => summary.failed += 1,
            }
        }
        summary
    }

    fn sync_ready(&mut self, id: &TaskKey) {
        let is_ready = self.nodes.get(id).is_some_and(|node| {
            node.task.status == TaskStatus::Pending && node.unmet_dependencies == 0
        });
        if is_ready {
            self.ready.insert(id.clone());
        } else {
            self.ready.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintains_ready_and_blocked_indexes_incrementally() {
        let mut graph = CampaignGraph::default();
        let setup = TaskKey::from("setup");
        let build = TaskKey::from("build");
        let docs = TaskKey::from("docs");
        graph.add_task(setup.clone(), "Set up", []).unwrap();
        graph
            .add_task(build.clone(), "Build", [setup.clone()])
            .unwrap();
        graph
            .add_task(docs.clone(), "Document", [setup.clone()])
            .unwrap();

        let ready: Vec<_> = graph.next_ready(10).map(|task| task.id.as_str()).collect();
        assert_eq!(ready, ["setup"]);
        graph.set_status(&setup, TaskStatus::Completed).unwrap();
        let ready: Vec<_> = graph.next_ready(10).map(|task| task.id.as_str()).collect();
        assert_eq!(ready, ["build", "docs"]);
        graph.set_status(&build, TaskStatus::Failed).unwrap();
        assert_eq!(graph.summary().failed, 1);
    }
}
