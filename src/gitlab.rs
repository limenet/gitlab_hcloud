//! GitLab API client for pipeline status queries.
//!
//! Uses raw HTTP requests to query all projects and their pipeline status.
//!
//! API usage is kept low to stay within GitLab rate limits: the project list is
//! cached, each project needs a single jobs request, and scanning stops at the
//! first matching job. A 429 response aborts the scan and is surfaced as
//! [`GitLabError::RateLimited`] so the caller can back off.

use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::config::GitLabConfig;

/// Errors that can occur during GitLab API calls.
#[derive(Error, Debug)]
pub enum GitLabError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("GitLab API error (status {status}): {message}")]
    Api { status: u16, message: String },

    #[error("Invalid API response: {0}")]
    Parse(String),

    #[error("GitLab rate limit reached, retry after {}s", .retry_after.as_secs())]
    RateLimited { retry_after: Duration },
}

/// Wait time used when a 429 response carries no usable `Retry-After` header.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Page size for the jobs API.
const JOBS_PER_PAGE: usize = 100;

/// Timeout for a single GitLab API request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A GitLab project (simplified representation).
///
/// NOTE: Additional fields like `name`, `description` etc. were removed -
/// not needed in this context, only `id` and `path_with_namespace` are relevant.
#[derive(Debug, Deserialize, Clone)]
pub struct Project {
    /// Project ID
    pub id: u64,
    /// Full path (e.g., "group/project")
    pub path_with_namespace: String,
}

/// A GitLab job.
///
/// NOTE: Only `id`, `status`, and `tag_list` are relevant here.
#[derive(Debug, Deserialize, Clone)]
pub struct Job {
    /// Job ID
    pub id: u64,
    /// Status (pending, running, success, failed, etc.)
    pub status: String,
    /// Runner tags assigned to this job
    #[serde(default)]
    pub tag_list: Vec<String>,
}

/// Information about an active job with project context.
#[derive(Debug, Clone)]
pub struct ActiveJob {
    /// The project the job belongs to
    pub project: Project,
    /// The job itself
    pub job: Job,
}

/// GitLab API client.
pub struct GitLabClient {
    /// HTTP client
    client: Client,
    /// Base URL of the GitLab instance
    base_url: String,
    /// API token
    token: String,
    /// How long the project list is cached before it is fetched again
    project_refresh: Duration,
    /// Cached project list with the time it was fetched
    project_cache: Mutex<Option<(Instant, Vec<Project>)>>,
}

impl GitLabClient {
    /// Creates a new GitLab client.
    ///
    /// # Arguments
    /// * `config` - GitLab configuration with URL and token
    pub fn new(config: &GitLabConfig) -> Self {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("gitlab_hcloud/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("Failed to build HTTP client");

        // Remove trailing slash if present
        let base_url = config.url.trim_end_matches('/').to_string();

        info!("GitLab client initialized for: {}", base_url);

        Self {
            client,
            base_url,
            token: config.token.clone(),
            project_refresh: Duration::from_secs(config.project_refresh_minutes.saturating_mul(60)),
            project_cache: Mutex::new(None),
        }
    }

    /// Executes an authenticated GET request.
    async fn get<T: for<'de> Deserialize<'de>>(&self, endpoint: &str) -> Result<T, GitLabError> {
        let url = format!("{}/api/v4{}", self.base_url, endpoint);
        debug!("GitLab API GET: {}", url);

        let response = self
            .client
            .get(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .send()
            .await?;

        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = parse_retry_after(
                response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|v| v.to_str().ok()),
            );
            warn!(
                "GitLab rate limit reached ({}), retry after {}s",
                endpoint,
                retry_after.as_secs()
            );
            return Err(GitLabError::RateLimited { retry_after });
        }
        if !status.is_success() {
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(GitLabError::Api {
                status: status.as_u16(),
                message,
            });
        }

        log_rate_limit(response.headers());

        response
            .json::<T>()
            .await
            .map_err(|e| GitLabError::Parse(format!("JSON parsing failed: {}", e)))
    }

    /// Fetches all projects from the GitLab instance (with pagination).
    ///
    /// Uses `membership=true` to only fetch projects the token has access to.
    /// Archived projects are skipped since they cannot run jobs. The default
    /// ordering (creation date) is kept: it is stable across pages, unlike
    /// activity-based ordering where projects can shift between pages mid-fetch.
    async fn get_all_projects(&self) -> Result<Vec<Project>, GitLabError> {
        let mut all_projects = Vec::new();
        let mut page = 1;
        let per_page = 100;

        loop {
            let endpoint = format!(
                "/projects?membership=true&simple=true&archived=false&per_page={}&page={}",
                per_page, page
            );

            let projects: Vec<Project> = self.get(&endpoint).await?;
            let count = projects.len();

            debug!("Page {}: {} projects loaded", page, count);
            all_projects.extend(projects);

            // If fewer than per_page are returned, we're done
            if count < per_page {
                break;
            }
            page += 1;
        }

        info!("Total {} projects loaded", all_projects.len());
        Ok(all_projects)
    }

    /// Returns the project list, refreshing the cache when it is stale.
    async fn projects(&self) -> Result<Vec<Project>, GitLabError> {
        let mut cache = self.project_cache.lock().await;

        if let Some((fetched_at, projects)) = cache.as_ref() {
            if !is_stale(*fetched_at, Instant::now(), self.project_refresh) {
                debug!("Using cached project list ({} projects)", projects.len());
                return Ok(projects.clone());
            }
        }

        let projects = self.get_all_projects().await?;
        *cache = Some((Instant::now(), projects.clone()));
        Ok(projects)
    }

    /// Fetches one page of the pending and running jobs of a project.
    ///
    /// Uses the Jobs API which includes `tag_list` per job, unlike the Pipelines API.
    /// Requires only `read_api` scope.
    async fn get_active_jobs(&self, project_id: u64, page: u32) -> Result<Vec<Job>, GitLabError> {
        let endpoint = format!(
            "/projects/{}/jobs?scope[]=pending&scope[]=running&per_page={}&page={}",
            project_id, JOBS_PER_PAGE, page
        );

        self.get(&endpoint).await
    }

    /// Returns the first active job of a project that matches the tag filter.
    ///
    /// Usually a single request; further pages are only fetched when a page
    /// is full and contained no match.
    async fn find_matching_job(
        &self,
        project: &Project,
        tag_filter: Option<&[String]>,
    ) -> Result<Option<Job>, GitLabError> {
        let mut page = 1;

        loop {
            let jobs = self.get_active_jobs(project.id, page).await?;
            let count = jobs.len();

            for job in jobs {
                if let Some(tags) = tag_filter {
                    if !job.tag_list.iter().any(|t| tags.contains(t)) {
                        debug!(
                            "Skipping {} job {} in {} (tags: {:?} don't match filter)",
                            job.status, job.id, project.path_with_namespace, job.tag_list
                        );
                        continue;
                    }
                }
                debug!(
                    "{} job found: {} in {} (tags: {:?})",
                    job.status, job.id, project.path_with_namespace, job.tag_list
                );
                return Ok(Some(job));
            }

            // If fewer than JOBS_PER_PAGE are returned, there are no more jobs
            if count < JOBS_PER_PAGE {
                return Ok(None);
            }
            page += 1;
        }
    }

    /// Searches all projects for an active job (pending or running) and returns
    /// the first match.
    ///
    /// When `tag_filter` is non-empty, only jobs whose `tag_list` contains at least
    /// one of the specified tags are considered. This is the mechanism for restricting
    /// runner creation to specific job types without requiring additional API scopes.
    ///
    /// Scanning stops at the first match to save API requests. A rate-limit
    /// response aborts the scan immediately; other per-project errors are logged
    /// and the scan continues.
    ///
    /// # Returns
    /// * `Ok(Some(ActiveJob))` - The first matching active job
    /// * `Ok(None)` - No matching active job found
    pub async fn find_first_active_job(
        &self,
        tag_filter: Option<&[String]>,
    ) -> Result<Option<ActiveJob>, GitLabError> {
        let projects = self.projects().await?;

        if let Some(tags) = tag_filter {
            debug!("Tag filter active: {:?}", tags);
        }

        for project in projects {
            match self.find_matching_job(&project, tag_filter).await {
                Ok(Some(job)) => return Ok(Some(ActiveJob { project, job })),
                Ok(None) => {}
                Err(e @ GitLabError::RateLimited { .. }) => return Err(e),
                Err(e) => {
                    warn!(
                        "Error fetching active jobs for {}: {}",
                        project.path_with_namespace, e
                    );
                }
            }
        }

        info!("No active jobs found");
        Ok(None)
    }
}

/// Parses the `Retry-After` header value (in seconds), falling back to a default.
fn parse_retry_after(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RETRY_AFTER)
}

/// Logs the remaining rate-limit budget reported by GitLab.
///
/// Warns when less than 10% of the budget is left.
fn log_rate_limit(headers: &HeaderMap) {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
    };

    if let (Some(remaining), Some(limit)) =
        (header("RateLimit-Remaining"), header("RateLimit-Limit"))
    {
        if remaining * 10 < limit {
            warn!(
                "GitLab rate limit almost exhausted: {}/{} remaining",
                remaining, limit
            );
        } else {
            debug!("GitLab rate limit: {}/{} remaining", remaining, limit);
        }
    }
}

/// Returns true if a cache entry fetched at `fetched_at` is older than `ttl`.
fn is_stale(fetched_at: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(fetched_at) >= ttl
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_retry_after() {
        assert_eq!(parse_retry_after(Some("5")), Duration::from_secs(5));
        assert_eq!(parse_retry_after(Some(" 120 ")), Duration::from_secs(120));
        assert_eq!(parse_retry_after(Some("not-a-number")), DEFAULT_RETRY_AFTER);
        assert_eq!(parse_retry_after(None), DEFAULT_RETRY_AFTER);
    }

    #[test]
    fn test_is_stale() {
        let ttl = Duration::from_secs(600);
        let fetched_at = Instant::now();

        assert!(!is_stale(fetched_at, fetched_at, ttl));
        assert!(!is_stale(
            fetched_at,
            fetched_at + Duration::from_secs(599),
            ttl
        ));
        assert!(is_stale(fetched_at, fetched_at + ttl, ttl));
        assert!(is_stale(
            fetched_at,
            fetched_at + Duration::from_secs(3600),
            ttl
        ));
    }
}
