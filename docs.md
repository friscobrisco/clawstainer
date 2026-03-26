# clawstainer Documentation

## Overview

clawstainer is a CLI tool that spins up disposable Linux sandboxes for AI agents. Each sandbox is an isolated machine with its own filesystem, network, and process space. Agents interact with sandboxes by executing commands and reading structured JSON output.

Two runtime backends are available:
- **nspawn** (default) — container-based isolation via `systemd-nspawn`. Works on any Linux host, including macOS via Lima.
- **Firecracker** — microVM-based isolation with a separate kernel per sandbox. Requires a KVM-capable Linux host (bare metal or cloud with nested virtualization).

---

## Installation

### From source

```bash
git clone https://github.com/friscobrisco/clawstainer.git
cd clawstainer
cargo install --path .
```

### macOS

clawstainer uses [Lima](https://lima-vm.io/) to transparently run a Linux VM on macOS. You don't interact with Lima directly — the CLI manages it automatically.

```bash
brew install lima
cargo install --path .
clawstainer create --name my-box   # just works
```

On first run, Lima provisions an Ubuntu 24.04 VM with `systemd-nspawn`, builds the Linux binary, and caches everything. This takes ~2 minutes once. After that, commands run immediately.

**macOS limitations**:
- Only the **nspawn** runtime is available. Firecracker requires `/dev/kvm` (hardware virtualization), which is not available inside Lima VMs on Apple Silicon — there is no nested virtualization support.
- Sandboxes require `sudo` inside the VM. The CLI handles this transparently.

### Linux

Runs natively with no VM layer.

```bash
# nspawn runtime (default)
sudo apt-get install -y systemd-container debootstrap
cargo install --path .
sudo clawstainer create --name my-box
```

For Firecracker (requires `/dev/kvm`):

```bash
sudo clawstainer create --name fast-box --runtime firecracker
```

### Platform matrix

| Platform | nspawn | Firecracker | Notes |
|----------|--------|-------------|-------|
| macOS (Apple Silicon) | Via Lima | No | No nested KVM in Lima VMs |
| macOS (Intel) | Via Lima | No | Same limitation |
| Linux bare metal | Native | Native | Full support |
| Linux VM (cloud) | Native | Needs nested virt | GCP supports it, AWS needs `.metal` |
| AWS `.metal` instances | Native | Native | Full support |

---

## Runtimes

### nspawn (default)

Uses `systemd-nspawn` to create containers with:
- Overlay filesystem (each sandbox gets its own writable layer over a shared base image)
- PID, network, and mount namespaces
- cgroup v2 resource limits (memory, CPU)
- Seccomp syscall filtering

Sandboxes share the host kernel. This is fast and lightweight but means a kernel exploit could escape the sandbox.

```bash
clawstainer create --name my-box                    # uses nspawn by default
clawstainer create --name my-box --runtime nspawn   # explicit
```

### Firecracker

Uses [Firecracker](https://firecracker-microvm.github.io/) microVMs with:
- Separate Linux kernel per sandbox (true VM isolation)
- Hardware-enforced isolation via KVM
- ~125ms boot time
- Per-VM ext4 rootfs (sparse copies for efficiency)
- Communication via vsock (no network dependency for exec)

Requires `/dev/kvm`. Not available on macOS or inside VMs without nested virtualization.

```bash
clawstainer create --name fast-box --runtime firecracker
```

The `--runtime` flag is only needed on `create`. All other commands (`exec`, `shell`, `destroy`, etc.) automatically detect which runtime the sandbox is using.

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
| `--runtime <RUNTIME>` | string | `nspawn` | Runtime backend: `nspawn` or `firecracker`. Firecracker requires `/dev/kvm` |

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
| `claude-code` | Claude Code CLI (installed via `claude.ai`). Timeout: 300s |
| `hermes-agent` | Hermes agent from NousResearch (installed via remote script). Timeout: 600s |
| `openclaw` | OpenClaw gateway (installed via `openclaw.ai`, runs as systemd service). Timeout: 600s |

#### Available bundles

Bundles are groups of components installed together:

| Bundle | Components |
|--------|-----------|
| `agent-default` | python3, nodejs, git, curl, jq, ripgrep |
| `web-dev` | nodejs, git, build-essential, curl |
| `ml` | python3, git, build-essential, curl |
| `openclaw` | curl, openclaw |

#### AI Agent Templates

Ready-to-use components for spinning up AI agent sandboxes:

```bash
# Claude Code sandbox
clawstainer create --name claude-box --memory 2048 --cpus 2
clawstainer provision <id> --components claude-code

# Hermes Agent sandbox (needs 4GB+ for Python, Node.js, and agent)
clawstainer create --name hermes-box --memory 4096 --cpus 2
clawstainer provision <id> --components hermes-agent

# OpenClaw Gateway sandbox
clawstainer create --name openclaw-box --memory 2048 --cpus 2
clawstainer provision <id> --components openclaw
```

These components have built-in timeouts so you don't need to pass `--timeout` manually.

#### Examples

```bash
# Single component
clawstainer provision sb-a1b2c3d4 --components git

# Multiple components
clawstainer provision sb-a1b2c3d4 --components python3,git,curl

# Install a bundle
clawstainer provision sb-a1b2c3d4 --components agent-default

# Override per-component timeout with CLI flag
clawstainer provision sb-a1b2c3d4 --components nodejs --timeout 300
```

#### Notes

- Components can define their own `timeout` (in seconds) in `components.yaml`. The CLI `--timeout` flag overrides the default (120s) but per-component timeouts take priority when set.
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

### `clawstainer port-forward`

Forward a host port into a running sandbox via iptables DNAT rules.

```bash
clawstainer port-forward <MACHINE_ID> <PORT>
```

#### Arguments

| Argument | Description |
|----------|-------------|
| `MACHINE_ID` | The sandbox ID (e.g. `sb-a1b2c3d4`) |
| `PORT` | Port mapping: `HOST_PORT:SANDBOX_PORT` (e.g. `8080:3000`) or a single port for same on both sides (e.g. `8080`) |

#### Examples

```bash
# Forward host port 8080 to sandbox port 8080
clawstainer port-forward sb-a1b2c3d4 8080

# Forward host port 9090 to sandbox port 3000
clawstainer port-forward sb-a1b2c3d4 9090:3000
```

#### Notes

- Sets up PREROUTING and OUTPUT DNAT rules so traffic to `localhost:<HOST_PORT>` reaches the sandbox.
- Also adds a FORWARD ACCEPT rule for the destination.
- Requires root (iptables). On macOS via Lima, this runs inside the VM transparently.
- The sandbox must be running and have an IP (i.e., not created with `--network none`).
- Port forwarding rules are not persisted — they are lost if the host reboots or the sandbox is destroyed.

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
macOS                                    Linux (bare metal / cloud)
  └── clawstainer CLI                      └── clawstainer CLI
        └── Lima proxy (transparent)             └── Runtime Interface
              └── Linux VM                             ├── NspawnRuntime (containers)
                    └── NspawnRuntime                  └── FirecrackerRuntime (microVMs, needs KVM)
```

### How the Lima proxy works (macOS)

On macOS, the CLI detects it's not on Linux and transparently:
1. Ensures a Lima VM is running (creates one on first use)
2. Builds the Linux binary inside the VM (cached after first build)
3. Re-executes the same command inside the VM via `limactl shell`
4. Passes stdout/stderr/exit code back to the user

You never interact with Lima directly. The project directory is mounted writable in the VM, so state is shared.

### Isolation layers

#### nspawn (containers)

| Layer | Mechanism | What it prevents |
|-------|-----------|-----------------|
| Filesystem | Overlay mount | Sandbox can't modify base image or host |
| Process | PID namespace | Can't see or signal host processes |
| Network | Network namespace + veth | Isolated network stack |
| Resources | cgroups v2 | Can't exhaust host CPU/memory |
| Syscalls | Seccomp (nspawn default) | Blocks dangerous syscalls |

#### Firecracker (microVMs)

| Layer | Mechanism | What it prevents |
|-------|-----------|-----------------|
| Filesystem | Separate ext4 disk image | Complete filesystem isolation |
| Process | Separate kernel | Full process isolation — different OS instance |
| Network | TAP device + bridge | Isolated network stack |
| Resources | Firecracker VMM limits | Hardware-enforced CPU/memory limits |
| Syscalls | KVM hardware boundary | No shared kernel attack surface |
| Communication | vsock (claw-agent) | No SSH needed, zero network dependency |

### Networking

Both runtimes use the same networking model:

- Bridge: `claw-br0` at `10.0.0.1/24`
- Each sandbox gets `10.0.0.N/24` (auto-allocated from pool)
- NAT masquerade for outbound internet
- Sandboxes cannot reach each other (iptables FORWARD DROP)
- DNS: `8.8.8.8` / `8.8.4.4`
- Use `--network none` to disable networking entirely

The difference is how sandboxes connect to the bridge:
- **nspawn**: veth pairs (one end in container, one end on bridge)
- **Firecracker**: TAP devices (attached to bridge, passed to VM as NIC)

### Filesystem layout

```
~/.clawstainer/
├── state.json              # Machine state (flock-protected)
├── state.lock              # Advisory lock file
└── logs/                   # Per-machine exec logs (JSONL)
    └── sb-a1b2c3d4.jsonl

/var/lib/clawstainer/
├── base-images/
│   └── ubuntu-24.04/       # Shared read-only base image (directory)
├── firecracker/
│   ├── bin/firecracker      # Firecracker binary (auto-downloaded)
│   ├── bin/claw-agent       # Guest agent binary
│   ├── kernels/vmlinux      # Linux kernel for VMs (auto-downloaded)
│   └── base-rootfs.ext4    # Base image converted to ext4 (for Firecracker)
└── machines/
    └── sb-a1b2c3d4/        # Per-machine storage
        ├── upper/           # nspawn: overlay upper layer
        ├── work/            # nspawn: overlay work dir
        ├── rootfs/          # nspawn: merged overlay mount
        ├── rootfs.ext4      # Firecracker: per-VM disk image
        ├── firecracker.sock # Firecracker: API socket
        └── vsock.sock       # Firecracker: vsock UDS for guest agent
```

### State management

Machine state is stored in `~/.clawstainer/state.json` with filesystem advisory locking (`flock`). Writes are atomic (write to temp file, then rename). Concurrent CLI invocations are safe.

Each machine record tracks which runtime backend created it (`"runtime": "nspawn"` or `"runtime": "firecracker"`), so commands like `exec` and `destroy` automatically use the correct backend.

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
