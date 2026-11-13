#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
YTRUST="$ROOT/target/release/ytrust"
VIDEO="${YTRUST_TEST_VIDEO:-https://www.youtube.com/watch?v=dQw4w9WgXcQ}"
BLOCKED_VIDEO="${YTRUST_TEST_BLOCKED_VIDEO:-https://www.youtube.com/watch?v=x5ab3TJqK40}"
COOKIES="${YTRUST_TEST_COOKIES:-${YTRUST_COOKIES:-}}"
BROWSER="${YTRUST_TEST_BROWSER:-}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ ! -x "$YTRUST" ]]; then
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
fi

args=()
if [[ -n "$COOKIES" ]]; then
  args+=(--cookies "$COOKIES")
elif [[ -n "$BROWSER" ]]; then
  args+=(--cookies-from-browser "$BROWSER")
fi

echo "metadata"
"$YTRUST" "${args[@]}" -J "$VIDEO" >"$TMP/video.json"
python3 - "$TMP/video.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["_type"] == "video", data.get("_type")
assert data["formats"], "no formats"
assert data["best_audio"], "no best_audio"
assert data["best_audio"].get("url"), "best_audio has no URL"
print(f"  {data['id']}: {len(data['formats'])} formats, best={data['best_audio']['format_id']}")
PY

echo "formats"
"$YTRUST" "${args[@]}" -F "$VIDEO" >/dev/null

echo "playlist (flat)"
"$YTRUST" "${args[@]}" -j --flat-playlist --playlist-items 1-2 \
  "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RDdQw4w9WgXcQ" >"$TMP/playlist.jsonl"
python3 - "$TMP/playlist.jsonl" <<'PY'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
assert len(rows) == 2, len(rows)
assert all(row.get("playlist_title") for row in rows)
print(f"  {len(rows)} entries")
PY

echo "stdout stream (closed pipe)"
set +o pipefail
"$YTRUST" "${args[@]}" -f 18 -o - "$VIDEO" 2>"$TMP/pipe.err" | head -c 65536 >/dev/null
status="${PIPESTATUS[0]}"
set -o pipefail
if [[ "$status" -ne 0 ]] && ! grep -q "BrokenPipe" "$TMP/pipe.err"; then
  cat "$TMP/pipe.err" >&2
  exit 1
fi

echo "section download + progress JSON"
"$YTRUST" "${args[@]}" --progress-json --download-sections '*0-2' -f 18 \
  -P "$TMP" "$VIDEO" >"$TMP/progress.ndjson"
python3 - "$TMP/progress.ndjson" "$TMP" <<'PY'
import json, pathlib, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
events = {row["event"] for row in rows}
assert {"start", "progress", "done"} <= events, events
done = [row for row in rows if row["event"] == "done"]
assert done and done[-1]["filepath"], "done event has no filepath"
assert pathlib.Path(done[-1]["filepath"]).is_file()
print(f"  events={sorted(events)}")
PY

echo "audio conversion"
"$YTRUST" "${args[@]}" -x -f 251 --audio-format opus --download-sections '*0-2' \
  -P "$TMP" "$VIDEO" >/dev/null
ls "$TMP"/*.opus >/dev/null

echo "merge"
"$YTRUST" "${args[@]}" -f '313+251' --download-sections '*0-2' -P "$TMP" "$VIDEO" >/dev/null

echo "adaptive 403 -> muxed fallback (video)"
mkdir -p "$TMP/fallback"
"$YTRUST" "${args[@]}" --progress-json --download-sections '*0-3' \
  -f 'bestvideo[height<=144]+bestaudio' --merge-output-format mp4 \
  -P "$TMP/fallback" -o '%(id)s.%(ext)s' "$BLOCKED_VIDEO" >"$TMP/fallback.ndjson"
python3 - "$TMP/fallback.ndjson" "$TMP/fallback" <<'PY'
import json, pathlib, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
events = {row["event"] for row in rows}
assert "fallback" in events, events
fallback = next(row for row in rows if row["event"] == "fallback")
assert fallback["to"]["formats"], fallback
slot = fallback["to"]["formats"]
assert all(f.get("vcodec") and f.get("acodec") for f in slot), slot
done = [row for row in rows if row["event"] == "done"]
assert done and done[-1]["filepath"], "fallback done event has no filepath"
assert pathlib.Path(done[-1]["filepath"]).is_file()
print(f"  {fallback['from']['formats'][0]['format_id']} -> {slot[0]['format_id']}")
PY

echo "adaptive 403 -> muxed fallback (audio)"
mkdir -p "$TMP/fallback-audio"
"$YTRUST" "${args[@]}" -x -f bestaudio --audio-format mp3 --download-sections '*0-3' \
  -P "$TMP/fallback-audio" -o '%(id)s.%(ext)s' "$BLOCKED_VIDEO" >/dev/null
ls "$TMP/fallback-audio"/*.mp3 >/dev/null

echo "fallback can be disabled"
if "$YTRUST" "${args[@]}" --no-fallback --download-sections '*0-3' \
  -f 'bestvideo[height<=144]+bestaudio' -P "$TMP/no-fallback" -o '%(id)s.%(ext)s' \
  "$BLOCKED_VIDEO" >/dev/null 2>"$TMP/no-fallback.err"; then
  echo "expected --no-fallback to fail" >&2
  exit 1
fi
grep -q "403" "$TMP/no-fallback.err"

echo "Shorts URL"
mkdir -p "$TMP/short"
"$YTRUST" "${args[@]}" --download-sections '*0-2' -f 18 \
  -P "$TMP/short" "https://www.youtube.com/shorts/dQw4w9WgXcQ" >/dev/null
ls "$TMP/short"/* >/dev/null

echo "live checks passed"
