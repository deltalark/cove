# Cove

Self-hosted microVMs for people and coding agents. Run it on your Mac or on a Linux server, and get fast, persistent Linux VMs you can drive from a CLI, an HTTP API or MCP. It is built to work hand in hand with [Runo](https://github.com/deltalark/runo): a Runo session can run inside a VM instead of on your machine.

**Status: early.** The macOS backend, the guest agent, the API, the CLI and the MCP server are here. The Linux backend, the HTTPS proxy, sharing and pairing with Runo come next; see [the plan](docs/plan.md).

## What you get

- **Real Linux VMs** (Debian 13, you are root) that boot from a shared base image. Each VM's disk is a clone of the base, so a new VM costs almost no disk until it changes things.
- **Folders shared live** from the host into the VM (virtio-fs), so you can edit on your Mac and build in the VM, or the other way round.
- **Processes that outlive your connection.** `cove spawn` starts a named process in the VM; disconnect, come back later, and `cove attach` replays what you missed and keeps streaming. Runo uses this so agents keep working while the laptop sleeps.
- **Ports forwarded** from the VM to your machine with one command.
- **An MCP server** so any agent can create VMs, run commands, move files and share ports.
- **Isolation:** the VM only sees the folders you share and the credentials you pass it. Commands an agent runs there can't touch your home folder, keychain or SSH keys.

## Quick start (macOS, Apple Silicon)

```sh
brew install vfkit                      # the Virtualization.framework launcher
cargo build --release
scripts/build-guest.sh                  # the agent that runs inside VMs (needs zig + cargo-zigbuild)
target/release/cove serve &           # the server; data lives in ~/.cove
target/release/cove doctor
target/release/cove image pull debian-13
target/release/cove up dev --mount ~/code/myapp:/work
target/release/cove shell dev --cwd /work
```

`scripts/smoke-vm.sh` boots a VM and checks exec, shared folders, networking, ports and restarts.

## CLI

```
cove serve [--listen 127.0.0.1:7457]
cove doctor | key
cove image ls | pull <image>
cove up <name> [--cpus 2] [--memory 4096] [--disk 20] [--mount HOST:GUEST[:ro]]... [--ephemeral] [--tag T]...
cove ls | show <name> | start <name> | stop <name> | rm <name> | copy <from> <to>
cove exec <name> [-t] [--cwd DIR] [-e K=V]... -- <command...>
cove shell <name> [--cwd DIR]
cove spawn <name> <proc> [-t] [-a] -- <command...>
cove attach <name> <proc> [--from BYTES]
cove procs <name> | kill <name> <proc>
cove push <name> <local> <remote> | pull <name> <remote> <local>
cove ports <name> | forward <name> <port>
cove mcp
```

Add `--json` to any command for machine-readable output, and `--host URL` to talk to another server.

## API

Every request carries `Authorization: Bearer <key>`; the key is created on first start in `~/.cove/api-key` (`cove key` prints it).

| Method | Path | |
| --- | --- | --- |
| GET | `/v1/health` | no auth |
| GET | `/v1/images` | |
| POST | `/v1/images/{name}/pull` | |
| GET, POST | `/v1/vms` | POST takes `name, image, cpus, memory_mb, disk_gb, mounts, ephemeral, tags, start` |
| GET, DELETE | `/v1/vms/{name}` | |
| POST | `/v1/vms/{name}/start`, `/stop`, `/copy` | |
| POST | `/v1/vms/{name}/exec` | runs to completion, returns `code, stdout, stderr` |
| GET (WebSocket) | `/v1/vms/{name}/stream` | first message: an `exec`, `spawn` or `attach` request; then frames out, input in |
| GET | `/v1/vms/{name}/procs` | |
| DELETE | `/v1/vms/{name}/procs/{proc}` | `?signal=` |
| GET, PUT | `/v1/vms/{name}/files?path=` | raw bytes; PUT takes `&mode=755` |
| GET | `/v1/vms/{name}/dir?path=` | |
| GET | `/v1/vms/{name}/ports` | listening ports and forwards |
| POST, DELETE | `/v1/vms/{name}/forwards`, `/forwards/{port}` | |

The stream's frames and requests are the types in [`crates/proto`](crates/proto/src/lib.rs).

## MCP

```json
{ "mcpServers": { "Cove": { "command": "cove", "args": ["mcp"] } } }
```

Tools: `vm_list`, `vm_create`, `vm_exec`, `vm_read_file`, `vm_write_file`, `vm_share_port`, `vm_destroy`.

## How it works

```
 cove CLI ─┐
 Runo ───────┼─ HTTP/WebSocket ─▶ cove serve ──▶ vfkit (Virtualization.framework) ──▶ VM
 MCP agents ─┘                        │                                                  │
                                      └──────────── vsock ──────────▶ cove-guest ◀───┘
```

- `cove serve` keeps VM records in `~/.cove/vms/<name>/`, clones the base disk for each VM, and builds a small cloud-init seed disk on every start that installs the guest agent.
- `cove-guest` is a static Linux binary inside the VM. It listens on vsock port 1024 and runs commands, terminals, file operations and port pipes, one request per connection.
- VMs run as their own processes, so restarting the server doesn't stop them; it finds them again from `run.json`.

## Development

```sh
cargo build --workspace && cargo test --workspace
```

The end-to-end tests use the `host` backend, which runs the guest agent as a normal process on your machine with **no isolation**. It is only allowed when the server runs with `COVE_ALLOW_HOST_BACKEND=1`, and exists so the whole stack can be tested without a hypervisor.

## License

MIT
