import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

let native;
let nativeLoadError;
try {
  native = require("./native.cjs");
} catch (error) {
  nativeLoadError = error;
}

function requireNative() {
  if (!native) {
    const detail =
      nativeLoadError instanceof Error ? nativeLoadError.message : String(nativeLoadError ?? "unknown error");
    throw new YtrustError("NATIVE_LOAD", `native addon is unavailable: ${detail}`, { cause: nativeLoadError });
  }
  return native;
}

const ERROR_CODES = [
  ["NATIVE_LOAD", /native addon is unavailable|Cannot find native binding|Failed to load native binding/i],
  ["AGE_RESTRICTED", /age.?restricted|confirm your age/i],
  ["BOTCHECK", /confirm you.?re not a bot|sign in to confirm you.?re not a bot/i],
  ["PRIVATE", /private video|video is private/i],
  ["UNAVAILABLE", /unavailable|does not exist|removed|terminated/i],
  ["CHALLENGE_FAILED", /challenge|player js|cipher/i],
  ["FFMPEG", /ffmpeg/i],
  ["COOKIES", /cookie/i],
  [
    "UNSUPPORTED_URL",
    /not a YouTube URL|unsupported YouTube URL|only YouTube is supported|channel URLs are not supported/i,
  ],
  [
    "INVALID_INPUT",
    /invalid (url|playlist|search|download section|video id|numeric filter)|search query is empty|playlist items start at 1|playlist URL has no list parameter|multiple download sections|unsupported format (selector|filter)|unclosed format filter|too many components|empty format selector/i,
  ],
  ["NO_AUDIO", /no audio/i],
  ["FORMAT_NOT_FOUND", /format not available|no format matched/i],
  ["TIMEOUT", /timed out|timeout/i],
  ["HTTP", /HTTP \d{3}|status \d{3}/i],
  ["NETWORK", /request failed|connection|dns|tls|network|unreachable/i],
  ["INVALID_RESPONSE", /JSON:|invalid JSON|no video details|no streaming data|invalid response/i],
  ["IO", /I\/O:|os error|no such file/i],
];

const KNOWN_CODES = new Set([
  "AGE_RESTRICTED",
  "BOTCHECK",
  "PRIVATE",
  "UNAVAILABLE",
  "COOKIES",
  "UNSUPPORTED_URL",
  "INVALID_INPUT",
  "TIMEOUT",
  "NETWORK",
  "HTTP",
  "NO_AUDIO",
  "INVALID_RESPONSE",
  "NATIVE_LOAD",
  "FORMAT_NOT_FOUND",
  "FFMPEG",
  "IO",
  "CHALLENGE_FAILED",
  "UNKNOWN",
]);

export class YtrustError extends Error {
  constructor(code, message, options) {
    super(message, options);
    this.name = "YtrustError";
    this.code = code;
  }
}

export function errorCode(message) {
  const text = String(message ?? "");
  for (const [code, pattern] of ERROR_CODES) {
    if (pattern.test(text)) return code;
  }
  return "UNKNOWN";
}

const ERROR_PREFIX = /^\[([A-Z_]+)\]\s*([\s\S]*)$/;

function toYtrustError(error) {
  if (error instanceof YtrustError) return error;
  const message = error instanceof Error ? error.message : String(error);
  const match = ERROR_PREFIX.exec(message);
  if (match && KNOWN_CODES.has(match[1])) {
    return new YtrustError(match[1], match[2], { cause: error });
  }
  return new YtrustError(errorCode(message), message, { cause: error });
}

export function version() {
  return requireNative().version();
}

function normalizeOptions(options) {
  if (options == null) return {};
  if (typeof options !== "object" || Array.isArray(options)) {
    throw new YtrustError("INVALID_INPUT", "options must be an object");
  }
  const normalized = { ...options };
  for (const key of ["timeout", "playlistLimit"]) {
    if (normalized[key] == null) continue;
    const value = Number(normalized[key]);
    if (!Number.isInteger(value) || value < 0 || value > 0xffffffff) {
      throw new YtrustError("INVALID_INPUT", `${key} must be a non-negative integer`);
    }
    normalized[key] = value;
  }
  for (const key of ["cookiesPath", "cookiesFromBrowser", "proxy", "playlistItems"]) {
    if (normalized[key] == null) continue;
    if (typeof normalized[key] !== "string") {
      throw new YtrustError("INVALID_INPUT", `${key} must be a string`);
    }
  }
  if (normalized.noPlaylist != null && typeof normalized.noPlaylist !== "boolean") {
    throw new YtrustError("INVALID_INPUT", "noPlaylist must be a boolean");
  }
  return normalized;
}

const PLAYER_JS_CACHE = new Map();
const PREPROCESSED_PLAYER_CACHE = new Map();
const SOLVED_N_CACHE = new Map();
const SOLVED_SIG_CACHE = new Map();
const PLAYER_JS_CACHE_LIMIT = 4;
const CHALLENGE_CACHE_LIMIT = 2000;

function warn(message, error) {
  process.emitWarning(error ? `${message}: ${error.message}` : message, { code: "YTRUST_CHALLENGE" });
}

function remember(cache, key, value, limit = CHALLENGE_CACHE_LIMIT) {
  if (cache.size >= limit) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) cache.delete(oldest);
  }
  cache.set(key, value);
}

function challengeValue(url) {
  try {
    return new URL(url).searchParams.get("n");
  } catch {
    return null;
  }
}

function cipherParams(format) {
  const raw = format?.signature_cipher ?? format?.cipher;
  if (typeof raw !== "string" || !raw) return null;
  const params = new URLSearchParams(raw);
  const url = params.get("url");
  if (!url) return null;
  return { url, s: params.get("s"), sp: params.get("sp") || "signature" };
}

function cacheKey(playerUrl, challenge) {
  return `${playerUrl}\u0000${challenge}`;
}

async function playerSource(url, options) {
  const cached = PLAYER_JS_CACHE.get(url);
  if (cached) return cached;
  const pending = Promise.resolve()
    .then(() => requireNative().fetchPlayerJs(url, options))
    .catch((error) => {
      PLAYER_JS_CACHE.delete(url);
      throw error;
    });
  remember(PLAYER_JS_CACHE, url, pending, PLAYER_JS_CACHE_LIMIT);
  return pending;
}

async function solveInWorker(player, requests, playerUrl) {
  const preprocessed = playerUrl ? PREPROCESSED_PLAYER_CACHE.get(playerUrl) : undefined;
  const { solve } = await import("./solver/pool.js");
  let output;
  let usedPreprocessed = false;

  if (preprocessed) {
    try {
      output = await solve("", requests, { preprocessed });
      usedPreprocessed = true;
    } catch {
      PREPROCESSED_PLAYER_CACHE.delete(playerUrl);
    }
  }
  if (!output) output = await solve(player, requests);

  if (!usedPreprocessed && playerUrl && typeof output?.preprocessed_player === "string") {
    remember(PREPROCESSED_PLAYER_CACHE, playerUrl, output.preprocessed_player, PLAYER_JS_CACHE_LIMIT);
    delete output.preprocessed_player;
  }
  return output;
}

function responseData(responses, requests) {
  const data = {};
  for (let index = 0; index < requests.length; index += 1) {
    const response = responses[index];
    if (response?.type === "result" && response.data) {
      data[requests[index].type] = response.data;
    }
  }
  return data;
}

async function ensurePlayerUrl(result, options) {
  if (typeof result?.player_js_url === "string" && result.player_js_url) return result.player_js_url;
  if (typeof result?.id !== "string" || !result.id) return null;
  try {
    const url = await requireNative().playerJsUrl(result.id, options);
    if (typeof url === "string" && url) {
      result.player_js_url = url;
      return url;
    }
  } catch (error) {
    warn("could not resolve player JS URL", error);
  }
  return null;
}

function collectFormats(result) {
  const formats = [];
  if (result?.best_audio) formats.push(result.best_audio);
  if (Array.isArray(result?.formats)) formats.push(...result.formats);
  return formats;
}

async function applyChallenges(result, options, formats = collectFormats(result)) {
  const knownPlayerUrl = typeof result?.player_js_url === "string" ? result.player_js_url : "";
  const sigChallenges = new Set();
  const nChallenges = new Set();
  let needsWork = false;
  for (const format of formats) {
    if (!format) continue;
    const cipher = cipherParams(format);
    if (cipher?.s) {
      needsWork = true;
      if (!SOLVED_SIG_CACHE.has(cacheKey(knownPlayerUrl, cipher.s))) sigChallenges.add(cipher.s);
    } else if (cipher && !format.url) {
      format.url = cipher.url;
      delete format.signature_cipher;
      delete format.cipher;
    }
    const url = typeof format.url === "string" && format.url ? format.url : cipher?.url;
    const value = typeof url === "string" ? challengeValue(url) : null;
    if (value) {
      needsWork = true;
      if (!SOLVED_N_CACHE.has(cacheKey(knownPlayerUrl, value))) nChallenges.add(value);
    }
  }
  if (!needsWork) return result;

  const playerUrl = await ensurePlayerUrl(result, options);
  if (!playerUrl) {
    warn("resolved formats need challenge solving, but the player JS URL is unknown");
    return result;
  }

  if (knownPlayerUrl !== playerUrl) {
    for (const challenge of sigChallenges) {
      if (SOLVED_SIG_CACHE.has(cacheKey(playerUrl, challenge))) sigChallenges.delete(challenge);
    }
    for (const challenge of nChallenges) {
      if (SOLVED_N_CACHE.has(cacheKey(playerUrl, challenge))) nChallenges.delete(challenge);
    }
  }

  let sigData = {};
  let nData = {};
  if (sigChallenges.size || nChallenges.size) {
    try {
      const player = await playerSource(playerUrl, options);
      const requests = [];
      if (sigChallenges.size) requests.push({ type: "sig", challenges: [...sigChallenges] });
      if (nChallenges.size) requests.push({ type: "n", challenges: [...nChallenges] });
      const output = await solveInWorker(player, requests, playerUrl);
      const responses = Array.isArray(output?.responses) ? output.responses : [];
      const solved = responseData(responses, requests);
      sigData = solved.sig ?? {};
      nData = solved.n ?? {};
      for (const [challenge, value] of Object.entries(sigData)) {
        if (typeof value === "string") remember(SOLVED_SIG_CACHE, cacheKey(playerUrl, challenge), value);
      }
      for (const [challenge, value] of Object.entries(nData)) {
        if (typeof value === "string") remember(SOLVED_N_CACHE, cacheKey(playerUrl, challenge), value);
      }
    } catch (error) {
      warn("challenge solving failed", error);
    }
  }

  let unresolved = 0;
  for (const format of formats) {
    if (!format) continue;
    const cipher = cipherParams(format);
    if (cipher) {
      let cipherResolved = false;
      if (!cipher.s) {
        format.url = cipher.url;
        cipherResolved = Boolean(format.url);
      } else {
        const sig = sigData[cipher.s] ?? SOLVED_SIG_CACHE.get(cacheKey(playerUrl, cipher.s));
        if (sig) {
          try {
            const parsed = new URL(cipher.url);
            parsed.searchParams.set(cipher.sp, sig);
            format.url = parsed.toString();
            cipherResolved = true;
          } catch {
            cipherResolved = false;
          }
        }
      }
      if (cipherResolved) {
        delete format.signature_cipher;
        delete format.cipher;
      } else {
        unresolved += 1;
      }
    }
    if (typeof format.url === "string" && format.url) {
      const value = challengeValue(format.url);
      const replacement = value ? (nData[value] ?? SOLVED_N_CACHE.get(cacheKey(playerUrl, value))) : null;
      if (replacement) {
        try {
          const parsed = new URL(format.url);
          parsed.searchParams.set("n", replacement);
          format.url = parsed.toString();
        } catch {
          unresolved += 1;
        }
      } else if (value) {
        unresolved += 1;
      }
    }
  }
  if (unresolved > 0) {
    warn(`${unresolved} format(s) could not be challenge-solved`);
  }
  return result;
}

function audioRate(format) {
  return Number(format.abr ?? format.tbr ?? 0);
}

function isOpus(format) {
  return typeof format.acodec === "string" && format.acodec.toLowerCase().startsWith("opus");
}

function pickPlayableAudio(formats) {
  if (!Array.isArray(formats)) return null;
  let best = null;
  let bestKey = null;
  for (const format of formats) {
    if (!format || typeof format.url !== "string" || !format.url) continue;
    const hasAudio = typeof format.acodec === "string" && format.acodec !== "none";
    if (!hasAudio && format.protocol !== "m3u8_native") continue;
    const key = [
      format.vcodec === "none" ? 1 : 0,
      format.audio_track?.audio_is_default ? 1 : 0,
      isOpus(format) ? 1 : 0,
      audioRate(format),
    ];
    if (!best || compareKeys(key, bestKey) > 0) {
      best = format;
      bestKey = key;
    }
  }
  return best;
}

function compareKeys(a, b) {
  for (let index = 0; index < a.length; index += 1) {
    if (a[index] === b[index]) continue;
    return a[index] > b[index] ? 1 : -1;
  }
  return 0;
}

function formatKey(format) {
  const track = format?.audio_track?.id ?? "";
  if (typeof format?.format_id === "string" && format.format_id) {
    return `id:${format.format_id}:${track}`;
  }
  return `${format?.url ?? ""}\u0000${format?.signature_cipher ?? format?.cipher ?? ""}`;
}

function audioKey(format) {
  return [
    format.vcodec === "none" ? 1 : 0,
    format.audio_track?.audio_is_default ? 1 : 0,
    isOpus(format) ? 1 : 0,
    audioRate(format),
  ];
}

function compareAudioCandidates(a, b) {
  return compareKeys(audioKey(b), audioKey(a));
}

function audioCandidates(result) {
  const candidates = [];
  const seen = new Set();
  const add = (format) => {
    if (!format) return;
    const hasAudio = typeof format.acodec === "string" && format.acodec !== "none";
    if (!hasAudio && format.protocol !== "m3u8_native") return;
    const key = formatKey(format);
    if (seen.has(key)) return;
    seen.add(key);
    candidates.push(format);
  };
  add(result?.best_audio);
  const formats = Array.isArray(result?.formats) ? [...result.formats] : [];
  formats.sort(compareAudioCandidates);
  for (const format of formats) add(format);
  return candidates;
}

export async function resolve(input, options = {}) {
  let result;
  try {
    const nativeOptions = normalizeOptions(options);
    result = await requireNative().resolve(String(input), nativeOptions);
    result = await applyChallenges(result, nativeOptions);
    if (result?.best_audio && !result.best_audio.url) {
      const fallback = pickPlayableAudio(result.formats);
      if (fallback) result.best_audio = fallback;
    }
    return result;
  } catch (error) {
    throw toYtrustError(error);
  }
}

/**
 * Resolve a single video and return its best playable audio stream together
 * with the HTTP headers required to fetch it.
 */
export async function resolveAudio(input, options = {}) {
  try {
    const normalized = normalizeOptions(options);
    const result = await requireNative().resolve(String(input), {
      ...normalized,
      noPlaylist: true,
    });
    if (result?._type !== "video") {
      throw new YtrustError("NO_AUDIO", `no audio format found for ${input}`);
    }

    const candidates = audioCandidates(result);
    let chosen = null;

    // Solve only the most likely audio formats first. The full solve below is
    // a safety net for unusual players where the preferred format is unusable.
    for (const candidate of candidates.slice(0, 3)) {
      await applyChallenges(result, normalized, [candidate]);
      if (typeof candidate.url === "string" && candidate.url) {
        chosen = candidate;
        break;
      }
    }

    if (!chosen) {
      await applyChallenges(result, normalized);
      chosen = pickPlayableAudio(candidates) ?? pickPlayableAudio(result.formats);
    }

    if (!chosen) {
      const code = result.best_audio ? "CHALLENGE_FAILED" : "NO_AUDIO";
      throw new YtrustError(code, `no playable audio format found for ${input}`);
    }

    return {
      ...chosen,
      id: result.id,
      title: result.title,
      duration: result.duration,
      thumbnail: result.thumbnail,
      webpage_url: result.webpage_url,
      uploader: result.uploader,
      channel: result.channel,
      upload_date: result.upload_date,
      is_live: result.is_live,
    };
  } catch (error) {
    throw toYtrustError(error);
  }
}
