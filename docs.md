# clawstainer Documentation

## Overview

clawstainer is a CLI tool that spins up disposable Linux sandboxes for AI agents. Each sandbox is an isolated machine with its own filesystem, network, and process space. Agents interact with sandboxes by executing commands and reading structured JSON output.

---

## Installation

### From source

```bash
git clone https://github.com/friscobrisco/clawstainer.git
cd clawstainer
cargo install --path .
```

### Platform support

| Platform | How it works |
|----------|-------------|
| Linux | Runs natively using `systemd-nspawn` |
| macOS | Automatically provisions a Lima Linux VM and proxies commands into it |

#### Linux dependencies

```bash
sudo apt-get install -y systemd-container debootstrap
```

#### macOS dependencies

```bash
brew install lima
```

Lima is installed automatically if missing when you first run a command.

---

## CLI Reference

### `clawstainer create`

Spin up a new sandbox.

```bash
clawstainer create [OPTIONS]
```

#### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--name <NAME>` | string | auto-generated | Human-readable name. If omitted, generates a random name like `bold-parrot` |
| `--memory <MB>` | integer | `512` | Memory limit in megabytes |
| `--cpus <N>` | integer | `1` | Number of CPU cores |
| `--network <MODE>` | string | `nat` | Network mode. `nat` gives internet access via NAT. `none` disables networking entirely |
| `--timeout <SECONDS>` | integer | `0` | Auto-destroy the sandbox after N seconds. `0` means no timeout |

#### Output

```json
{
  "id": "sb-a1b2c3d4",
  "name": "bold-parrot",
  "status": "running",
  "ip": "10.0.0.2",
  "created_at": "2026-03-24T10:30:00Z"
}
```

#### Examples

```bash
# Minimal — random name, 512MB, 1 CPU
clawstainer create

# Named with more resources
clawstainer create --name dev-box --memory 2048 --cpus 4

# No internet access
clawstainer create --name isolated --network none

# Self-destructing after 1 hour
clawstainer create --name temp --timeout 3600
```

#### Notes

- Machine IDs follow the format `sb-<8 hex chars>` (e.g. `sb-a1b2c3d4`)
- The first `create` on a fresh system downloads a base Ubuntu 24.04 rootfs (~200MB). Subsequent creates reuse the cached image.
- Each sandbox gets its own overlay filesystem, so changes inside one sandbox never affect another or the base image.

---

### `clawstainer exec`

Run a command inside a sandbox. This is the primary interface for AI agents.

```bash
clawstainer exec <MACHINE_ID> <COMMAND> [OPTIONS]
```

#### Arguments

| Argument | Description |
|----------|-------------|
| `MACHINE_ID` | The sandbox ID (e.g. `sb-a1b2c3d4`) |
| `COMMAND` | Shell command to execute (runs via `sh -c`) |

#### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--timeout <SECONDS>` | integer | `30` | Kill the command after N seconds |
| `--workdir <PATH>` | string | `/root` | Working directory inside the sandbox |
| `--env <KEY=VAL>` | string | — | Set an environment variable. Repeatable |
| `--user <USER>` | string | `root` | Run as this user |

#### Output

```json
{
  "machine_id": "sb-a1b2c3d4",
  "exit_code": 0,
  "stdout": "Hello, world!\n",
  "stderr": "",
  "duration_ms": 42,
  "timed_out": false
}
```

#### Additional output fields

| Field | When present | Description |
|-------|-------------|-------------|
| `truncated` | stdout > 1MB | Output was truncated to 1MB |
| `total_bytes` | when truncated | Original output size in bytes |

#### Examples

```bash
# Simple command
clawstainer exec sb-a1b2c3d4 "echo hello"

# Install something
clawstainer exec sb-a1b2c3d4 "apt-get update && apt-get install -y vim"

# Run with custom timeout for long operations
clawstainer exec sb-a1b2c3d4 --timeout 300 "curl -fsSL https://example.com/install.sh | bash"

# Set environment variables
clawstainer exec sb-a1b2c3d4 --env API_KEY=secret --env DEBUG=1 "python3 app.py"

# Run as non-root user
clawstainer exec sb-a1b2c3d4 --user nobody "whoami"

# Different working directory
clawstainer exec sb-a1b2c3d4 --workdir /tmp "pwd"
```

#### Environment variables

Every exec automatically sets these environment variables inside the sandbox:

| Variable | Value |
|----------|-------|
| `HOME` | `/root` (or `/home/<user>` for non-root) |
| `USER` | The `--user` value |
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `LANG` | `C.UTF-8` |
| `TERM` | `xterm-256color` |

Pass `--env` to override any of these.

---

### `clawstainer provision`

Install components inside a running sandbox using predefined recipes.

```bash
clawstainer provision <MACHINE_ID> [OPTIONS]
```

#### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--components <LIST>` | string | — | Comma-separated component names |
| `--file <PATH>` | string | — | Path to a YAML file listing components |
| `--timeout <SECONDS>` | integer | `120` | Per-component install timeout |

One of `--components` or `--file` is required.

#### Output

```json
{
  "machine_id": "sb-a1b2c3d4",
  "results": [
    {"component": "python3", "status": "ok", "duration_ms": 88300},
    {"component": "git", "status": "ok", "duration_ms": 15394},
    {"component": "nodejs", "status": "error", "error": "install timed out", "duration_ms": 120000}
  ]
}
```

#### Available components

| Component | What it installs |
|-----------|-----------------|
| `python3` | Python 3, pip, venv (enables universe repo) |
| `nodejs` | Node.js 20 via NodeSource |
| `git` | Git |
| `curl` | curl |
| `build-essential` | gcc, g++, make |
| `docker-cli` | Docker CLI |
| `ripgrep` | ripgrep (`rg`) |
| `jq` | jq |

#### Available bundles

Bundles are groups of components installed together:

| Bundle | Components |
|--------|-----------|
| `agent-default` | python3, nodejs, git, curl, jq, ripgrep |
| `web-dev` | nodejs, git, build-essential, curl |
| `ml` | python3, git, build-essential, curl |

#### Examples

```bash
# Single component
clawstainer provision sb-a1b2c3d4 --components git

# Multiple components
clawstainer provision sb-a1b2c3d4 --components python3,git,curl

# Install a bundle
clawstainer provision sb-a1b2c3d4 --components agent-default

# Longer timeout for heavy installs
clawstainer provision sb-a1b2c3d4 --components nodejs --timeout 300
```

#### Notes

- Components that fail do NOT block the rest. Each result is reported individually.
- Each component has a `verify` step that confirms the install succeeded (e.g. runs `python3 --version`).
- Components are defined in `components.yaml`. You can add custom components there.

---

### `clawstainer shell`

Open an interactive terminal inside a sandbox. For human use, not agents.

```bash
clawstainer shell <MACHINE_ID> [OPTIONS]
```

#### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--user <USER>` | string | `root` | Shell user |

#### Examples

```bash
clawstainer shell sb-a1b2c3d4
clawstainer shell sb-a1b2c3d4 --user nobody
```

Type `exit` or `Ctrl+D` to leave. The sandbox keeps running.

---

### `clawstainer destroy`

Tear down a sandbox and clean up all resources.

```bash
clawstainer destroy <MACHINE_ID>
clawstainer destroy --all
```

#### Options

| Flag | Description |
|------|-------------|
| `--all` | Destroy all sandboxes |

#### Output

```json
{
  "machine_id": "sb-a1b2c3d4",
  "status": "destroyed",
  "uptime_seconds": 3600
}
```

#### Notes

- Sends a graceful shutdown, waits up to 5 seconds, then force-kills.
- Removes the overlay filesystem and releases the IP address.
- When the last sandbox is destroyed, the network bridge and NAT rules are cleaned up.

---

### `clawstainer list`

List all sandboxes.

```bash
clawstainer list [OPTIONS]
```

#### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--format <FMT>` | string | `table` | Output format: `table` or `json` |
| `--status <STATUS>` | string | `all` | Filter: `running`, `stopped`, or `all` |

#### Table output

```
ID             NAME           STATUS     MEMORY   CPUS   UPTIME       IP
sb-a1b2c3d4    bold-parrot    running    512MB    1      2h 15m       10.0.0.2
sb-e5f6g7h8    shy-falcon     running    1024MB   2      45m          10.0.0.3
```

#### JSON output

```bash
clawstainer list --format json
```

---

### `clawstainer logs`

Show the command execution history for a sandbox.

```bash
clawstainer logs <MACHINE_ID> [OPTIONS]
```

#### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--last <N>` | integer | `20` | Show last N entries |
| `--format <FMT>` | string | `table` | Output format: `table` or `json` |
| `--follow` | flag | — | Stream new entries (not yet implemented) |

#### Table output

```
TIMESTAMP                COMMAND                                  EXIT   DURATION
2026-03-24 21:32:19      apt-get update                           0      11419ms
2026-03-24 21:34:18      python3 -c 'print("hello")'              0      16ms
```

Every `exec` call is logged automatically with timestamp, command, exit code, and duration.

---

### `clawstainer stats`

Show live resource usage for a sandbox.

```bash
clawstainer stats <MACHINE_ID> [OPTIONS]
```

#### Options

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--watch <SECONDS>` | integer | `0` | Refresh every N seconds. `0` = one-shot |
| `--format <FMT>` | string | `table` | Output format: `table` or `json` |

#### Table output

```
Sandbox: sb-a1b2c3d4
Uptime:  up 2 hours, 15 minutes

CPU:     12.3%
Memory:  256.4 MB / 512 MB (50.1%)
Disk:    1200 MB / 8853 MB
Procs:   8
```

#### Examples

```bash
# One-shot
clawstainer stats sb-a1b2c3d4

# Live monitoring (refresh every 2 seconds)
clawstainer stats sb-a1b2c3d4 --watch 2

# JSON for programmatic use
clawstainer stats sb-a1b2c3d4 --format json
```

---

## Error Handling

All errors are returned as JSON to stderr with a non-zero exit code:

```json
{
  "error": "machine_not_found",
  "message": "No machine with ID 'sb-xyz'",
  "hint": "Run 'clawstainer list' to see active machines"
}
```

### Exit codes

| Code | Error | Description |
|------|-------|-------------|
| 0 | — | Success |
| 1 | `internal_error` | Unexpected error |
| 2 | `machine_not_found` | Invalid machine ID |
| 3 | `machine_not_running` | Machine exists but is stopped |
| 4 | `create_failed` | Failed to create sandbox |
| 5 | `exec_timeout` | Command exceeded timeout |
| 6 | `exec_failed` | Failed to run command |
| 7 | `provision_failed` | Component install failed |
| 8 | `runtime_unavailable` | Linux/nspawn not available |
| 9 | `resource_limit` | Host resources exhausted |
| 10 | `permission_denied` | Needs root or wrong group |

---

## Architecture

```
macOS / Linux host
  └── clawstainer CLI
        ├── Lima proxy (macOS only — transparent VM management)
        └── Runtime Interface
              ├── NspawnRuntime (MVP — systemd-nspawn)
              └── FirecrackerRuntime (future)
```

### Isolation layers

| Layer | Mechanism | What it prevents |
|-------|-----------|-----------------|
| Filesystem | Overlay mount | Sandbox can't modify base image or host |
| Process | PID namespace | Can't see or signal host processes |
| Network | Network namespace + veth | Isolated network stack |
| Resources | cgroups v2 | Can't exhaust host CPU/memory |
| Syscalls | Seccomp (nspawn default) | Blocks dangerous syscalls |

### Networking

Each sandbox gets a virtual ethernet pair connected to a host bridge:

- Bridge: `claw-br0` at `10.0.0.1/24`
- Each sandbox gets `10.0.0.N/24` (auto-allocated)
- NAT masquerade for outbound internet
- Sandboxes cannot reach each other
- DNS: `8.8.8.8` / `8.8.4.4`
- Use `--network none` to disable networking entirely

### Filesystem layout

```
~/.clawstainer/
├── state.json              # Machine state (flock-protected)
├── state.lock              # Advisory lock file
└── logs/                   # Per-machine exec logs (JSONL)
    └── sb-a1b2c3d4.jsonl

/var/lib/clawstainer/
├── base-images/
│   └── ubuntu-24.04/       # Shared read-only base image
└── machines/
    └── sb-a1b2c3d4/        # Per-machine overlay
        ├── upper/
        ├── work/
        └── rootfs/          # Merged overlay mount
```

### State management

Machine state is stored in `~/.clawstainer/state.json` with filesystem advisory locking (`flock`). Writes are atomic (write to temp file, then rename). Concurrent CLI invocations are safe.

---

## Configuration

### components.yaml

Components are defined in `components.yaml` at the project root. Each component has:

```yaml
components:
  my-tool:
    install: apt-get install -y my-tool    # Shell command to install
    verify: my-tool --version              # Shell command to verify success
    tags: [tool]                           # Metadata tags

bundles:
  my-stack:
    components: [my-tool, python3, git]    # Group of components
```

This file is embedded into the binary at compile time. To add custom components, edit it and rebuild.

---

## For AI Agents

clawstainer is designed for AI agent tool use. Every command returns structured JSON. A typical agent workflow:

```
1. create  → get sandbox ID
2. exec    → run commands, read stdout/stderr/exit_code
3. exec    → install dependencies, write code, run tests
4. destroy → clean up when done
```

The `exec` command is the primary interface. Agents don't need `provision`, `shell`, or `stats` — those are convenience commands for humans.
