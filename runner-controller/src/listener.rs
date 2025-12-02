use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::container::ContainerManager;
use crate::github::GitHubClient;
use crate::state::{ContainerState, StateDb};

pub struct JobListener {
    config: Config,
    github: GitHubClient,
    containers: Arc<ContainerManager>,
    state_db: Arc<StateDb>,
    shutdown_rx: watch::Receiver<bool>,
}

impl JobListener {
    pub fn new(
        config: Config,
        github: GitHubClient,
        containers: Arc<ContainerManager>,
        state_db: Arc<StateDb>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            config,
            github,
            containers,
            state_db,
            shutdown_rx,
        }
    }

    /// Check if job labels are a subset of runner labels
    fn labels_match(job_labels: &[String], runner_labels: &[String]) -> bool {
        let runner_set: HashSet<&str> = runner_labels.iter().map(|s| s.as_str()).collect();

        for label in job_labels {
            if !runner_set.contains(label.as_str()) {
                return false;
            }
        }

        true
    }

    /// Reconcile state on startup - clean up stale containers
    pub async fn reconcile_on_startup(&self) -> Result<()> {
        info!("Reconciling state on startup");

        // Get containers from nixos-container
        let active_containers = self.containers.list().await?;

        // Check each container
        for name in &active_containers {
            match self.containers.is_runner_completed(name).await {
                Ok(true) => {
                    info!(name = %name, "Cleaning up completed container from previous run");
                    self.cleanup_container_full(name).await?;
                }
                Ok(false) => {
                    info!(name = %name, "Container still has active runner");
                }
                Err(e) => {
                    warn!(name = %name, error = %e, "Failed to check container, cleaning up");
                    self.cleanup_container_full(name).await?;
                }
            }
        }

        // Clean up stale state entries (containers in DB but not in nixos-container list)
        let db_containers = self.state_db.list_containers()?;
        let active_set: HashSet<&str> = active_containers.iter().map(|s| s.as_str()).collect();

        for (name, _) in db_containers {
            if !active_set.contains(name.as_str()) {
                info!(name = %name, "Removing stale state entry");
                self.state_db.remove_container(&name)?;
            }
        }

        Ok(())
    }

    /// Check active containers for completion or timeout
    async fn check_containers(&self) -> Result<()> {
        let containers = self.containers.list().await?;
        let active_set: HashSet<&str> = containers.iter().map(|s| s.as_str()).collect();

        for name in &containers {
            // Check if runner completed
            if self.containers.is_runner_completed(name).await? {
                info!(name = %name, "Container runner completed");
                self.cleanup_container_full(name).await?;
                continue;
            }

            // Check for timeout
            if let Some(state) = self.state_db.get_container(name)? {
                let running_secs = state.running_seconds();
                let timeout_secs = self.config.job_timeout.as_secs();

                if running_secs > timeout_secs {
                    warn!(
                        name = %name,
                        running_secs,
                        timeout_secs,
                        "Container exceeded timeout, force killing"
                    );
                    self.cleanup_container_full(name).await?;
                }
            } else {
                // Container exists but no state - orphaned
                warn!(name = %name, "Orphaned container (no state), cleaning up");
                self.cleanup_container_full(name).await?;
            }
        }

        // Clean up stale state entries (in DB but container no longer exists)
        let db_containers = self.state_db.list_containers()?;
        for (name, _) in db_containers {
            if !active_set.contains(name.as_str()) {
                info!(name = %name, "Removing stale state entry (container no longer exists)");
                self.state_db.remove_container(&name)?;
            }
        }

        Ok(())
    }

    /// Full cleanup: deregister from GitHub, destroy container, remove state
    async fn cleanup_container_full(&self, name: &str) -> Result<()> {
        // Deregister from GitHub
        if let Err(e) = self.github.delete_runner_by_name(name).await {
            warn!(name = %name, error = %e, "Failed to deregister runner from GitHub");
        }

        // Destroy container
        self.containers.cleanup_container(name).await?;

        // Remove from state DB
        self.state_db.remove_container(name)?;

        Ok(())
    }

    /// Process queued jobs and spawn containers
    async fn process_queued_jobs(&self) -> Result<()> {
        // Check concurrency limit
        let active_count = self.containers.count_active().await?;
        if active_count >= self.config.max_concurrent_jobs {
            debug!(
                active = active_count,
                max = self.config.max_concurrent_jobs,
                "At max concurrency, skipping job check"
            );
            return Ok(());
        }

        // Collect jobs from runs with various statuses
        let mut all_jobs = Vec::new();

        for status in ["queued", "waiting", "pending", "in_progress"] {
            match self.github.list_workflow_runs(status).await {
                Ok(runs) => {
                    for run in runs {
                        match self.github.list_jobs_for_run(run.id).await {
                            Ok(jobs) => all_jobs.extend(jobs),
                            Err(e) => {
                                debug!(run_id = run.id, error = %e, "Failed to list jobs for run");
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!(status, error = %e, "Failed to list workflow runs");
                }
            }
        }

        // Get existing container names to avoid duplicates
        let existing_containers: HashSet<String> =
            self.containers.list().await?.into_iter().collect();

        // Process each job
        for job in all_jobs {
            // Skip if already at max concurrency
            let current_count = self.containers.count_active().await?;
            if current_count >= self.config.max_concurrent_jobs {
                debug!("At max concurrency, stopping job processing");
                break;
            }

            // Skip if job already has a runner assigned
            if job.has_runner() {
                continue;
            }

            // Skip if job is not waiting
            if !job.is_waiting() {
                continue;
            }

            // Check if labels match
            if !Self::labels_match(&job.labels, &self.config.runner_labels) {
                debug!(
                    job_id = job.id,
                    labels = ?job.labels,
                    "Job labels don't match runner labels"
                );
                continue;
            }

            // Check if we already have a container for this job
            let container_name = ContainerManager::job_id_to_container_name(job.id);
            if existing_containers.contains(&container_name) {
                debug!(job_id = job.id, name = %container_name, "Container already exists");
                continue;
            }

            // Also check state DB
            if self.state_db.get_container(&container_name)?.is_some() {
                debug!(job_id = job.id, name = %container_name, "Container state already exists");
                continue;
            }

            // Spawn container for this job
            info!(
                job_id = job.id,
                labels = ?job.labels,
                "Spawning container for job"
            );

            match self.spawn_container_for_job(job.id).await {
                Ok(name) => {
                    info!(job_id = job.id, name = %name, "Container spawned successfully");
                }
                Err(e) => {
                    warn!(job_id = job.id, error = %e, "Failed to spawn container");
                }
            }
        }

        Ok(())
    }

    /// Spawn a container for a specific job
    async fn spawn_container_for_job(&self, job_id: u64) -> Result<String> {
        // Get registration token
        let token = self.github.get_registration_token().await?;

        // Spawn container
        let name = self.containers.spawn_container(job_id, &token).await?;

        // Record in state DB
        let state = ContainerState::new(job_id);
        self.state_db.put_container(&name, &state)?;

        Ok(name)
    }

    /// Main run loop
    pub async fn run(&mut self) -> Result<()> {
        info!(
            poll_interval = ?self.config.poll_interval,
            max_concurrent = self.config.max_concurrent_jobs,
            "Job listener starting"
        );

        // Reconcile on startup
        self.reconcile_on_startup().await?;

        loop {
            // Check for shutdown signal
            if *self.shutdown_rx.borrow() {
                info!("Shutdown signal received");
                break;
            }

            // Check existing containers
            if let Err(e) = self.check_containers().await {
                warn!(error = %e, "Error checking containers");
            }

            // Process queued jobs
            if let Err(e) = self.process_queued_jobs().await {
                warn!(error = %e, "Error processing queued jobs");
            }

            // Wait for next poll or shutdown
            tokio::select! {
                _ = tokio::time::sleep(self.config.poll_interval) => {}
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        info!("Shutdown signal received during sleep");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Graceful shutdown - kill all containers
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down, cleaning up all containers");

        let containers = self.containers.list().await?;
        info!(count = containers.len(), "Containers to clean up");

        for name in containers {
            info!(name = %name, "Cleaning up container on shutdown");
            if let Err(e) = self.cleanup_container_full(&name).await {
                warn!(name = %name, error = %e, "Failed to cleanup container on shutdown");
            }
        }

        // Clear all state
        self.state_db.clear_all()?;

        info!("Shutdown complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_labels_match_exact() {
        let job_labels = vec!["self-hosted".to_string(), "linux".to_string()];
        let runner_labels = vec!["self-hosted".to_string(), "linux".to_string()];
        assert!(JobListener::labels_match(&job_labels, &runner_labels));
    }

    #[test]
    fn test_labels_match_subset() {
        let job_labels = vec!["self-hosted".to_string()];
        let runner_labels = vec![
            "self-hosted".to_string(),
            "linux".to_string(),
            "x64".to_string(),
        ];
        assert!(JobListener::labels_match(&job_labels, &runner_labels));
    }

    #[test]
    fn test_labels_no_match() {
        let job_labels = vec!["self-hosted".to_string(), "windows".to_string()];
        let runner_labels = vec!["self-hosted".to_string(), "linux".to_string()];
        assert!(!JobListener::labels_match(&job_labels, &runner_labels));
    }

    #[test]
    fn test_labels_empty_job() {
        let job_labels: Vec<String> = vec![];
        let runner_labels = vec!["self-hosted".to_string()];
        assert!(JobListener::labels_match(&job_labels, &runner_labels));
    }
}
