# GitLab Runner Orchestrator for Hetzner Cloud

Automatic provisioning of Hetzner Cloud servers as GitLab CI runners - **pay only when you need it**.

## Features

- **Automatic server creation** when pipelines are pending/running
- **Cost-optimized deletion** - server is deleted 5 minutes before the next billing cycle
- **Polls all projects** - one runner for the entire GitLab instance
- **State persistence** - survives restarts without data loss
- **CSV logging** - documents all server starts/stops with reason and duration
- **Rotating log files** - daily rotation

## Architecture

```mermaid
flowchart TB
    subgraph main [Main Loop]
        Poll[Polling Loop]
        Poll --> FetchProjects[GitLab: Fetch all projects]
        FetchProjects --> CheckPipelines[Per project: Pending/Running pipelines?]
        CheckPipelines -->|Yes + No server| LogStart[CSV: Log reason]
        LogStart --> CreateServer[Hetzner: Create server]
        CheckPipelines -->|No everywhere + Server exists| CheckDelete[Check delete logic]
        CheckDelete -->|Conditions met| LogStop[CSV: Log usage]
        LogStop --> DeleteServer[Hetzner: Delete server]
    end

    subgraph config [Config]
        ConfigToml[config/config.toml]
        RunnerToml[config/runner.toml]
    end

    subgraph logs [Logging]
        CSV[logs/runner_usage.csv]
        LogFile[logs/orchestrator.log]
    end

    ConfigToml --> main
    RunnerToml --> CreateServer
    main --> CSV
    main --> LogFile
```

## Quick Start

### 1. Build the binary

```bash
cargo build --release
```

### 2. Create configuration

On first start, `config/config.example.toml` is automatically created. Copy and customize it:

```bash
cp config/config.example.toml config/config.toml
# Edit config/config.toml with your API keys
```

### 3. Create runner configuration

Create `config/runner.toml` with your GitLab Runner configuration.
You can register the GitLab Runner with the following command:

```bash
docker run -it -v /var/run/docker.sock:/var/run/docker.sock -v ./config:/etc/gitlab-runner gitlab/gitlab-runner:latest register --url https://gitlab.example.com --token YOUR_TOKEN
```

**IMPORTANT:**
Under `[runners.docker]` there is a `pull_policy` setting.
Set it to:

```toml
pull_policy = ["if-not-present"]
```

Otherwise the runner will do too many `docker image pull` requests and your IP will be banned!

### 4. Start

```bash
cargo run --release
# or
./target/release/hetzner_gitlab_runner
```

## Configuration

### config/config.toml

```toml
[gitlab]
url = "https://gitlab.example.com"
token = "glpat-xxxxxxxxxxxxxxxxxxxx"  # read_api scope
# tag_filter = ["hetzner"]           # optional: only react to jobs with these tags
# project_refresh_minutes = 10       # how often the project list is refreshed

[hetzner]
token = "xxxxxxxxxxxxxxxxxxxxxxxx"
server_type = "ccx23"      # AMD dedicated CPU
location = "nbg1"          # Nuremberg
image = "ubuntu-24.04"
ssh_key_name = "my-ssh-key"

[runner]
name = "flexi-runner"
min_lifetime_minutes = 20  # Minimum runtime
poll_interval_seconds = 30  # 10-300
```

## Debug vs Release

| Feature          | Debug     | Release                              |
| ---------------- | --------- | ------------------------------------ |
| Polling interval | 15s       | 30s (from config)                    |
| Server deletion  | Immediate | After min. 20min + billing-optimized |

## GitLab API Usage / Rate Limits

GitLab.com enforces API rate limits (stricter on the Free tier). To stay within them:

- The project list (non-archived projects only) is cached and refreshed every `project_refresh_minutes`
- Each poll makes **one** jobs request per project (pending + running combined; more only if a project has 100+ active jobs) and stops at the first matching job
- While a server is running, GitLab is only polled when the server could actually be deleted (deletion window)
- On HTTP 429 the scan is aborted and polling pauses for the `Retry-After` duration

Rough worst case while idle: `3600 / poll_interval_seconds × number_of_projects` requests per hour.
With many projects, increase `poll_interval_seconds` accordingly (up to 300s; idle servers are
only deleted within a ~10-minute window per billing hour, so longer intervals could miss it).

## Billing Optimization

Hetzner charges per **started hour from server creation**.

Example:

- Server created at 14:47
- Pipeline finished at 15:10 (server ran 23min)
- Next billing cycle: 15:47
- Server is deleted at 15:42 (5min buffer)
  This ensures that if the server is needed again, the runner doesn't have to be set up again. Hetzner charges per started hour, meaning 5x10min = 5x60min billed.

## Logs

- `logs/orchestrator.log` - Detailed logs (daily rotation)
- `logs/runner_usage.csv` - Server usage documentation

### CSV Format

```csv
timestamp,event,server_id,project,pipeline_id,reason,duration_minutes
2026-01-14T10:30:00Z,START,12345678,mygroup/myproject,9876,pipeline_pending,
2026-01-14T11:15:00Z,STOP,12345678,,,all_pipelines_done,45
```
