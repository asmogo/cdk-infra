use serde::Deserialize;

/// Response from /repos/{owner}/{repo}/actions/runs
#[derive(Debug, Deserialize)]
pub struct WorkflowRunsResponse {
    pub workflow_runs: Vec<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
}

/// Response from /repos/{owner}/{repo}/actions/runs/{run_id}/jobs
#[derive(Debug, Deserialize)]
pub struct JobsResponse {
    pub jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
pub struct Job {
    pub id: u64,
    pub status: String,
    pub labels: Vec<String>,
    pub runner_id: Option<u64>,
}

impl Job {
    /// Check if this job is waiting for a runner
    pub fn is_waiting(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "waiting" | "pending")
    }

    /// Check if this job has a runner assigned
    pub fn has_runner(&self) -> bool {
        self.runner_id.is_some() && self.runner_id != Some(0)
    }
}

/// Response from /repos/{owner}/{repo}/actions/runners
#[derive(Debug, Deserialize)]
pub struct RunnersResponse {
    pub runners: Vec<Runner>,
}

#[derive(Debug, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
}

/// Response from /repos/{owner}/{repo}/actions/runners/registration-token
#[derive(Debug, Deserialize)]
pub struct RegistrationTokenResponse {
    pub token: String,
}
