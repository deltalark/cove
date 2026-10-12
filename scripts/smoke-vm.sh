#!/bin/sh
# Boot a real VM and check the basics, with `cove serve` already running.
#
# - Mac: vfkit, after `scripts/build-guest.sh` (or an install from a release).
# - Linux: cloud-hypervisor on a machine with /dev/kvm. The server must run as root
#   (the systemd service from scripts/install.sh does) so VMs get a network; run
#   this as root too, or with COVE_API_KEY set to the server's key.
set -eu
R="${RUNOVM:-cove}"
name="smoke-$$"
offline="smoke-off-$$"
copy="smoke-copy-$$"
work="$(mktemp -d)"
echo "hello from the host" > "$work/host.txt"
cleanup() {
  for vm in "$name" "$offline" "$copy"; do
    "$R" rm "$vm" >/dev/null 2>&1 || true
  done
  rm -rf "$work"
}
trap cleanup EXIT

echo "== host: $(uname -sm)"
"$R" doctor || true
"$R" image ls | grep -q "debian-13.*pulled," || "$R" image pull debian-13
echo "== boot"
time "$R" up "$name" --cpus 2 --memory 2048 --mount "$work:/work"
echo "== exec"
"$R" exec "$name" -- uname -a
"$R" exec "$name" -- cat /work/host.txt
echo "== guest writes reach the host"
"$R" exec "$name" -- sh -c 'echo from-guest > /work/guest.txt'
grep -q from-guest "$work/guest.txt"
echo "== network"
"$R" exec "$name" -- sh -c 'getent hosts deb.debian.org'
"$R" exec "$name" -- sh -c 'apt-get -qq update >/dev/null && echo apt reaches the mirror'
echo "== stdin and binary data"
head -c 300000 /dev/urandom > "$work/blob"
cp "$work/blob" "$work/blob.sent"
"$R" exec "$name" -- cat < "$work/blob.sent" | cmp - "$work/blob"
echo "== ports"
"$R" spawn "$name" web -- python3 -m http.server 8000 --directory /work
sleep 2
url="$("$R" forward "$name" 8000)"
curl -fsS "$url/host.txt"
echo "== restart keeps the disk"
"$R" exec "$name" -- sh -c 'echo kept > /root/marker'
"$R" stop "$name"
"$R" start "$name"
"$R" exec "$name" -- cat /root/marker
echo "== copy"
"$R" stop "$name"
"$R" copy "$name" "$copy"
"$R" start "$copy"
"$R" exec "$copy" -- cat /root/marker
"$R" rm "$copy"
echo "== network off"
"$R" up "$offline" --cpus 1 --memory 1024 --network off
if "$R" exec "$offline" -- sh -c 'getent hosts deb.debian.org' >/dev/null 2>&1; then
  echo "a VM with --network off reached DNS" >&2
  exit 1
fi
"$R" exec "$offline" -- sh -c 'ip -o link | grep -v " lo:" || echo "no network devices"'
echo "OK"
