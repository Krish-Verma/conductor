#!/bin/bash
# Git workspace isolation experiment — Conductor architecture convergence pass.
# Question: does `git clone` (local, hardlinked) provide an adversarial isolation
# boundary against a same-user agent, or can the clone corrupt the source repo?
set -u

BASE="$(dirname "$0")/gitexp"
rm -rf "$BASE"; mkdir -p "$BASE"; cd "$BASE" || exit 1
BASE="$(pwd)"

hr() { echo; echo "================ $* ================"; }

# stat helpers (macOS)
inode() { stat -f '%i' "$1"; }
nlink() { stat -f '%l' "$1"; }

hr "0. BUILD SOURCE REPO (loose objects + one pack)"
mkdir src && cd src || exit 1
git init -q -b main .
git config user.email t@t; git config user.name t
# incompressible content so pack size is meaningful
for i in $(seq 1 60); do head -c 60000 /dev/urandom | base64 > "f$i.bin"; done
git add -A && git commit -qm "c1"
git gc -q                                  # force a pack to exist
for i in $(seq 61 70); do head -c 60000 /dev/urandom | base64 > "f$i.bin"; done
git add -A && git commit -qm "c2 (loose objects after gc)"
SRC="$BASE/src"
echo "source: $SRC"
git count-objects -v
echo "source .git size: $(du -sh .git | cut -f1)"
SRC_HEAD=$(git rev-parse HEAD)
echo "source HEAD: $SRC_HEAD"

# ---------------------------------------------------------------- Experiment A
hr "A. NORMAL LOCAL CLONE (default: hardlinks)"
cd "$BASE" || exit 1
TA=$( { /usr/bin/time -p git clone -q --no-checkout "$SRC" clone-a ; } 2>&1 | awk '/^real/{print $2}')
echo "clone-a wall time: ${TA}s"
echo "clone-a .git size (du, counts hardlinks once per link): $(du -sh clone-a/.git | cut -f1)"
echo "clone-a apparent size (du -A style via find+stat sum):"
find clone-a/.git/objects -type f -exec stat -f '%z' {} + 2>/dev/null | awk '{s+=$1} END{printf "  %.1f MB\n", s/1048576}'

echo
echo "--- loose object inode comparison (source vs clone-a) ---"
SRC_LOOSE=$(find "$SRC/.git/objects" -type f -path '*/??/*' | head -1)
if [ -n "$SRC_LOOSE" ]; then
  REL=${SRC_LOOSE#"$SRC"/.git/objects/}
  CLONE_LOOSE="clone-a/.git/objects/$REL"
  echo "  source loose : $REL"
  echo "    inode=$(inode "$SRC_LOOSE") nlink=$(nlink "$SRC_LOOSE")"
  if [ -f "$CLONE_LOOSE" ]; then
    echo "    clone inode=$(inode "$CLONE_LOOSE") nlink=$(nlink "$CLONE_LOOSE")"
    if [ "$(inode "$SRC_LOOSE")" = "$(inode "$CLONE_LOOSE")" ]; then
      echo "    >>> SAME INODE — HARDLINKED (shared storage)"
    else
      echo "    >>> different inode — copied"
    fi
  else
    echo "    clone has no such loose object (may be packed)"
  fi
fi

echo
echo "--- pack file inode comparison (source vs clone-a) ---"
SRC_PACK=$(find "$SRC/.git/objects/pack" -name '*.pack' | head -1)
if [ -n "$SRC_PACK" ]; then
  PREL=$(basename "$SRC_PACK")
  CLONE_PACK="clone-a/.git/objects/pack/$PREL"
  echo "  $PREL"
  echo "    source inode=$(inode "$SRC_PACK") nlink=$(nlink "$SRC_PACK") size=$(stat -f '%z' "$SRC_PACK")"
  if [ -f "$CLONE_PACK" ]; then
    echo "    clone  inode=$(inode "$CLONE_PACK") nlink=$(nlink "$CLONE_PACK")"
    [ "$(inode "$SRC_PACK")" = "$(inode "$CLONE_PACK")" ] \
      && echo "    >>> SAME INODE — PACK IS HARDLINKED" \
      || echo "    >>> different inode"
  fi
fi

# ------------------------------------------------- Adversarial mutation, clone A
hr "A2. ADVERSARIAL IN-PLACE MUTATION FROM INSIDE clone-a"
echo "Pre-state: source fsck"
git -C "$SRC" fsck --no-progress 2>&1 | head -5; echo "  source fsck exit=$?"

TARGET=""
if [ -n "${CLONE_LOOSE:-}" ] && [ -f "$CLONE_LOOSE" ] \
   && [ "$(inode "$SRC_LOOSE")" = "$(inode "$CLONE_LOOSE")" ]; then
  TARGET="$CLONE_LOOSE"; KIND="loose object"
elif [ -f "${CLONE_PACK:-}" ] && [ "$(inode "$SRC_PACK")" = "$(inode "$CLONE_PACK")" ]; then
  TARGET="$CLONE_PACK"; KIND="pack file"
fi

if [ -n "$TARGET" ]; then
  echo "Target ($KIND): $TARGET"
  echo "  perms before: $(stat -f '%Sp' "$TARGET")"
  # An agent running as the same user owns the file and may chmod it.
  chmod u+w "$TARGET" 2>&1 && echo "  chmod u+w: OK (same user owns it)"
  echo "  writing 16 garbage bytes at offset 8, IN PLACE (conv=notrunc)..."
  printf 'CORRUPTEDBYAGENT' | dd of="$TARGET" bs=1 seek=8 conv=notrunc 2>/dev/null \
    && echo "  write: OK" || echo "  write: FAILED"

  echo
  echo ">>> EFFECT ON *SOURCE* REPOSITORY (the user's repo):"
  echo "  source fsck:"
  git -C "$SRC" fsck --no-progress 2>&1 | head -8
  echo "  source fsck exit=${PIPESTATUS[0]}"
  echo "  source: can we still read HEAD tree?"
  git -C "$SRC" log --oneline -1 2>&1 | head -3
  git -C "$SRC" cat-file -p "$SRC_HEAD" >/dev/null 2>&1 \
    && echo "  cat-file HEAD: OK" || echo "  cat-file HEAD: FAILED"
  echo "  source: full object read (git cat-file --batch-all-objects)"
  git -C "$SRC" cat-file --batch-all-objects --batch-check >/dev/null 2>&1 \
    && echo "    all objects readable: OK" || echo "    >>> SOURCE OBJECT READ FAILURE"
else
  echo "No shared-inode target found — nothing to corrupt."
fi

hr "A3. DOES DELETING AN OBJECT IN THE CLONE AFFECT SOURCE?"
DEL=$(find clone-a/.git/objects -type f -path '*/??/*' | head -1)
if [ -n "$DEL" ]; then
  DREL=${DEL#clone-a/.git/objects/}
  rm -f "$DEL"
  if [ -f "$SRC/.git/objects/$DREL" ]; then
    echo "  deleted in clone; source copy STILL PRESENT (unlink only decrements link count) — SAFE"
  else
    echo "  >>> source object GONE — UNSAFE"
  fi
fi

# ---------------------------------------------------------------- Experiment B
hr "B. git clone --no-hardlinks"
cd "$BASE" || exit 1
# rebuild a pristine source (A2 corrupted the first one)
rm -rf src2 && cp -R src src2 2>/dev/null
# repair src2 from src's pre-corruption? simpler: build fresh
rm -rf src2 && mkdir src2 && cd src2 || exit 1
git init -q -b main .; git config user.email t@t; git config user.name t
for i in $(seq 1 60); do head -c 60000 /dev/urandom | base64 > "f$i.bin"; done
git add -A && git commit -qm "c1"; git gc -q
for i in $(seq 61 70); do head -c 60000 /dev/urandom | base64 > "f$i.bin"; done
git add -A && git commit -qm "c2"
SRC2="$BASE/src2"
cd "$BASE" || exit 1

TB=$( { /usr/bin/time -p git clone -q --no-hardlinks --no-checkout "$SRC2" clone-b ; } 2>&1 | awk '/^real/{print $2}')
echo "clone-b wall time: ${TB}s"
B_PACK=$(find clone-b/.git/objects/pack -name '*.pack' | head -1)
S2_PACK=$(find "$SRC2/.git/objects/pack" -name '*.pack' | head -1)
if [ -n "$B_PACK" ] && [ -n "$S2_PACK" ]; then
  echo "  src2 pack  inode=$(inode "$S2_PACK") nlink=$(nlink "$S2_PACK")"
  echo "  clone-b pack inode=$(inode "$B_PACK") nlink=$(nlink "$B_PACK")"
  [ "$(inode "$S2_PACK")" = "$(inode "$B_PACK")" ] \
    && echo "  >>> SAME INODE — still hardlinked (unexpected)" \
    || echo "  >>> DIFFERENT INODE — independent copy (isolated)"
fi
B_LOOSE=$(find clone-b/.git/objects -type f -path '*/??/*' | head -1)
if [ -n "$B_LOOSE" ]; then
  BREL=${B_LOOSE#clone-b/.git/objects/}
  if [ -f "$SRC2/.git/objects/$BREL" ]; then
    echo "  loose: src2 inode=$(inode "$SRC2/.git/objects/$BREL") clone-b inode=$(inode "$B_LOOSE")"
    [ "$(inode "$SRC2/.git/objects/$BREL")" = "$(inode "$B_LOOSE")" ] \
      && echo "  >>> loose SAME INODE" || echo "  >>> loose DIFFERENT INODE — isolated"
  fi
fi

echo
echo "B2. adversarial mutation from clone-b:"
if [ -n "$B_PACK" ]; then
  chmod u+w "$B_PACK"; printf 'CORRUPTEDBYAGENT' | dd of="$B_PACK" bs=1 seek=8 conv=notrunc 2>/dev/null
  echo "  corrupted clone-b pack. src2 fsck:"
  git -C "$SRC2" fsck --no-progress 2>&1 | head -5
  echo "  src2 fsck exit=${PIPESTATUS[0]}"
  git -C "$SRC2" cat-file --batch-all-objects --batch-check >/dev/null 2>&1 \
    && echo "  >>> src2 fully readable — SOURCE UNAFFECTED" \
    || echo "  >>> src2 damaged"
fi

# ---------------------------------------------------------------- Experiment C
hr "C. git clone --no-local (transport, no filesystem sharing at all)"
cd "$BASE" || exit 1
rm -rf src3 && mkdir src3 && cd src3 || exit 1
git init -q -b main .; git config user.email t@t; git config user.name t
for i in $(seq 1 60); do head -c 60000 /dev/urandom | base64 > "f$i.bin"; done
git add -A && git commit -qm c1; git gc -q
SRC3="$BASE/src3"; cd "$BASE" || exit 1
TC=$( { /usr/bin/time -p git clone -q --no-local --no-checkout "$SRC3" clone-c ; } 2>&1 | awk '/^real/{print $2}')
echo "clone-c wall time: ${TC}s"

# ---------------------------------------------------------------- Sizes
hr "D. COST SUMMARY"
printf "%-34s %-10s %-12s\n" "variant" "wall(s)" "objects size"
objsize() { find "$1" -type f -exec stat -f '%z' {} + 2>/dev/null | awk '{s+=$1} END{printf "%.1f MB", s/1048576}'; }
printf "%-34s %-10s %-12s\n" "source repo (.git/objects)"  "-"    "$(objsize "$SRC3/.git/objects")"
printf "%-34s %-10s %-12s\n" "clone default (hardlink)"    "$TA"  "$(objsize clone-a/.git/objects)"
printf "%-34s %-10s %-12s\n" "clone --no-hardlinks"        "$TB"  "$(objsize clone-b/.git/objects)"
printf "%-34s %-10s %-12s\n" "clone --no-local"            "$TC"  "$(objsize clone-c/.git/objects)"
echo
echo "NOTE: 'objects size' is the apparent byte total. For the hardlinked clone those"
echo "bytes are NOT new disk usage — they are the same blocks as the source."
echo "Real incremental disk (du, which counts each inode once per traversal):"
echo "  clone-a: $(du -sh clone-a/.git/objects | cut -f1)   clone-b: $(du -sh clone-b/.git/objects | cut -f1)"

hr "DONE"
