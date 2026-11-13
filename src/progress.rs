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
/// error: it only disables further event output. Other I/O failures are reported
/// once on stderr so a truncated stream does not silently look successful.
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
        if let Err(error) = writeln!(stdout, "{line}").and_then(|()| stdout.flush()) {
            self.inner.broken.store(true, Ordering::Relaxed);
            if error.kind() != std::io::ErrorKind::BrokenPipe {
                let _ = writeln!(
                    std::io::stderr(),
                    "ytrust: failed to write progress JSON: {error}"
                );
            }
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
    components: Vec<ComponentState>,
    phase: Option<PhaseState>,
}

#[derive(Clone)]
pub struct Progress {
    sink: EventSink,
    state: Arc<Mutex<ProgressState>>,
    index: Arc<std::collections::HashMap<String, usize>>,
}

impl Progress {
    pub fn new(sink: EventSink, meta: ProgressMeta, components: Vec<ComponentSpec>) -> Self {
        let index = components
            .iter()
            .enumerate()
            .map(|(index, component)| (component.name.clone(), index))
            .collect();
        let state = ProgressState {
            meta,
            started: Instant::now(),
            last_emit: Instant::now(),
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
            index: Arc::new(index),
        };
        progress.emit_start();
        progress
    }

    fn emit_start(&self) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let total = total_if_known(&state.components);
        let estimate = estimated_total(&state.components);
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
        let value = json!({
            "v": 1,
            "event": "start",
            "id": state.meta.id,
            "title": state.meta.title,
            "original_url": state.meta.original_url,
            "playlist_index": state.meta.playlist_index,
            "playlist_count": state.meta.playlist_count,
            "component_count": components.len(),
            "total_bytes": total,
            "total_bytes_estimate": (estimate > 0).then_some(estimate),
            "components": components,
        });
        drop(state);
        self.sink.emit(&value);
    }

    pub fn add(&self, component: &str, delta: u64) {
        if delta == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = self.index.get(component).copied() else {
            return;
        };
        state.components[index].done = state.components[index].done.saturating_add(delta);
        if state.last_emit.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        state.last_emit = Instant::now();
        let value = progress_value(&state);
        drop(state);
        self.sink.emit(&value);
    }

    pub fn set_total(&self, component: &str, total: u64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = self.index.get(component).copied() else {
            return;
        };
        state.components[index].total = (total > 0).then_some(total);
        state.last_emit = Instant::now();
        let value = progress_value(&state);
        drop(state);
        self.sink.emit(&value);
    }

    pub fn set_done(&self, component: &str, done: u64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = self.index.get(component).copied() else {
            return;
        };
        state.components[index].done = done;
        state.last_emit = Instant::now();
        let value = progress_value(&state);
        drop(state);
        self.sink.emit(&value);
    }

    pub fn finish_component(&self, component: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = self.index.get(component).copied() else {
            return;
        };
        if let Some(total) = state.components[index].total {
            state.components[index].done = total;
        }
        state.last_emit = Instant::now();
        let value = progress_value(&state);
        drop(state);
        self.sink.emit(&value);
    }

    pub fn phase_started(&self, processor: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.phase = Some(PhaseState {
            processor: processor.to_string(),
            percent: None,
        });
        state.last_emit = Instant::now();
        let value = postprocess_value(&state, processor, "started", None);
        drop(state);
        self.sink.emit(&value);
    }

    pub fn phase_progress(&self, percent: f64) {
        let percent = percent.clamp(0.0, 100.0);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(phase) = state.phase.as_mut() else {
            return;
        };
        phase.percent = Some(percent);
        if state.last_emit.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        state.last_emit = Instant::now();
        let processor = state.phase.as_ref().map(|phase| phase.processor.clone());
        let postprocess = processor
            .as_deref()
            .map(|processor| postprocess_value(&state, processor, "progress", Some(percent)));
        let progress = progress_value(&state);
        drop(state);
        if let Some(postprocess) = postprocess {
            self.sink.emit(&postprocess);
        }
        self.sink.emit(&progress);
    }

    pub fn phase_finished(&self, processor: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(phase) = state.phase.as_mut() {
            phase.percent = Some(100.0);
        }
        state.last_emit = Instant::now();
        let postprocess = postprocess_value(&state, processor, "finished", Some(100.0));
        let progress = progress_value(&state);
        state.phase = None;
        drop(state);
        self.sink.emit(&postprocess);
        self.sink.emit(&progress);
    }

    pub fn phase_failed(&self, processor: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.phase = None;
        let value = postprocess_value(&state, processor, "failed", None);
        drop(state);
        self.sink.emit(&value);
    }

    pub fn file(&self, kind: &str, path: &Path, language: Option<&str>, auto: bool) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let value = json!({
            "v": 1,
            "event": "file",
            "id": state.meta.id,
            "kind": kind,
            "path": absolute_path(path).to_string_lossy(),
            "language": language,
            "auto": auto,
            "playlist_index": state.meta.playlist_index,
            "playlist_count": state.meta.playlist_count,
        });
        drop(state);
        self.sink.emit(&value);
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
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let value = json!({
            "v": 1,
            "event": "done",
            "id": state.meta.id,
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
            "playlist_index": state.meta.playlist_index,
            "playlist_count": state.meta.playlist_count,
        });
        drop(state);
        self.sink.emit(&value);
    }

    pub fn error(&self, code: &str, message: &str) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let value = json!({
            "v": 1,
            "event": "error",
            "id": state.meta.id,
            "code": code,
            "message": message,
            "playlist_index": state.meta.playlist_index,
            "playlist_count": state.meta.playlist_count,
        });
        drop(state);
        self.sink.emit(&value);
    }
}

fn progress_value(state: &ProgressState) -> Value {
    let percent = aggregate_percent(&state.components, state.phase.as_ref());
    let done = state
        .components
        .iter()
        .map(|component| component.done)
        .sum::<u64>();
    let total = total_if_known(&state.components);
    let estimate = estimated_total(&state.components);
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
    json!({
        "v": 1,
        "event": "progress",
        "id": state.meta.id,
        "downloaded_bytes": done,
        "total_bytes": total,
        "total_bytes_estimate": (estimate > 0).then_some(estimate),
        "percent": percent,
        "speed_bps": speed,
        "eta_seconds": eta,
        "elapsed_seconds": elapsed,
        "phase": phase,
        "phase_percent": phase_percent,
        "playlist_index": state.meta.playlist_index,
        "playlist_count": state.meta.playlist_count,
        "components": components,
    })
}

fn postprocess_value(
    state: &ProgressState,
    processor: &str,
    status: &str,
    percent: Option<f64>,
) -> Value {
    json!({
        "v": 1,
        "event": "postprocess",
        "id": state.meta.id,
        "processor": processor,
        "status": status,
        "percent": percent,
        "playlist_index": state.meta.playlist_index,
        "playlist_count": state.meta.playlist_count,
    })
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

fn estimated_total(components: &[ComponentState]) -> u64 {
    components
        .iter()
        .filter_map(|component| component.total)
        .sum()
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
    let known_total = estimated_total(components);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn component(name: &str, total: Option<u64>, done: u64) -> ComponentState {
        ComponentState {
            name: name.to_string(),
            total,
            done,
        }
    }

    #[test]
    fn aggregates_known_and_partial_totals() {
        let components = vec![component("video", Some(100), 50)];
        assert_eq!(aggregate_percent(&components, None), 50.0);
        let components = vec![
            component("video", Some(100), 50),
            component("audio", None, 50),
        ];
        assert_eq!(aggregate_percent(&components, None), 50.0);
    }

    #[test]
    fn phase_percent_takes_precedence() {
        let phase = PhaseState {
            processor: "ffmpeg".to_string(),
            percent: Some(25.0),
        };
        let components = vec![component("media", Some(100), 100)];
        assert_eq!(aggregate_percent(&components, Some(&phase)), 25.0);
    }
}
