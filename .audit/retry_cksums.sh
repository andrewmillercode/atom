#!/usr/bin/env bash
# Retry checksum verification for packages not covered by cache or first pass.
set -u
OUT="/Users/andrewmiller/projects/atom/.audit/verify_results.tsv"
DLDIR="/Users/andrewmiller/projects/atom/.audit/dl"
TRIPLES="/Users/andrewmiller/projects/atom/.audit/lock_triples.tsv"
mkdir -p "$DLDIR"

verify_one() {
  local name="$1" ver="$2" want="$3"
  local fname="${name}-${ver}.crate"
  local path="$DLDIR/$fname"
  # cache still counts
  local CACHE=$(ls -d ~/.cargo/registry/cache/index.crates.io-* 2>/dev/null | head -1)
  if [ -n "$CACHE" ] && [ -f "$CACHE/$fname" ]; then
    path="$CACHE/$fname"
  else
    [ -s "$path" ] || return 2
  fi
  local got
  got=$(shasum -a 256 "$path" | awk '{print $1}')
  [ "$got" = "$want" ]
}

pass=0
while IFS=$'\t' read -r n v c; do
  # skip ones already OK
  if grep -qP "^OK\t${n//./\\.}\t${v}\t" "$OUT" 2>/dev/null; then continue; fi
  fname="${n}-${v}.crate"
  ok=0
  for attempt in 1 2 3 4 5; do
    if ! [ -s "$DLDIR/$fname" ]; then
      curl -sfL --max-time 25 --retry 2 --retry-all-errors \
        "https://static.crates.io/crates/${n}/${fname}" -o "$DLDIR/$fname" 2>/dev/null
    fi
    if verify_one "$n" "$v" "$c"; then
      echo -e "OK\t${n}\t${v}\tnet" >> "$OUT"
      ok=1; break
    fi
    sleep 2
  done
  [ $ok -eq 1 ] || echo -e "FAIL\t${n}\t${v}\t-" >> "$OUT"
  pass=$((pass+1))
done < "$TRIPLES"

echo "retried $pass; final: $(grep -c '^OK' "$OUT") OK, $(grep -c '^MISMATCH' "$OUT") mismatch, $(grep -c '^FAIL' "$OUT") fail"