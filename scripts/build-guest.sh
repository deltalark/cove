#!/bin/sh
# Build the guest agent for Linux VMs (static musl) and install it where cove looks.
#
# On a Mac this cross-compiles with cargo-zigbuild:
#   brew install zig && cargo install cargo-zigbuild
set -eu
cd "$(dirname "$0")/.."
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
target="$arch-unknown-linux-musl"
rustup target add "$target" >/dev/null
if [ "$(uname -s)" = Linux ] && command -v musl-gcc >/dev/null 2>&1; then
  cargo build --release -p cove-guest --target "$target"
else
  cargo zigbuild --release -p cove-guest --target "$target"
fi
dest="${COVE_HOME:-$HOME/.cove}/guest"
mkdir -p "$dest"
cp "target/$target/release/cove-guest" "$dest/cove-guest-linux-$arch"
echo "Installed $dest/cove-guest-linux-$arch"
