#!/bin/bash
# Round 2 — corrects two flawed tests from round 1:
#   (1) "outside workspace" was itself under /tmp, which workspace-write permits.
#       Retest against non-/tmp locations.
#   (2) AF_UNIX test failed on sun_path length (>104 chars), not on the sandbox.
#       Retest with short socket paths, inside AND outside the permitted area.
set -u
MODE="-c sandbox_mode=workspace-write"
WS="$HOME/Documents/cnd-lab/ws"          # workspace under $HOME, like a real project
OUTHOME="$HOME/Documents/cnd-lab/outside" # sibling, under $HOME, NOT under /tmp
rm -rf "$HOME/Documents/cnd-lab"; mkdir -p "$WS" "$OUTHOME"
echo "planted" > "$OUTHOME/secret.txt"

run() { local label="$1"; shift; local out rc
  out=$(cd "$WS" && codex sandbox $MODE -- "$@" 2>&1); rc=$?
  printf '%-52s exit=%-4s %s\n' "$label" "$rc" "$(echo "$out" | tr '\n' ' ' | cut -c1-100)"; }

echo "workspace = $WS   (cwd for every run below)"
echo "=========== A. WRITABLE ROOTS: where can workspace-write actually write? ==========="
run "write in workspace (cwd)"                 /bin/sh -c "echo x > '$WS/a.txt'"
run "write SIBLING dir under \$HOME"           /bin/sh -c "echo x > '$OUTHOME/breach.txt'"
run "write \$HOME root"                        /bin/sh -c "echo x > '$HOME/breach2.txt'"
run "write \$HOME/Documents root"              /bin/sh -c "echo x > '$HOME/Documents/breach3.txt'"
run "write /tmp"                               /bin/sh -c "echo x > /tmp/cnd-b4.txt"
run "write \$TMPDIR"                           /bin/sh -c "echo x > \"\$TMPDIR/cnd-b5.txt\""
run "write /Users/Shared"                      /bin/sh -c "echo x > /Users/Shared/cnd-b6.txt"
run "write ~/.codex (agent's own auth dir)"    /bin/sh -c "echo x > '$HOME/.codex/cnd-b7.txt'"
run "write ~/.ssh"                             /bin/sh -c "echo x > '$HOME/.ssh/cnd-b8.txt'"
run "write ~/.claude (other agent's config)"   /bin/sh -c "echo x > '$HOME/.claude/cnd-b9.txt'"

echo
echo "=========== B. READS (round 1 said unrestricted — confirm) ==========="
run "read sibling secret"                      /bin/sh -c "cat '$OUTHOME/secret.txt'"
run "read ~/.codex/auth.json (first 40 bytes)" /bin/sh -c "head -c 40 '$HOME/.codex/auth.json' 2>&1"
run "read ~/.ssh/known_hosts (line count)"     /bin/sh -c "wc -l < '$HOME/.ssh/known_hosts' 2>&1"

echo
echo "=========== C. AF_UNIX CONTROL SURFACE (short paths — round 1 was invalid) ==========="
for LOC in "/tmp/cndA.sock" "$HOME/cndB.sock" "$HOME/.conductor-cndC.sock"; do
  rm -f "$LOC"
  python3 - "$LOC" <<'PY' &
import socket,sys,os
p=sys.argv[1]
try: os.unlink(p)
except FileNotFoundError: pass
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.bind(p); s.listen(2); os.chmod(p,0o600)
try:
    c,_=s.accept(); c.sendall(b"APPROVAL_GRANTED\n"); c.close()
except Exception: pass
PY
  LP=$!; sleep 0.8
  if [ -S "$LOC" ]; then
    run "AF_UNIX connect -> $LOC" python3 -c "
import socket,sys
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
try:
    s.connect('$LOC'); print('CONNECTED:',s.recv(32)); sys.exit(0)
except Exception as e: print('DENIED:',type(e).__name__,e); sys.exit(7)"
  else
    echo "  (listener failed to bind at $LOC)"
  fi
  kill $LP 2>/dev/null; rm -f "$LOC"
done

echo
echo "=========== D. EXPLICIT ALLOW FLAG (proves default-deny is real) ==========="
rm -f /tmp/cndD.sock
python3 - /tmp/cndD.sock <<'PY' &
import socket,sys,os
p=sys.argv[1]
try: os.unlink(p)
except FileNotFoundError: pass
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.bind(p); s.listen(2)
try:
    c,_=s.accept(); c.sendall(b"ALLOWED_VIA_FLAG\n"); c.close()
except Exception: pass
PY
LP=$!; sleep 0.8
out=$(cd "$WS" && codex sandbox $MODE --allow-unix-socket /tmp/cndD.sock -- python3 -c "
import socket,sys
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
try:
    s.connect('/tmp/cndD.sock'); print('CONNECTED:',s.recv(32)); sys.exit(0)
except Exception as e: print('DENIED:',type(e).__name__,e); sys.exit(7)" 2>&1); rc=$?
printf '%-52s exit=%-4s %s\n' "with --allow-unix-socket /tmp/cndD.sock" "$rc" "$(echo "$out"|tr '\n' ' '|cut -c1-100)"
kill $LP 2>/dev/null; rm -f /tmp/cndD.sock

echo
echo "=========== E. CHILD ESCAPE at a non-/tmp path ==========="
run "nested sh writes to \$HOME sibling"        /bin/sh -c "/bin/sh -c \"echo x > '$OUTHOME/nested.txt'\""

echo
echo "=========== F. WHAT ACTUALLY LANDED ==========="
for f in "$WS/a.txt" "$OUTHOME/breach.txt" "$OUTHOME/nested.txt" "$HOME/breach2.txt" \
         "$HOME/Documents/breach3.txt" /tmp/cnd-b4.txt /Users/Shared/cnd-b6.txt \
         "$HOME/.codex/cnd-b7.txt" "$HOME/.ssh/cnd-b8.txt" "$HOME/.claude/cnd-b9.txt"; do
  [ -e "$f" ] && echo "  ESCAPED : $f" || echo "  blocked : $f"
done

rm -rf "$HOME/Documents/cnd-lab"
rm -f "$HOME/breach2.txt" "$HOME/Documents/breach3.txt" /tmp/cnd-b4.txt /Users/Shared/cnd-b6.txt \
      "$HOME/.codex/cnd-b7.txt" "$HOME/.ssh/cnd-b8.txt" "$HOME/.claude/cnd-b9.txt" 2>/dev/null
echo "cleanup done."
