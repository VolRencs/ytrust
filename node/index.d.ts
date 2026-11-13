export interface ResolveOptions {
  cookiesPath?: string;
  cookiesFromBrowser?: string;
  proxy?: string;
  timeout?: number;
  playlistLimit?: number;
  playlistItems?: string;
  noPlaylist?: boolean;
  /**
   * `resolveAudio` only: wait until the selected media URL passes YouTube's
   * CDN cold-start window before returning it. Retries transient 403s for a
   * few seconds. Defaults to false.
   */
  ensurePlayable?: boolean;
}

export interface HttpHeaders {
  "User-Agent": string;
  Referer: string;
}

export interface Format {
  format_id: string;
  format: string;
  url: string | null;
  /** False when the URL still has an unresolved signature/n challenge. */
  playable?: boolean;
  signature_cipher?: string | null;
  cipher?: string | null;
  ext: string;
  protocol: string;
  vcodec: string;
  acodec: string;
  width?: number | null;
  height?: number | null;
  fps?: number | null;
  tbr?: number;
  abr?: number;
  filesize?: number | null;
  filesize_approx?: number | null;
  language?: string | null;
  resolution?: string | null;
  format_note?: string | null;
  is_live?: boolean;
  is_hls?: boolean;
  http_headers: HttpHeaders;
  expires_at?: number | null;
  audio_track?: {
    id: string | null;
    display_name: string | null;
    audio_is_default: boolean;
    language?: string | null;
  };
}

export interface VideoResult {
  _type: "video";
  id: string;
  title: string;
  description?: string;
  duration?: number | null;
  thumbnail?: string | null;
  thumbnails?: Array<{ url: string; width?: number | null; height?: number | null }>;
  webpage_url: string;
  original_url?: string;
  uploader?: string;
  channel?: string;
  channel_id?: string;
  upload_date?: string;
  view_count?: number | null;
  playlist_title?: string | null;
  playlist_index?: number | null;
  playlist_count?: number | null;
  is_live?: boolean;
  formats: Format[];
  best_audio?: Format | null;
  player_js_url?: string | null;
  hls_manifest_url?: string | null;
  subtitles?: Record<string, SubtitleTrack[]>;
  automatic_captions?: Record<string, SubtitleTrack[]>;
}

export interface SubtitleTrack {
  ext: string;
  url: string;
  name: string;
}

export interface PlaylistEntry {
  _type: "url";
  id: string;
  url: string;
  webpage_url: string;
  title: string;
  duration?: number | null;
  playlist_index?: number;
  thumbnail?: string | null;
  thumbnails?: Array<{ url: string; width?: number | null; height?: number | null }>;
}

export interface PlaylistResult {
  _type: "playlist";
  id: string;
  title: string;
  webpage_url: string;
  original_url?: string;
  playlist_count: number;
  entries: PlaylistEntry[];
}

export type ResolveResult = VideoResult | PlaylistResult;

export interface AudioResult extends Format {
  url: string;
  playable: true;
  id: string;
  title: string;
  duration?: number | null;
  thumbnail?: string | null;
  webpage_url: string;
  uploader?: string;
  channel?: string;
  upload_date?: string;
  is_live?: boolean;
}

export type YtrustErrorCode =
  | "BOTCHECK"
  | "AGE_RESTRICTED"
  | "PRIVATE"
  | "UNAVAILABLE"
  | "COOKIES"
  | "UNSUPPORTED_URL"
  | "INVALID_INPUT"
  | "TIMEOUT"
  | "NETWORK"
  | "HTTP"
  | "NO_AUDIO"
  | "INVALID_RESPONSE"
  | "NATIVE_LOAD"
  | "FORMAT_NOT_FOUND"
  | "FFMPEG"
  | "IO"
  | "CHALLENGE_FAILED"
  | "UNKNOWN";

export declare class YtrustError extends Error {
  code: YtrustErrorCode;
  constructor(code: YtrustErrorCode, message: string, options?: ErrorOptions);
}

export declare function errorCode(message: unknown): YtrustErrorCode;

export declare function version(): string;

export declare function resolve(
  input: string,
  options?: ResolveOptions,
): Promise<ResolveResult>;

export declare function resolveAudio(
  input: string,
  options?: ResolveOptions,
): Promise<AudioResult>;
