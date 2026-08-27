#!/usr/bin/env bash
# Verify every package checksum in the ratatex audit lock against
# crates.io artifacts (static.crates.io), using cargo's local cache when present.
set -u
LOCK="/Users/andrewmiller/projects/atom/.audit/ratatex/Cargo.lock"
OUT="/Users/andrewmiller/projects/atom/.audit/verify_results.tsv"
DLDIR="/Users/andrewmiller/projects/atom/.audit/dl"
CACHE=$(ls -d ~/.cargo/registry/cache/index.crates.io-* 2>/dev/null | head -1)
mkdir -p "$DLDIR"
: > "$OUT"

# Parse lock into name<TAB>version<TAB>checksum
awk '
/^name = /    { gsub(/[",]/,""); n=$3 }
/^version = / { gsub(/[",]/,""); v=$3 }
/^checksum = /{ gsub(/[",]/,""); print n "\t" v "\t" $3 }
' "$LOCK" > /Users/andrewmiller/projects/atom/.audit/lock_triples.tsv

total=$(wc -l < /Users/andrewmiller/projects/atom/.audit/lock_triples.tsv)
echo "packages in lock: $total"

verify_one() {
  local name="$1" ver="$2" want="$3"
  local fname="${name}-${ver}.crate"
  local path=""
  # 1) cargo cache
  if [ -n "$CACHE" ] && [ -f "$CACHE/$fname" ]; then
    path="$CACHE/$fname"
  else
    # 2) previously downloaded
    if [ ! -s "$DLDIR/$fname" ]; then
      curl -sf --max-time 20 --retry 2 \
        "https://static.crates.io/crates/${name}/${fname}" -o "$DLDIR/$fname" || return 2
    fi
    path="$DLDIR/$fname"
  fi
  local got
  got=$(shasum -a 256 "$path" | awk '{print $1}')
  if [ "$got" = "$want" ]; then
    echo -e "OK\t${name}\t${ver}\tcache:$([ "$path" = "$CACHE/$fname" ] && echo 1 || echo 0)" >> "$OUT"
    return 0
  else
    echo -e "MISMATCH\t${name}\t${ver}\t$got" >> "$OUT"
    return 1
  fi
}
export -f verify_one
export CACHE DLDIR OUT

fail=0
while IFS=$'\t' read -r n v c; do
  verify_one "$n" "$v" "$c" || fail=$((fail+1))
done < /Users/andrewmiller/projects/atom/.audit/lock_triples.tsv

echo "done: $(grep -c '^OK' "$OUT") OK, $(grep -c '^MISMATCH' "$OUT") mismatch, $fail failures"