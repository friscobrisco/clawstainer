# clawstainer

Lightweight, isolated Linux environments for AI agents. Spin up a sandbox in seconds, run commands, install packages, tear it down — all from a single CLI.

Built in Rust. Two runtime backends:
- **systemd-nspawn** — container-based, works everywhere including macOS via Lima
- **Firecracker** — microVM-based, hardware-level isolation, requires bare metal or KVM-capable Linux

## Quick Start

```bash
# Install
cargo install --path .

# Create a sandbox
clawstainer create --name dev-box --memory 1024 --cpus 2

# Run a command
clawstainer exec <id> "echo hello world"

# Install packages
clawstainer provision <id> --components python3,git,curl

# Open a shell
clawstainer shell <id>

# Copy files in/out of the sandbox
clawstainer cp <id>:/root/output.txt ./local/
clawstainer cp ./input.txt <id>:/root/

# Forward a port into the sandbox
clawstainer port-forward <id> 8080:8080

# Snapshot a provisioned sandbox for reuse
clawstainer snapshot create <id> --name python3-ready

# Create a new sandbox from the snapshot (no re-provisioning needed)
clawstainer create --name fast-box --from python3-ready

# Check resource usage
clawstainer stats <id>

# Tear it down
clawstainer destroy <id>
```

## Platform Support

### macOS (Apple Silicon / Intel)

clawstainer uses [Lima](https://lima-vm.io/) to run a lightweight Linux VM transparently. You just type `clawstainer create` — the CLI handles everything.

```bash
brew install lima
cargo install --path .
clawstainer create --name my-box
```

On first run, Lima provisions an Ubuntu 24.04 VM with `systemd-nspawn` and builds the Linux binary automatically. This takes ~2 minutes. Subsequent runs are instant.

> **Note**: Only the nspawn runtime works on macOS. Firecracker requires hardware virtualization (KVM), which isn't available inside Lima VMs on Apple Silicon due to the lack of nested virtualization.

### Linux (bare metal or VM)

Runs natively with no VM layer. Both runtimes are available.

```bash
# nspawn runtime (default)
sudo apt-get install -y systemd-container debootstrap
cargo install --path .
sudo clawstainer create --name my-box

# Firecracker runtime (requires /dev/kvm)
sudo clawstainer create --name fast-box --runtime firecracker
```

### Runtime Comparison

| | nspawn (default) | Firecracker |
|---|---|---|
| Isolation | Container (shared kernel) | VM (separate kernel) |
| Boot time | ~2-3s | ~125ms |
| Security | Namespace/cgroup isolation | Hardware-enforced |
| macOS via Lima | Yes | No (no nested KVM) |
| Linux bare metal | Yes | Yes |
| Linux cloud VM | Yes | Needs nested virt or metal |
| `/dev/kvm` required | No | Yes |

## AI Agent Templates

Built-in provisioning templates for popular AI agents:

```bash
# Claude Code
clawstainer create --name claude-box --memory 2048 --cpus 2
clawstainer provision <id> --components claude-code

# Hermes Agent (NousResearch)
clawstainer create --name hermes-box --memory 2048 --cpus 2 --linger
clawstainer provision <id> --components hermes-agent

# OpenClaw Gateway
clawstainer create --name openclaw-box --memory 2048 --cpus 2 --linger
clawstainer provision <id> --components openclaw
```

Each template includes all dependencies and has built-in timeouts. Use `--linger` for agents that run long-lived services (like OpenClaw and Hermes) — without it, systemd may stop user sessions on logout/idle and kill the gateway process.

## Fleet Management

Deploy multiple agent sandboxes from a single YAML definition:

```yaml
# fleet.yaml
machines:
  - name: hermes-worker
    count: 3
    memory: 2048
    cpus: 2
    provision: hermes-agent
    linger: true

  - name: openclaw
    count: 10
    memory: 1024
    cpus: 2
    provision: openclaw
    linger: true
```

```bash
# Create all machines, then provision in parallel (default: 3 at a time)
clawstainer fleet create --file fleet.yaml

# Higher parallelism
clawstainer fleet create --file fleet.yaml --parallel 5

# Destroy a specific group
clawstainer fleet destroy --name hermes-worker

# Destroy all fleet machines
clawstainer fleet destroy --all
```

Fleet create uses a two-pass approach: creates all machines first (visible in `list` immediately), then provisions them in parallel batches.

## Documentation

See [docs.md](docs.md) for the full CLI reference, architecture, and configuration.

## License

MIT
