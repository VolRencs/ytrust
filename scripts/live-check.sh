#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
YTRUST="$ROOT/target/release/ytrust"
VIDEO="${YTRUST_TEST_VIDEO:-https://www.youtube.com/watch?v=dQw4w9WgXcQ}"
COOKIES="${YTRUST_TEST_COOKIES:-${YTRUST_COOKIES:-}}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ ! -x "$YTRUST" ]]; then
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
fi

args=()
if [[ -n "$COOKIES" ]]; then
  args+=(--cookies "$COOKIES")
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

echo "live checks passed"
