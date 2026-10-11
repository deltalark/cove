#!/bin/sh
# Boot a real VM and check the basics. Run on a Mac with vfkit, after
# `scripts/build-guest.sh`, with `cove serve` running in another terminal.
set -eu
R="${RUNOVM:-cove}"
name="smoke-$$"
work="$(mktemp -d)"
echo "hello from the host" > "$work/host.txt"

"$R" doctor || true
"$R" image ls | grep -q "debian-13.*pulled," || "$R" image pull debian-13
echo "== boot"
time "$R" up "$name" --cpus 2 --memory 2048 --mount "$work:/work"
trap '"$R" rm "$name" >/dev/null 2>&1 || true' EXIT
echo "== exec"
"$R" exec "$name" -- uname -a
"$R" exec "$name" -- cat /work/host.txt
echo "== guest writes reach the host"
"$R" exec "$name" -- sh -c 'echo from-guest > /work/guest.txt'
grep -q from-guest "$work/guest.txt"
echo "== network"
"$R" exec "$name" -- sh -c 'getent hosts deb.debian.org'
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
echo "OK"
