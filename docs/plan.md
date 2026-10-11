# Cove: plan

Status: proposal, waiting for Anish's go. Nothing built yet.
Revision 2: reworked after Anish said Cove is its own product, like [exe.dev](https://exe.dev), that people run on their own machine or on a VPS, and that it must work seamlessly with Runo.
Based on: `deltalark/cove` (empty, no commits yet), Runo's `native/runo-app/src` (`agent.rs`, `pty.rs`, `browser.rs`, `memory_mcp.rs`, `git.rs`) at `5adcbfc`, and exe.dev's public docs (home, docs index, proxy, GUTS pages).

## The short version

- **Cove is a self-hosted exe.dev.** Install one binary on a Mac or a Linux VPS and you get fast, persistent Linux VMs that people and agents can create over SSH, an HTTP API or MCP, each with its own HTTPS URL, private by default and shareable.
- **One server, two places it runs:**
  - **Local (macOS):** Apple Virtualization.framework, URLs like `https://myapp.runo.localhost`.
  - **VPS or bare metal (Linux with KVM):** cloud-hypervisor, URLs like `https://myapp.vm.example.com` with automatic TLS.
- **Runo treats each Cove server as a "Machine".** In New session you pick where the session runs: This Mac, Local VM, or `my-vps`. Everything in Runo (chat, CLI view, terminal, Changes, diffs, commit and PR, browser) works the same whichever you pick. That is the "seamless" part, and it is most of Runo's side of the work.
- **The big win on a VPS:** agents keep working when the laptop sleeps or closes. Runo reattaches and catches up when it reconnects.
- **Build order:** server core and macOS backend first, then the Runo "Machine" integration locally, then the Linux/VPS backend with the HTTPS proxy and auth, then sharing, MCP, snapshots and the web dashboard.

---

## 1. What we take from exe.dev, and what we change

| exe.dev | Cove |
| --- | --- |
| Hosted cloud only | **Self-hosted**: your Mac, your VPS, your server. A hosted version could come later. |
| `ssh exe.dev` front door: create, list, share VMs over SSH | Same idea: `ssh <your-host>` (default port 2222 on a VPS) gives a small command shell: `new`, `ls`, `rm`, `share`, `cp`, `restart`. `ssh <vm>@<your-host>` drops you into that VM as root. |
| `https://<vm>.exe.xyz`, TLS handled for you, private by default, `share set-public`, ports 3000–9999 forwarded | Same model: `https://<vm>.<your-domain>`, auto TLS, private by default (login required), `share set-public`, `share port`, extra ports as `https://<vm>.<domain>:<port>` or `https://<port>-<vm>.<domain>`. |
| Persistent VMs, root, apt, systemd; fast boot | Same. Full Debian/Ubuntu VM, systemd, root, persistent disk. Throwaway VMs are a flag (`--ephemeral`). |
| Pool of CPU and memory shared by your VMs | Same: the server's limits are a pool; each VM has a soft share and a hard cap. |
| HTTP API: list, create, inspect, share, resize, tag, copy VM, `POST /exec` | Same verbs, plus `exec` streaming with PTY over WebSocket, which Runo needs. |
| Built-in agent (Shelley) | **No agent of our own.** Runo is the agent app; any agent CLI also works over SSH or MCP. |
| Copy a VM (disk and config) | Copy and snapshot, using reflinks so a copy takes about a second. |

## 2. The product

### Pieces (one Rust workspace, one main binary)

| Piece | What it does |
| --- | --- |
| `cove serve` | The server. Manages VMs, disks, images, networking, the HTTPS proxy, the SSH front door, the API and auth. Runs as a launchd agent on macOS and a systemd service on Linux. |
| `cove` CLI | Client for local or remote servers (`cove --host my-vps ls`). Same verbs as the SSH shell. |
| `cove mcp` | MCP server over stdio for agents, pointed at a server with an API key. |
| `cove-guest` | Small static binary inside every VM: exec, PTY, files, port discovery, health, log capture, and a supervisor for long-running agent processes (§4). Talks to the host over vsock. |
| Web dashboard (later) | A small page at `https://<your-domain>` listing VMs, sharing and usage. |

### Backends
| Where | Backend | Notes |
| --- | --- | --- |
| macOS 13+ on Apple Silicon | Apple Virtualization.framework (`objc2-virtualization`) | Needs the `com.apple.security.virtualization` entitlement, so release builds are signed. |
| Linux with `/dev/kvm` (VPS with nested virtualization, or bare metal) | cloud-hypervisor | Fast boot, virtio-fs, vsock, snapshots, CPU and memory hotplug. Firecracker would also work on a VPS, but cloud-hypervisor covers local folder sharing and full VMs with one backend. |
| Linux without KVM | Not supported in v1. `cove doctor` says so plainly. | Many cheap VPS plans (Hetzner Cloud, most shared plans) don't expose KVM; bare-metal hosts (Hetzner Robot, OVH, Latitude) and cloud VMs with nested virtualization (GCP, Azure, some DigitalOcean) do. A gVisor fallback could come later. |

### Storage
- Each VM has a persistent raw disk, created as a reflink copy of the base image on btrfs or XFS (APFS clones on macOS), so new VMs and VM copies take about a second and only use space for changes.
- Snapshots are reflink copies too. Backups to S3-compatible storage come later.

### Networking and URLs
- **HTTPS proxy** built into the server. On a VPS it gets a wildcard certificate from Let's Encrypt via DNS-01 (Cloudflare, Route 53 and similar DNS APIs), or per-host certificates via HTTP-01 if no DNS API is available. Locally it uses `*.runo.localhost` with a locally trusted CA, installed once with the user's consent.
- **Private by default.** Visitors must sign in (passkey or a magic link on a VPS; nothing to do locally). `share set-public` opens one port publicly; `share add <email>` grants access to a person.
- **Egress policy per VM:** full (default for interactive VMs), allowlist (default for agent sessions), or off. The allowlist is enforced by a host-side proxy.
- **SSH front door** on its own port, using the user's registered SSH keys.

### Auth
- **Users and SSH keys:** single-user by default. Teams come later.
- **API keys** for the CLI, MCP and Runo, scoped to read, write or admin, and optionally limited to tagged VMs.
- **Pairing with Runo:** `cove pair` on the server prints a one-time code. Runo's "Add machine" takes the host and the code, and gets a long-lived key, stored in the macOS keychain.

### API (HTTP + WebSocket, JSON)
```
POST   /v1/vms                 {name, image, cpus, memory, disk, ephemeral, tags, egress}
GET    /v1/vms | /v1/vms/{id}
POST   /v1/vms/{id}/start|stop|restart|resize|copy|snapshot|restore
DELETE /v1/vms/{id}
POST   /v1/vms/{id}/exec       {cmd, cwd, env}             -> exit code, stdout, stderr
WS     /v1/vms/{id}/exec       {cmd, cwd, env, pty, rows, cols}   streaming, resizable
WS     /v1/vms/{id}/procs/{name}/attach   reattach to a supervised process (§4)
GET    /v1/vms/{id}/ports      ports listening in the guest, with their URLs
POST   /v1/vms/{id}/share      {port, public, users}
PUT/GET /v1/vms/{id}/files?path=
GET    /v1/vms/{id}/events     SSE: state, ports opened, files changed
```
The CLI, MCP server, SSH shell and Runo all use this API.

### MCP tools for agents
`vm_create`, `vm_exec`, `vm_read_file`, `vm_write_file`, `vm_share_port` (returns the URL), `vm_destroy`. Kept short so tool descriptions don't eat context. Any agent with an API key can then spin up boxes, deploy a preview and hand back a link.

## 3. How Runo integrates (the "seamless" part)

### What the user sees
- **Settings › Machines:** "This Mac" is always there. Runo finds a local Cove server automatically. "Add machine" pairs a VPS with a host and a code. Each machine shows status, CPU, memory and disk.
- **New session popup:** a "Runs on" picker: This Mac, Local VM, `my-vps` … The last choice is remembered per project. Hidden when no Cove machine exists.
- **Sidebar and tabs:** a small machine label on sessions that don't run on this Mac.
- **Session header:** a "VM" menu with Open shell, Open preview URL, Share preview, Restart VM, Copy VM.
- **Everything else is identical.** Chat and CLI views, terminal tabs, Changes, diff notes, commit, push and PR, history, the Browser tab and the planned agent browser all work in VM sessions.

### What Runo changes underneath: one "Machine" layer
Today Runo calls `Command::new` directly in `agent.rs`, `git.rs`, `pty.rs`, `github.rs` and `projects.rs`, and reads files from disk in `fileview.rs` and `history.rs`. The core of the integration is putting those behind one trait:

```rust
trait Machine {
    fn exec(&self, cmd, cwd, env) -> Output;            // git status, git diff, gh …
    fn spawn(&self, cmd, cwd, env) -> Lines;           // agent runs (stream-json)
    fn spawn_pty(&self, cmd, cwd, env, size) -> Pty;   // CLI view, terminal tabs
    fn read_file / write_file / list_dir / watch;      // file viewer, Changes refresh
    fn url_for_port(&self, port) -> Url;               // browser, preview links
}
```
`LocalMachine` is today's behaviour, unchanged. `VmMachine` calls the Cove API. Every feature then works on every machine without per-feature work, and the same layer is what Runo's README calls "remote machines".

### Where the code lives for each kind of machine
| | Local VM (same Mac) | Remote VM (VPS) |
| --- | --- | --- |
| Worktree | Created on the Mac as today, shared into the VM live with virtio-fs. | Lives inside the VM. Runo clones the repo there (over the VM's own network) and creates the worktree inside it. |
| Git, diffs, Changes | Run on the Mac, as today. | Run in the VM through `Machine::exec`. Output is the same, so the UI code doesn't change. |
| Commit, push, PR | On the Mac with the Mac's credentials. | In the VM, with a GitHub token scoped to that repo that Runo passes in only for the push, or by fetching the branch back to the Mac and pushing from there (the user picks once). |
| Open in editor | Opens the local folder. | Opens VS Code or Cursor Remote-SSH to `ssh <vm>@<host>`. |

### Agent runs that survive the laptop closing
- In VM sessions, Runo starts the agent CLI under `cove-guest`'s supervisor (`/v1/vms/{id}/procs/{session}`) instead of as a child process of Runo.
- The supervisor keeps the process running and stores its output.
- When Runo reconnects, it reattaches, replays what it missed into the conversation, and keeps streaming. The CLI view reattaches to the same PTY, like tmux.
- Runo's notifications fire for turns that finished while it was away, using the unread and needs-attention indicators it already has.

### Agent sign-in inside VMs
The VM never sees the Mac's home folder or keychain:
- **Claude Code:** a long-lived token from `claude setup-token` passed as `CLAUDE_CODE_OAUTH_TOKEN`, or `ANTHROPIC_API_KEY`.
- **Codex:** `~/.codex/auth.json` copied in, or `OPENAI_API_KEY`.
- **Other agents:** their API key env vars.

Runo stores these in the keychain, writes them into the VM's environment at start, and shows a one-time "Sign this agent in for VMs" card the first time. Agent CLIs are installed in the VM on first use, at the same version as on the Mac. Each CLI's token behaviour still needs checking before building.

### Ports, preview and the browser
- `cove-guest` reports every port the guest starts listening on.
- Runo's Browser tab lists them with their real URLs (`https://<vm>.<domain>:5173`) instead of scanning fixed localhost ports, and `localhost:5173` links in agent output are rewritten to those URLs.
- "Share preview" makes one port public and copies the link, which is handy for showing work to someone.
- The planned agent browser (headless Chromium on the Mac) opens these URLs with the user's Cove session cookie.

### Runo's own MCP servers
`runo_memory` (and later `runo_browser`) run on the Mac. For VM sessions, Cove bridges them into the guest over the API, so the agent inside the VM sees the same tools.

## 4. Security model

- **Isolation boundary is the VM.** Agents run as root inside it and can do anything there; nothing reaches the host or other VMs except through the API.
- **What a VM gets:** its own disk, the session worktree (local VMs only, via virtio-fs), the credentials Runo passes explicitly, and the egress its policy allows. For local VMs the repo's shared `.git` is mounted read-only with a writable overlay for this worktree, and Runo runs host git with hooks and `core.fsmonitor` disabled for VM sessions so nothing written in the VM runs on the Mac.
- **What it never gets:** the host's home folder, keychain, SSH agent, other VMs' disks, the server's API keys.
- **Network:** VM-to-VM traffic is blocked unless VMs share a tag. Inbound only through the HTTPS proxy and the SSH front door. The proxy strips Cove auth cookies before forwarding.
- **Server exposure on a VPS:** only 443 (HTTPS) and the SSH front-door port are open. Admin actions need an admin API key or a signed-in admin. Login is rate-limited.
- **Limits:** CPU, memory, disk and process caps per VM, and an idle-stop timer for agent VMs.
- **Audit log:** every create, exec, share and login, viewable per VM in Runo and the dashboard.

## 5. Repo layout

```
Cove/
  Cargo.toml                   workspace
  crates/
    core/                      VM model, pool, disks, images, tags, events
    backend-vz/                macOS Virtualization.framework
    backend-chv/               Linux cloud-hypervisor
    proto/                     host <-> guest vsock protocol
    guest/                     cove-guest (static musl)
    proxy/                     HTTPS proxy, ACME, auth cookies, sharing
    sshd/                      SSH front door (russh)
    api/                       HTTP + WebSocket API, API keys
    cli/                       cove binary: serve, CLI verbs, mcp, pair, doctor
  image/                       base image recipes (Debian and Ubuntu, arm64 and x86_64), built in CI
  deploy/                      install.sh for VPS, systemd unit, launchd plist, Homebrew formula
  docs/
```

Runo talks to it only over the API (a small client module plus the `Machine` trait), so the two release on their own schedules.

## 6. Install

- **Mac:** `brew install deltalark/tap/Cove`, or Runo offers to install it from Settings › Machines. Runs as a launchd agent.
- **VPS:** `curl -fsSL https://…/install.sh | sh` then `cove setup --domain vm.example.com` (asks for a DNS API token or uses HTTP-01), then `cove pair` to connect Runo.
- `cove doctor` checks KVM or the entitlement, disk filesystem for reflinks, DNS and TLS, and open ports.

## 7. Build order

1. **Core and image.** Workspace, CI, base image, `cove-guest`, vsock protocol, `cove doctor`.
2. **macOS backend and API.** Boot, exec (plain and PTY), files, ports, persistent disks with APFS clones, `serve` with the HTTP and WebSocket API, the CLI.
3. **Runo Machine layer, local VMs.** The `Machine` trait with `LocalMachine` (no behaviour change), `VmMachine` over the API, "Runs on: Local VM" in New session, agent sign-in card, supervised agent runs, Browser tab ports. *First point where a real Runo session runs in a VM.*
4. **Linux/VPS backend.** cloud-hypervisor, reflink disks, systemd install, `install.sh`, `cove setup`.
5. **HTTPS proxy, auth and pairing.** ACME, private-by-default proxy, passkey or magic-link login, API keys, `cove pair`, Settings › Machines with remote machines. *Runo sessions on a VPS.*
6. **Remote worktrees and reattach.** Clone and worktree inside the VM, git through `Machine::exec`, push options, reattach and catch-up after reconnect, Remote-SSH open-in-editor.
7. **SSH front door, sharing and MCP.** `ssh <host>` shell, `ssh <vm>@<host>`, `share` commands, `cove mcp`, MCP bridge for `runo_memory`.
8. **Snapshots, copy, egress allowlist, dashboard.**
9. **Later:** teams, backups to S3, a gVisor fallback for VPSes without KVM, Windows hosts, a hosted service.

## 8. Open choices (defaults picked; say if you want otherwise)

- **Pushing from a remote VM.** Default: fetch the branch back to the Mac and push from there, so GitHub credentials never enter the VM. Alternative: pass a repo-scoped token into the VM.
- **Default "Runs on".** Default: This Mac, with the last choice remembered per project.
- **Login on a VPS.** Default: passkeys, with magic links as a backup. Email delivery needs an SMTP setting.
- **License.** Default: MIT, like Runo. A hosted version later could have its own terms.
- **Name and domain for local URLs.** Default: `*.runo.localhost`.

## 9. Recommendation

Build Cove as a self-hosted exe.dev: one Rust server that runs the same way on a Mac (Virtualization.framework) and on a KVM-capable VPS (cloud-hypervisor), with SSH, an HTTP API and MCP as its three front doors, and a private-by-default HTTPS proxy. Make Runo seamless by adding one `Machine` layer that every feature goes through, so a session on a VPS behaves exactly like one on the Mac and keeps running when the laptop is closed. Start with steps 1–3, which give a working Runo session in a local VM, then steps 4–6 for the VPS. That is where the product becomes clearly more than a sandbox.
