# GitHub Actions Runner Controller

A Rust-based on-demand GitHub Actions runner controller for NixOS, implementing ARC-style (Actions Runner Controller) job detection with ephemeral NixOS containers.

## Overview

Instead of maintaining a pool of idle runners, this controller:
1. Polls GitHub API for queued jobs
2. Spawns ephemeral NixOS containers on-demand
3. Each container runs exactly one job then self-destructs
4. Zero resource usage when no jobs are queued

```
┌──────────────────────────────────────────────────────────────────┐
│                  NixOS Host (cdk-runner-01)                      │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │           runner-controller.service (persistent)           │  │
│  │                                                            │  │
│  │  - Polls GitHub API every 10s for queued workflow runs     │  │
│  │  - Filters for jobs matching our labels                    │  │
│  │  - Spawns container when job is queued                     │  │
│  │  - Monitors container completion and cleans up             │  │
│  └────────────────────────────────────────────────────────────┘  │
│                              │                                   │
│                              │ job detected                      │
│                              ▼                                   │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │           Ephemeral Container (j1234567)                   │  │
│  │                                                            │  │
│  │  1. Container created with job-specific name               │  │
│  │  2. GitHub runner registers with --ephemeral               │  │
│  │  3. Runner picks up the specific job                       │  │
│  │  4. Job executes                                           │  │
│  │  5. Runner exits (--ephemeral auto-deregisters)            │  │
│  │  6. Container destroyed                                    │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

## Configuration

The controller is configured via environment variables in the systemd service:

| Variable | Default | Description |
|----------|---------|-------------|
| `GITHUB_REPO` | required | Repository in `owner/repo` format |
| `GITHUB_TOKEN_FILE` | required | Path to GitHub PAT with `repo` and `admin:org` scopes |
| `MAX_CONCURRENT` | 7 | Maximum concurrent job containers |
| `POLL_INTERVAL` | 10 | Seconds between GitHub API polls |
| `JOB_TIMEOUT` | 7200 | Maximum job duration (2 hours) |
| `RUNNER_LABELS` | self-hosted,ci,nix,x64,Linux | Comma-separated runner labels |
| `STATE_DIR` | /var/lib/runner-controller | State directory for tracking |
| `HTTP_PORT` | 8080 | HTTP API port for status/health |

## Container Lifecycle

1. **Job Detection**: Controller polls GitHub API for queued/waiting/pending workflow runs
2. **Label Matching**: Jobs must request a subset of the labels we provide
3. **Container Creation**:
   - Name format: `j` + last 7 digits of job ID (e.g., `j1234567`)
   - Unique subnet allocated (192.168.100-199.0/24)
   - Registration token written to container filesystem
4. **Runner Registration**: Container's systemd service configures and starts the GitHub runner with `--ephemeral`
5. **Job Execution**: Runner picks up the job and executes the workflow
6. **Cleanup**: On completion/failure/timeout, the controller:
   - Deregisters runner from GitHub via API
   - Stops and destroys the container
   - Cleans up nspawn config, profiles, and network interfaces

## HTTP API

The controller exposes an HTTP API for monitoring:

- `GET /health` - Health check (returns 200 OK)
- `GET /status` - JSON status with active containers and configuration

Example status response:
```json
{
  "active_containers": [
    {
      "name": "j1234567",
      "job_id": 41234567890,
      "running_seconds": 145
    }
  ],
  "max_concurrent": 7,
  "poll_interval_seconds": 10,
  "job_timeout_seconds": 7200
}
```

## Helper Scripts

### runner-status
Shows current controller status, active containers, and GitHub runners:
```bash
runner-status
```

### cleanup-github-runners
Removes offline runners from GitHub that no longer have active containers:
```bash
cleanup-github-runners
```

### cleanup-all-containers
Emergency cleanup - stops all job containers and deregisters from GitHub:
```bash
cleanup-all-containers
```

## Container Template

Each container is created from `/etc/nixos/ci-container-template.nix` which provides:
- Docker daemon for container-based actions
- GitHub runner package from nixpkgs
- Nix with flakes enabled
- Common build tools (git, curl, jq, etc.)
- nix-ld for running dynamically-linked binaries

## Resource Limits

Containers are constrained via systemd resource controls (see `container-resource-limits.nix`):
- CPU quota per container
- Memory limits
- No swap

## Logs

View controller logs:
```bash
journalctl -u runner-controller -f
```

View container logs:
```bash
# List containers
nixos-container list | grep '^j'

# View specific container's runner logs
nixos-container run j1234567 -- journalctl -u github-runner
```

## Troubleshooting

### Container stuck / not cleaning up
```bash
# Check container status
nixos-container status j1234567

# Force cleanup
cleanup-all-containers
```

### Ghost runners in GitHub
```bash
# Remove offline runners
cleanup-github-runners
```

### Controller not starting
```bash
# Check for token file
cat /run/secrets/github-runner/token

# Check service status
systemctl status runner-controller
journalctl -u runner-controller --no-pager -n 50
```

## Architecture Notes

- **Why polling instead of webhooks?** Simpler deployment - no need for public endpoint, firewall rules, or webhook secret management. 10s polling adds minimal latency for CI workloads.

- **Why ephemeral containers?** Each job gets a clean environment. No state leakage between jobs. Automatic deregistration via `--ephemeral` flag prevents ghost runners.

- **Why Rust?** The original bash implementation (~450 lines) had issues with error handling, race conditions, and state management. Rust provides proper error handling, async concurrency, and typed API responses.
