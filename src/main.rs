//! GitLab Runner Orchestrator for Hetzner Cloud.
//!
//! Automatically creates Hetzner servers as GitLab runners when pipelines
//! are pending and deletes them cost-optimized after completion.
//!
//! # How it works
//!
//! 1. Polls the GitLab API for active pipelines (pending/running)
//! 2. On active pipeline: Create Hetzner server (if not present)
//! 3. On no active pipelines: Delete server (after minimum runtime)
//! 4. Deletion ideally 5min before next billing cycle
//!
//! # Configuration
//!
//! Expects `config/config.toml` with GitLab and Hetzner credentials.
//! Expects `config/runner.toml` with GitLab Runner configuration.

mod cloud_init;
mod config;
mod csv_log;
mod gitlab;
mod hetzner;
mod state;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::sleep;
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, Layer};

use crate::cloud_init::generate_cloud_init;
use crate::config::{load_runner_config, Config};
use crate::csv_log::CsvLogger;
use crate::gitlab::{GitLabClient, GitLabError};
use crate::hetzner::HetznerClient;
use crate::state::{OrchestratorState, RunnerState};

/// Buffer in minutes before billing cycle for optimal deletion.
const BILLING_BUFFER_MINUTES: u64 = 5;

/// Default path for configuration.
const CONFIG_PATH: &str = "config/config.toml";

/// Default path for runner configuration.
const RUNNER_CONFIG_PATH: &str = "config/runner.toml";

/// Default path for persisted state.
const STATE_PATH: &str = "config/state.json";

/// Default path for logs.
const LOG_DIR: &str = "logs";

/// Default path for example configuration.
const CONFIG_EXAMPLE_PATH: &str = "config/config.example.toml";

/// Content of the example configuration.
const CONFIG_EXAMPLE_CONTENT: &str = r#"# GitLab Runner Orchestrator - Example Configuration
# Copy this file to config.toml and customize the values.

[gitlab]
# URL of your GitLab instance
url = "https://gitlab.example.com"
# Personal Access Token with API access (read_api scope is sufficient)
token = "glpat-xxxxxxxxxxxxxxxxxxxx"
# Optional: only spin up a runner when a pending job has one of these tags.
# Remove or leave empty to react to all pending jobs.
# tag_filter = ["hetzner", "my-runner-tag"]
# How often the project list is refreshed, in minutes (cached in between)
# project_refresh_minutes = 10

[hetzner]
# Hetzner Cloud API Token
token = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
# Server type (e.g., cx22, ccx23, cpx31)
server_type = "ccx23"
# Datacenter location (nbg1, fsn1, hel1, ash, hil)
location = "nbg1"
# OS Image
image = "ubuntu-24.04"
# Name of the SSH key in Hetzner Cloud
ssh_key_name = "my-ssh-key"

[runner]
# Name of the server in Hetzner Cloud
name = "flexi-runner"
# Minimum runtime in minutes before the server can be deleted
min_lifetime_minutes = 20
# Polling interval in seconds (10-300). Each poll makes about one GitLab API
# request per project, so keep this high enough for GitLab's rate limits.
poll_interval_seconds = 30
"#;

/// Returns true if compiled in debug mode.
/// In debug mode, the server is immediately deleted when no pipelines are active.
const fn is_debug_build() -> bool {
    cfg!(debug_assertions)
}

/// Creates the example configuration if it doesn't exist.
fn ensure_example_config() {
    if !Path::new(CONFIG_EXAMPLE_PATH).exists() {
        // Create config directory if needed
        std::fs::create_dir_all("config").ok();

        if let Err(e) = std::fs::write(CONFIG_EXAMPLE_PATH, CONFIG_EXAMPLE_CONTENT) {
            eprintln!("Warning: Could not create example configuration: {}", e);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Create example configuration if not present
    ensure_example_config();

    // Create log directory if not present
    std::fs::create_dir_all(LOG_DIR).ok();

    // File appender for rotating logs (daily)
    let file_appender = RollingFileAppender::new(Rotation::DAILY, LOG_DIR, "orchestrator.log");

    // Initialize tracing with console + file output
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        // Console layer - compact format
        .with(
            fmt::layer()
                .with_target(false)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        )
        // File layer - detailed format with timestamps
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false) // No ANSI colors in file
                .with_target(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG),
        )
        .init();

    info!("=== GitLab Runner Orchestrator ===");
    info!("Logs are written to: {}/orchestrator.log", LOG_DIR);
    if is_debug_build() {
        warn!("DEBUG BUILD: Server will be deleted immediately when no pipelines are active!");
    }
    info!("Starting...");

    // Check if config directory exists
    if !Path::new("config").exists() {
        error!("Config directory 'config/' does not exist!");
        error!("Please create config/config.toml and config/runner.toml");
        std::process::exit(1);
    }

    // Load configuration
    let config = Config::load(CONFIG_PATH).context("Error loading configuration")?;

    // Load runner configuration
    let runner_config =
        load_runner_config(RUNNER_CONFIG_PATH).context("Error loading runner configuration")?;

    // Initialize CSV logger
    let csv_logger = CsvLogger::new(LOG_DIR).context("Error initializing CSV logger")?;

    // Create API clients
    let gitlab_client = GitLabClient::new(&config.gitlab);
    let hetzner_client = HetznerClient::new(&config.hetzner);

    // Generate cloud-init template
    let cloud_init = generate_cloud_init(&runner_config);

    // Load orchestrator state with persistence
    let mut state =
        OrchestratorState::with_persistence(STATE_PATH).context("Error loading state")?;

    // Verify that saved state matches Hetzner
    verify_state_with_hetzner(&hetzner_client, &config.runner.name, &mut state).await?;

    // Polling interval (Debug: 15s, Release: from config)
    let poll_interval_secs = if is_debug_build() {
        15
    } else {
        config.runner.poll_interval_seconds
    };
    let poll_interval = Duration::from_secs(poll_interval_secs);

    info!("Starting polling loop (interval: {}s)", poll_interval_secs);
    info!(
        "Minimum server runtime: {} minutes",
        config.runner.min_lifetime_minutes
    );

    // Main loop
    loop {
        let mut wait = poll_interval;

        if let Err(e) = orchestration_tick(
            &gitlab_client,
            &hetzner_client,
            &csv_logger,
            &cloud_init,
            &config,
            &mut state,
        )
        .await
        {
            // Back off when GitLab rate-limits us
            if let Some(GitLabError::RateLimited { retry_after }) = e.downcast_ref() {
                wait = wait.max(*retry_after);
                warn!("Rate limited by GitLab - pausing for {}s", wait.as_secs());
            } else {
                error!("Error in orchestration tick: {}", e);
            }
            // Continue on errors
        }

        sleep(wait).await;
    }
}

/// Verifies that the saved state matches Hetzner.
///
/// Possible scenarios:
/// - State says server exists, Hetzner too → OK, keep state
/// - State says server exists, Hetzner doesn't → Clear state
/// - State says no server, Hetzner has one → Update state (emergency)
/// - Both say no server → OK
async fn verify_state_with_hetzner(
    hetzner_client: &HetznerClient,
    server_name: &str,
    state: &mut OrchestratorState,
) -> Result<()> {
    info!("Verifying state with Hetzner API...");

    let hetzner_server = hetzner_client.find_server_by_name(server_name).await?;

    match (&state.runner, hetzner_server) {
        // State and Hetzner match - server exists
        (Some(runner), Some(server)) if runner.server_id == server.id => {
            info!(
                "State verified: Server {} (ID: {}) exists, running for {} minutes",
                server.name,
                server.id,
                runner.uptime_minutes()
            );
        }

        // State says server exists, but Hetzner doesn't know it anymore
        (Some(runner), None) => {
            warn!(
                "State inconsistency: Server {} (ID: {}) no longer exists at Hetzner!",
                runner.server_name, runner.server_id
            );
            warn!("Clearing state...");
            state.clear_runner();
        }

        // State says server exists, but with different ID (very unlikely)
        (Some(runner), Some(server)) => {
            warn!(
                "State inconsistency: State knows server ID {}, Hetzner has ID {}!",
                runner.server_id, server.id
            );
            warn!("Updating state with Hetzner data (creation time unknown)...");
            let new_runner = RunnerState::new(server.id, server.name);
            state.set_runner(new_runner);
        }

        // State says no server, but Hetzner has one (emergency recovery)
        (None, Some(server)) => {
            warn!(
                "Orphaned server found: {} (ID: {}) - not in state!",
                server.name, server.id
            );
            warn!("Adding to state (creation time unknown)...");
            let new_runner = RunnerState::new(server.id, server.name);
            state.set_runner(new_runner);
        }

        // All OK - no server
        (None, None) => {
            info!("No server active - state is consistent");
        }
    }

    Ok(())
}

/// Why an idle runner is deleted in release builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteReason {
    /// Minimum runtime reached and close to the next billing cycle
    OptimalBillingTime,
    /// Minimum runtime reached and just past a full hour,
    /// otherwise we'd wait almost a full hour
    AllPipelinesDone,
}

impl DeleteReason {
    /// Reason string used in logs and the CSV log.
    fn as_str(self) -> &'static str {
        match self {
            DeleteReason::OptimalBillingTime => "optimal_billing_time",
            DeleteReason::AllPipelinesDone => "all_pipelines_done",
        }
    }
}

/// Returns the reason for deleting the runner if it is currently in a
/// deletion window, i.e. a moment where an idle runner would be deleted.
fn deletion_reason(runner: &RunnerState, min_lifetime_minutes: u32) -> Option<DeleteReason> {
    if runner.should_delete(min_lifetime_minutes, BILLING_BUFFER_MINUTES) {
        Some(DeleteReason::OptimalBillingTime)
    } else if runner.can_force_delete(min_lifetime_minutes)
        && runner.minutes_until_next_billing_cycle() > 55
    {
        Some(DeleteReason::AllPipelinesDone)
    } else {
        None
    }
}

/// One pass of the orchestration logic.
async fn orchestration_tick(
    gitlab_client: &GitLabClient,
    hetzner_client: &HetznerClient,
    csv_logger: &CsvLogger,
    cloud_init: &str,
    config: &Config,
    state: &mut OrchestratorState,
) -> Result<()> {
    // While a runner exists, the GitLab result only matters if the runner could
    // be deleted right now. Outside a deletion window, skip the API calls.
    // (Debug builds delete immediately, so they always need to poll.)
    if let Some(runner) = &state.runner {
        if !is_debug_build()
            && deletion_reason(runner, config.runner.min_lifetime_minutes).is_none()
        {
            info!(
                "Server running ({}min), {} minutes until billing - skipping GitLab poll",
                runner.uptime_minutes(),
                runner.minutes_until_next_billing_cycle()
            );
            return Ok(());
        }
    }

    // Query GitLab for an active job (filtered by tag if configured)
    let active_job = gitlab_client
        .find_first_active_job(config.gitlab.tag_filter.as_deref())
        .await?;

    match active_job {
        Some(job) if !state.has_runner() => {
            // Active job, no server present - create one
            info!(
                "Active job found: {} (ID: {}) in {}",
                job.job.status, job.job.id, job.project.path_with_namespace
            );

            create_runner(
                hetzner_client,
                csv_logger,
                cloud_init,
                config,
                state,
                &job.project.path_with_namespace,
                job.job.id,
            )
            .await?;
        }
        Some(_) => {
            // Server is already running
            if let Some(uptime) = state.runner_uptime() {
                info!("Server running ({}min), active jobs present", uptime);
            }
        }
        None if state.has_runner() => {
            // Server is running, but no jobs anymore - check if we should delete
            maybe_delete_runner(hetzner_client, csv_logger, config, state).await?;
        }
        None => {
            info!("No active jobs, no server active - waiting...");
        }
    }

    Ok(())
}

/// Creates a new runner server.
async fn create_runner(
    hetzner_client: &HetznerClient,
    csv_logger: &CsvLogger,
    cloud_init: &str,
    config: &Config,
    state: &mut OrchestratorState,
    project: &str,
    pipeline_id: u64,
) -> Result<()> {
    info!("Creating new runner server...");

    let server = hetzner_client
        .create_server(&config.runner.name, cloud_init)
        .await
        .context("Error creating server")?;

    // Update state
    let runner_state = RunnerState::new(server.id, server.name.clone());
    state.set_runner(runner_state);

    // CSV log
    if let Err(e) = csv_logger.log_start(server.id, project, pipeline_id, "pipeline_pending") {
        warn!("Error in CSV logging: {}", e);
    }

    info!("Runner server created and ready");
    Ok(())
}

/// Checks if the server should be deleted and performs deletion if so.
///
/// In debug build, the server is deleted immediately.
/// In release build, it waits for optimal billing time. The deletion window is
/// re-checked here (after the GitLab scan) so a slow scan can't push the
/// deletion past the billing boundary.
async fn maybe_delete_runner(
    hetzner_client: &HetznerClient,
    csv_logger: &CsvLogger,
    config: &Config,
    state: &mut OrchestratorState,
) -> Result<()> {
    let runner = match &state.runner {
        Some(r) => r,
        None => return Ok(()),
    };

    let uptime = runner.uptime_minutes();

    // DEBUG BUILD: Delete immediately without waiting for billing
    if is_debug_build() {
        info!(
            "[DEBUG] Server running for {}min - deleting immediately (no pipelines active)",
            uptime
        );
        delete_runner(hetzner_client, csv_logger, state, "debug_immediate_delete").await?;
        return Ok(());
    }

    // RELEASE BUILD: Only delete within a deletion window
    let minutes_to_billing = runner.minutes_until_next_billing_cycle();
    match deletion_reason(runner, config.runner.min_lifetime_minutes) {
        Some(reason) => {
            if reason == DeleteReason::AllPipelinesDone {
                info!(
                    "Force-delete: Minimum runtime reached ({}min), {} minutes until billing",
                    uptime, minutes_to_billing
                );
            }
            delete_runner(hetzner_client, csv_logger, state, reason.as_str()).await?;
        }
        None => {
            // Window closed during the GitLab scan - wait for the next one
            info!(
                "Server running for {}min, no pipelines active. {} minutes until billing - waiting...",
                uptime, minutes_to_billing
            );
        }
    }

    Ok(())
}

/// Deletes the runner server.
async fn delete_runner(
    hetzner_client: &HetznerClient,
    csv_logger: &CsvLogger,
    state: &mut OrchestratorState,
    reason: &str,
) -> Result<()> {
    let runner = match &state.runner {
        Some(r) => r,
        None => return Ok(()),
    };

    let server_id = runner.server_id;
    let uptime = runner.uptime_minutes();

    info!("Deleting runner server (reason: {})", reason);

    // Delete server
    hetzner_client
        .delete_server(server_id)
        .await
        .context("Error deleting server")?;

    // CSV log
    if let Err(e) = csv_logger.log_stop(server_id, reason, uptime) {
        warn!("Error in CSV logging: {}", e);
    }

    // Reset state
    state.clear_runner();

    info!("Runner server deleted (runtime: {} minutes)", uptime);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    /// Runner that has been up for the given minutes (plus 30s, to avoid boundary flakiness).
    fn runner_with_uptime(minutes: i64) -> RunnerState {
        RunnerState {
            server_id: 1,
            server_name: "test-runner".to_string(),
            created_at: Utc::now()
                - chrono::Duration::minutes(minutes)
                - chrono::Duration::seconds(30),
        }
    }

    #[test]
    fn test_deletion_reason() {
        // Below minimum lifetime
        assert_eq!(deletion_reason(&runner_with_uptime(10), 20), None);
        // Mid billing hour - wait
        assert_eq!(deletion_reason(&runner_with_uptime(30), 20), None);
        // Within buffer before next billing cycle
        assert_eq!(
            deletion_reason(&runner_with_uptime(56), 20),
            Some(DeleteReason::OptimalBillingTime)
        );
        // Just past a full hour
        assert_eq!(
            deletion_reason(&runner_with_uptime(62), 20),
            Some(DeleteReason::AllPipelinesDone)
        );
    }
}
