# Port Allocation for Multiple Runners - Scaling Guide

This document explains how Fedimint solves port conflicts with multiple runners and how to apply this to CDK when scaling beyond 1 runner.

---

## Table of Contents

1. [Problem Overview](#problem-overview)
2. [Fedimint's Solution: portalloc](#fedimints-solution-portalloc)
3. [How portalloc Works](#how-portalloc-works)
4. [Integration Guide for CDK](#integration-guide-for-cdk)
5. [Alternative Solutions](#alternative-solutions)
6. [Implementation Checklist](#implementation-checklist)

---

## Problem Overview

### The Port Conflict Issue

When running multiple GitHub Actions runners on the same server, integration tests that use hardcoded ports will conflict:

```
Timeline:
Runner-a starts: docker run -p 5433:5432 postgres  ✅
Runner-b starts: docker run -p 5433:5432 postgres  ❌ ERROR: Port already in use
```

### CDK's Current Port Usage (Hardcoded)

**PostgreSQL Test Database:**
- File: `crates/cdk-postgres/start_db_for_test.sh`
- Port: `5433` (hardcoded)

**Mint Services:**
- File: `crates/cdk-integration-tests/src/bin/start_regtest_mints.rs`
- CLN Mint: `8085`
- Fake Mint: `8086`
- LND Mint: `8087`
- LDK Mint: `8089`
- LDK P2P: `8099` (calculated: base + 10)
- LDK Web: `8090` (calculated: base + 1)

### Why This Matters

With 1 runner: ✅ No conflicts
With 2+ runners: ❌ Tests fail randomly when ports collide

---

## Fedimint's Solution: portalloc

### Overview

Fedimint created `fedimint-portalloc`, a Rust library that provides **cooperative port allocation** across multiple processes.

**Key Features:**
- Multi-process safe (uses file locking)
- Automatic cleanup (120-second expiration)
- Range allocation (request multiple consecutive ports)
- Verification (actually tries to bind ports)
- Simple (no database, just JSON + filesystem)

### Repository Information

**Crate:** `fedimint-portalloc` (published on crates.io)
**Source:** https://github.com/fedimint/fedimint/tree/master/utils/portalloc
**Docs:** https://docs.rs/fedimint-portalloc/latest/

---

## How portalloc Works

### Architecture

```
┌─────────────┐        ┌─────────────┐        ┌─────────────┐
│  Runner-a   │        │  Runner-b   │        │  Runner-c   │
│  Test       │        │  Test       │        │  Test       │
└──────┬──────┘        └──────┬──────┘        └──────┬──────┘
       │                      │                       │
       │ port_alloc(3)        │ port_alloc(5)         │ port_alloc(2)
       │                      │                       │
       └──────────────────────┼───────────────────────┘
                              │
                              ▼
                    ┌─────────────────────┐
                    │  Shared Directory   │
                    │  ~/.cache/port-alloc│
                    ├─────────────────────┤
                    │ lock (advisory)     │
                    │ fm-portalloc.json   │
                    └─────────────────────┘
```

### File-Based Locking

**Lock File:** `~/.cache/port-alloc/lock`

```rust
// From fedimint/utils/portalloc/src/data.rs
fn lock(&mut self) -> Result<()> {
    if self.lock_file.try_lock_exclusive().is_err() {
        info!("Lock taken, waiting...");
        self.lock_file.lock_exclusive()?;  // Blocks until available
        info!("Acquired lock after wait");
    };
    Ok(())
}
```

**How it works:**
1. Thread/process requests lock on shared file
2. If locked, waits (blocks) until available
3. Only one allocator can run at a time
4. Releases lock after allocation completes

### JSON State File

**Location:** `~/.cache/port-alloc/fm-portalloc.json`

**Format:**
```json
{
  "next": 10015,
  "keys": {
    "10000": {
      "size": 3,
      "expires": 1731688200
    },
    "10003": {
      "size": 5,
      "expires": 1731688205
    },
    "10008": {
      "size": 2,
      "expires": 1731688210
    }
  }
}
```

**Fields:**
- `next`: Next port to try (optimization to avoid scanning from beginning)
- `keys`: BTreeMap of allocated ranges
  - Key: Base port (first port in range)
  - Value: `{ size: number of ports, expires: Unix timestamp }`

### Data Structures

```rust
// From fedimint/utils/portalloc/src/data/dto.rs

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RootData {
    /// Next port to try.
    next: u16,

    /// Map of port ranges. For each range, the key is the first port in the
    /// range and the range size and expiration time are stored in the value.
    keys: BTreeMap<u16, RangeData>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct RangeData {
    /// Port range size.
    size: u16,

    /// Unix timestamp when this range expires.
    expires: UnixTimestamp,
}
```

### Allocation Algorithm

**Port Range:** 10,000 - 32,000 (avoids system ports and ephemeral range)

```rust
// Simplified from fedimint/utils/portalloc/src/data/dto.rs

pub fn get_free_port_range(&mut self, range_size: u16) -> u16 {
    self.reclaim();  // Remove expired entries first

    let mut base_port: u16 = self.next;

    'retry: loop {
        // Wrap around if we hit the upper limit
        if base_port > HIGH {
            self.reclaim();
            base_port = LOW;
        }

        let range = base_port..base_port + range_size;

        // Check if range conflicts with existing allocations
        if let Some(next_port) = self.contains(range.clone()) {
            base_port = next_port;
            continue 'retry;
        }

        // Verify ports are actually bindable (real kernel check)
        for port in range.clone() {
            match (
                TcpListener::bind(("127.0.0.1", port)),
                UdpSocket::bind(("127.0.0.1", port)),
            ) {
                (Err(err), _) | (_, Err(err)) => {
                    // Port not available, try next one
                    base_port = port + 1;
                    continue 'retry;
                }
                (Ok(tcp), Ok(udp)) => {
                    // Port is free, drop bindings immediately
                    drop((tcp, udp));
                }
            };
        }

        // Record allocation with 120-second expiration
        self.insert(range);
        return base_port;
    }
}
```

### Conflict Detection

```rust
// Check if requested range overlaps with any allocated range
fn contains(&self, range: std::ops::Range<u16>) -> Option<u16> {
    self.keys.range(..range.end).next_back().and_then(|(k, v)| {
        let start = *k;
        let end = start + v.size;

        if start < range.end && range.start < end {
            // Overlap detected, return next available port
            Some(end)
        } else {
            None
        }
    })
}
```

**Example:**
```
Allocated: [10000-10002] (size=3)
Request: [10001-10005] (size=5)

Overlap detected: 10001 is within [10000-10002]
Returns: 10003 (next free port after allocated range)
Next retry starts at: 10003
```

### Automatic Cleanup

```rust
fn reclaim(&mut self) {
    let now = Self::now_ts();
    // Remove all entries where expiration has passed
    self.keys.retain(|_k, v| now < v.expires);
}
```

**Expiration Time:** 120 seconds (2 minutes)
- Long enough for test setup/teardown
- Short enough to avoid permanent leaks
- Automatically cleans up crashed tests

### Public API

```rust
/// Allocate a range of consecutive ports
///
/// Returns: Base port of the allocated range
/// Panics: If unable to allocate after many retries
pub fn port_alloc(range_size: u16) -> anyhow::Result<u16>
```

**Usage Example:**
```rust
use fedimint_portalloc::port_alloc;

// Allocate 3 consecutive ports
let base_port = port_alloc(3)?;
// Use: base_port, base_port+1, base_port+2

println!("PostgreSQL: {}", base_port);
println!("Gateway: {}", base_port + 1);
println!("Esplora: {}", base_port + 2);
```

---

## Integration Guide for CDK

### Step 1: Add Dependency

**Add to `crates/cdk-integration-tests/Cargo.toml`:**
```toml
[dependencies]
fedimint-portalloc = "0.5"  # Check latest version
```

### Step 2: Allocate Ports in Test Setup

**Modify `crates/cdk-integration-tests/src/bin/start_regtest_mints.rs`:**

```rust
use fedimint_portalloc::port_alloc;

fn main() -> anyhow::Result<()> {
    // OLD: Hardcoded ports
    // let cln_port = 8085;
    // let lnd_port = 8087;
    // let ldk_port = 8089;

    // NEW: Dynamic allocation
    let base_port = port_alloc(10)?;  // Allocate 10 consecutive ports

    let cln_port = base_port;
    let lnd_port = base_port + 2;
    let ldk_port = base_port + 4;
    let postgres_port = base_port + 6;

    println!("Allocated ports: CLN={}, LND={}, LDK={}, PG={}",
             cln_port, lnd_port, ldk_port, postgres_port);

    // ... rest of setup using allocated ports
}
```

### Step 3: Update PostgreSQL Test Script

**Modify `crates/cdk-postgres/start_db_for_test.sh`:**

```bash
#!/usr/bin/env bash

# OLD: Hardcoded port
# DB_PORT="5433"

# NEW: Use environment variable from test code
DB_PORT="${CDK_TEST_POSTGRES_PORT:-5433}"  # Fallback to 5433 if not set

echo "Starting PostgreSQL on port ${DB_PORT}..."

docker run -d --rm \
  --name "${CONTAINER_NAME}" \
  -e POSTGRES_USER="${DB_USER}" \
  -e POSTGRES_PASSWORD="${DB_PASS}" \
  -e POSTGRES_DB="${DB_NAME}" \
  -p ${DB_PORT}:5432 \
  postgres:16
```

### Step 4: Pass Allocated Ports to Tests

**In test setup code:**

```rust
use fedimint_portalloc::port_alloc;
use std::env;

pub fn setup_test_environment() -> anyhow::Result<()> {
    // Allocate 10 consecutive ports for the test
    let base_port = port_alloc(10)?;

    // Set environment variables for scripts and test code
    env::set_var("CDK_TEST_POSTGRES_PORT", (base_port).to_string());
    env::set_var("CDK_ITESTS_MINT_PORT_0", (base_port + 1).to_string());
    env::set_var("CDK_ITESTS_MINT_PORT_1", (base_port + 2).to_string());
    env::set_var("CDK_ITESTS_MINT_PORT_2", (base_port + 3).to_string());

    Ok(())
}
```

### Step 5: Update Infrastructure Configuration

**Modify `hosts/runner/github-runner.nix`:**

```nix
serviceOverrides = {
  # ... existing overrides ...

  # Share the same portalloc dir so workers don't suffer random port conflicts
  Environment = ''
    "FM_PORTALLOC_DATA_DIR=/home/github-runner/.cache/port-alloc"
  '';
};
```

### Step 6: Update flake.nix for Multiple Runners

**Change from:**
```nix
runners = ["a"];  # 1 runner
```

**To:**
```nix
runners = ["a" "b"];  # 2 runners (now safe with portalloc)
```

### Step 7: Test the Implementation

**Verify port allocation works:**

```bash
# Run this on the runner server
cd /tmp
git clone https://github.com/cashubtc/cdk
cd cdk

# Run multiple tests in parallel
cargo test --test integration_test_1 &
cargo test --test integration_test_2 &
cargo test --test integration_test_3 &

wait

# Check portalloc state
cat ~/.cache/port-alloc/fm-portalloc.json
```

---

## Alternative Solutions

### Option 1: GitHub Actions Concurrency Groups

**Simpler alternative - no code changes:**

```yaml
# .github/workflows/ci.yml
jobs:
  regtest-itest:
    name: "Integration regtest tests"
    runs-on: [self-hosted, ci]
    concurrency:
      group: integration-tests
      cancel-in-progress: false
    # ... rest of job

  fake-mint-itest:
    name: "Integration fake mint tests"
    runs-on: [self-hosted, ci]
    concurrency:
      group: integration-tests
      cancel-in-progress: false
    # ... rest of job
```

**Result:** Integration tests run sequentially, but compilation jobs can still run in parallel.

### Option 2: Port Offset by Runner Name

**Use GitHub Actions runner name to calculate offset:**

```yaml
# .github/workflows/ci.yml
jobs:
  regtest-itest:
    name: "Integration regtest tests"
    runs-on: [self-hosted, ci]
    steps:
      - name: Calculate port offset
        id: ports
        run: |
          # Extract runner suffix (a, b, c, d)
          RUNNER_SUFFIX="${{ runner.name }}"
          RUNNER_SUFFIX="${RUNNER_SUFFIX##*-}"

          # Calculate offset (a=0, b=100, c=200, d=300)
          case "$RUNNER_SUFFIX" in
            a) OFFSET=0 ;;
            b) OFFSET=100 ;;
            c) OFFSET=200 ;;
            d) OFFSET=300 ;;
            *) OFFSET=0 ;;
          esac

          echo "POSTGRES_PORT=$((5433 + OFFSET))" >> $GITHUB_ENV
          echo "CLN_PORT=$((8085 + OFFSET))" >> $GITHUB_ENV
          echo "LND_PORT=$((8087 + OFFSET))" >> $GITHUB_ENV

      - name: Run tests
        run: |
          export DB_PORT=$POSTGRES_PORT
          cargo test
```

**Port assignments:**
- Runner-a: PostgreSQL 5433, CLN 8085, LND 8087
- Runner-b: PostgreSQL 5533, CLN 8185, LND 8187
- Runner-c: PostgreSQL 5633, CLN 8285, LND 8287
- Runner-d: PostgreSQL 5733, CLN 8385, LND 8387

### Option 3: Docker Random Port Mapping

**Let Docker assign random host ports:**

```bash
# Instead of: docker run -p 5433:5432 postgres
# Use: docker run -p 0:5432 postgres  # Docker picks random available port

# Discover the assigned port
POSTGRES_PORT=$(docker port container_name 5432 | cut -d: -f2)
export DATABASE_URL="postgresql://user:pass@localhost:${POSTGRES_PORT}/db"
```

**Pros:** Fully automatic, no coordination needed
**Cons:** Requires discovering assigned ports, more complex test code

---

## Implementation Checklist

### Pre-Implementation

- [ ] Review current CDK test port usage
- [ ] Identify all hardcoded ports in test code
- [ ] Decide on portalloc vs alternative solutions
- [ ] Plan testing strategy for port allocation

### Phase 1: Add portalloc Dependency

- [ ] Add `fedimint-portalloc` to `Cargo.toml`
- [ ] Test basic allocation in a simple example
- [ ] Verify it compiles and runs

### Phase 2: Modify Test Setup Code

- [ ] Update `start_regtest_mints.rs` to use `port_alloc()`
- [ ] Update `start_db_for_test.sh` to accept dynamic ports
- [ ] Update any other test setup scripts
- [ ] Pass allocated ports via environment variables

### Phase 3: Update Infrastructure

- [ ] Add `FM_PORTALLOC_DATA_DIR` to `github-runner.nix`
- [ ] Update `flake.nix` to use 2 runners
- [ ] Deploy configuration to test server
- [ ] Verify shared directory is created

### Phase 4: Testing

- [ ] Run single test - verify it allocates ports
- [ ] Run two tests in parallel - verify no conflicts
- [ ] Check `fm-portalloc.json` shows correct allocations
- [ ] Verify ports are released after tests complete
- [ ] Test expiration cleanup (wait 2+ minutes)

### Phase 5: Production Deployment

- [ ] Update CDK workflows to use `[self-hosted, ci]`
- [ ] Deploy with 2 runners
- [ ] Monitor for port conflicts in CI runs
- [ ] Document new setup in CDK repository

---

## Performance Considerations

### Allocation Overhead

**Typical allocation time:** <10ms
- File lock: ~1ms
- JSON parse: ~1-2ms
- Port binding verification: ~5ms per port
- Total: Negligible compared to test runtime

### Scalability

**Maximum runners:** Limited by port range
- Range: 10,000 - 32,000 = 22,000 ports
- If each test uses 10 ports: ~2,200 concurrent tests
- Practical limit: 4-8 runners per server (CPU/RAM limited)

### Lock Contention

**Impact:** Minimal with 2-4 runners
- Lock hold time: <10ms
- Probability of collision: Low
- Blocking time if collision: <20ms

---

## Troubleshooting

### Issue: Port allocation fails

**Check:**
```bash
# Verify shared directory exists
ls -la /home/github-runner/.cache/port-alloc/

# Check file permissions
ls -l /home/github-runner/.cache/port-alloc/lock
ls -l /home/github-runner/.cache/port-alloc/fm-portalloc.json

# View current allocations
cat /home/github-runner/.cache/port-alloc/fm-portalloc.json | jq
```

**Solution:** Ensure directory is writable by `github-runner` user

### Issue: Ports not released

**Check:**
```bash
# View allocations with timestamps
cat /home/github-runner/.cache/port-alloc/fm-portalloc.json

# Current time
date +%s

# Compare expiration times (should be < current time + 120)
```

**Solution:** Wait for expiration (120 seconds) or manually delete JSON file

### Issue: Stale lock file

**Check:**
```bash
# Check if lock file is held
lsof /home/github-runner/.cache/port-alloc/lock
```

**Solution:** If no process holds it, delete the lock file:
```bash
rm /home/github-runner/.cache/port-alloc/lock
```

### Issue: Too many allocated ports

**Check:**
```bash
# Count allocated ranges
cat /home/github-runner/.cache/port-alloc/fm-portalloc.json | jq '.keys | length'
```

**Solution:** Trigger cleanup or increase port range if needed

---

## References

- **Fedimint portalloc source:** https://github.com/fedimint/fedimint/tree/master/utils/portalloc
- **Crate documentation:** https://docs.rs/fedimint-portalloc/latest/
- **Advisory file locking (fs2):** https://docs.rs/fs2/latest/fs2/
- **Current CDK migration plan:** [CDK-MIGRATION-PLAN.md](CDK-MIGRATION-PLAN.md)

---

## Future Enhancements

### Dynamic Port Range Expansion

If 22,000 ports isn't enough, modify constants:
```rust
const LOW: u16 = 10_000;
const HIGH: u16 = 50_000;  // Expanded range
```

### Persistent Allocations

For long-running services (not ephemeral tests):
```rust
let base_port = port_alloc(3)?;
// Periodically refresh allocation before 120s expires
```

### Custom Expiration Times

Modify for longer/shorter test durations:
```rust
const TIMEOUT: u64 = 300;  // 5 minutes instead of 2
```

---

**Document Version:** 1.0
**Last Updated:** 2025-01-15
**Maintained By:** CDK Infrastructure Team
