use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// Writes newline-delimited JSON events to stdout.
///
/// A closed stdout (for example `ytrust --progress-json ... | head`) is not an
/// error: it only disables further event output.
#[derive(Clone, Default)]
pub struct EventSink {
    inner: Arc<EventSinkInner>,
}

#[derive(Default)]
struct EventSinkInner {
    broken: AtomicBool,
    lock: Mutex<()>,
}

impl EventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn emit(&self, value: &Value) {
        if self.inner.broken.load(Ordering::Relaxed) {
            return;
        }
        let _guard = self
            .inner
            .lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.inner.broken.load(Ordering::Relaxed) {
            return;
        }
        let Ok(line) = serde_json::to_string(value) else {
            return;
        };
        let mut stdout = std::io::stdout().lock();
        if writeln!(stdout, "{line}").is_err() || stdout.flush().is_err() {
            self.inner.broken.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtraFile {
    pub kind: String,
    pub path: PathBuf,
    pub language: Option<String>,
    pub auto: bool,
}

#[derive(Debug, Clone)]
pub struct ProgressMeta {
    pub id: String,
    pub title: String,
    pub original_url: String,
    pub playlist_index: Option<usize>,
    pub playlist_count: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ComponentSpec {
    pub name: String,
    pub total: Option<u64>,
}

impl ComponentSpec {
    pub fn new(name: impl Into<String>, total: Option<u64>) -> Self {
        Self {
            name: name.into(),
            total,
        }
    }
}

struct ComponentState {
    name: String,
    total: Option<u64>,
    done: u64,
}

struct PhaseState {
    processor: String,
    percent: Option<f64>,
}

struct ProgressState {
    meta: ProgressMeta,
    started: Instant,
    last_emit: Instant,
    last_percent: f64,
    components: Vec<ComponentState>,
    phase: Option<PhaseState>,
}

#[derive(Clone)]
pub struct Progress {
    sink: EventSink,
    state: Arc<Mutex<ProgressState>>,
}

impl Progress {
    pub fn new(sink: EventSink, meta: ProgressMeta, components: Vec<ComponentSpec>) -> Self {
        let state = ProgressState {
            meta,
            started: Instant::now(),
            last_emit: Instant::now(),
            last_percent: 0.0,
            components: components
                .into_iter()
                .map(|component| ComponentState {
                    name: component.name,
                    total: component.total,
                    done: 0,
                })
                .collect(),
            phase: None,
        };
        let progress = Self {
            sink,
            state: Arc::new(Mutex::new(state)),
        };
        progress.emit_start();
        progress
    }

    fn emit_start(&self) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let total = total_if_known(&state.components);
        let estimate = state
            .components
            .iter()
            .filter_map(|component| component.total)
            .sum::<u64>();
        let components: Vec<Value> = state
            .components
            .iter()
            .map(|component| {
                json!({
                    "name": component.name,
                    "total_bytes": component.total,
                })
            })
            .collect();
        drop(state);

        self.sink.emit(&json!({
            "v": 1,
            "event": "start",
            "id": self.meta_value(|meta| meta.id.clone()),
            "title": self.meta_value(|meta| meta.title.clone()),
            "original_url": self.meta_value(|meta| meta.original_url.clone()),
            "playlist_index": self.meta_value(|meta| meta.playlist_index),
            "playlist_count": self.meta_value(|meta| meta.playlist_count),
            "component_count": components.len(),
            "total_bytes": total,
            "total_bytes_estimate": (estimate > 0).then_some(estimate),
            "components": components,
        }));
    }

    fn meta_value<T>(&self, value: impl FnOnce(&ProgressMeta) -> T) -> T {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        value(&state.meta)
    }

    pub fn add(&self, component: &str, delta: u64) {
        if delta == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(state) = state
            .components
            .iter_mut()
            .find(|candidate| candidate.name == component)
        {
            state.done = state.done.saturating_add(delta);
        }
        drop(state);
        self.emit_progress(false);
    }

    pub fn set_total(&self, component: &str, total: u64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(state) = state
            .components
            .iter_mut()
            .find(|candidate| candidate.name == component)
        {
            state.total = Some(total);
        }
        drop(state);
        self.emit_progress(true);
    }

    pub fn set_done(&self, component: &str, done: u64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(state) = state
            .components
            .iter_mut()
            .find(|candidate| candidate.name == component)
        {
            state.done = done;
        }
        drop(state);
        self.emit_progress(true);
    }

    pub fn finish_component(&self, component: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(state) = state
            .components
            .iter_mut()
            .find(|candidate| candidate.name == component)
            && let Some(total) = state.total
        {
            state.done = total;
        }
        drop(state);
        self.emit_progress(true);
    }

    pub fn phase_started(&self, processor: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.phase = Some(PhaseState {
            processor: processor.to_string(),
            percent: None,
        });
        drop(state);
        self.emit_postprocess(processor, "started", None);
    }

    pub fn phase_progress(&self, percent: f64) {
        let percent = percent.clamp(0.0, 100.0);
        let processor = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(phase) = state.phase.as_mut() else {
                return;
            };
            phase.percent = Some(percent);
            phase.processor.clone()
        };
        if self.should_emit() {
            self.emit_postprocess(&processor, "progress", Some(percent));
        }
        self.emit_progress(false);
    }

    pub fn phase_finished(&self, processor: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.phase = None;
        drop(state);
        self.emit_postprocess(processor, "finished", Some(100.0));
    }

    pub fn phase_failed(&self, processor: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.phase = None;
        drop(state);
        self.emit_postprocess(processor, "failed", None);
    }

    pub fn file(&self, kind: &str, path: &Path, language: Option<&str>, auto: bool) {
        self.sink.emit(&json!({
            "v": 1,
            "event": "file",
            "id": self.meta_value(|meta| meta.id.clone()),
            "kind": kind,
            "path": absolute_path(path).to_string_lossy(),
            "language": language,
            "auto": auto,
        }));
    }

    pub fn done(
        &self,
        path: Option<&Path>,
        kind: &str,
        skipped: bool,
        simulated: bool,
        language: Option<&str>,
    ) {
        let filepath = path.map(absolute_path);
        let filesize = filepath
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len());
        self.sink.emit(&json!({
            "v": 1,
            "event": "done",
            "id": self.meta_value(|meta| meta.id.clone()),
            "ok": true,
            "kind": kind,
            "filepath": filepath.as_ref().map(|path| path.to_string_lossy().into_owned()),
            "filename": filepath
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned()),
            "filesize": filesize,
            "language": language,
            "skipped": skipped,
            "simulated": simulated,
            "playlist_index": self.meta_value(|meta| meta.playlist_index),
            "playlist_count": self.meta_value(|meta| meta.playlist_count),
        }));
    }

    pub fn error(&self, code: &str, message: &str) {
        self.sink.emit(&json!({
            "v": 1,
            "event": "error",
            "id": self.meta_value(|meta| meta.id.clone()),
            "code": code,
            "message": message,
            "playlist_index": self.meta_value(|meta| meta.playlist_index),
            "playlist_count": self.meta_value(|meta| meta.playlist_count),
        }));
    }

    fn should_emit(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        if now.duration_since(state.last_emit) < PROGRESS_INTERVAL {
            return false;
        }
        state.last_emit = now;
        true
    }

    fn emit_progress(&self, force: bool) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let percent = aggregate_percent(&state.components, state.phase.as_ref());
        let done = state
            .components
            .iter()
            .map(|component| component.done)
            .sum::<u64>();
        let total = total_if_known(&state.components);
        let estimate = state
            .components
            .iter()
            .filter_map(|component| component.total)
            .sum::<u64>();
        let elapsed = state.started.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            done as f64 / elapsed
        } else {
            0.0
        };
        let eta = total
            .filter(|total| *total > done && speed > 0.0)
            .map(|total| ((total - done) as f64 / speed).ceil() as u64);
        let components: Vec<Value> = state
            .components
            .iter()
            .map(|component| {
                let percent = component
                    .total
                    .filter(|total| *total > 0)
                    .map(|total| (component.done.min(total) as f64 / total as f64) * 100.0);
                json!({
                    "name": component.name,
                    "downloaded_bytes": component.done,
                    "total_bytes": component.total,
                    "percent": percent,
                })
            })
            .collect();
        let phase = state.phase.as_ref().map(|phase| phase.processor.clone());
        let phase_percent = state.phase.as_ref().and_then(|phase| phase.percent);
        let now = Instant::now();
        let enough_time = now.duration_since(state.last_emit) >= PROGRESS_INTERVAL;
        drop(state);
        if !force && !enough_time {
            return;
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.last_emit = now;
            state.last_percent = percent;
        }
        self.sink.emit(&json!({
            "v": 1,
            "event": "progress",
            "id": self.meta_value(|meta| meta.id.clone()),
            "downloaded_bytes": done,
            "total_bytes": total,
            "total_bytes_estimate": (estimate > 0).then_some(estimate),
            "percent": percent,
            "speed_bps": speed,
            "eta_seconds": eta,
            "elapsed_seconds": elapsed,
            "phase": phase,
            "phase_percent": phase_percent,
            "components": components,
        }));
    }

    fn emit_postprocess(&self, processor: &str, status: &str, percent: Option<f64>) {
        self.sink.emit(&json!({
            "v": 1,
            "event": "postprocess",
            "id": self.meta_value(|meta| meta.id.clone()),
            "processor": processor,
            "status": status,
            "percent": percent,
        }));
    }
}

pub fn emit_playlist(sink: &EventSink, id: &str, title: &str, count: usize) {
    sink.emit(&json!({
        "v": 1,
        "event": "playlist",
        "id": id,
        "title": title,
        "count": count,
    }));
}

pub fn emit_error(sink: &EventSink, code: &str, message: &str) {
    sink.emit(&json!({
        "v": 1,
        "event": "error",
        "code": code,
        "message": message,
    }));
}

fn total_if_known(components: &[ComponentState]) -> Option<u64> {
    if components.is_empty() {
        return None;
    }
    components
        .iter()
        .map(|component| component.total)
        .collect::<Option<Vec<_>>>()
        .map(|totals| totals.into_iter().sum())
}

fn aggregate_percent(components: &[ComponentState], phase: Option<&PhaseState>) -> f64 {
    if let Some(phase) = phase
        && let Some(percent) = phase.percent
    {
        return percent.clamp(0.0, 100.0);
    }
    let done = components.iter().map(|c| c.done).sum::<u64>();
    let total = total_if_known(components);
    if let Some(total) = total.filter(|total| *total > 0) {
        return (done.min(total) as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
    }
    let known_total = components
        .iter()
        .filter_map(|component| component.total)
        .sum::<u64>();
    if known_total > 0 {
        let known_done = components
            .iter()
            .filter(|component| component.total.is_some())
            .map(|component| component.done)
            .sum::<u64>();
        return (known_done.min(known_total) as f64 / known_total as f64 * 100.0).clamp(0.0, 100.0);
    }
    0.0
}

pub fn absolute_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}
