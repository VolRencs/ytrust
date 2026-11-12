use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};
use url::Url;

use crate::error::{ErrorCode, Result, YtrustError};
use crate::http::RequestConfig;
use crate::youtube::{self, Format, VideoInfo};

const SOLVER_TIMEOUT: Duration = Duration::from_secs(30);
const SOLVER_CLI: &str = include_str!("../node/solver/cli.js");
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
    runtime: Option<String>,
    cache: Mutex<SolverCache>,
}

#[derive(Default)]
struct SolverCache {
    sources: HashMap<String, String>,
    preprocessed: HashMap<String, String>,
    sig: HashMap<(String, String), String>,
    n: HashMap<(String, String), String>,
}

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
                runtime: None,
                cache: Mutex::new(SolverCache::default()),
            });
        }

        let explicit = spec.is_some();
        let candidates: Vec<String> = spec
            .map(|spec| {
                spec.split(',')
                    .map(str::trim)
                    .filter(|candidate| !candidate.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(|| vec!["node".to_string()]);

        let mut runtime = None;
        for candidate in candidates {
            if let Some(found) = probe_runtime(&candidate).await {
                runtime = Some(found);
                break;
            }
        }
        if runtime.is_none() && explicit {
            return Err(YtrustError::coded(
                ErrorCode::InvalidInput,
                "no usable JavaScript runtime found; install Node.js 20+ or pass --js-runtimes node:<path>",
            ));
        }
        if let Some(runtime) = &runtime {
            debug!("challenge solver runtime: {runtime}");
        } else {
            debug!("no JavaScript runtime found; ciphered formats will be skipped");
        }

        Ok(Self {
            runtime,
            cache: Mutex::new(SolverCache::default()),
        })
    }

    pub fn available(&self) -> bool {
        self.runtime.is_some()
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
        let Some(runtime) = self.runtime.as_deref() else {
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
        let mut sig_values = cached_values(&self.cache, &player_url, true);
        let mut n_values = cached_values(&self.cache, &player_url, false);

        if !requests.is_empty() {
            let source = {
                let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
                cache.sources.get(&player_url).cloned()
            };
            let source = match source {
                Some(source) => source,
                None => match youtube::fetch_player_source(&player_url, config).await {
                    Ok(source) => {
                        let mut cache =
                            self.cache.lock().unwrap_or_else(|error| error.into_inner());
                        if cache.sources.len() >= 4 && !cache.sources.contains_key(&player_url) {
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
            };
            let preprocessed = {
                let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
                cache.preprocessed.get(&player_url).cloned()
            };
            report.attempted = true;
            match run_solver(runtime, &source, preprocessed.as_deref(), &requests).await {
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
                                        cache.sig.insert((player_url.clone(), challenge), value);
                                    } else if is_n {
                                        n_values.insert(challenge.clone(), value.clone());
                                        let mut cache = self
                                            .cache
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner());
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
    let mut url = Url::parse(raw).ok()?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(candidate, _)| candidate != key)
        .map(|(candidate, value)| (candidate.into_owned(), value.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair(key, value);
    Some(url.to_string())
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

async fn run_solver(
    runtime: &str,
    player: &str,
    preprocessed: Option<&str>,
    requests: &[SolverRequest],
) -> std::result::Result<SolverOutput, String> {
    let dir = solver_dir()?;
    let input = serde_json::to_vec(&SolverInput {
        player,
        preprocessed,
        requests,
    })
    .map_err(|error| format!("could not encode solver input: {error}"))?;

    let mut child = tokio::process::Command::new(runtime)
        .arg(dir.join("cli.js"))
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not start JavaScript runtime: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "solver stdin unavailable".to_string())?;

    let output = tokio::time::timeout(SOLVER_TIMEOUT, async {
        stdin
            .write_all(&input)
            .await
            .map_err(|error| format!("could not write solver input: {error}"))?;
        drop(stdin);
        child
            .wait_with_output()
            .await
            .map_err(|error| format!("solver process failed: {error}"))
    })
    .await
    .map_err(|_| {
        format!(
            "challenge solver timed out after {}s",
            SOLVER_TIMEOUT.as_secs()
        )
    })??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail: String = stderr
            .chars()
            .rev()
            .take(4000)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return Err(format!(
            "solver exited with {}: {}",
            output.status,
            detail.trim()
        ));
    }
    serde_json::from_slice::<SolverOutput>(&output.stdout)
        .map_err(|error| format!("could not decode solver output: {error}"))
}

fn solver_dir() -> std::result::Result<PathBuf, String> {
    static DIR: OnceLock<std::result::Result<tempfile::TempDir, String>> = OnceLock::new();
    match DIR.get_or_init(|| {
        let dir = tempfile::tempdir().map_err(|error| format!("temp dir failed: {error}"))?;
        for (name, contents) in [
            ("package.json", "{\"type\":\"module\"}"),
            ("cli.js", SOLVER_CLI),
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
            mime_type: "audio/webm".to_string(),
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
            user_agent: "test".to_string(),
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
