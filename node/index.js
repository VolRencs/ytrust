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
const PLAYER_JS_CACHE_LIMIT = 4;

function challengeValue(url) {
  try {
    const parsed = new URL(url);
    return parsed.searchParams.get("n");
  } catch {
    return null;
  }
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
  const challenges = new Set();
  for (const format of formats) {
    if (typeof format?.url !== "string") continue;
    const value = challengeValue(format.url);
    if (value && !SOLVED_N_CACHE.has(value)) challenges.add(value);
  }
  if (!challenges.size && !formats.some(format => typeof format?.url === "string" && SOLVED_N_CACHE.has(challengeValue(format.url)))) {
    return result;
  }
  let solved = {};
  if (challenges.size) {
    try {
      const player = await playerSource(playerUrl);
      const { solve } = await import("./solver/index.js");
      const output = solve(player, [{ type: "n", challenges: [...challenges] }]);
      const data = output?.responses?.[0]?.data;
      if (data && typeof data === "object") {
        solved = data;
        for (const [challenge, value] of Object.entries(data)) {
          if (typeof value === "string") SOLVED_N_CACHE.set(challenge, value);
        }
      }
    } catch {
      solved = {};
    }
  }
  for (const format of formats) {
    if (typeof format?.url !== "string") continue;
    try {
      const parsed = new URL(format.url);
      const value = parsed.searchParams.get("n");
      const replacement = value ? solved[value] ?? SOLVED_N_CACHE.get(value) : undefined;
      if (replacement) {
        parsed.searchParams.set("n", replacement);
        format.url = parsed.toString();
      }
    } catch { }
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
