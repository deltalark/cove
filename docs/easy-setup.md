# Proposal: Cove in five minutes, for any agent

Cove works well once it is set up. Getting there on a server still takes a terminal and
some patience: SSH in, run the installer, run `cove pair`, open an SSH tunnel, then paste a
URL and a code somewhere. Runo is getting a wizard that does all of that behind a few
buttons. This proposal gives everyone else, Claude Code, Codex, Gemini, Cursor or a person
at a terminal, the same short path, with no Runo required.

## The goal

From a laptop with nothing installed to an agent running in a VM on your own server, in
three commands and no copy and paste:

```console
$ curl -fsSL https://raw.githubusercontent.com/deltalark/cove/main/scripts/install.sh | sh
$ cove connect ubuntu@203.0.113.7
  ✓ SSH works (key ~/.ssh/id_ed25519)
  ✓ Installed Cove 0.2.0 on the server (cloud-hypervisor, KVM)
  ✓ Paired: this laptop has a key of its own ("cli on anish-mbp")
  ✓ Reachable at https://cove.example.com
  ? Sign agents in on the server? Codex and Gemini can use this laptop's logins.  [Y/n]
  ✓ Codex, Gemini signed in. Claude Code: run cove agents login claude
$ cove mcp install claude
  ✓ Claude Code can now make and use Cove VMs (claude mcp add -s user cove -- cove mcp)
```

## What to build

### 1. `cove serve --domain`: HTTPS on your own domain, by itself

Today the API listens on plain HTTP at `127.0.0.1:7457`. To reach it you open an SSH
tunnel, or put a reverse proxy with a certificate in front of it. Point a DNS name at the
server and Cove should do the rest:

```sh
curl -fsSL …/install.sh | sudo sh -s -- --domain cove.example.com
```

- Listens on 443 (and on 80, for the ACME challenge and to redirect), and gets and renews a
  Let's Encrypt certificate with `instant-acme` or `rustls-acme`. Cove already builds with
  rustls, so this adds no OpenSSL.
- Saves the public URL in the server's settings, so `cove pair` prints a line you can paste
  as is: `cove login https://cove.example.com 4F7K-Q2XM`.
- `127.0.0.1:7457` keeps working on the server itself, unchanged.
- **No domain?** Cove gets a Let's Encrypt certificate for the server's bare IP address,
  using the short-lived profile (about 160 hours), and renews it by itself over HTTP-01 or
  TLS-ALPN-01. No DNS and no third-party name service are involved.
- **No public port?** Cove makes a self-signed certificate and puts its SHA-256
  fingerprint in the `cove pair` code. The client pins that fingerprint, so the connection
  is still checked end to end.
- Pairing over HTTPS also fixes the weak point of pairing over plain HTTP on an open port:
  the key that comes back can't be read on the way.
- `cove doctor` checks that the name points at this server, that ports 80 and 443 are
  open, and when the certificate expires.

### 2. `cove connect user@host`: the Runo wizard, for the terminal

One command on the laptop that does what a person does by hand today:

1. Checks the SSH login, using `~/.ssh/config` and the agent like `ssh` does.
2. Runs the installer over SSH with sudo, or offers to upgrade an older Cove. If sudo
   needs a password, it asks once, through ssh's own prompt, and doesn't keep it.
3. Runs `cove pair` there, takes the code itself and trades it for a key of the laptop's
   own, the same way `cove login` does now.
4. Picks how to reach the server: its HTTPS domain when it has one (step 1). Otherwise
   an SSH tunnel that Cove keeps up itself (`cove tunnel`, a launchd or systemd user
   service that reconnects after sleep and network changes). The saved login records which.
5. Offers to bring agent logins over: `cove agents import` for the ones this computer
   keeps in files (Codex, Gemini, OpenCode, Qwen Code), and `cove agents login claude`
   for Claude Code, whose Mac login lives in the keychain.

It is idempotent: running it again fixes whichever step is broken (an expired key, a
dropped tunnel, an old server) and says what it changed. Runo's wizard can call the same
code, so the two never drift apart.

### 3. `cove mcp install <agent>`: one line per agent

`cove mcp` already gives an agent tools to make VMs, run commands, move files and share
ports, on whichever server `cove login` points at. Registering it differs for each agent,
so Cove should do it:

| Agent | What `cove mcp install` runs or writes |
| --- | --- |
| Claude Code | `claude mcp add -s user cove -- cove mcp` |
| Codex | `codex mcp add cove -- cove mcp` |
| Gemini CLI | `gemini mcp add -s user cove cove mcp` |
| Qwen Code | `qwen mcp add -s user cove cove mcp` |
| OpenCode | `"mcp": {"cove": {"type": "local", "command": ["cove", "mcp"]}}` in `~/.config/opencode/opencode.json` |
| Cursor | `"mcpServers": {"cove": {"command": "cove", "args": ["mcp"]}}` in `~/.cursor/mcp.json` |

Each also gets a short note in the agent's own instructions file (`CLAUDE.md`,
`AGENTS.md`, `GEMINI.md`) on when to reach for Cove: to run untrusted code with the network
off, to try something without touching the worktree, or to hand a long job to the server.
`cove mcp install --all` registers Cove with every agent it finds, and `cove mcp uninstall`
undoes it.

### 4. A server agents can reach over HTTP, too

Hosted agents (a cloud Claude, a CI job, a Slack bot) can't run `cove mcp` on your laptop.
Once the server has an HTTPS domain, serve the same MCP tools at
`https://cove.example.com/v1/mcp` (streamable HTTP), behind a named key:

```sh
claude mcp add --transport http cove https://cove.example.com/v1/mcp \
  --header "Authorization: Bearer $(cove keys create claude-web --quiet)"
```

The rule that MCP can't widen its own sandbox (no secrets, no allowlist changes) stays.

### 5. Smaller things that add up

- **The installer ends with what to do next**, for the case it just set up: the
  `cove connect` line to run from a laptop, or the `cove login` line with a fresh code.
- **Cloud-init user data** in the docs, so a new VPS comes up with Cove installed and a
  domain set, from any provider's "user data" box.
- **`cove status`**: one screen showing the server it talks to, how (domain or tunnel),
  its version and backend, the agents signed in, and the VMs running.

## Order of work

1. `cove serve --domain` with `--domain auto`, and the public URL in `cove pair`.
2. `cove connect` with the managed tunnel, then the agent logins step.
3. `cove mcp install` for the six agents above.
4. MCP over HTTP.
5. The installer's next steps, cloud-init, `cove status`.

Each step is useful on its own and ships behind a `/v1/health` feature (`tls`, `connect`,
`mcp-http`), so Runo and older CLIs keep working as before.
