# clawstainer — AI Agent Skills Reference

> Version 0.1.0

Machine-readable reference for AI agents using clawstainer sandboxes. All commands default to `--format auto`: JSON when piped (typical agent usage), table in interactive terminals. Pass `--format json` explicitly to guarantee JSON output. Errors go to stderr as JSON with a non-zero exit code.

---

## Lifecycle

### Create a sandbox

```bash
clawstainer create [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--name <NAME>` | random | Human-readable name |
| `--memory <MB>` | `512` | Memory limit in MB |
| `--cpus <N>` | `1` | CPU cores |
| `--network <MODE>` | `nat` | `nat` (internet access) or `none` (isolated) |
| `--timeout <SEC>` | `0` | Auto-destroy after N seconds (0 = never) |
| `--runtime <RT>` | `nspawn` | `nspawn` or `firecracker` |
| `--security <PROF>` | `strict` | `strict` (drops dangerous caps) or `standard` |
| `--cap-add <CAP,...>` | — | Add capabilities back |
| `--cap-drop <CAP,...>` | — | Drop additional capabilities |
| `--env-file <PATH>` | — | Inject KEY=VAL pairs from file |
| `--linger` | `false` | Enable systemd lingering (keeps agent services alive after logout). Recommended for OpenClaw, Hermes, and other long-lived agents |
| `--from <SNAPSHOT>` | — | Create from a named snapshot |
| `--format <FMT>` | `auto` | `auto` (json when piped), `table`, or `json` |

**Returns (JSON):**
```json
{
  "id": "sb-a1b2c3d4",
  "name": "bold-parrot",
  "status": "running",
  "ip": "10.0.0.2",
  "created_at": "2026-03-27T10:30:00Z"
}
```

The `id` field is used in all subsequent commands.

### Destroy a sandbox

```bash
clawstainer destroy <MACHINE_ID> [--format json]
clawstainer destroy --all [--format json]
```

**Returns (JSON):**
```json
{
  "machine_id": "sb-a1b2c3d4",
  "status": "destroyed",
  "uptime_seconds": 3600
}
```

---

## Execute Commands

This is the primary agent interface. Run any shell command inside a sandbox.

```bash
clawstainer exec <MACHINE_ID> <COMMAND> [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--timeout <SEC>` | `30` | Kill after N seconds |
| `--workdir <PATH>` | `/root` | Working directory |
| `--env <KEY=VAL>` | — | Set env var (repeatable) |
| `--user <USER>` | `root` | Run as user |
| `--format <FMT>` | `auto` | `auto` (json when piped), `table`, or `json` |

**Returns (JSON):**
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

**Optional fields (present when applicable):**

| Field | Condition | Description |
|-------|-----------|-------------|
| `truncated` | stdout > 1MB | Output was truncated |
| `total_bytes` | when truncated | Original size in bytes |
| `peak_memory_bytes` | nspawn only | Peak memory usage (bytes) |
| `cpu_time_us` | nspawn only | Total CPU time (microseconds) |

**Default environment inside sandbox:**

| Variable | Value |
|----------|-------|
| `HOME` | `/root` (or `/home/<user>`) |
| `USER` | the `--user` value |
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `LANG` | `C.UTF-8` |
| `TERM` | `xterm-256color` |

**Examples:**
```bash
clawstainer exec sb-xxx "echo hello"
clawstainer exec sb-xxx --timeout 300 "apt-get update && apt-get install -y python3"
clawstainer exec sb-xxx --env API_KEY=secret "python3 app.py"
clawstainer exec sb-xxx --workdir /tmp "ls -la"
```

---

## Copy Files

Copy files and directories between the host and a sandbox.

```bash
clawstainer cp <SRC> <DST>
```

Use `MACHINE_ID:/path` for sandbox paths. Plain paths are host paths.

| Direction | Example |
|-----------|---------|
| Pull (sandbox to host) | `clawstainer cp sb-xxx:/root/output.txt ./local/` |
| Push (host to sandbox) | `clawstainer cp ./input.txt sb-xxx:/root/` |

Pass `--format json` to guarantee JSON output.

**Returns (JSON):**
```json
{
  "machine_id": "sb-a1b2c3d4",
  "direction": "pull",
  "src": "/root/output.txt",
  "dst": "./local/"
}
```

Handles files and directories recursively. Cannot copy between two sandboxes directly.

---

## Provision

Install predefined software components.

```bash
clawstainer provision <MACHINE_ID> --components <LIST> [--timeout <SEC>] [--format json]
```

Progress is printed to stderr per component. Pass `--format json` to guarantee JSON output.

**Returns (JSON):**
```json
{
  "machine_id": "sb-a1b2c3d4",
  "results": [
    {"component": "python3", "status": "ok", "duration_ms": 88300},
    {"component": "git", "status": "ok", "duration_ms": 15394}
  ]
}
```

**Available components:** `python3`, `nodejs`, `git`, `curl`, `build-essential`, `docker-cli`, `ripgrep`, `jq`, `claude-code`, `hermes-agent`, `openclaw`

**Bundles:** `agent-default` (python3, nodejs, git, curl, jq, ripgrep), `web-dev`, `ml`, `openclaw`

Each component that fails does NOT block others. Results are reported individually.

---

## Snapshots

Capture a provisioned sandbox for reuse. Avoids re-provisioning on every create.

### Create snapshot
```bash
clawstainer snapshot create <MACHINE_ID> --name <NAME>
```

**Returns:**
```json
{
  "name": "python3-ready",
  "size_bytes": 157286400,
  "created_at": "2026-03-27T10:30:00Z"
}
```

### List snapshots
```bash
clawstainer snapshot list [--format json]
```

### Delete snapshot
```bash
clawstainer snapshot delete <NAME>
```

### Use snapshot
```bash
clawstainer create --name worker --from python3-ready
```

**Typical workflow:**
```bash
# One-time setup
clawstainer create --name base
clawstainer provision sb-xxx --components python3,git
clawstainer snapshot create sb-xxx --name python3-git
clawstainer destroy sb-xxx

# Reuse (instant, no provisioning)
clawstainer create --name worker-1 --from python3-git
clawstainer create --name worker-2 --from python3-git
```

---

## Monitoring

### List sandboxes
```bash
clawstainer list [--format json] [--status running|stopped|all] [--watch <SEC>]
```

### Stats
```bash
clawstainer stats                          # Global overview
clawstainer stats <MACHINE_ID>             # Per-sandbox details
clawstainer stats --format json            # JSON output
```

### Exec logs
```bash
clawstainer logs <MACHINE_ID> [--last <N>] [--format json]
```

---

## Fleet Management

Deploy multiple sandboxes from YAML.

```bash
clawstainer fleet create --file fleet.yaml [--parallel <N>] [--format json]
clawstainer fleet destroy --all [--format json]
clawstainer fleet destroy --name <GROUP> [--format json]
```

**fleet.yaml format:**
```yaml
machines:
  - name: worker
    count: 5
    memory: 1024
    cpus: 2
    provision: python3
    security: strict        # optional
    cap_add: []             # optional
    cap_drop: [CAP_NET_RAW] # optional
    env_file: .env          # optional
```

---

## Other

### Port forwarding
```bash
clawstainer port-forward <MACHINE_ID> <HOST_PORT:SANDBOX_PORT>
```

### Interactive shell (human use)
```bash
clawstainer shell <MACHINE_ID> [--user <USER>]
```

---

## Error Codes

All errors return JSON to stderr:
```json
{"error": "machine_not_found", "message": "No machine with ID 'sb-xyz'", "hint": "Run 'clawstainer list' to see active machines"}
```

| Code | Error | Meaning |
|------|-------|---------|
| 0 | — | Success |
| 1 | `internal_error` | Unexpected error |
| 2 | `machine_not_found` | Invalid machine ID |
| 3 | `machine_not_running` | Machine is stopped |
| 4 | `create_failed` | Sandbox creation failed |
| 5 | `exec_timeout` | Command exceeded timeout |
| 6 | `exec_failed` | Command execution failed |
| 7 | `provision_failed` | Component install failed |
| 8 | `runtime_unavailable` | Linux/nspawn not available |
| 9 | `resource_limit` | Host resources exhausted |
| 10 | `permission_denied` | Needs root |
| 11 | `copy_failed` | File copy failed |
| 12 | `snapshot_failed` | Snapshot operation failed |

---

## Agent Workflow Pattern

```
1. create          → get machine ID
2. provision       → install dependencies (or use --from snapshot)
3. cp (push)       → send input files
4. exec            → run commands, read stdout/stderr/exit_code
5. exec            → iterate: write code, run tests, fix errors
6. cp (pull)       → retrieve output files
7. destroy         → clean up
```

For pipelines with repeated setups, snapshot after step 2 and use `--from` on subsequent runs.
