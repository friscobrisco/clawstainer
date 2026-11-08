# clawstainer v0.2.1

[![Version](https://img.shields.io/badge/version-0.2.1-blue.svg)]()
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-blue.svg)]()
[![Open Source](https://img.shields.io/badge/open%20source-%E2%9C%93-brightgreen.svg)](https://github.com/friscobrisco/clawstainer)

Lightweight, isolated Linux environments for AI agents. Spin up a sandbox in seconds, run commands, install packages, tear it down — all from a single CLI.

> clawstainer is open source and MIT-licensed. Issues, ideas, and contributions are welcome on [GitHub](https://github.com/friscobrisco/clawstainer).

Built in Rust. Two runtime backends:
- **systemd-nspawn** — container-based, works everywhere including macOS via Lima
- **Firecracker** — microVM-based, hardware-level isolation, requires bare metal or KVM-capable Linux. Each VMM is jailed, each VM gets its own firewalled network, and sandboxes can be paused to encrypted snapshots and resumed *(experimental — see [Firecracker runtime](#firecracker-runtime-experimental))*

## Quick Start

```bash
# Install from crates.io
cargo install clawstainer

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
cargo install clawstainer
clawstainer create --name my-box
```

On first run, Lima provisions an Ubuntu 24.04 VM with `systemd-nspawn` and builds the Linux binary automatically. This takes ~2 minutes. Subsequent runs are instant.

> **Note**: The default Lima VM runs the nspawn runtime. Firecracker needs KVM, which Lima only exposes with nested virtualization (Apple M3 or later, macOS 15 or later). That setup is used for testing; see [Firecracker runtime](#firecracker-runtime-experimental).

Manage the local Lima VM through clawstainer itself:

```bash
clawstainer vm status             # Does not start the VM
clawstainer vm start              # Creates it on first use
clawstainer vm stop               # Preserves all VM data
clawstainer vm rebuild            # Rebuilds the Linux CLI, preserves sandboxes
clawstainer vm repair             # Removes a confirmed-stale Lima PID file
clawstainer vm recreate           # Destructive; requires confirmation
```

`vm recreate` deletes every sandbox, snapshot, log, and state record stored inside the Lima VM. It refuses to proceed when sandboxes exist unless `--force` is supplied, and non-interactive use also requires `--yes`.

### Linux (bare metal or VM)

Runs natively with no VM layer. Both runtimes are available.

```bash
# nspawn runtime (default)
sudo apt-get install -y systemd-container debootstrap
cargo install clawstainer
sudo clawstainer create --name my-box

# Firecracker runtime (requires /dev/kvm and nftables)
sudo clawstainer create --name fast-box --runtime firecracker
```

### Firecracker runtime (experimental)

Firecracker sandboxes are full microVMs:

- **Jailed VMM:** each VM's Firecracker runs under the jailer in its own chroot, as its own unprivileged uid with no capabilities, with seccomp filters and a cgroup limiting memory, CPU and processes.
- **Isolated network:** each VM gets its own routed TAP and `/30`. nftables pins its MAC and IP, blocks VM-to-VM traffic, host services, private and metadata addresses, and outbound SMTP, and allows normal internet access.
- **Streaming agent:** `exec` and `shell` talk to an in-guest agent over vsock. Output streams live, `shell` gets a real PTY, and an `exec` survives a dropped connection by reattaching.
- **Pause and resume:** `pause` freezes the guest filesystem, snapshots memory, encrypts it (AES-256-GCM, bound to the CPU model, kernel and Firecracker version), and stops the VM. `resume` restores it with its processes intact, even after a host reboot.

```bash
sudo clawstainer create --runtime firecracker --memory 1024 --cpus 2
sudo clawstainer exec <id> "uname -a"
sudo clawstainer pause <id>     # frees the VM's CPU and memory
sudo clawstainer resume <id>    # processes continue where they were
```

> **Status:** validated end to end on aarch64 under nested virtualization (create, exec, shell, isolation, pause/resume, and fault tests for guest OOM, disk full, VMM crash, interrupted pause/resume, and host reboot). Not yet validated on x86 production hardware. The guest is a full Ubuntu userspace, so a cold boot takes about 12 s before the agent answers. Use nspawn (the default) for everyday local work.

`scripts/fc-smoke.sh` (lifecycle and leak checks, `--loop N` for soak runs), `scripts/fc-interrupt.sh` (kills the CLI mid-pause and mid-resume), and `scripts/fc-capacity.sh` (density and timings) exercise the runtime on a KVM host.

### Runtime Comparison

| | nspawn (default) | Firecracker |
|---|---|---|
| Isolation | Container (shared kernel) | VM (separate kernel), jailed VMM |
| Boot time | ~2-3s | ~125 ms VMM start; ~12 s until the Ubuntu guest's agent answers |
| Security | Namespace/cgroup isolation | Hardware-enforced |
| Network | Shared bridge | Per-VM routed TAP + nftables isolation |
| Pause / resume | No | Yes, to encrypted snapshots |
| macOS via Lima | Yes | Only with nested virtualization (M3+, macOS 15+) |
| Linux bare metal | Yes | Yes |
| Linux cloud VM | Yes | Needs nested virt or metal |
| `/dev/kvm` required | No | Yes |

## AI Agent Templates

Built-in provisioning templates for popular AI agents:

```bash
# Claude Code
clawstainer create --name claude-box --memory 2048 --cpus 2
clawstainer provision <id> --components claude-code

# Hermes Agent (NousResearch) — requires 4GB+ RAM
clawstainer create --name hermes-box --memory 4096 --cpus 2 --linger
clawstainer provision <id> --components hermes-agent
# Complete the credential-bearing setup interactively after provisioning
clawstainer shell <id>
hermes setup

# OpenClaw Gateway
clawstainer create --name openclaw-box --memory 2048 --cpus 2 --linger
clawstainer provision <id> --components openclaw
```

Each template includes all dependencies and has built-in timeouts. The Hermes recipe uses the official installer and leaves its interactive setup pending so reusable snapshots do not contain credentials. Use `--linger` for agents that run long-lived services (like OpenClaw and Hermes) — without it, systemd may stop user sessions on logout/idle and kill the gateway process.

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

## Contributing

clawstainer is open to contributions. If you have a bug report, feature request, or want to submit a pull request, head to the [GitHub repository](https://github.com/friscobrisco/clawstainer/issues).

## Author

Built by [@rerosillo](https://x.com/rerosillo)

## License

MIT
