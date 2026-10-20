#!/bin/bash
# The `universal` template: the tools coding agents usually reach for, installed once.
#
#   cove snapshot build universal --script scripts/templates/universal.sh
#   cove settings set snapshot=universal     # optional: new VMs start from it
#
# Runs as root in a fresh Debian 13 VM with the network on. Edit freely: anything a
# project needs every time (its own toolchains, a cloned repo, dependencies) can go
# in a copy of this script.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

echo "== packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  build-essential pkg-config ca-certificates curl wget git git-lfs gnupg \
  python3 python3-pip python3-venv pipx \
  nodejs npm golang-go \
  jq ripgrep fd-find tree unzip zip xz-utils less vim-tiny nano \
  tmux mosh htop procps lsof iproute2 iputils-ping dnsutils openssh-client \
  sqlite3 gh >/dev/null

echo "== docker"
apt-get install -y -qq --no-install-recommends docker.io docker-compose >/dev/null
if [ -d /run/systemd/system ]; then
  systemctl enable --now docker >/dev/null 2>&1 || true
else
  # gVisor sandboxes have no systemd: Docker is installed but not started.
  echo "no systemd here, so dockerd isn't started"
fi

echo "== rust"
if ! command -v cargo >/dev/null 2>&1; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
  ln -sf /root/.cargo/bin/* /usr/local/bin/ 2>/dev/null || true
fi

echo "== coding agents"
npm install -g --silent @anthropic-ai/claude-code @openai/codex >/dev/null 2>&1 \
  || echo "couldn't install the agent CLIs from npm; install them later"

echo "== tidy up"
apt-get clean
rm -rf /var/lib/apt/lists/* /root/.npm /tmp/*
# New VMs get their own identity on first boot.
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id

echo "== versions"
for tool in git python3 node go cargo docker gh claude codex; do
  printf '%-8s ' "$tool"
  if command -v "$tool" >/dev/null 2>&1; then
    "$tool" --version 2>/dev/null | head -1
  else
    echo "not installed"
  fi
done
