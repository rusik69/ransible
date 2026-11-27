#!/usr/bin/env bash
# Differential e2e: run each case with real ansible-playbook and with rancible against
# fresh containers; compare recap (changed/failed/unreachable), remote state snapshot and
# idempotence (second run changed=0).
#   tests/e2e/run.sh [case ...]       (default: all cases)    STRICT=1 also compares ok/skipped
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
WORK="$HERE/.work"; mkdir -p "$WORK"
RANCIBLE="${RANCIBLE:-$ROOT/target/debug/rancible}"
IMAGE=rancible-target; N=2; BASEPORT=2220
command -v podman >/dev/null || { echo "podman required"; exit 2; }
podman info >/dev/null 2>&1 || podman machine start >/dev/null 2>&1
[ -x "$RANCIBLE" ] || (cd "$ROOT" && cargo build -q) || exit 2

[ -f "$WORK/key" ] || ssh-keygen -q -t ed25519 -N "" -f "$WORK/key"
if ! podman image exists $IMAGE; then
  cp "$HERE/Dockerfile" "$WORK/Dockerfile"; cp "$WORK/key.pub" "$WORK/key.pub.ctx" 2>/dev/null
  (cd "$WORK" && podman build -q -t $IMAGE -f Dockerfile . >/dev/null) || exit 2
fi

reset() {
  for i in $(seq 1 $N); do
    podman rm -f "rt$i" >/dev/null 2>&1
    podman run -d --systemd=always --name "rt$i" -p "$((BASEPORT+i)):22" $IMAGE >/dev/null
  done
  for i in $(seq 1 $N); do for _ in $(seq 1 30); do ssh -q -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "$WORK/key" -p "$((BASEPORT+i))" deploy@127.0.0.1 true 2>/dev/null && break; sleep 1; done; done
}
inventory() {
  { echo "[app]"; for i in $(seq 1 $N); do echo "c$i ansible_host=127.0.0.1 ansible_port=$((BASEPORT+i))"; done
    echo "[all:vars]"; echo "ansible_user=deploy"; echo "ansible_ssh_private_key_file=$WORK/key"
    echo "ansible_ssh_common_args='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null'"
    echo "ansible_python_interpreter=/usr/bin/python3"; } > "$WORK/inventory.ini"
}
recap() { # normalized "host ok changed unreachable failed skipped"
  sed -n '/PLAY RECAP/,$p' "$1" | grep -E '^c[0-9]+ +:' |
    sed -E 's/ +/ /g; s/ : / /; s/(ok|changed|unreachable|failed|skipped|rescued|ignored)=//g' | awk '{print $1,$2,$3,$4,$5,$6}' | sort
}
field() { awk -v f="$2" '{print $1, $f}' <<<"$1"; }
snapshot() {
  local snap="$1"
  [ -f "$snap" ] || return 0
  for i in $(seq 1 $N); do echo "## c$i"; podman exec -i "rt$i" bash -s < "$snap" 2>&1; done
}
run_tool() { # tool case_dir playbook logfile
  local tool="$1" dir="$2" pb="$3" log="$4"
  ( cd "$dir"
    if [ "$tool" = ansible ]; then
      ANSIBLE_NOCOLOR=1 ANSIBLE_PIPELINING=True ANSIBLE_HOST_KEY_CHECKING=False ansible-playbook -i "$WORK/inventory.ini" "$pb" ${EXTRA:-} > "$log" 2>&1
    else
      NO_COLOR=1 "$RANCIBLE" playbook "$pb" -i "$WORK/inventory.ini" ${EXTRA:-} > "$log" 2>&1
    fi )
}

cases=("$@"); [ ${#cases[@]} -eq 0 ] && cases=($(ls "$HERE/cases"))
inventory
fail=0; summary=()
for c in "${cases[@]}"; do
  dir="$HERE/cases/$c"; [ -d "$dir" ] || { echo "no such case $c"; fail=1; continue; }
  [ -x "$dir/setup.sh" ] && (cd "$dir" && ./setup.sh >/dev/null 2>&1)
  pb="$(cat "$dir/playbook" 2>/dev/null || echo site.yml)"
  echo "=== case: $c"
  for tool in ansible rancible; do
    reset
    run_tool $tool "$dir" "$pb" "$WORK/$c.$tool.1.log"
    snapshot "$dir/snapshot.sh" > "$WORK/$c.$tool.snap"
    run_tool $tool "$dir" "$pb" "$WORK/$c.$tool.2.log"
  done
  ok=1; msgs=""
  for run in 1 2; do
    a="$(recap "$WORK/$c.ansible.$run.log")"; r="$(recap "$WORK/$c.rancible.$run.log")"
    if [ -z "$r" ]; then ok=0; msgs+=" run$run:rancible-produced-no-recap($(tail -1 "$WORK/$c.rancible.$run.log" | cut -c1-120))"; continue; fi
    cols="3 4 5"; [ "${STRICT:-0}" = 1 ] && cols="2 3 4 5 6"
    for f in $cols; do
      [ "$(field "$a" $f)" = "$(field "$r" $f)" ] || { ok=0; msgs+=" run$run:col$f-differs"; }
    done
  done
  if ! diff -q "$WORK/$c.ansible.snap" "$WORK/$c.rancible.snap" >/dev/null; then ok=0; msgs+=" snapshot-differs"; fi
  if [ $ok = 1 ]; then summary+=("PASS  $c"); else summary+=("FAIL  $c ($msgs )"); fail=1; fi
done
echo; printf '%s\n' "${summary[@]}"
podman rm -f $(seq -f "rt%g" 1 $N) >/dev/null 2>&1
exit $fail
