<div align="center">

<a href="#get-started">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/hero-dark.svg">
    <img alt="Cove: your own cloud computers for coding agents. A terminal starts a VM with an allowlist and a secret, lists signed-in agents, runs Claude Code in the VM and pauses it to disk." src="docs/assets/hero-light.svg" width="100%">
  </picture>
</a>

<br/>

**Real Linux VMs that boot in seconds, pause to disk, snapshot while running and keep your keys out of reach.**<br/>
On your Mac, your server or a $10 VPS. Driven from a CLI, an HTTP API, MCP or [Runo](https://github.com/deltalark/runo).

[![CI](https://github.com/deltalark/cove/actions/workflows/ci.yml/badge.svg)](https://github.com/deltalark/cove/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/built%20with-Rust-b7410e?logo=rust&logoColor=white)
![macOS · Apple Silicon](https://img.shields.io/badge/macOS-Apple%20Silicon-lightgrey?logo=apple)
![Linux · KVM or gVisor](https://img.shields.io/badge/Linux-KVM%20or%20gVisor-lightgrey?logo=linux&logoColor=white)
![Status: early](https://img.shields.io/badge/status-early-orange)

[**Get started**](#get-started) · [Use it from your agent](#use-it-from-your-agent) · [Run it on a server](#run-it-on-a-server) · [Tour](#a-quick-tour) · [Secrets](#secrets-and-the-allowlist) · [Reference](#reference) · [FAQ](#faq)

</div>

<br/>

Coding agents are most useful when you let them run anything, and most dangerous for the same reason. Cove gives every agent, and every person, a real Linux machine of its own: it boots in seconds, keeps its state, sees only the folders you share, reaches only the hosts you allow, and never holds your API keys.

## Get started

**1. Install.** One line, on a Mac with Apple Silicon or a Linux box:

```sh
curl -fsSL https://raw.githubusercontent.com/deltalark/cove/main/scripts/install.sh | sh        # Mac
curl -fsSL https://raw.githubusercontent.com/deltalark/cove/main/scripts/install.sh | sudo sh   # Linux server or VPS
```

<details>
<summary><b>What the install script does, and its options</b></summary>

- Puts `cove` and the guest agent in `~/.cove/bin`, from the latest release, or builds them from source with cargo when no release fits. Building from source needs [Rust](https://rustup.rs); the script fetches the rest for the guest agent (zig and cargo-zigbuild on a Mac, musl-tools on Linux).
- Links `cove` into `/opt/homebrew/bin` or `/usr/local/bin`, so apps such as Runo find it.
- **Mac:** installs vfkit with Homebrew if it is missing, and starts `cove serve` at login with a launchd agent.
- **Linux with KVM:** installs cloud-hypervisor (the official static build), and virtiofsd, iptables and iproute2 from your distribution.
- **Linux without KVM:** asks to install gVisor (the official release, checksum checked). Say no and you get just the CLI, to drive a Cove server elsewhere.
- On Linux, installs a systemd service for `cove serve`, runs `cove doctor` and pulls the Debian 13 image. gVisor's image comes from Docker Hub, or Google's mirror of it when Docker Hub's pull limit refuses a shared VPS address, with every layer checked against its digest.

Running it again upgrades in place; your VMs, images and keys in `~/.cove` are left alone. Options go after `sh -s --`, for example `| sh -s -- --no-service`: `--prefix DIR`, `--version TAG`, `--listen ADDR`, `--no-service`, `--no-image`, `--no-link`, `--no-deps`, `--gvisor` / `--no-gvisor`, `--from-source`, `--from-file TARBALL`.

**Uninstall** with `--uninstall`. It stops the service and removes the binaries and keeps `~/.cove`; add `--purge` to stop every VM and delete `~/.cove` too.

</details>

**2. Open a shell in a fresh VM.** The first time makes a Debian 13 VM called `dev`; after that it drops you straight back in.

```console
$ cove sh
root@dev:~# cargo test
```

**3. Hand it to your agent.** One line, and Claude Code can make its own VMs, run code in them, move files and share ports:

```sh
claude mcp add -s user cove -- cove mcp
```

That's it. `cove doctor` checks the machine if anything looks off, and [the rest of the tour](#a-quick-tour) shows pausing, snapshots and secrets.

## Why Cove

<table>
<tr>
<td width="50%" valign="top">

### 🖥️ Real VMs, not containers
Debian 13 with its own kernel, and you are root. Apple's Virtualization.framework on a Mac, cloud-hypervisor on Linux with KVM, gVisor on a VPS without it.

</td>
<td width="50%" valign="top">

### ⏸️ Pause to disk, wake on use
A paused VM keeps its memory and running programs, and on Linux it uses no RAM at all. Idle VMs pause themselves, and the next command or port visit wakes them in about a second.

</td>
</tr>
<tr>
<td valign="top">

### 📸 Snapshots
Save a VM while it runs and start new ones from it. Set a project up once with `cove snapshot build`, and every VM after that starts ready.

</td>
<td valign="top">

### 🔑 Keys never enter the VM
Inside, your API keys are placeholders. Cove's proxy swaps in the real values only on HTTPS requests to the hosts each key belongs to.

</td>
</tr>
<tr>
<td valign="top">

### 🧱 Only the hosts you allow
`--network allowlist` gives a VM no network device at all: it reaches only its allowlist, through Cove's proxy, and every attempt is logged.

</td>
<td valign="top">

### 🌙 Work that outlives your laptop
`cove spawn` starts a named process that keeps running when you disconnect. `cove attach` replays what you missed and follows on.

</td>
</tr>
<tr>
<td valign="top">

### 🤖 Built for agents
An MCP server lets any agent make its own VMs, run commands and background jobs, move files, share ports, pause and snapshot. Sign an agent in once per server, and every VM is signed in.

</td>
<td valign="top">

### 🏠 One binary, your hardware
One install line on a Mac or Linux box. No account, no per-second bill. Pair Runo or another computer with a one-time code.

</td>
</tr>
</table>

### Cove next to a container

| | Cove | A Docker container |
| --- | --- | --- |
| Kernel | Its own (or gVisor's, without KVM) | Shares the host's |
| Pause | Memory and processes kept, written to disk on Linux | `docker pause` freezes it, still in RAM |
| Keep a running machine as an image | `cove snapshot save`, processes included | `docker commit` saves files only |
| API keys | Placeholders inside; real values added on the way out | Whatever you pass in, readable inside |
| Network limits | Per-VM allowlist with a log, built in | Up to you |
| Agent tools | `cove mcp`, ready for any MCP agent | Up to you |

## Use it from your agent

`cove mcp` gives an agent tools to make VMs, run commands and background jobs, read and write files, share ports, pause and snapshot. Register it with every agent on this computer in one go:

```sh
cove mcp install            # or name some: cove mcp install claude codex
```

It finds the agents that are installed, adds Cove the way each one expects, and leaves their other settings alone. `--print` shows what it would do, and `cove mcp uninstall` takes it out again. By hand, it's one line per agent:

| Agent | Command |
| --- | --- |
| **Claude Code** | `claude mcp add -s user cove -- cove mcp` |
| **Codex** | `codex mcp add cove -- cove mcp` |
| **Gemini CLI** | `gemini mcp add -s user cove cove mcp` |
| **Qwen Code** | `qwen mcp add -s user cove cove mcp` |
| **OpenCode** | in `~/.config/opencode/opencode.json`: `"mcp": { "cove": { "type": "local", "command": ["cove", "mcp"] } }` |
| **Cursor**, and any other MCP client | in its MCP config (`~/.cursor/mcp.json` for Cursor): `"mcpServers": { "cove": { "command": "cove", "args": ["mcp"] } }` |
| **[Runo](https://github.com/deltalark/runo)** | Built in: [see below](#cove-and-runo) |

Then just ask: *"Make a Cove VM with the network off and run this script in it"*, *"Try upgrading to React 19 in a scratch VM and tell me what breaks"*, *"Start the integration tests in a VM and check back on them later."*

The tools act on whichever server `cove` talks to: the one on this machine, or [a server you logged in to](#run-it-on-a-server). On purpose, they can't give a VM secrets or widen its allowlist, so an agent can't loosen its own sandbox.

## Run it on a server

For VMs that keep working while your laptop sleeps, and for Linux machines with more cores than your laptop.

**1. On the server**, install Cove with HTTPS on, and make a one-time pairing line:

```console
$ curl -fsSL https://raw.githubusercontent.com/deltalark/cove/main/scripts/install.sh | sudo sh -s -- --https 0.0.0.0:7443
$ cove pair
Pair with:  cove://203.0.113.7:7443/4F7K-Q2XM?fp=sha256:9f2c41ab…
Works once, for 10 minutes.
```

The server makes a certificate of its own, and the pairing line carries its fingerprint. The computer that pairs trusts exactly that certificate, the way SSH trusts a host key, so there's no domain to buy, no certificate authority and no warning to click through. If other computers reach the server by another name or address, add `--public-host`.

**2. Log in from your computer** with that line:

```sh
cove login 'cove://203.0.113.7:7443/4F7K-Q2XM?fp=sha256:9f2c41ab…'
cove ls                       # every cove command, and cove mcp, now uses that server
```

Each computer gets an API key of its own, which `cove keys rm` on the server revokes. Ten wrong pairing codes in a row lock pairing for 10 minutes. Sign your agents in there once with [`cove agents login`](#agent-logins), and every VM on that server is signed in.

<details>
<summary><b>Rather not open a port?</b> An SSH tunnel, or your own domain behind a proxy</summary>

Without `--https`, the API listens only on `127.0.0.1:7457` on the server, so nothing is open to the internet.

- **An SSH tunnel:** `ssh -fN -L 7457:127.0.0.1:7457 you@server`, then `cove login http://127.0.0.1:7457 <code>`. Keep the tunnel up with `autossh` or your SSH config.
- **Your own domain:** point a DNS name at the server and let [Caddy](https://caddyserver.com) get the certificate with `cove.example.com { reverse_proxy 127.0.0.1:7457 }`, then `cove login https://cove.example.com <code>`.

</details>

> [!TIP]
> Coming next: Let's Encrypt certificates for your own domain or the server's bare IP address, so browsers and any MCP client trust it too, and `cove connect you@server` to do all of this from your laptop in one command. See [the proposal](docs/easy-setup.md).

## A quick tour

**A shell in a fresh machine.** `cove sh` makes a VM called `dev` the first time, and opens a root shell in it every time after.

```sh
cove sh                                     # create or resume "dev" and open a shell
cove up app --mount ~/code/app:/work        # or make one with options, sharing a folder
cove shell app --cwd /work                  # edit on your Mac, build in the VM
```

**Pause, resume, and pause by itself.** Memory and running programs are kept. On Linux the memory goes to disk, so a paused VM costs only disk space.

```sh
cove pause app                              # frozen; on Linux it now uses no RAM
cove resume app                             # carries on where it was, in about a second
cove auto-pause app 30                      # pause after 30 idle minutes; using it wakes it
cove timeout app 2h --on-timeout delete     # a scratch VM that cleans itself up
```

**Set a project up once.** Snapshots hold a VM's installed software and files. The `universal` template has Docker, Node, Python, Go, Rust, gh, Claude Code and Codex.

```sh
cove snapshot build universal --script scripts/templates/universal.sh
cove settings set snapshot=universal auto_pause_minutes=30   # new VMs start from it
cove snapshot save app --name app-ready                       # save any VM, even running
cove up agent-2 --snapshot app-ready
```

**Let agents loose, safely.** Keep keys out of the VM and limit where it can connect.

```sh
cove up agent --network allowlist --allow @dev --secret ANTHROPIC_API_KEY --secret GH_TOKEN
cove egress agent                           # what it may reach, and what it tried to
```

**Keep work running without you.**

```sh
cove spawn app tests -- cargo test          # keeps going after you disconnect
cove attach app tests                       # replay what you missed, then follow
cove forward app 3000                       # a dev server, on a localhost URL
```

Every VM moves through the same few states:

```mermaid
stateDiagram-v2
    direction LR
    [*] --> running: cove up
    running --> paused: cove pause, idle timer, timeout
    paused --> running: cove resume, or any use after an idle pause
    running --> stopped: cove stop
    paused --> stopped: cove stop
    stopped --> running: cove start
    running --> [*]: cove rm, or a delete timeout
    stopped --> [*]: cove rm
```

## Agent logins

Sign in to an agent once per server, and every VM is signed in:

```console
$ cove agents login claude
Signing in to Claude Code in VM cove-login. Pick a login method, open the link it shows on any device and paste the code back here.
…
Claude Code is signed in on this server. Every VM gets this login, and refreshed tokens come back.
$ cove agents ls
claude     Claude Code  signed in, from VM cove-login
codex      Codex        not signed in: cove agents login codex
```

- **Each agent CLI keeps its login in files in the home folder**: `~/.claude/.credentials.json` for Claude Code, `~/.codex/auth.json` for Codex, `~/.gemini/` for Gemini, `~/.local/share/opencode/auth.json` for OpenCode, `~/.qwen/` for Qwen Code. Cove keeps a copy of each on the server, in `~/.cove/logins/`, readable only by Cove.
- **Every VM gets them when it starts**, and Cove checks running VMs every 30 seconds and when they pause or stop. A login made or refreshed in any VM is saved and reaches the others, so a token that rotates doesn't sign the rest out. Signing in inside any VM counts too.
- **`cove agents logout <agent>`** forgets the login and removes it from the VMs. `cove agents import <agent>` copies this computer's login files to the server instead (a Mac keeps Claude Code's in the keychain, so sign in on the server for that one).
- **Opt a VM out** with `--no-logins` on `cove up` or `cove exec --create`.

These are the agents' own files, so the agent in a VM can read them, unlike `--secret` keys, which never enter it. Use secrets for API keys when that matters.

## Secrets and the allowlist

```console
$ export ANTHROPIC_API_KEY=sk-ant-…
$ cove up agent --mount ~/code/app:/work --network allowlist --allow @dev \
    --secret ANTHROPIC_API_KEY --secret GH_TOKEN
$ cove exec agent -- sh -c 'echo $ANTHROPIC_API_KEY'
sk-ant-api03-cove5f0c…            # a placeholder, not your key
$ cove egress agent
Network: allowlist. Reaches only: @dev
Secret ANTHROPIC_API_KEY goes only to api.anthropic.com
Secret GH_TOKEN goes only to github.com, api.github.com

api.anthropic.com                          443  allowed, added ANTHROPIC_API_KEY
registry.npmjs.org                         443  allowed
evil.example                               443  BLOCKED: not on this VM's allowlist
```

- **The proxy runs in `cove serve`, outside the VM.** Commands in the VM get `HTTP_PROXY` and `HTTPS_PROXY` pointing at `127.0.0.1:3128` inside it, where the guest agent hands each connection to the server over its own channel. So an `allowlist` VM needs no network device.
- **Secrets** come from your environment (`--secret NAME`, never a value on the command line). The VM gets a placeholder that keeps the key's public prefix. For that secret's hosts, the proxy terminates TLS with Cove's own authority (installed in the VM), swaps the placeholder in the request headers, `Basic` credentials included, and sends the request on over TLS. Well-known names know their hosts (`ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `GEMINI_API_KEY`, `GH_TOKEN`, `GITLAB_TOKEN` and more); name others with `--secret NAME@host,host`.
- **Everything else passes straight through** as a plain `CONNECT` tunnel: the proxy checks the host name and never sees the traffic.
- **Never reachable:** the host's own addresses and link-local ones such as the cloud metadata service. Private networks only for hosts allowed by exact name, so `*` doesn't open your LAN.
- **Limits:** secrets go in request headers only, over HTTPS, on HTTP/1.1 (streaming responses work; WebSocket upgrades don't yet). With `--network full`, secrets still work but the allowlist isn't enforced.

On purpose, MCP can't give a VM secrets or change its allowlist: an agent shouldn't be able to widen its own sandbox. People do that with the CLI or the API.

## Where Cove runs

| Host | Backend | Isolation | Pause |
| --- | --- | --- | --- |
| macOS on Apple Silicon | `vfkit`: Apple's Virtualization.framework | A real VM with its own kernel | In memory |
| Linux with KVM (bare metal, or a VPS with nested virtualization) | `cloud-hypervisor` | A real VM with its own kernel | To disk, about 1 s each way |
| Linux without KVM (most cheap VPS plans) | `gvisor`: gVisor sandboxes | gVisor's own kernel in user space between the VM's processes and the host's | To disk, under half a second |

`--backend auto`, the default, picks the strongest one the machine has, and `cove doctor` shows which. Each VM keeps the backend it was made with. A VM whose shared folders are in use when it pauses keeps its memory in RAM, so nothing it has open breaks.

On Linux, VMs with the network on get their own /30 in `10.177.0.0/16` with NAT to the internet. They can't reach each other, the host's own services, or the cloud metadata endpoint (`169.254.0.0/16`), which on many providers hands out the server's credentials. gVisor sandboxes boot in about a second and need no KVM, but don't enforce `--disk` and have no systemd inside.

## Cove and Runo

Cove is built to work hand in hand with [Runo](https://github.com/deltalark/runo), the native app for running coding agents side by side. In Runo, a Cove server is a **Connection**, and every session picks where it **Runs on**: your Mac, or any connection, moving between them between agent turns.

**Set it up from Runo.** Settings › Connections finds the Cove on your Mac by itself, and installs it if it isn't there. For a server, [install Cove and run `cove pair` there](#run-it-on-a-server), then add a Cove connection in Runo with the server's URL and the code, and press **Pair**. Runo gets a key of its own, which `cove keys rm` on the server revokes. Its **Sign in** buttons sign agents in on that server, for every VM.

**What a session gets:**

- **On your Mac's Cove**, a VM of its own with the worktree shared in at the same path. The agent and terminals run in the VM; git, diffs and pull requests keep running on the Mac against the same files, so moving between the two is instant.
- **On a remote Cove**, the VM keeps its own copy of the worktree, synced through git after every agent turn, uncommitted changes and the agent's conversation included. Close your laptop and the turn keeps running as a Cove named process; when Runo opens again it reattaches and brings the work back.
- **When idle**, a session's VM pauses (or stops, if you prefer), and the next message wakes it.
- **Agents use Cove too.** Through `cove mcp`, an agent can make scratch VMs of its own: to research without touching the worktree, run untrusted code with the network off, or hand a heavy build to a bigger machine.

Runo checks `/v1/health` to see what a server supports, so an older Cove keeps working for what it can do.

## Reference

<details>
<summary><b>CLI</b></summary>

| Command | What it does |
| --- | --- |
| `cove sh [name]` | Open a shell in `name` (default `dev`), creating, starting or resuming it as needed |
| `cove up <name> [options]` | Create and start a VM (options below) |
| `cove ls [--label K=V]` · `cove show <name>` | List VMs (only those with every label given), or show one |
| `cove start` · `stop` · `rm <name>` | Start, stop or delete a VM |
| `cove pause` · `resume <name>` | Pause a VM with its memory and processes kept, and carry on |
| `cove auto-pause <name> <minutes>` | Pause it once idle that long; `0` turns it off |
| `cove timeout <name> <30m\|2h\|0> [--on-timeout pause\|delete]` | Pause or delete it that long from now, once; `0` clears it |
| `cove label <name> K=V... [--rm K]` | Set or remove labels |
| `cove rename <from> <to>` · `cove copy <from> <to>` | Rename a stopped VM, or copy one |
| `cove snapshot save <vm> --name N` · `ls` · `rm N` | Save a VM (while it runs, too), list and delete snapshots |
| `cove snapshot build N --script FILE` | Run a setup script in a fresh VM and save the result |
| `cove exec <name> [-t] [--cwd DIR] [-e K=V]... -- <cmd...>` | Run a command and wait for it |
| `cove shell <name> [--cwd DIR]` | Open an interactive shell |
| `cove spawn <name> <proc> [-t] [-a] -- <cmd...>` | Start a named background process |
| `cove attach <name> <proc> [--from BYTES]` | Reattach to a process and replay its output |
| `cove procs <name>` · `cove kill <name> <proc>` | List or stop processes |
| `cove push <name> <local> <remote>` · `cove pull <name> <remote> <local>` | Copy files in and out |
| `cove ports <name>` · `cove forward <name> <port>` | List ports and forward one |
| `cove egress <name>` · `cove allow <name> [rules...]` | What a VM may reach and tried to; replace its allowlist |
| `cove settings show` · `set K=V...` · `unset K...` | The server's defaults for new VMs |
| `cove pair` · `cove login <cove://…>` or `<url> <code>` · `cove logout` | Connect Runo or another computer with a one-time code |
| `cove keys ls` · `create <name>` · `rm <id>` | Named API keys, one per person or device |
| `cove agents ls` · `login <agent> [--vm V]` · `logout <agent>` · `import <agent>` | Agent logins kept on the server for every VM |
| `cove image ls` · `cove image pull <image> [--backend B]` | List and download base images |
| `cove serve [--listen 127.0.0.1:7457] [--https 0.0.0.0:7443]` · `cove doctor` · `cove key` | Run the server, check the host, print its key |
| `cove mcp` · `cove mcp install [agent...]` · `uninstall` | The MCP server over stdio; add it to or remove it from agents' settings |
| `cove version` | Versions, and the server's features |

Add `--json` to any command for machine-readable output, and `--host URL` to talk to another server. `cove exec` and `cove shell` start a stopped VM, or resume a paused one, by themselves, and with `--create` make a missing one. `cove exec` passes stdin through byte for byte, so `git upload-pack` and friends run through it unchanged.

</details>

<details>
<summary><b><code>cove up</code> options</b></summary>

| Option | Default | What it does |
| --- | --- | --- |
| `--snapshot NAME` · `--image NAME` | `debian-13` | Start from a snapshot, or a base image |
| `--cpus N` · `--memory MB` · `--disk GB` | `2` · `4096` · `20` | Size |
| `--mount HOST:GUEST[:ro]` | | Share a folder; repeat for more |
| `--backend NAME` | `auto` | `auto`, `vfkit`, `cloud-hypervisor` or `gvisor` |
| `--network full\|off\|allowlist` | `full` | `off` and `allowlist` give the VM no network device; `allowlist` reaches its `--allow` hosts through Cove's proxy |
| `--allow RULE` | `@dev` with `allowlist` | `example.com`, `*.example.com`, `*`, or a preset: `@dev` (git hosts, registries, Debian mirrors, model APIs) or `@agents` (model APIs and sign-in hosts) |
| `--secret NAME[@HOSTS]` | | Keep `NAME` from your environment out of the VM |
| `--auto-pause MINUTES` · `--idle-stop MINUTES` | off | Pause, or stop, after that many minutes with no commands, streams, forwarded connections or running named processes |
| `--timeout DURATION` `[--on-timeout pause\|delete]` | | Pause or delete it that long from now: `30m`, `2h`, up to `24h` |
| `--label KEY=VALUE` · `--tag T` | | Metadata to find it by (up to 32 labels) |
| `--ephemeral` | | Delete the VM when it stops |

Options left out come from the server's settings (`cove settings`), then the defaults above.

</details>

<details>
<summary><b>HTTP API</b></summary>

Every request carries `Authorization: Bearer <key>`: the server's own key (`cove key`), or a named key from `cove keys create` or pairing. Named keys are stored as hashes and revoked one by one.

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/v1/health` | No auth. `ok, version, protocol, backend, features` |
| `GET` `POST` | `/v1/vms` | POST takes `name, image, snapshot, cpus, memory_mb, disk_gb, mounts, ephemeral, tags, labels, backend, network, allow, secrets, auto_pause_minutes, idle_stop_minutes, timeout_minutes, on_timeout, start`; what it leaves out comes from `/v1/settings`. GET takes `?label=key=value`, repeated |
| `GET` `PATCH` `DELETE` | `/v1/vms/{name}` | PATCH takes `labels`, `auto_pause_minutes`, `idle_stop_minutes` and `timeout` (`{minutes, action}`); `null` clears one |
| `POST` | `/v1/vms/{name}/start`, `/stop`, `/pause`, `/resume`, `/copy`, `/rename` | `copy` and `rename` take `{"to": ...}` |
| `POST` | `/v1/vms/{name}/exec` | Runs to completion: `code, stdout, stderr` |
| `GET` (WebSocket) | `/v1/vms/{name}/stream` | First message is an `exec`, `spawn` or `attach` request; then frames out, input in |
| `GET` · `DELETE` | `/v1/vms/{name}/procs` · `/procs/{proc}?signal=` | |
| `GET` `PUT` · `GET` | `/v1/vms/{name}/files?path=` · `/dir?path=` | Raw bytes; PUT takes `&mode=755` |
| `GET` · `POST` `DELETE` | `/v1/vms/{name}/ports` · `/forwards`, `/forwards/{port}` | |
| `GET` `PUT` | `/v1/vms/{name}/egress` | Policy, secrets' names and hosts, and the latest 200 connections. PUT `{"allow": [...]}` replaces the allowlist |
| `POST` · `GET` `DELETE` | `/v1/vms/{name}/snapshots` · `/v1/snapshots`, `/v1/snapshots/{name}` | Save a VM `{name, description}`; list and delete |
| `GET` `PUT` | `/v1/settings` | Defaults for new VMs: `cpus, memory_mb, disk_gb, backend, network, allow, auto_pause_minutes, idle_stop_minutes, snapshot` |
| `GET` `POST` · `DELETE` | `/v1/keys` · `/v1/keys/{id}` | POST `{name}` returns a new key once |
| `POST` | `/v1/pair/codes` | A one-time code, good for 10 minutes. With `--https`, also `url`, `fingerprint` and `pair`, the `cove://` line |
| `GET` | `/v1/logins` | Each agent and whether it is signed in on this server (never the files) |
| `PUT` · `DELETE` | `/v1/logins/{agent}` | PUT `{files: {path: base64}}` saves login files from elsewhere; DELETE signs the agent out here and in the VMs |
| `POST` | `/v1/vms/{name}/logins/sync` | Sync that VM's logins now, as after signing in there |
| `POST` | `/v1/pair` | No auth. `{code, name}` returns `{id, name, key, server}`. Ten wrong codes in a row lock pairing for 10 minutes |
| `GET` · `POST` | `/v1/images` · `/v1/images/{name}/pull?backend=` | |

A VM's `state` is `stopped`, `starting`, `running`, `stopping` or `paused`. A paused one says why in `paused_by` (`user`, `idle` or `timeout`) and has `paused_to_disk: true` when its memory is on disk. An idle pause wakes on the next request; the others answer 409 until resumed.

`/v1/health` lists `features`, such as `pause`, `snapshots`, `pair` and `backend:gvisor`. Features are only ever added, so check for the one you need rather than comparing versions. The stream's frames and requests are the types in [`crates/proto`](crates/proto/src/lib.rs).

```sh
curl -s -H "Authorization: Bearer $(cove key)" -H "Content-Type: application/json" \
  -d '{"cmd":["uname","-a"]}' http://127.0.0.1:7457/v1/vms/dev/exec
```

</details>

<details>
<summary><b>MCP tools</b></summary>

```json
{ "mcpServers": { "Cove": { "command": "cove", "args": ["mcp"] } } }
```

| Tool | What it does |
| --- | --- |
| `vm_list` · `vm_create` | List VMs; create one (`name`, and optionally `cpus`, `memory_mb`, `ephemeral`, `network`, `allow`, `auto_pause_minutes`, `idle_stop_minutes`, `snapshot`, `timeout_minutes`) |
| `vm_exec` | Run a command (`bash -lc`) and wait for it |
| `vm_spawn` · `vm_logs` · `vm_kill` | Background processes: start, read their output so far, signal |
| `vm_read_file` · `vm_write_file` | Files |
| `vm_share_port` | Forward a port to the host |
| `vm_pause` · `vm_resume` · `vm_stop` · `vm_start` | Lifecycle |
| `vm_snapshot_save` · `vm_snapshot_list` · `vm_copy` | Snapshots and copies |
| `vm_egress` · `vm_destroy` | What a VM reached; delete it |

</details>

## How it works

```mermaid
flowchart LR
    CLI["cove CLI"] --> S
    Runo["Runo"] --> S
    MCP["MCP agents"] --> S
    S["cove serve<br/>HTTP + WebSocket<br/>egress proxy"] --> V["vfkit (macOS)<br/>cloud-hypervisor (Linux + KVM)<br/>gVisor runsc (Linux)"]
    V --> VM
    subgraph VM["Linux VM or sandbox"]
        G["cove-guest"]
    end
    S <-- "vsock, or a Unix socket for gVisor" --> G
```

- **`cove serve`** keeps everything in `~/.cove`: VM records and disks (copy-on-write clones of a base image, copied without filling their holes), snapshots, settings and keys. For real VMs it builds a small cloud-init seed on every start that installs the guest agent.
- **`cove-guest`** is a static Linux binary inside each VM. It runs commands, terminals, file operations and port pipes, one request per connection.
- **VMs are their own processes**, so restarting the server doesn't stop them; it finds them again from `run.json`. A VM paused to disk has no process at all, and comes back after a restart too.
- **Pausing to disk** uses cloud-hypervisor's snapshot and restore (shared folders are unplugged around it) and gVisor's checkpoint and restore (the guest agent lets go of its host sockets first).

| Crate | Role |
| --- | --- |
| [`crates/cli`](crates/cli) | The `cove` binary: server, CLI client and MCP server |
| [`crates/core`](crates/core) | VM manager, backends, images, snapshots, settings, keys and the egress proxy |
| [`crates/guest`](crates/guest) | `cove-guest`, the agent inside each VM |
| [`crates/proto`](crates/proto) | Wire types shared by server, client and guest |

## Roadmap

**Shipped**

- [x] Three backends: vfkit on a Mac, cloud-hypervisor with KVM, gVisor without it, each checked end to end with `scripts/smoke-vm.sh`
- [x] One-line install and upgrade, with a launchd agent or a systemd service
- [x] Shared folders, background processes that outlive the connection, port forwarding, an MCP server
- [x] Secrets kept out of VMs, and an egress allowlist with a log
- [x] Pause and resume, to disk on Linux; auto-pause with wake on use; timeouts, labels and rename
- [x] Snapshots, `cove snapshot build` and a `universal` template
- [x] Server settings, named keys and pairing codes; Runo Connections with turns that outlive your laptop
- [x] Agent logins: sign in once per server, refreshed tokens kept in step across VMs
- [x] HTTPS with a self-signed certificate pinned through the pairing line (`cove serve --https`)
- [x] `cove mcp install`: add Cove to Claude Code, Codex, Gemini CLI, Qwen Code, OpenCode and Cursor in one command

**Next**, with the plan for the first three in [docs/easy-setup.md](docs/easy-setup.md)

- [ ] Let's Encrypt certificates for your own domain or the server's bare IP
- [ ] `cove connect you@server`: install, pair and connect from your laptop in one command
- [ ] SSH from any terminal: a gateway with keys and paste-anywhere tokens
- [ ] Public HTTPS ports on your own domain, with wake on visit
- [ ] Secrets stored on the server and changeable on a running VM
- [ ] A desktop agents can drive (computer use), with share links
- [ ] A TypeScript SDK and a web dashboard
- [ ] Pause to disk on a Mac

## FAQ

<details>
<summary><b>Does my server need KVM?</b></summary>

No. With KVM (bare metal, or a VPS plan with nested virtualization) Cove runs real VMs with cloud-hypervisor. Without it, which is most cheap VPS plans, it runs gVisor sandboxes: gVisor's own kernel in user space between the VM's processes and the host's. `cove doctor` tells you which you have.

</details>

<details>
<summary><b>Is it safe to open to the internet?</b></summary>

The API listens on `127.0.0.1` unless you change it. Behind HTTPS, every request but `/v1/health` and pairing needs an API key. Keys are stored as hashes and revoked one by one, and ten wrong pairing codes in a row lock pairing for 10 minutes. Don't open the plain HTTP port itself: use a tunnel or HTTPS.

</details>

<details>
<summary><b>What does it cost?</b></summary>

Nothing beyond the machine it runs on. Cove has no account, no telemetry and no per-second bill. A paused VM on Linux costs only its disk space.

</details>

<details>
<summary><b>Can the agent in a VM see my API keys?</b></summary>

Not the ones you give with `--secret`: inside, they are placeholders, and the real value is added by Cove's proxy on the way out, only to that key's hosts. Agent logins from `cove agents` are different. They are the agents' own login files, so the agent in the VM can read them. Use secrets when that matters.

</details>

<details>
<summary><b>Windows? Intel Macs?</b></summary>

Not yet. The CLI works anywhere Rust does, so you can drive a Linux server from any computer, but `cove serve` needs Apple Silicon or Linux.

</details>

## Development

Building needs [Rust](https://rustup.rs), plus [zig](https://ziglang.org) and [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) for the guest agent on a Mac (or `musl-tools` on Linux).

```sh
cargo build --release
scripts/build-guest.sh                  # the agent that runs inside VMs, into ~/.cove/guest
target/release/cove serve &             # data lives in ~/.cove
target/release/cove doctor
cargo build --workspace && cargo test --workspace
```

`scripts/install.sh --from-source` does the same from a checkout, and `scripts/smoke-vm.sh` boots real VMs on any backend and checks them end to end.

<details>
<summary><b>Test switches</b></summary>

The end-to-end tests use the `host` backend, which runs the guest agent as a normal process on your machine with **no isolation**. It is only allowed when the server runs with `COVE_ALLOW_HOST_BACKEND=1`.

- `COVE_GVISOR_E2E=1` adds a test that boots a real gVisor sandbox (root, `runsc`, a static guest agent in `COVE_GUEST_BINARY`).
- `COVE_GVISOR_PLATFORM` forces gVisor's platform; `COVE_CH_FIRMWARE` points cloud-hypervisor at your own firmware.
- `COVE_IDLE_CHECK_SECS` changes how often idle timers and timeouts are checked (30 s), and `COVE_IDLE_MINUTE_SECS` shortens what counts as a minute.

</details>

## License

[MIT](LICENSE)
