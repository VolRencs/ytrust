export interface ResolveOptions {
  cookiesPath?: string;
  cookiesFromBrowser?: string;
  proxy?: string;
  timeout?: number;
  playlistLimit?: number;
  playlistItems?: string;
  noPlaylist?: boolean;
}

export interface HttpHeaders {
  "User-Agent": string;
  Referer: string;
}

export interface Format {
  format_id: string;
  format: string;
  url: string;
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
  resolution?: string | null;
  format_note?: string | null;
  is_live?: boolean;
  http_headers: HttpHeaders;
  expires_at?: number | null;
  audio_track?: {
    id: string | null;
    display_name: string | null;
    audio_is_default: boolean;
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
  is_live?: boolean;
  formats: Format[];
  best_audio?: Format | null;
  player_js_url?: string | null;
  subtitles?: Record<string, unknown>;
  automatic_captions?: Record<string, unknown>;
}

export interface PlaylistEntry {
  _type: "url";
  id: string;
  url: string;
  webpage_url: string;
  title: string;
  duration?: number | null;
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
  id: string;
  title: string;
  duration?: number | null;
  thumbnail?: string | null;
  webpage_url: string;
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
  | "UNKNOWN";

export declare class YtrustError extends Error {
  code: YtrustErrorCode;
  constructor(code: YtrustErrorCode, message: string, options?: ErrorOptions);
}

export declare function errorCode(message: unknown): YtrustErrorCode;

export declare function probe(): string;
export declare function version(): string;

export declare function resolve(
  input: string,
  options?: ResolveOptions,
): Promise<ResolveResult>;

export declare function resolveAudio(
  input: string,
  options?: ResolveOptions,
): Promise<AudioResult>;
