export interface ResolveOptions {
  cookiesPath?: string;
  cookiesFromBrowser?: string;
  proxy?: string;
  timeout?: number;
  playlistLimit?: number;
  playlistItems?: string;
  noPlaylist?: boolean;
}

export type ResolveResult = Record<string, unknown>;

export declare function probe(): string;
export declare function version(): string;
export declare function resolve(
  input: string,
  options?: ResolveOptions,
): Promise<ResolveResult>;
