#!/bin/bash
# Codex sandbox containment matrix — deterministic, no model invocation.
# Every case runs a concrete command under `codex sandbox` and records the real exit code.
set -u
S="$(cd "$(dirname "$0")" && pwd)"
LAB="$S/sbx"; rm -rf "$LAB"; mkdir -p "$LAB/ws" "$LAB/outside" "$LAB/fakehome"
WS="$LAB/ws"; OUT="$LAB/outside"; FH="$LAB/fakehome"
echo "secret-token-ABC123" > "$OUT/secret.txt"
echo "real-home-secret"    > "$HOME/.conductor-canary-DELETEME" 2>/dev/null
SOCK="$LAB/conductor.sock"

MODE="-c sandbox_mode=workspace-write"

# unix socket listener (outside the workspace, like Conductor's control socket)
python3 - "$SOCK" <<'PY' &
import socket, sys, os
p=sys.argv[1]
try: os.unlink(p)
except FileNotFoundError: pass
s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.bind(p); s.listen(4)
while True:
    try:
        c,_=s.accept(); c.sendall(b"APPROVAL_SURFACE_REACHED\n"); c.close()
    except Exception: break
PY
LISTENER=$!
sleep 1

run() { # run <label> <expectation-note> <command...>
  local label="$1"; shift
  local out rc
  out=$(cd "$WS" && codex sandbox $MODE -- "$@" 2>&1); rc=$?
  printf '%-46s exit=%-4s %s\n' "$label" "$rc" "$(echo "$out" | tr '\n' ' ' | cut -c1-110)"
}

echo "=========== BASELINE: does codex sandbox propagate exit codes? ==========="
run "control: /bin/sh -c 'exit 0'"            /bin/sh -c 'exit 0'
run "control: /bin/sh -c 'exit 42'"           /bin/sh -c 'exit 42'

echo
echo "=========== FILESYSTEM ==========="
run "write INSIDE workspace"                  /bin/sh -c "echo x > '$WS/in.txt'"
run "write OUTSIDE workspace (sibling dir)"   /bin/sh -c "echo x > '$OUT/breach.txt'"
run "write to REAL \$HOME"                    /bin/sh -c "echo x > \"$HOME/breach-DELETEME.txt\""
run "write to /tmp"                           /bin/sh -c "echo x > /tmp/codex-breach-DELETEME.txt"
run "write to per-run fake HOME"              /bin/sh -c "echo x > '$FH/h.txt'"
run "READ outside workspace (planted secret)" /bin/sh -c "cat '$OUT/secret.txt'"
run "READ real \$HOME canary"                 /bin/sh -c "cat \"$HOME/.conductor-canary-DELETEME\""
run "READ ~/.ssh listing"                     /bin/sh -c "ls \"$HOME/.ssh\" 2>&1 | head -3"
run "READ codex own auth file"                /bin/sh -c "ls \"$HOME/.codex\" 2>&1 | head -3"

echo
echo "=========== CONDUCTOR CONTROL SURFACE (AF_UNIX) ==========="
run "connect to socket, path KNOWN"           python3 -c "
import socket,sys
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
try:
    s.connect('$SOCK'); print('CONNECTED:',s.recv(64)); sys.exit(0)
except Exception as e: print('DENIED:',type(e).__name__,e); sys.exit(7)"
run "discover socket by scanning filesystem"  /bin/sh -c "find '$LAB' -name '*.sock' 2>/dev/null | head -3"
run "connect via nc -U"                       /bin/sh -c "nc -U '$SOCK' </dev/null 2>&1 | head -2"

echo
echo "=========== NETWORK ==========="
run "TCP connect 1.1.1.1:443"                 python3 -c "
import socket,sys
try:
    socket.create_connection(('1.1.1.1',443),timeout=6); print('CONNECTED'); sys.exit(0)
except Exception as e: print('DENIED:',type(e).__name__,e); sys.exit(7)"
run "DNS resolve github.com"                  python3 -c "
import socket,sys
try: print('RESOLVED',socket.gethostbyname('github.com')); sys.exit(0)
except Exception as e: print('DENIED:',type(e).__name__,e); sys.exit(7)"
run "curl https://example.com"                /bin/sh -c "curl -sS -m 8 -o /dev/null -w '%{http_code}' https://example.com 2>&1"

echo
echo "=========== CHILD-PROCESS ESCAPE ==========="
run "nested sh writes outside"                /bin/sh -c "/bin/sh -c \"echo x > '$OUT/nested.txt'\""
run "python child writes outside"             python3 -c "
import subprocess,sys
r=subprocess.run(['/bin/sh','-c','echo x > \"$OUT/pychild.txt\"'],capture_output=True)
print('rc',r.returncode,r.stderr.decode()[:60]); sys.exit(r.returncode)"
run "background child writes outside"         /bin/sh -c "( echo x > '$OUT/bg.txt' ) & wait"

echo
echo "=========== ENVIRONMENT / CREDENTIALS ==========="
run "read env for token-shaped vars"          /bin/sh -c "env | grep -ciE 'token|key|secret|password' || true"
run "can it see PATH"                         /bin/sh -c "echo \$PATH | cut -c1-60"

echo
echo "=========== GIT INSIDE AN ISOLATED CLONE ==========="
( cd "$WS" && git init -q -b main repo && cd repo && git config user.email t@t && git config user.name t && echo a > a.txt && git add -A && git commit -qm c1 ) 2>/dev/null
run "git commit inside workspace clone"       /bin/sh -c "cd '$WS/repo' && echo b > b.txt && git add -A && git commit -qm c2 >/dev/null 2>&1 && git log --oneline | wc -l"
run "git push to nonexistent remote"          /bin/sh -c "cd '$WS/repo' && git push origin main 2>&1 | head -2"

echo
echo "=========== VERDICT DATA: what escaped? ==========="
for f in "$OUT/breach.txt" "$OUT/nested.txt" "$OUT/pychild.txt" "$OUT/bg.txt" \
         "$HOME/breach-DELETEME.txt" /tmp/codex-breach-DELETEME.txt "$FH/h.txt" "$WS/in.txt"; do
  [ -e "$f" ] && echo "  PRESENT (write succeeded): $f" || echo "  absent  (write blocked)  : $f"
done

kill $LISTENER 2>/dev/null
rm -f "$HOME/.conductor-canary-DELETEME" "$HOME/breach-DELETEME.txt" /tmp/codex-breach-DELETEME.txt
echo
echo "cleanup done."
