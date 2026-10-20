#!/bin/sh
# Boot a real VM and check the basics, with `cove serve` already running.
#
# - Mac: vfkit, after `scripts/build-guest.sh` (or an install from a release).
# - Linux: cloud-hypervisor on a machine with /dev/kvm, or gVisor without it (the
#   backend `auto` picks; set COVE_BACKEND to choose). The server must run as root
#   (the systemd service from scripts/install.sh does) so VMs get a network; run
#   this as root too, or with COVE_API_KEY set to the server's key.
set -eu
R="${RUNOVM:-cove}"
name="smoke-$$"
offline="smoke-off-$$"
copy="smoke-copy-$$"
snap="smoke-snap-$$"
fromsnap="smoke-from-$$"
work="$(mktemp -d)"
echo "hello from the host" > "$work/host.txt"
cleanup() {
  for vm in "$name" "$offline" "$copy" "$fromsnap"; do
    "$R" rm "$vm" >/dev/null 2>&1 || true
  done
  "$R" snapshot rm "$snap" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

echo "== host: $(uname -sm)"
"$R" doctor || true
"$R" image ls | grep -q "debian-13.*pulled," || "$R" image pull debian-13
echo "== boot"
started=$(date +%s)
"$R" up "$name" --backend "${COVE_BACKEND:-auto}" --cpus 2 --memory 2048 --mount "$work:/work"
echo "booted in $(($(date +%s) - started))s"
"$R" show "$name" | grep '"backend"'
echo "== exec"
"$R" exec "$name" -- uname -a
"$R" exec "$name" -- cat /work/host.txt
echo "== guest writes reach the host"
"$R" exec "$name" -- sh -c 'echo from-guest > /work/guest.txt'
grep -q from-guest "$work/guest.txt"
# COVE_SMOKE_OFFLINE=1 skips this on a host that has no internet itself.
if [ "${COVE_SMOKE_OFFLINE:-0}" != 1 ]; then
  echo "== network"
  "$R" exec "$name" -- sh -c 'getent hosts deb.debian.org'
  "$R" exec "$name" -- sh -c 'apt-get -qq update >/dev/null && echo apt reaches the mirror'
fi
echo "== stdin and binary data"
head -c 300000 /dev/urandom > "$work/blob"
cp "$work/blob" "$work/blob.sent"
"$R" exec "$name" -- cat < "$work/blob.sent" | cmp - "$work/blob"
echo "== ports"
# Perl is in every Debian image, including gVisor's minimal rootfs.
cat > "$work/serve.pl" <<'EOF'
use IO::Socket::INET;
my $s = IO::Socket::INET->new(LocalAddr => '0.0.0.0:8000', Listen => 5, ReuseAddr => 1) or die;
while (my $c = $s->accept) { <$c>; open my $f, '<', '/work/host.txt'; print $c "HTTP/1.0 200 OK\r\n\r\n", <$f>; close $c }
EOF
"$R" spawn "$name" web -- perl /work/serve.pl
sleep 2
url="$("$R" forward "$name" 8000)"
curl -fsS "$url/host.txt"
echo "== pause and resume"
"$R" spawn "$name" ticker -- sh -c 'while true; do date +%s%N > /root/tick; sleep 0.2; done'
sleep 1
"$R" pause "$name" | grep -q paused
# Paused on purpose, the API waits for resume (cove exec would resume it).
if "$R" ports "$name" >/dev/null 2>&1; then
  echo "a paused VM answered through the API" >&2
  exit 1
fi
"$R" resume "$name" >/dev/null
t1="$("$R" exec "$name" -- cat /root/tick)"
sleep 1
t2="$("$R" exec "$name" -- cat /root/tick)"
[ "$t1" != "$t2" ] || { echo "processes didn't carry on after resume" >&2; exit 1; }
"$R" procs "$name" | grep -q "ticker *running"
"$R" kill "$name" ticker
echo "== snapshot while running"
"$R" exec "$name" -- sh -c 'echo in-the-snapshot > /root/snap-marker'
"$R" snapshot save "$name" --name "$snap"
"$R" exec "$name" -- true
"$R" up "$fromsnap" --snapshot "$snap" --backend "${COVE_BACKEND:-auto}" --cpus 1 --memory 1024
"$R" exec "$fromsnap" -- cat /root/snap-marker
"$R" rm "$fromsnap"
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
"$R" up "$offline" --backend "${COVE_BACKEND:-auto}" --cpus 1 --memory 1024 --network off
if "$R" exec "$offline" -- sh -c 'getent hosts deb.debian.org' >/dev/null 2>&1; then
  echo "a VM with --network off reached DNS" >&2
  exit 1
fi
"$R" exec "$offline" -- cat /proc/net/dev
echo "OK"
