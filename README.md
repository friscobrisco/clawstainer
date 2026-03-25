# clawstainer

Lightweight, isolated Linux environments for AI agents. Spin up a sandbox in seconds, run commands, install packages, tear it down — all from a single CLI.

Built in Rust. Uses `systemd-nspawn` under the hood, with an interface designed to swap in Firecracker microVMs later.

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

# Check resource usage
clawstainer stats <id>

# Tear it down
clawstainer destroy <id>
```

## Requirements

- **Linux**: `systemd-nspawn` (`apt-get install systemd-container`) and `debootstrap`
- **macOS**: Automatically managed via [Lima](https://lima-vm.io/) (`brew install lima`). The CLI proxies into a Linux VM transparently.

## Documentation

See [docs.md](docs.md) for full CLI reference, architecture, and configuration.

## License

MIT
