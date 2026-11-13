use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::OnceCell as AsyncOnceCell;
use tracing::{debug, warn};
use url::Url;

use crate::error::{ErrorCode, Result, YtrustError};
use crate::http::RequestConfig;
use crate::youtube::{self, Format, VideoInfo};

const SOLVER_TIMEOUT: Duration = Duration::from_secs(30);
const SOLVER_SERVER: &str = include_str!("../node/solver/server.js");
const SOLVER_INDEX: &str = include_str!("../node/solver/index.js");
const SOLVER_LIB: &str = include_str!("../node/solver/lib.min.js");
const SOLVER_CORE: &str = include_str!("../node/solver/core.min.js");

#[derive(Default, Debug)]
pub struct SolveReport {
    pub attempted: bool,
    pub runtime_missing: bool,
    pub unresolved: usize,
    pub error: Option<String>,
}

pub struct ChallengeSolver {
    runtime: AsyncOnceCell<Option<String>>,
    explicit: bool,
    spec: Option<String>,
    cache: Mutex<SolverCache>,
    server: tokio::sync::Mutex<Option<SolverServer>>,
}

struct SolverServer {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

#[derive(Default)]
struct SolverCache {
    sources: HashMap<String, String>,
    preprocessed: HashMap<String, String>,
    sig: HashMap<(String, String), String>,
    n: HashMap<(String, String), String>,
}

const SOLVER_CACHE_ENTRIES: usize = 4096;

struct CipherParams {
    url: String,
    s: Option<String>,
    sp: String,
}

#[derive(Serialize)]
struct SolverInput<'a> {
    player: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    preprocessed: Option<&'a str>,
    requests: &'a [SolverRequest],
}

#[derive(Serialize, Deserialize, Debug)]
struct SolverRequest {
    #[serde(rename = "type")]
    kind: String,
    challenges: Vec<String>,
}

#[derive(Deserialize)]
struct SolverOutput {
    #[serde(rename = "type")]
    kind: String,
    responses: Option<Vec<SolverResponse>>,
    preprocessed_player: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct SolverResponse {
    #[serde(rename = "type")]
    kind: String,
    data: Option<HashMap<String, Option<String>>>,
}

impl ChallengeSolver {
    pub async fn new(spec: Option<&str>) -> Result<Self> {
        let disabled = spec.is_some_and(|spec| {
            let spec = spec.trim();
            spec.eq_ignore_ascii_case("none") || spec.eq_ignore_ascii_case("off")
        });
        if disabled {
            return Ok(Self {
                runtime: AsyncOnceCell::from(None),
                explicit: false,
                spec: None,
                cache: Mutex::new(SolverCache::default()),
                server: tokio::sync::Mutex::new(None),
            });
        }

        let explicit = spec.is_some();
        let solver = Self {
            runtime: AsyncOnceCell::new(),
            explicit,
            spec: spec.map(str::to_string),
            cache: Mutex::new(SolverCache::default()),
            server: tokio::sync::Mutex::new(None),
        };
        if explicit {
            // Probe early only when the user explicitly requested a runtime so
            // a typo fails before any network work. The default Node probe is
            // lazy and costs nothing for metadata-only runs.
            let runtime = solver.probe().await;
            if runtime.is_none() {
                return Err(YtrustError::coded(
                    ErrorCode::InvalidInput,
                    "no usable JavaScript runtime found; install Node.js 20+ or pass --js-runtimes node:<path>",
                ));
            }
            let _ = solver.runtime.set(runtime);
        }
        Ok(solver)
    }

    async fn probe(&self) -> Option<String> {
        let candidates: Vec<String> = self
            .spec
            .as_deref()
            .map(|spec| {
                spec.split(',')
                    .map(str::trim)
                    .filter(|candidate| !candidate.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(|| vec!["node".to_string()]);

        for candidate in candidates {
            if let Some(found) = probe_runtime(&candidate).await {
                return Some(found);
            }
        }
        None
    }

    async fn runtime(&self) -> Option<&str> {
        let runtime = self
            .runtime
            .get_or_init(|| async {
                let runtime = self.probe().await;
                if let Some(runtime) = &runtime {
                    debug!("challenge solver runtime: {runtime}");
                } else if self.explicit {
                    warn!("no usable JavaScript runtime found");
                } else {
                    debug!("no JavaScript runtime found; ciphered formats will be skipped");
                }
                runtime
            })
            .await;
        runtime.as_deref()
    }

    pub fn available(&self) -> bool {
        self.runtime.get().is_some_and(Option::is_some)
    }

    pub async fn solve_formats(
        &self,
        info: &mut VideoInfo,
        config: &RequestConfig,
    ) -> Result<SolveReport> {
        let mut report = SolveReport::default();
        if !needs_challenges(info) {
            return Ok(report);
        }
        let Some(runtime) = self.runtime().await else {
            report.runtime_missing = true;
            warn!("ciphered formats require a JavaScript runtime (Node.js 20+)");
            return Ok(report);
        };

        let player_url = match info.player_js_url.clone() {
            Some(url) => url,
            None => match youtube::player_js_url(&info.id, config).await {
                Ok(url) => {
                    info.player_js_url = Some(url.clone());
                    url
                }
                Err(error) => {
                    report.error = Some(format!("could not resolve player JS URL: {error}"));
                    warn!("{}", report.error.as_deref().unwrap_or_default());
                    return Ok(report);
                }
            },
        };

        let (requests, sig_challenges, n_challenges) =
            collect_requests(info, &player_url, &self.cache);
        debug!(
            "challenge solve: player={}, sig={}, n={}",
            player_url,
            sig_challenges.len(),
            n_challenges.len()
        );
        let mut sig_values = cached_values(&self.cache, &player_url, true);
        let mut n_values = cached_values(&self.cache, &player_url, false);

        if !requests.is_empty() {
            let preprocessed = {
                let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
                cache.preprocessed.get(&player_url).cloned()
            };
            // The preprocessed player is all the solver needs. Fetching the
            // multi-megabyte raw source as well is pure overhead.
            let source = if preprocessed.is_some() {
                String::new()
            } else {
                let source = {
                    let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
                    cache.sources.get(&player_url).cloned()
                };
                match source {
                    Some(source) => source,
                    None => match youtube::fetch_player_source(&player_url, config).await {
                        Ok(source) => {
                            let mut cache =
                                self.cache.lock().unwrap_or_else(|error| error.into_inner());
                            if cache.sources.len() >= 4 && !cache.sources.contains_key(&player_url)
                            {
                                cache.sources.clear();
                            }
                            cache.sources.insert(player_url.clone(), source.clone());
                            source
                        }
                        Err(error) => {
                            report.error = Some(format!("could not fetch player JS: {error}"));
                            warn!("{}", report.error.as_deref().unwrap_or_default());
                            report.unresolved = apply_values(info, &sig_values, &n_values);
                            return Ok(report);
                        }
                    },
                }
            };
            report.attempted = true;
            match self
                .run_solver(runtime, &source, preprocessed.as_deref(), &requests)
                .await
            {
                Ok(output) => {
                    if output.kind == "error" {
                        report.error = Some(
                            output
                                .error
                                .unwrap_or_else(|| "challenge solver failed".to_string()),
                        );
                        warn!("{}", report.error.as_deref().unwrap_or_default());
                    } else {
                        if let Some(preprocessed) = output.preprocessed_player {
                            let mut cache =
                                self.cache.lock().unwrap_or_else(|error| error.into_inner());
                            if cache.preprocessed.len() >= 4
                                && !cache.preprocessed.contains_key(&player_url)
                            {
                                cache.preprocessed.clear();
                            }
                            cache.preprocessed.insert(player_url.clone(), preprocessed);
                        }
                        if let Some(responses) = output.responses {
                            for response in responses {
                                if response.kind != "result" {
                                    continue;
                                }
                                let Some(data) = response.data else {
                                    continue;
                                };
                                for (challenge, value) in data {
                                    let Some(value) = value else {
                                        continue;
                                    };
                                    let is_sig = sig_challenges.contains(&challenge);
                                    let is_n = n_challenges.contains(&challenge);
                                    if is_sig {
                                        sig_values.insert(challenge.clone(), value.clone());
                                        let mut cache = self
                                            .cache
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner());
                                        if cache.sig.len() >= SOLVER_CACHE_ENTRIES {
                                            cache.sig.clear();
                                        }
                                        cache.sig.insert((player_url.clone(), challenge), value);
                                    } else if is_n {
                                        debug!("n challenge {challenge} solved as {value}");
                                        n_values.insert(challenge.clone(), value.clone());
                                        let mut cache = self
                                            .cache
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner());
                                        if cache.n.len() >= SOLVER_CACHE_ENTRIES {
                                            cache.n.clear();
                                        }
                                        cache.n.insert((player_url.clone(), challenge), value);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    report.error = Some(error);
                    warn!("{}", report.error.as_deref().unwrap_or_default());
                }
            }
        }

        report.unresolved = apply_values(info, &sig_values, &n_values);
        if report.unresolved > 0 && report.error.is_none() {
            report.error = Some(format!(
                "{} format(s) still have unresolved challenges",
                report.unresolved
            ));
        }
        Ok(report)
    }
}

pub fn needs_challenges(info: &VideoInfo) -> bool {
    info.formats.iter().any(|format| {
        format.signature_cipher.is_some()
            || format.cipher.is_some()
            || (!format.url.is_empty() && challenge_value(&format.url, "n").is_some())
    })
}

fn collect_requests(
    info: &VideoInfo,
    player_url: &str,
    cache: &Mutex<SolverCache>,
) -> (Vec<SolverRequest>, BTreeSet<String>, BTreeSet<String>) {
    let cache = cache.lock().unwrap_or_else(|error| error.into_inner());
    let mut sig = BTreeSet::new();
    let mut n = BTreeSet::new();
    for format in &info.formats {
        if let Some(raw) = format
            .signature_cipher
            .as_deref()
            .or(format.cipher.as_deref())
            && let Some(cipher) = parse_cipher(raw)
            && let Some(value) = cipher.s.as_deref().filter(|value| !value.is_empty())
            && !cache
                .sig
                .contains_key(&(player_url.to_string(), value.to_string()))
        {
            sig.insert(value.to_string());
        }
        let url = cipher_url(format);
        if let Some(value) = url.as_deref().and_then(|url| challenge_value(url, "n"))
            && !cache
                .n
                .contains_key(&(player_url.to_string(), value.clone()))
        {
            n.insert(value);
        }
    }
    let mut requests = Vec::new();
    if !sig.is_empty() {
        requests.push(SolverRequest {
            kind: "sig".to_string(),
            challenges: sig.iter().cloned().collect(),
        });
    }
    if !n.is_empty() {
        requests.push(SolverRequest {
            kind: "n".to_string(),
            challenges: n.iter().cloned().collect(),
        });
    }
    (requests, sig, n)
}

fn cached_values(
    cache: &Mutex<SolverCache>,
    player_url: &str,
    signature: bool,
) -> HashMap<String, String> {
    let cache = cache.lock().unwrap_or_else(|error| error.into_inner());
    let values = if signature { &cache.sig } else { &cache.n };
    values
        .iter()
        .filter(|((url, _), _)| url == player_url)
        .map(|((_, challenge), value)| (challenge.clone(), value.clone()))
        .collect()
}

fn apply_values(
    info: &mut VideoInfo,
    sig_values: &HashMap<String, String>,
    n_values: &HashMap<String, String>,
) -> usize {
    let mut unresolved = 0;
    for format in &mut info.formats {
        let cipher = format
            .signature_cipher
            .as_deref()
            .or(format.cipher.as_deref())
            .and_then(parse_cipher);
        let mut format_unresolved = false;
        if let Some(cipher) = cipher {
            match cipher.s.as_deref().filter(|value| !value.is_empty()) {
                Some(challenge) => match sig_values.get(challenge) {
                    Some(signature) => {
                        if let Some(url) = set_query_param(&cipher.url, &cipher.sp, signature) {
                            format.url = url;
                            format.signature_cipher = None;
                            format.cipher = None;
                        } else {
                            format_unresolved = true;
                        }
                    }
                    None => format_unresolved = true,
                },
                None => {
                    format.url = cipher.url;
                    format.signature_cipher = None;
                    format.cipher = None;
                }
            }
        }

        if format.url.is_empty() {
            if format.signature_cipher.is_some() || format.cipher.is_some() {
                format_unresolved = true;
            }
        } else if let Some(challenge) = challenge_value(&format.url, "n") {
            match n_values.get(&challenge) {
                Some(value) => {
                    if let Some(url) = set_query_param(&format.url, "n", value) {
                        format.url = url;
                    } else {
                        format_unresolved = true;
                    }
                }
                None => format_unresolved = true,
            }
        }

        if format_unresolved {
            unresolved += 1;
        }
    }
    unresolved
}

fn cipher_url(format: &Format) -> Option<String> {
    if !format.url.is_empty() {
        return Some(format.url.clone());
    }
    format
        .signature_cipher
        .as_deref()
        .or(format.cipher.as_deref())
        .and_then(parse_cipher)
        .map(|cipher| cipher.url)
}

fn parse_cipher(raw: &str) -> Option<CipherParams> {
    let parsed = Url::parse(&format!("https://www.youtube.com/?{raw}")).ok()?;
    let mut url = None;
    let mut s = None;
    let mut sp = "signature".to_string();
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "url" => url = Some(value.into_owned()),
            "s" => s = Some(value.into_owned()),
            "sp" => sp = value.into_owned(),
            _ => {}
        }
    }
    Some(CipherParams { url: url?, s, sp })
}

fn challenge_value(raw: &str, key: &str) -> Option<String> {
    Url::parse(raw)
        .ok()?
        .query_pairs()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

fn set_query_param(raw: &str, key: &str, value: &str) -> Option<String> {
    let (base, query) = raw.split_once('?').unwrap_or((raw, ""));
    let encoded = encode_query_value(value);
    let mut result = String::with_capacity(raw.len() + encoded.len() + key.len() + 2);
    result.push_str(base);
    result.push('?');
    let mut first = true;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        if pair.split_once('=').map(|(candidate, _)| candidate) == Some(key) {
            continue;
        }
        if !first {
            result.push('&');
        }
        result.push_str(pair);
        first = false;
    }
    if !first {
        result.push('&');
    }
    result.push_str(key);
    result.push('=');
    result.push_str(&encoded);
    Some(result)
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

async fn probe_runtime(raw: &str) -> Option<String> {
    let command = raw
        .strip_prefix("node:")
        .filter(|path| !path.is_empty())
        .unwrap_or(raw);
    if command.is_empty() {
        return None;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(command)
            .arg("--version")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let major = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())?;
    (major >= 20).then(|| command.to_string())
}

impl ChallengeSolver {
    async fn run_solver(
        &self,
        runtime: &str,
        player: &str,
        preprocessed: Option<&str>,
        requests: &[SolverRequest],
    ) -> std::result::Result<SolverOutput, String> {
        let dir = solver_dir()?;
        debug!(
            "solver input: player={} bytes, preprocessed={}, requests={}",
            player.len(),
            preprocessed.is_some(),
            requests.len()
        );
        let mut input = serde_json::to_vec(&SolverInput {
            player,
            preprocessed,
            requests,
        })
        .map_err(|error| format!("could not encode solver input: {error}"))?;
        input.push(b'\n');

        let mut server = self.server.lock().await;
        let mut last_error = String::from("challenge solver failed");
        for attempt in 0..2 {
            if server.is_none() {
                match SolverServer::spawn(runtime, &dir).await {
                    Ok(spawned) => *server = Some(spawned),
                    Err(error) => {
                        last_error = error;
                        continue;
                    }
                }
            }
            let Some(process) = server.as_mut() else {
                continue;
            };
            let result = tokio::time::timeout(SOLVER_TIMEOUT, async {
                process
                    .stdin
                    .write_all(&input)
                    .await
                    .map_err(|error| format!("could not write solver input: {error}"))?;
                process
                    .stdin
                    .flush()
                    .await
                    .map_err(|error| format!("could not flush solver input: {error}"))?;
                let mut line = String::new();
                let read = process
                    .stdout
                    .read_line(&mut line)
                    .await
                    .map_err(|error| format!("could not read solver output: {error}"))?;
                if read == 0 {
                    return Err("solver process exited unexpectedly".to_string());
                }
                serde_json::from_str::<SolverOutput>(&line)
                    .map_err(|error| format!("could not decode solver output: {error}"))
            })
            .await;
            match result {
                Ok(Ok(output)) => return Ok(output),
                Ok(Err(error)) => {
                    last_error = error;
                    *server = None;
                    if attempt == 0 {
                        continue;
                    }
                }
                Err(_) => {
                    *server = None;
                    return Err(format!(
                        "challenge solver timed out after {}s",
                        SOLVER_TIMEOUT.as_secs()
                    ));
                }
            }
        }
        Err(last_error)
    }
}

impl SolverServer {
    async fn spawn(runtime: &str, dir: &Path) -> std::result::Result<Self, String> {
        let mut child = tokio::process::Command::new(runtime)
            .arg(dir.join("server.js"))
            .current_dir(dir)
            .env_remove("NODE_OPTIONS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not start JavaScript runtime: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "solver stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "solver stdout unavailable".to_string())?;
        Ok(Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }
}

fn solver_dir() -> std::result::Result<PathBuf, String> {
    static DIR: OnceLock<std::result::Result<tempfile::TempDir, String>> = OnceLock::new();
    match DIR.get_or_init(|| {
        let dir = tempfile::tempdir().map_err(|error| format!("temp dir failed: {error}"))?;
        for (name, contents) in [
            ("package.json", "{\"type\":\"module\"}"),
            ("server.js", SOLVER_SERVER),
            ("index.js", SOLVER_INDEX),
            ("lib.min.js", SOLVER_LIB),
            ("core.min.js", SOLVER_CORE),
        ] {
            std::fs::write(dir.path().join(name), contents)
                .map_err(|error| format!("could not write {name}: {error}"))?;
        }
        Ok(dir)
    }) {
        Ok(dir) => Ok(dir.path().to_path_buf()),
        Err(error) => Err(error.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::youtube::Format;

    fn base_format(url: &str) -> Format {
        Format {
            itag: 251,
            url: url.to_string(),
            signature_cipher: None,
            cipher: None,
            container: "webm".to_string(),
            video_codec: None,
            audio_codec: Some("opus".to_string()),
            has_video: false,
            has_audio: true,
            width: None,
            height: None,
            fps: None,
            bitrate: 128_000,
            audio_bitrate: Some(128_000),
            content_length: Some(1_000),
            quality_label: None,
            is_hls: false,
            is_live: false,
            audio_track_name: None,
            audio_track_id: None,
            audio_is_default: true,
            language: Some("en".to_string()),
            user_agent: "test",
        }
    }

    #[test]
    fn parses_cipher_parameters() {
        let cipher = parse_cipher(
            "s=abc&sp=sig&url=https%3A%2F%2Frr1---sn-test.googlevideo.com%2Fvideoplayback%3Fexpire%3D42",
        )
        .unwrap();
        assert_eq!(cipher.s.as_deref(), Some("abc"));
        assert_eq!(cipher.sp, "sig");
        assert!(cipher.url.contains("expire=42"));
    }

    #[test]
    fn replaces_query_parameters() {
        let url = set_query_param(
            "https://rr1.googlevideo.com/videoplayback?expire=42&n=old",
            "n",
            "new value",
        )
        .unwrap();
        let url = Url::parse(&url).unwrap();
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "n").unwrap().1,
            "new value"
        );
        assert!(url.query_pairs().any(|(key, _)| key == "expire"));
    }

    #[test]
    fn preserves_raw_query_bytes_when_replacing() {
        let url = set_query_param(
            "https://rr1.googlevideo.com/videoplayback?expire=42&x=a%2Fb&n=old&y=1",
            "n",
            "abc",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://rr1.googlevideo.com/videoplayback?expire=42&x=a%2Fb&y=1&n=abc"
        );
        let url = set_query_param(
            "https://rr1.googlevideo.com/videoplayback?expire=42",
            "sig",
            "a b",
        )
        .unwrap();
        assert!(url.ends_with("&sig=a%20b"));
    }

    #[test]
    fn applies_signature_and_n_challenges() {
        let mut format = base_format("https://rr1.googlevideo.com/videoplayback?expire=42&n=old");
        format.signature_cipher =
            Some("s=abc&sp=sig&url=https%3A%2F%2Frr1.googlevideo.com%2Fvideoplayback%3Fexpire%3D42%26n%3Dold".to_string());
        format.url.clear();
        let mut info = VideoInfo {
            id: "dQw4w9WgXcQ".to_string(),
            title: "test".to_string(),
            uploader: String::new(),
            channel: String::new(),
            channel_id: String::new(),
            webpage_url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string(),
            description: String::new(),
            duration_secs: Some(10),
            view_count: None,
            upload_date: String::new(),
            is_live: false,
            thumbnails: Vec::new(),
            subtitles: Vec::new(),
            automatic_captions: Vec::new(),
            formats: vec![format],
            hls_manifest_url: None,
            player_js_url: Some("https://www.youtube.com/player.js".to_string()),
        };
        assert!(needs_challenges(&info));
        let unresolved = apply_values(
            &mut info,
            &HashMap::from([("abc".to_string(), "solved".to_string())]),
            &HashMap::from([("old".to_string(), "fresh".to_string())]),
        );
        assert_eq!(unresolved, 0);
        let format = &info.formats[0];
        assert!(format.signature_cipher.is_none());
        let url = Url::parse(&format.url).unwrap();
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("sig").map(String::as_str), Some("solved"));
        assert_eq!(pairs.get("n").map(String::as_str), Some("fresh"));
    }

    #[test]
    fn reports_unresolved_cipher() {
        let mut format = base_format("");
        format.signature_cipher =
            Some("s=abc&url=https%3A%2F%2Frr1.googlevideo.com%2Fvideoplayback".to_string());
        let mut info = VideoInfo {
            id: "x".to_string(),
            title: "x".to_string(),
            uploader: String::new(),
            channel: String::new(),
            channel_id: String::new(),
            webpage_url: "https://www.youtube.com/watch?v=x".to_string(),
            description: String::new(),
            duration_secs: None,
            view_count: None,
            upload_date: String::new(),
            is_live: false,
            thumbnails: Vec::new(),
            subtitles: Vec::new(),
            automatic_captions: Vec::new(),
            formats: vec![format],
            hls_manifest_url: None,
            player_js_url: None,
        };
        assert_eq!(apply_values(&mut info, &HashMap::new(), &HashMap::new()), 1);
    }
}
