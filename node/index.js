let native;
let nativeLoadError;
try {
  native = await import("./native.js");
} catch (error) {
  nativeLoadError = error;
}

function requireNative() {
  if (!native) {
    const detail = nativeLoadError instanceof Error ? nativeLoadError.message : String(nativeLoadError ?? "unknown error");
    throw new YtrustError("NATIVE_LOAD", `native addon is unavailable: ${detail}`, { cause: nativeLoadError });
  }
  return native;
}

const ERROR_CODES = [
  ["AGE_RESTRICTED", /age.?restricted|confirm your age|inappropriate for some users/i],
  ["BOTCHECK", /confirm you.?re not a bot|sign in to confirm you.?re not a bot/i],
  ["PRIVATE", /private video|video is private/i],
  ["UNAVAILABLE", /unavailable|does not exist|removed|terminated/i],
  ["COOKIES", /cookie/i],
  ["UNSUPPORTED_URL", /not a YouTube URL|unsupported YouTube URL|only YouTube is supported|channel URLs are not supported/i],
  ["INVALID_INPUT", /invalid (url|playlist|search|download section)|search query is empty|playlist items start at 1|playlist URL has no list parameter/i],
  ["TIMEOUT", /timed out|timeout/i],
  ["NETWORK", /request failed|connection|dns|tls|network|unreachable/i],
  ["HTTP", /HTTP \d{3}|status \d{3}/i],
];

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

function toYtrustError(error) {
  if (error instanceof YtrustError) return error;
  const message = error instanceof Error ? error.message : String(error);
  return new YtrustError(errorCode(message), message, { cause: error });
}

export function probe() {
  return requireNative().probe();
}

export function version() {
  return requireNative().version();
}

const PLAYER_JS_CACHE = new Map();
const SOLVED_N_CACHE = new Map();
const SOLVED_SIG_CACHE = new Map();
const PLAYER_JS_CACHE_LIMIT = 4;
const CHALLENGE_CACHE_LIMIT = 1000;

function remember(cache, key, value) {
  if (cache.size >= CHALLENGE_CACHE_LIMIT) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) cache.delete(oldest);
  }
  cache.set(key, value);
}

function challengeValue(url) {
  try {
    const parsed = new URL(url);
    return parsed.searchParams.get("n");
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

async function playerSource(url) {
  const solveUrl = url.replace("/player_embed_es6.", "/player_es6.").replace("/player_embed_ias.", "/player_ias.");
  const cached = PLAYER_JS_CACHE.get(solveUrl);
  if (cached) return cached;
  const pending = fetch(solveUrl, {
    headers: {
      "user-agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36",
      accept: "*/*",
    },
  }).then(async response => {
    if (!response.ok) throw new Error(`player JS HTTP ${response.status}`);
    return response.text();
  }).catch(error => {
    PLAYER_JS_CACHE.delete(solveUrl);
    throw error;
  });
  if (PLAYER_JS_CACHE.size >= PLAYER_JS_CACHE_LIMIT) {
    const oldest = PLAYER_JS_CACHE.keys().next().value;
    if (oldest !== undefined) PLAYER_JS_CACHE.delete(oldest);
  }
  PLAYER_JS_CACHE.set(solveUrl, pending);
  return pending;
}

async function applyChallenges(result) {
  const playerUrl = result?.player_js_url;
  if (!playerUrl) return result;
  const formats = [];
  if (result.best_audio) formats.push(result.best_audio);
  if (Array.isArray(result.formats)) formats.push(...result.formats);

  const sigChallenges = new Set();
  const nChallenges = new Set();
  let needsWork = false;
  for (const format of formats) {
    if (!format) continue;
    const cipher = cipherParams(format);
    if (cipher) needsWork = true;
    if (cipher?.s && !SOLVED_SIG_CACHE.has(cipher.s)) sigChallenges.add(cipher.s);
    const url = typeof format.url === "string" && format.url ? format.url : cipher?.url;
    const value = typeof url === "string" ? challengeValue(url) : null;
    if (value) {
      needsWork = true;
      if (!SOLVED_N_CACHE.has(value)) nChallenges.add(value);
    }
  }
  if (!needsWork) return result;

  let sigData = {};
  let nData = {};
  if (sigChallenges.size || nChallenges.size) {
    try {
      const player = await playerSource(playerUrl);
      const { solve } = await import("./solver/index.js");
      const requests = [];
      if (sigChallenges.size) requests.push({ type: "sig", challenges: [...sigChallenges] });
      if (nChallenges.size) requests.push({ type: "n", challenges: [...nChallenges] });
      const responses = solve(player, requests)?.responses ?? [];
      let at = 0;
      if (sigChallenges.size) {
        const response = responses[at++];
        if (response?.type === "result" && response.data) {
          sigData = response.data;
          for (const [challenge, value] of Object.entries(response.data)) {
            if (typeof value === "string") remember(SOLVED_SIG_CACHE, challenge, value);
          }
        }
      }
      if (nChallenges.size) {
        const response = responses[at++];
        if (response?.type === "result" && response.data) {
          nData = response.data;
          for (const [challenge, value] of Object.entries(response.data)) {
            if (typeof value === "string") remember(SOLVED_N_CACHE, challenge, value);
          }
        }
      }
    } catch {
      // Keep plain URLs; ciphered formats stay without url.
    }
  }

  for (const format of formats) {
    if (!format) continue;
    const cipher = cipherParams(format);
    if (cipher) {
      const sig = cipher.s ? sigData[cipher.s] ?? SOLVED_SIG_CACHE.get(cipher.s) : null;
      if (sig) {
        try {
          const parsed = new URL(cipher.url);
          parsed.searchParams.set(cipher.sp, sig);
          format.url = parsed.toString();
        } catch { }
      }
    }
    if (typeof format.url === "string" && format.url) {
      const value = challengeValue(format.url);
      const replacement = value ? nData[value] ?? SOLVED_N_CACHE.get(value) : null;
      if (replacement) {
        try {
          const parsed = new URL(format.url);
          parsed.searchParams.set("n", replacement);
          format.url = parsed.toString();
        } catch { }
      }
    }
  }
  return result;
}

export async function resolve(input, options = {}) {
  let raw;
  try {
    raw = await requireNative().resolve(String(input), options);
  } catch (error) {
    throw toYtrustError(error);
  }
  let result;
  try {
    result = JSON.parse(raw);
  } catch (error) {
    throw new YtrustError("INVALID_RESPONSE", "ytrust returned invalid JSON", { cause: error });
  }
  return applyChallenges(result);
}

/**
 * Resolve a single video and return its best playable audio stream together
 * with the HTTP headers required to fetch it.
 */
export async function resolveAudio(input, options = {}) {
  const result = await resolve(input, { ...options, noPlaylist: true });
  const audio = result?.best_audio;
  if (result?._type !== "video" || !audio) {
    throw new YtrustError("NO_AUDIO", `no audio format found for ${input}`);
  }
  return {
    ...audio,
    id: result.id,
    title: result.title,
    duration: result.duration,
    thumbnail: result.thumbnail,
    webpage_url: result.webpage_url,
  };
}
