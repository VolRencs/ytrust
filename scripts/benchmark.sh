#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
YTRUST="$ROOT/target/release/ytrust"
YTDLP="${YTDLP:-yt-dlp}"
VIDEO="https://www.youtube.com/watch?v=dQw4w9WgXcQ"
PLAYLIST="https://www.youtube.com/playlist?list=PL63F0C78739B09958"
RUNS="${RUNS:-5}"
ITEMS="${ITEMS:-5}"

if [[ ! -x "$YTRUST" ]]; then
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
fi

now_ms() { date +%s%N | cut -b1-13; }

median() {
  sort -n | awk '{a[NR]=$1} END { if (NR%2) print a[(NR+1)/2]; else print int((a[NR/2]+a[NR/2+1])/2) }'
}

measure() {
  local label="$1"; shift
  local times=()
  for _ in $(seq 1 "$RUNS"); do
    local start end
    start="$(now_ms)"
    "$@" >/dev/null 2>/dev/null || true
    end="$(now_ms)"
    times+=("$((end - start))")
  done
  printf '%-42s median %6s ms  runs %s\n' "$label" "$(printf '%s\n' "${times[@]}" | median)" "${times[*]}"
}

echo "binary: $(basename "$YTRUST") $(stat -c '%s' "$YTRUST" | numfmt --to=iec 2>/dev/null || stat -c '%s' "$YTRUST") bytes"
echo "runs: $RUNS, playlist items: $ITEMS"
echo

# Warm up DNS/TLS/player caches.
"$YTRUST" --skip-download -J "$VIDEO" >/dev/null 2>&1 || true
"$YTRUST" --skip-download --flat-playlist --playlist-items "1-$ITEMS" -J "$PLAYLIST" >/dev/null 2>&1 || true

measure "ytrust startup (--version)" "$YTRUST" --version
measure "ytrust scan single video (-J)" "$YTRUST" --skip-download -J "$VIDEO"
measure "ytrust scan playlist 1-$ITEMS (-J)" "$YTRUST" --skip-download --flat-playlist --playlist-items "1-$ITEMS" -J "$PLAYLIST"

if command -v "$YTDLP" >/dev/null 2>&1; then
  YTDLP_CMD=("$YTDLP" --ignore-config --js-runtimes node --remote-components ejs:github)
  "${YTDLP_CMD[@]}" --skip-download -J "$VIDEO" >/dev/null 2>&1 || true
  "${YTDLP_CMD[@]}" --skip-download --flat-playlist --playlist-items "1-$ITEMS" -J "$PLAYLIST" >/dev/null 2>&1 || true

  measure "yt-dlp startup (--version)" "$YTDLP" --version
  measure "yt-dlp scan single video (-J)" "${YTDLP_CMD[@]}" --skip-download -J "$VIDEO"
  measure "yt-dlp scan playlist 1-$ITEMS (-J)" "${YTDLP_CMD[@]}" --skip-download --flat-playlist --playlist-items "1-$ITEMS" -J "$PLAYLIST"
else
  echo "yt-dlp not found; comparison skipped"
fi
