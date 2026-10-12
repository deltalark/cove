#!/bin/sh
# Install Cove on this machine.
#
#   curl -fsSL https://raw.githubusercontent.com/deltalark/cove/main/scripts/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --no-service      # pass options after `sh -s --`
#
# On a Mac with Apple Silicon this installs the latest release into ~/.cove/bin,
# installs vfkit with Homebrew if it is missing, starts `cove serve` at login with a
# launchd agent, checks the setup with `cove doctor` and pulls the default image.
#
# On Linux (a VPS or your own server), run it as root. With KVM it installs what
# VMs need: cloud-hypervisor (the official static build, if missing), virtiofsd,
# iptables and iproute2 (from the distribution). Without KVM (many cheap VPS
# plans) it offers gVisor instead, the official release with its checksum
# checked, which runs each VM as a sandbox with its own user-space kernel. Then it
# installs a systemd service. Declined, it installs only the CLI, which can drive
# a Cove server elsewhere with --host.
#
# Running it again upgrades in place. Your VMs, images and API key in ~/.cove are
# never touched, except by --uninstall --purge.
set -eu

REPO="deltalark/cove"
IMAGE="debian-13"
LABEL="com.deltalark.cove"

prefix="${COVE_PREFIX:-$HOME/.cove}"
data="${COVE_HOME:-$HOME/.cove}"
version=""
listen="127.0.0.1:7457"
service=1
image=1
from_source=0
from_file=""
uninstall=0
purge=0
link=1
deps=1
gvisor=ask

usage() {
  cat <<'EOF'
Install Cove: self-hosted microVMs for people and coding agents.

Usage: install.sh [options]

  --prefix DIR        Install into DIR/bin (default: ~/.cove)
  --version TAG       Install this release, e.g. v0.2.0 (default: the latest)
  --listen ADDR       Address the server listens on (default: 127.0.0.1:7457)
  --no-service        Don't start `cove serve` at login or boot
  --no-image          Don't pull the default image
  --no-link           Don't link cove into /opt/homebrew/bin or /usr/local/bin
  --no-deps           Don't install vfkit, cloud-hypervisor, gVisor, virtiofsd or iptables
  --gvisor            Without KVM, install gVisor without asking
  --no-gvisor         Without KVM, don't install gVisor
  --from-source       Build from source with cargo instead of downloading
  --from-file FILE    Install from a release tarball you already have
  --uninstall         Remove Cove's binaries and service; keep VMs and data
  --purge             With --uninstall: also stop all VMs and delete ~/.cove
  -h, --help          Show this help
EOF
}

say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
warn() { printf 'cove install: %s\n' "$*" >&2; }
die() {
  warn "$*"
  exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) [ $# -ge 2 ] || die "--prefix needs a folder"; prefix="$2"; shift 2 ;;
    --prefix=*) prefix="${1#*=}"; shift ;;
    --version) [ $# -ge 2 ] || die "--version needs a tag"; version="$2"; shift 2 ;;
    --version=*) version="${1#*=}"; shift ;;
    --listen) [ $# -ge 2 ] || die "--listen needs an address"; listen="$2"; shift 2 ;;
    --listen=*) listen="${1#*=}"; shift ;;
    --no-service) service=0; shift ;;
    --no-image) image=0; shift ;;
    --no-link) link=0; shift ;;
    --no-deps) deps=0; shift ;;
    --gvisor) gvisor=yes; shift ;;
    --no-gvisor) gvisor=no; shift ;;
    --from-source) from_source=1; shift ;;
    --from-file) [ $# -ge 2 ] || die "--from-file needs a tarball"; from_file="$2"; shift 2 ;;
    --from-file=*) from_file="${1#*=}"; shift ;;
    --uninstall) uninstall=1; shift ;;
    --purge) purge=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option $1" ;;
  esac
done

case "$version" in
  "" | v*) ;;
  *) version="v$version" ;;
esac
bindir="$prefix/bin"
port="${listen##*:}"
# The address to reach our own server at, even when it listens on every interface.
case "$listen" in
  0.0.0.0:* | \[::\]:*) local_url="http://127.0.0.1:$port" ;;
  *) local_url="http://$listen" ;;
esac

os="$(uname -s)"
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) die "unsupported processor: $(uname -m)" ;;
esac
case "$os" in
  Darwin) platform="macos-arm64" ;;
  Linux) platform="linux-$arch" ;;
  *) die "unsupported system: $os (Cove runs on macOS and Linux)" ;;
esac
if [ "$os" = Darwin ] && [ "$arch" != aarch64 ] && [ "$from_source" = 0 ] && [ -z "$from_file" ] && [ "$uninstall" = 0 ]; then
  die "Cove runs VMs on Apple Silicon Macs only. To build just the CLI here, use --from-source."
fi

tmp=""
cleanup() { [ -z "$tmp" ] || rm -rf "$tmp"; }
trap cleanup EXIT
trap 'exit 130' INT TERM
tmp="$(mktemp -d)"

plist="$HOME/Library/LaunchAgents/$LABEL.plist"
if [ "$(id -u)" = 0 ]; then
  unit="/etc/systemd/system/cove.service"
  systemctl_scope=""
else
  unit="$HOME/.config/systemd/user/cove.service"
  systemctl_scope="--user"
fi

# Run systemctl for our unit's scope (system for root, the user's otherwise).
sysctl() {
  if [ -n "$systemctl_scope" ]; then
    systemctl "$systemctl_scope" "$@"
  else
    systemctl "$@"
  fi
}

stop_service() {
  if [ "$os" = Darwin ]; then
    if [ -f "$plist" ]; then
      launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || launchctl unload "$plist" 2>/dev/null || true
    fi
  elif [ -f "$unit" ] && have systemctl; then
    sysctl disable --now cove.service 2>/dev/null || true
  fi
}

# Folders apps look in for `cove` even without the shell's PATH (Runo checks these).
link_dirs() {
  if [ "$os" = Darwin ]; then
    say /opt/homebrew/bin /usr/local/bin
  else
    say /usr/local/bin "$HOME/.local/bin"
  fi
}

do_uninstall() {
  step "Uninstalling Cove"
  if [ "$purge" = 1 ] && [ -x "$bindir/cove" ]; then
    # VMs are their own processes and outlive the server, so stop them first.
    for vm in $("$bindir/cove" ls 2>/dev/null | awk 'NR > 1 && $2 == "running" { print $1 }'); do
      say "Stopping VM $vm"
      "$bindir/cove" stop "$vm" >/dev/null 2>&1 || warn "could not stop $vm"
    done
  fi
  stop_service
  for f in "$plist" "$unit"; do
    if [ -f "$f" ]; then
      rm -f "$f"
      say "Removed $f"
    fi
  done
  if [ "$os" = Linux ] && have systemctl; then
    sysctl daemon-reload 2>/dev/null || true
  fi
  for dir in $(link_dirs); do
    link="$dir/cove"
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$bindir/cove" ]; then
      rm -f "$link"
      say "Removed $link"
    fi
  done
  rm -f "$bindir/cove" "$bindir/cove-guest-linux-aarch64" "$bindir/cove-guest-linux-x86_64"
  rmdir "$bindir" 2>/dev/null || true
  say "Removed the cove binaries from $bindir"
  if [ "$purge" = 1 ]; then
    rm -rf "$data"
    say "Deleted $data (VMs, images and the API key)"
  else
    say "Kept your VMs, images and API key in $data. Delete them with --uninstall --purge."
  fi
}

if [ "$uninstall" = 1 ]; then
  do_uninstall
  exit 0
fi
[ "$purge" = 0 ] || die "--purge goes with --uninstall"

# Put a file into $bindir atomically, so a running server keeps its old binary
# until it restarts.
place() {
  mkdir -p "$bindir"
  cp "$1" "$bindir/.$2.new"
  chmod 0755 "$bindir/.$2.new"
  mv -f "$bindir/.$2.new" "$bindir/$2"
}

latest_version() {
  # The /latest page redirects to /tag/<version>; this needs no API token.
  url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null)" || return 1
  case "$url" in
    */tag/*) say "${url##*/tag/}" ;;
    *) return 1 ;;
  esac
}

install_tarball() {
  mkdir -p "$tmp/unpack"
  tar -xzf "$1" -C "$tmp/unpack"
  [ -f "$tmp/unpack/cove/cove" ] || die "$1 has no cove/cove inside; is it a Cove release?"
  place "$tmp/unpack/cove/cove" cove
  for g in "$tmp"/unpack/cove/cove-guest-linux-*; do
    [ -f "$g" ] && place "$g" "$(basename "$g")"
  done
}

# Download the release for this platform. Returns non-zero when there is none.
download_release() {
  have curl || die "curl is needed to download Cove"
  if [ -z "$version" ]; then
    version="$(latest_version)" || return 1
  fi
  asset="cove-$version-$platform.tar.gz"
  base="https://github.com/$REPO/releases/download/$version"
  say "Downloading $asset"
  curl -fL --retry 2 --progress-bar -o "$tmp/$asset" "$base/$asset" || return 1
  if curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" 2>/dev/null; then
    want="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
    if have shasum; then
      got="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
    else
      got="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
    fi
    if [ -z "$want" ] || [ "$want" != "$got" ]; then
      die "checksum mismatch for $asset; not installing it"
    fi
  fi
  install_tarball "$tmp/$asset"
}

build_from_source() {
  have cargo || die "building from source needs Rust: https://rustup.rs"
  # Run from a checkout (scripts/install.sh), build that; piped from curl, clone.
  here=""
  case "$0" in
    *install.sh) here="$(cd "$(dirname "$0")" && pwd)" ;;
  esac
  if [ -n "$here" ] && [ -f "$here/../crates/cli/Cargo.toml" ]; then
    src="$(cd "$here/.." && pwd)"
    say "Building the checkout in $src"
  else
    have git || die "building from source needs git"
    src="$tmp/src"
    say "Cloning $REPO ${version:-(main)}"
    if [ -n "$version" ]; then
      git clone --quiet --depth 1 --branch "$version" "https://github.com/$REPO.git" "$src"
    else
      git clone --quiet --depth 1 "https://github.com/$REPO.git" "$src"
    fi
  fi
  (cd "$src" && cargo build --release -p cove)
  place "$src/target/release/cove" cove
  # VMs need the guest agent as a static Linux binary for this processor.
  if [ "$os" = Linux ] && have musl-gcc || have cargo-zigbuild; then
    (cd "$src" && COVE_HOME="$data" sh scripts/build-guest.sh)
  else
    warn "skipped the guest agent VMs need: install zig and cargo-zigbuild, then run scripts/build-guest.sh"
  fi
}

step "Installing Cove into $bindir"
if [ -n "$from_file" ]; then
  [ -f "$from_file" ] || die "no such file: $from_file"
  install_tarball "$from_file"
elif [ "$from_source" = 1 ]; then
  build_from_source
elif ! download_release; then
  warn "no release of Cove for $platform${version:+ at $version} could be downloaded"
  say "Building from source instead."
  build_from_source
fi
cove="$bindir/cove"
say "Installed $("$cove" --version)"

# Link it where apps started outside a shell look for it, unless something else
# is already there.
linked=""
for dir in $([ "$link" = 1 ] && link_dirs); do
  if [ -d "$dir" ] && [ -w "$dir" ]; then
    link="$dir/cove"
    if [ ! -e "$link" ] || { [ -L "$link" ] && [ "$(readlink "$link")" = "$bindir/cove" ]; }; then
      ln -sf "$bindir/cove" "$link"
      linked="$link"
      say "Linked $link"
    fi
    break
  fi
done

# Install a distribution package if its program is missing.
pkg_install() {
  if have apt-get; then
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null
  elif have dnf; then
    dnf install -y -q "$@"
  else
    return 1
  fi
}

# Debian and Ubuntu put virtiofsd outside PATH.
have_virtiofsd() {
  have virtiofsd || [ -x /usr/libexec/virtiofsd ] || [ -x /usr/lib/qemu/virtiofsd ]
}

linux_deps() {
  if [ "$(id -u)" != 0 ]; then
    warn "not root, so skipping cloud-hypervisor, virtiofsd and iptables; run with sudo to set up VMs"
    return 0
  fi
  if have cloud-hypervisor; then
    say "cloud-hypervisor is installed"
  else
    ch_asset=cloud-hypervisor-static
    [ "$arch" = aarch64 ] && ch_asset=cloud-hypervisor-static-aarch64
    say "Downloading cloud-hypervisor ($ch_asset)"
    curl -fL --retry 2 --progress-bar -o "$tmp/cloud-hypervisor" \
      "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/latest/download/$ch_asset"
    cp "$tmp/cloud-hypervisor" /usr/local/bin/.cloud-hypervisor.new
    chmod 0755 /usr/local/bin/.cloud-hypervisor.new
    mv -f /usr/local/bin/.cloud-hypervisor.new /usr/local/bin/cloud-hypervisor
    say "Installed /usr/local/bin/cloud-hypervisor"
  fi
  install_packages
}

# virtiofsd for shared folders (VMs only), iptables and iproute2 for VM networks.
install_packages() {
  missing=""
  if [ -e /dev/kvm ]; then
    have_virtiofsd || missing=" virtiofsd"
  fi
  have iptables || missing="$missing iptables"
  have ip || missing="$missing iproute2"
  if [ -n "$missing" ]; then
    say "Installing$missing"
    if have apt-get; then
      apt-get update -qq >/dev/null || true
    fi
    # shellcheck disable=SC2086 # one word per package
    pkg_install $missing || warn "could not install$missing; install it yourself (shared folders need virtiofsd, VM networking needs iptables)"
  fi
}

# Ask on the terminal even when the script itself comes from a pipe.
confirm() {
  if [ -r /dev/tty ] && [ -w /dev/tty ]; then
    printf '%s [Y/n] ' "$1" >/dev/tty
    read -r answer </dev/tty || answer=""
    case "$answer" in
      [Nn]*) return 1 ;;
    esac
    return 0
  fi
  # No terminal to ask on: go ahead, as a recommended default.
  return 0
}

install_gvisor() {
  if have runsc; then
    say "gVisor is installed: $(runsc --version 2>/dev/null | head -n 1)"
    return 0
  fi
  base="https://storage.googleapis.com/gvisor/releases/release/latest/$arch"
  say "Downloading gVisor"
  curl -fL --retry 2 --progress-bar -o "$tmp/gvisor.tar.bz2" "$base/gvisor.tar.bz2"
  curl -fsSL -o "$tmp/gvisor.tar.bz2.sha512" "$base/gvisor.tar.bz2.sha512"
  want="$(awk '{ print $1 }' "$tmp/gvisor.tar.bz2.sha512")"
  got="$(sha512sum "$tmp/gvisor.tar.bz2" | awk '{ print $1 }')"
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    die "checksum mismatch for gVisor; not installing it"
  fi
  # runsc finds its helpers next to itself, so keep the release together.
  mkdir -p "$tmp/gvisor"
  tar -xjf "$tmp/gvisor.tar.bz2" -C "$tmp/gvisor"
  [ -x "$tmp/gvisor/runsc" ] || die "the gVisor release has no runsc"
  rm -rf /usr/local/lib/gvisor.new
  mkdir -p /usr/local/lib
  cp -R "$tmp/gvisor" /usr/local/lib/gvisor.new
  rm -rf /usr/local/lib/gvisor
  mv /usr/local/lib/gvisor.new /usr/local/lib/gvisor
  ln -sf /usr/local/lib/gvisor/runsc /usr/local/bin/runsc
  say "Installed gVisor: $(/usr/local/bin/runsc --version 2>/dev/null | head -n 1)"
}

backend_ready=0
if [ "$os" = Linux ]; then
  step "Checking KVM"
  if [ -e /dev/kvm ]; then
    say "/dev/kvm is present: this machine can run VMs with cloud-hypervisor"
    if [ "$deps" = 1 ]; then
      linux_deps
    fi
  else
    say "/dev/kvm is missing, so this machine can't run hardware VMs."
    say "gVisor can run Cove VMs here instead, as sandboxes with their own user-space kernel:"
    say "weaker than a VM, much stronger than a container, and no KVM needed."
    if [ "$deps" = 0 ] || [ "$gvisor" = no ]; then
      say "Not installing gVisor; installing only the CLI."
    elif [ "$(id -u)" != 0 ]; then
      warn "installing gVisor needs root; run this script with sudo to set it up"
    elif have runsc || [ "$gvisor" = yes ] || confirm "Install gVisor?"; then
      install_gvisor
      install_packages
    else
      say "Not installing gVisor; installing only the CLI."
    fi
  fi
fi
if [ "$os" = Darwin ] && [ "$deps" = 1 ]; then
  step "Checking vfkit"
  if have vfkit || [ -x /opt/homebrew/bin/vfkit ] || [ -x /usr/local/bin/vfkit ]; then
    say "vfkit is installed"
  elif have brew; then
    brew install vfkit
  else
    warn "Cove needs vfkit, which comes from Homebrew: install https://brew.sh, then run: brew install vfkit"
  fi
fi
if "$cove" doctor 2>/dev/null | grep -q '^✓ backend'; then
  backend_ready=1
fi

started=0
if [ "$service" = 1 ] && [ "$backend_ready" = 1 ]; then
  if [ "$os" = Darwin ]; then
    step "Starting cove serve at login (launchd)"
    mkdir -p "$(dirname "$plist")" "$data"
    stop_service
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$cove</string>
    <string>serve</string>
    <string>--listen</string>
    <string>$listen</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>COVE_HOME</key><string>$data</string>
    <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>$data/serve.log</string>
  <key>StandardOutPath</key><string>$data/serve.log</string>
</dict>
</plist>
EOF
    launchctl bootstrap "gui/$(id -u)" "$plist" 2>/dev/null || launchctl load -w "$plist"
    say "Installed $plist"
    started=1
  elif have systemctl; then
    step "Starting cove serve at boot (systemd)"
    mkdir -p "$(dirname "$unit")" "$data"
    wanted_by=multi-user.target
    [ -z "$systemctl_scope" ] || wanted_by=default.target
    cat >"$unit" <<EOF
[Unit]
Description=Cove: self-hosted microVMs
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=$cove serve --listen $listen
Environment=COVE_HOME=$data
Restart=on-failure
# VMs are their own processes and keep running across server restarts.
KillMode=process

[Install]
WantedBy=$wanted_by
EOF
    sysctl daemon-reload
    sysctl enable --now cove.service
    sysctl restart cove.service
    say "Installed $unit"
    if [ -n "$systemctl_scope" ]; then
      say "To keep it running after you log out: loginctl enable-linger $(id -un)"
    fi
    started=1
  else
    warn "no launchd or systemd here; start the server yourself with: $cove serve"
  fi
fi

if [ "$started" = 1 ]; then
  # Wait for the server to answer before using it.
  i=0
  until curl -fsS -o /dev/null "$local_url/v1/health" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -lt 40 ] || {
      warn "the server didn't answer at $local_url; see $data/serve.log"
      break
    }
    sleep 0.5
  done
fi

step "Checking the setup"
COVE_URL="$local_url" "$cove" doctor || true

if [ "$started" = 1 ] && [ "$image" = 1 ]; then
  if COVE_URL="$local_url" "$cove" image ls 2>/dev/null | grep -q "^$IMAGE .* pulled,"; then
    say "The $IMAGE image is already pulled"
  else
    step "Pulling the $IMAGE image (a few hundred MB, once)"
    COVE_URL="$local_url" "$cove" image pull "$IMAGE" || warn "pulling failed; retry with: cove image pull $IMAGE"
  fi
fi

step "Done"
case ":$PATH:" in
  *":$bindir:"*) ;;
  *)
    if [ -z "$linked" ]; then
      say "Add Cove to your PATH:  export PATH=\"$bindir:\$PATH\""
    fi
    ;;
esac
if [ "$backend_ready" = 1 ]; then
  if [ "$started" = 1 ]; then
    say "The server runs at $local_url. Try:  cove up dev && cove shell dev"
  else
    say "Start the server with:  cove serve"
  fi
  if [ "$os" = Darwin ]; then
    say "Runo finds this machine's Cove by itself."
  fi
  say "To use it from Runo on another machine, run \`cove key\` here and add a Cove"
  say "connection in Runo with this server's URL and that key. The API is plain HTTP on"
  say "$listen: reach it through an SSH tunnel (ssh -L 7457:127.0.0.1:7457 this-server)"
  say "or put an HTTPS reverse proxy in front before opening it to the internet."
elif [ "$os" = Linux ] && [ ! -e /dev/kvm ]; then
  say "This machine has no /dev/kvm and no working gVisor, so it can't run Cove VMs."
  say "Run this script again as root and accept gVisor (or pass --gvisor), or pick a VPS"
  say "plan with KVM. Meanwhile the cove CLI drives a Cove server elsewhere:"
  say "  COVE_API_KEY=<key from \`cove key\` there> cove --host http://server:7457 ls"
elif [ "$os" = Linux ]; then
  say "This machine has KVM, but something VMs need is missing: see \`cove doctor\` above."
  if [ "$(id -u)" != 0 ]; then
    say "Running this script as root (sudo) installs what's missing and the service."
  fi
else
  say "Fix what \`cove doctor\` reports above, then run this script again."
fi
