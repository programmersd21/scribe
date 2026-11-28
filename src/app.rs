//! the state machine. no io, no terminal, no worker handles, no surprises.
//!
//! main.rs feeds this thing keys and worker events; ui.rs takes a snapshot and
//! draws it. keeping those jobs apart is why i can test most of scribe without
//! a terminal, and why the render code never has to wonder where a value came
//! from.

use crate::anim;
use crate::cli::Args;
use crate::export::{Format, Segment};
use crate::model::{self, ModelInfo};
use crate::theme::ThemeKind;
use crate::util::{
    CompletionCandidate, common_prefix, duration_short, is_url, sanitize_terminal, wrap,
};
use crate::worker::{Event as WorkerEvent, EventKind as WorkerEventKind, Stage};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Fetching,
    Loading,
    Transcribing,
    Paused,
    Ready,
    Cancelled,
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Fetching => "fetching",
            Status::Loading => "loading",
            Status::Transcribing => "transcribing",
            Status::Paused => "paused",
            Status::Ready => "ready",
            Status::Cancelled => "cancelled",
            Status::Error => "error",
        }
    }

    pub fn active(self) -> bool {
        matches!(
            self,
            Status::Fetching | Status::Loading | Status::Transcribing
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Input,
    Transcript,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    Settings,
    ModelPicker,
    ThemePicker,
    Export,
    Search,
    Message,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Idle,
    Downloading,
    Done,
    Failed,
}

/// the live autocomplete session. mirrors an intellisense popup: a ranked
/// candidate list, a highlighted row, and the prefix the list was built from
/// (so stale suggestions are discarded the moment the input changes).
#[derive(Clone, Debug, Default)]
pub struct Completion {
    pub candidates: Vec<CompletionCandidate>,
    pub selected: usize,
    /// input text that produced this list; if input no longer starts with it,
    /// the session is stale and recomputed from scratch.
    pub prefix: String,
    /// true when the user explicitly asked for the list (ctrl+space), which
    /// keeps it open even when there is a single obvious candidate.
    pub forced: bool,
    /// true when the user dismissed the list; typing again reopens it.
    pub dismissed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

type VisualLines = Vec<(usize, String, usize)>;
type VisualCache = Option<(usize, usize, VisualLines, usize)>;

/// a small, non-blocking notice that lives in the bottom-right corner and
/// fades out on its own. used for download and pause events so the transcript
/// is never interrupted by a modal.
#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub created: Instant,
}

impl Completion {
    pub fn current(&self) -> Option<&CompletionCandidate> {
        self.candidates.get(self.selected)
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }
}

pub struct App {
    pub status: Status,
    pub focus: Focus,
    pub overlay: Overlay,
    pub message: String,
    pub toasts: Vec<Toast>,

    pub input: String,
    pub completion: Completion,
    pub title: String,
    pub source_duration: Option<f64>,

    pub model_label: String,
    pub model_name: String,
    pub threads: i32,
    pub lang: String,
    pub translate: bool,
    pub out_dir: String,
    pub theme: ThemeKind,
    pub theme_before_preview: Option<ThemeKind>,
    pub format: Format,

    pub segments: Vec<Segment>,
    pub selected: usize,
    /// first visible *visual line* in the transcript (not a segment index).
    pub scroll: usize,
    /// smoothed scroll position in visual lines, eased toward `scroll` each tick.
    pub scroll_smooth: f32,
    /// when set, the transcript glides back to the top on completion/cancel.
    pub scroll_home: bool,
    pub follow: bool,

    pub query: String,
    pub matches: Vec<usize>,
    pub match_idx: usize,

    pub progress: f64,
    pub progress_smooth: f32,
    pub done_seconds: f64,
    pub envelope: Vec<f32>,
    pub envelope_smooth: Vec<f32>,
    pub started: Option<Instant>,
    pub finished_at: Option<Instant>,
    /// when the current pause began; elapsed time is frozen while set.
    pub paused_at: Option<Instant>,
    /// sticky marker that survives stray worker events until resume/start.
    /// Enter checks this, not just Status::Paused, so it can never restart
    /// a paused job by accident.
    pub pending_resume: bool,
    /// total seconds spent paused, subtracted from wall-clock elapsed.
    pub paused_total: f64,
    pub stage_started: Instant,

    pub download: DownloadState,
    pub download_frac: f64,
    pub download_info: String,
    pub download_file: String,
    pub download_discard: bool,
    /// a paused download has a valid partial file and should resume on enter.
    pub download_paused: bool,
    pub download_models: Vec<ModelInfo>,
    pub download_index: usize,
    /// first visible row in the model picker; the renderer follows the
    /// selection so it never scrolls off either edge of the viewport.
    pub picker_top: usize,

    pub export_index: usize,
    pub settings_index: usize,
    pub theme_index: usize,

    pub tick: u128,
    pub animation_time: f32,
    /// set by ctrl+c / ctrl+q. the loop checks it and unwinds normally, which
    /// matters because process::exit() skips destructors and leaves the
    /// terminal in raw mode. ask me how i learned that.
    pub quit: bool,
    /// identity of the job whose events this app will accept. incremented when
    /// a new job starts, so stragglers from the previous one are ignored.
    pub job_id: u64,
    /// wrapped transcript lines, rebuilt only when segments or width change.
    /// wrapping a 90-minute transcript on every frame was the hot path.
    visual_cache: VisualCache,
    /// bumped whenever the transcript itself changes.
    segments_version: usize,
}

impl App {
    pub fn new(args: &Args) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(2);
        let threads = args.threads.unwrap_or(cores.min(8)).max(1);
        let theme = args
            .theme
            .as_deref()
            .map(ThemeKind::parse)
            .unwrap_or(ThemeKind::CatppuccinMocha);
        let mut app = Self {
            status: Status::Idle,
            focus: Focus::Input,
            overlay: Overlay::None,
            message: String::new(),
            toasts: Vec::new(),
            input: args.input.clone().unwrap_or_default(),
            completion: Completion::default(),
            title: String::new(),
            source_duration: None,
            model_label: "-".into(),
            model_name: args
                .model
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            threads,
            lang: args.lang.clone().unwrap_or_else(|| "auto".into()),
            translate: args.translate.unwrap_or(false),
            out_dir: args
                .out
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            theme,
            theme_before_preview: None,
            format: args.format,
            segments: Vec::new(),
            selected: 0,
            scroll: 0,
            scroll_smooth: 0.0,
            scroll_home: false,
            follow: true,
            query: String::new(),
            matches: Vec::new(),
            match_idx: 0,
            progress: 0.0,
            progress_smooth: 0.0,
            done_seconds: 0.0,
            envelope: vec![0.0; crate::worker::ENVELOPE_BUCKETS],
            envelope_smooth: vec![0.0; crate::worker::ENVELOPE_BUCKETS],
            started: None,
            finished_at: None,
            paused_at: None,
            pending_resume: false,
            paused_total: 0.0,
            stage_started: Instant::now(),
            download: DownloadState::Idle,
            download_frac: 0.0,
            download_info: String::new(),
            download_file: String::new(),
            download_discard: false,
            download_paused: false,
            download_models: model::CATALOG.to_vec(),
            download_index: 0,
            picker_top: 0,
            export_index: 0,
            settings_index: 0,
            theme_index: ThemeKind::ALL.iter().position(|t| *t == theme).unwrap_or(0),
            tick: 0,
            animation_time: 0.0,
            quit: false,
            job_id: 0,
            visual_cache: None,
            segments_version: 0,
        };
        let suggested = model::default_model().label;
        app.set_message(format!(
            "model: {} · enter to start, m for models (try {}), ? for help",
            app.model_label, suggested
        ));
        app
    }

    pub fn set_message(&mut self, text: impl Into<String>) {
        self.message = text.into();
    }

    /// add a transient notice. at most four are kept so the corner never
    /// grows into a wall of text.
    pub fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toasts.push(Toast {
            text: text.into(),
            kind,
            created: Instant::now(),
        });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    pub fn set_model(&mut self, label: String, name: String) {
        self.model_label = label;
        self.model_name = name;
        let tools = if crate::fetch::tools_ready() {
            "tools ready"
        } else {
            "tools will fetch themselves on first url"
        };
        self.set_message(format!("model: {} · {tools}", self.model_label));
    }

    /// apply a worker event if and only if it belongs to the current job.
    /// returns whether the event was accepted.
    pub fn accept_event(&mut self, event: WorkerEvent) -> bool {
        if event.job_id != self.job_id {
            return false;
        }
        match event.kind {
            WorkerEventKind::Stage(s) => self.on_stage(s),
            WorkerEventKind::Info { title, duration } => self.on_info(title, duration),
            WorkerEventKind::Progress { done_s } => self.on_progress(done_s),
            WorkerEventKind::Envelope(e) => self.on_envelope(e),
            WorkerEventKind::Segment(s) => self.on_segment(s),
            WorkerEventKind::Done => self.on_done(),
            WorkerEventKind::Cancelled => self.on_cancelled(),
            WorkerEventKind::CancelledWithWarning(message) => self.on_cancelled_with_warning(message),
            WorkerEventKind::Error(e) => self.on_error(e),
        }
        true
    }

    /// visual lines for the given text width, plus the selected line. cached
    /// so idle frames and cursor moves do not re-wrap the whole transcript.
    pub fn visual_lines(&mut self, text_w: usize) -> (VisualLines, usize) {
        let version = self.segments_version;
        if let Some((cached_version, cached_w, lines, selected)) = &self.visual_cache
            && *cached_version == version
            && *cached_w == text_w
        {
            return (lines.clone(), *selected);
        }
        let mut lines = Vec::new();
        let mut selected_line = 0usize;
        for (idx, seg) in self.segments.iter().enumerate() {
            let wrapped = wrap(&sanitize_terminal(&seg.text), text_w);
            if idx == self.selected {
                selected_line = lines.len();
            }
            for (li, line) in wrapped.into_iter().enumerate() {
                lines.push((idx, line, li));
            }
        }
        self.visual_cache = Some((version, text_w, lines.clone(), selected_line));
        (lines, selected_line)
    }

    pub fn on_stage(&mut self, stage: Stage) {
        // a worker may still have queued stage events when we pause; letting
        // them through would flip Paused back to Transcribing and make the
        // next Enter look like a fresh start instead of a resume.
        if self.status == Status::Paused {
            return;
        }
        self.status = match stage {
            Stage::Fetching => Status::Fetching,
            Stage::Loading => Status::Loading,
            Stage::Transcribing => Status::Transcribing,
        };
        self.stage_started = Instant::now();
        if stage == Stage::Transcribing {
            self.started = Some(Instant::now());
        }
    }

    pub fn on_info(&mut self, title: String, duration: Option<f64>) {
        self.title = title;
        self.source_duration = duration;
    }

    pub fn on_progress(&mut self, done_s: f64) {
        if self.status == Status::Paused {
            return;
        }
        self.done_seconds = self.done_seconds.max(done_s);
        let progress = if let Some(d) = self.source_duration.filter(|d| *d > 0.0) {
            (self.done_seconds / d).clamp(0.0, 1.0)
        } else {
            // estimate duration dynamically based on decoded audio so far
            let estimated_total = (self.done_seconds + 15.0).max(30.0);
            (self.done_seconds / estimated_total).clamp(0.05, 0.98)
        };
        self.progress = self.progress.max(progress);
    }

    pub fn on_envelope(&mut self, env: Vec<f32>) {
        self.envelope = env;
    }

    pub fn on_segment(&mut self, seg: Segment) {
        if self.status == Status::Paused {
            return;
        }
        let matches_query =
            !self.query.is_empty() && seg.text.to_lowercase().contains(&self.query.to_lowercase());
        self.segments.push(seg);
        self.segments_version = self.segments_version.wrapping_add(1);
        if self.follow {
            self.selected = self.segments.len() - 1;
        }
        // appending a segment cannot invalidate earlier matches. keep the
        // query's hits and test only the new line instead of rescanning all.
        if matches_query {
            self.matches.push(self.segments.len() - 1);
        }
    }

    pub fn on_done(&mut self) {
        self.settle_pause();
        self.status = Status::Ready;
        self.progress = 1.0;
        self.scroll_home = true;
        self.follow = false;
        self.finished_at = Some(Instant::now());
        self.set_message(format!(
            "done. {} segments. press e to export",
            self.segments.len()
        ));
        self.toast(
            format!("transcription done · {} segments", self.segments.len()),
            ToastKind::Success,
        );
    }

    fn settle_pause(&mut self) {
        self.pending_resume = false;
        if let Some(at) = self.paused_at.take() {
            self.paused_total += at.elapsed().as_secs_f64();
        }
    }

    pub fn on_cancelled(&mut self) {
        self.settle_pause();
        self.status = Status::Cancelled;
        self.scroll_home = true;
        self.follow = false;
        self.finished_at = Some(Instant::now());
        self.set_message(format!(
            "hit the brakes early. saved {} segments before pulling over. hit e to export",
            self.segments.len()
        ));
    }

    pub fn on_cancelled_with_warning(&mut self, message: String) {
        self.on_cancelled();
        self.set_message(format!("cancelled, but cleanup failed: {message}"));
        self.overlay = Overlay::Message;
    }

    pub fn on_error(&mut self, message: String) {
        self.settle_pause();
        self.status = Status::Error;
        self.scroll_home = true;
        self.follow = false;
        self.finished_at = Some(Instant::now());
        self.set_message(message);
        self.overlay = Overlay::Message;
    }

    /// wall-clock seconds since transcription began. not CPU time; nobody here
    /// is impressed by how busy the machine was.
    pub fn elapsed_s(&self) -> f64 {
        let raw = match self.finished_at {
            Some(end) => self
                .started
                .map(|s| end.duration_since(s).as_secs_f64())
                .unwrap_or(0.0),
            None => self
                .started
                .map(|s| s.elapsed().as_secs_f64())
                .unwrap_or(0.0),
        };
        let current_pause = self
            .paused_at
            .map(|p| p.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        (raw - self.paused_total - current_pause).max(0.0)
    }

    /// guess the remaining time by dividing elapsed by progress. crude, but
    /// arithmetic has served unix well for fifty years.
    pub fn eta_s(&self) -> Option<f64> {
        if self.progress <= 0.0 || self.progress >= 1.0 {
            return None;
        }
        let elapsed = self.elapsed_s();
        if let Some(total_d) = self.source_duration.filter(|d| *d > 0.0)
            && let Some(speed) = self.speed_multiplier()
            && speed > 0.05
        {
            let remaining_s = (total_d - self.done_seconds).max(0.0);
            return Some(remaining_s / speed);
        }
        let total = elapsed / self.progress;
        Some((total - elapsed).max(0.0))
    }

    /// how many seconds of audio we chew through per second of real time.
    /// above 1x means we are keeping up with the tape.
    pub fn speed_multiplier(&self) -> Option<f64> {
        let elapsed = self.elapsed_s();
        if elapsed < 0.1 {
            return None;
        }
        let audio_s = if self.done_seconds > 0.0 {
            self.done_seconds
        } else {
            match self.source_duration {
                Some(d) => d * self.progress,
                None => self.progress * 60.0,
            }
        };
        if audio_s <= 0.0 {
            return None;
        }
        Some(audio_s / elapsed)
    }

    pub fn can_start(&mut self) -> bool {
        if self.status.active() || self.pending_resume || self.status == Status::Paused {
            self.set_message("a job is already live. pause with space, or stop it with c first");
            return false;
        }
        if self.input.trim().is_empty() {
            self.set_message("feed me a file path or a url first");
            return false;
        }
        if self.model_label == "-" {
            self.set_message("no model loaded. press m to download one");
            return false;
        }
        if !is_url(&self.input) && !std::path::Path::new(&self.input).is_file() {
            self.set_message("that file is playing hide-and-seek. double check path or url");
            return false;
        }
        let need = is_url(&self.input);
        if let Err(e) = crate::worker::check_tools(need) {
            self.set_message(e);
            self.overlay = Overlay::Message;
            return false;
        }
        true
    }

    pub fn start(&mut self, job_id: u64) {
        self.job_id = job_id;
        self.title.clear();
        self.source_duration = None;
        self.segments.clear();
        self.segments_version = self.segments_version.wrapping_add(1);
        self.selected = 0;
        self.scroll = 0;
        self.scroll_smooth = 0.0;
        self.scroll_home = false;
        self.follow = true;
        self.progress = 0.0;
        self.progress_smooth = 0.0;
        self.envelope.fill(0.0);
        self.envelope_smooth.fill(0.0);
        self.done_seconds = 0.0;
        self.started = None;
        self.finished_at = None;
        self.paused_at = None;
        self.pending_resume = false;
        self.paused_total = 0.0;
        self.query.clear();
        self.matches.clear();
        self.match_idx = 0;
        self.status = Status::Fetching;
        self.stage_started = Instant::now();
        self.overlay = Overlay::None;
        self.set_message("spinning up whisper engines...");
    }

    /// freeze the clock and mark the job paused. segments, progress and the
    /// worker thread are all left exactly where they are.
    pub fn pause(&mut self) {
        if !self.status.active() {
            return;
        }
        self.status = Status::Paused;
        self.pending_resume = true;
        self.paused_at = Some(Instant::now());
        self.stage_started = Instant::now();
        self.set_message(format!(
            "paused at {} · inference parked (a fetching/decoding step may still finish) · space or enter to resume",
            duration_short(self.done_seconds)
        ));
        self.toast(
            format!(
                "paused at {} · space to resume",
                duration_short(self.done_seconds)
            ),
            ToastKind::Info,
        );
    }

    /// Enter's fast path: if a pause is pending, resume no matter what some
    /// straggler worker event did to `status`. returns true when handled.
    /// finish the glide home: pin both scroll and selection at the top so the
    /// manual follow logic cannot drag us back down on the next frame.
    pub fn settle_scroll_home(&mut self) {
        // selection pins to the first line for the whole glide, otherwise the
        // renderer's "keep selection visible" logic drags us back down.
        self.selected = 0;
        if self.scroll_smooth < 0.5 {
            self.scroll = 0;
            self.scroll_smooth = 0.0;
            self.scroll_home = false;
        }
    }

    pub fn resume_from_enter(&mut self) -> bool {
        if self.pending_resume || self.status == Status::Paused {
            self.resume();
            true
        } else {
            false
        }
    }

    /// restart the clock, excluding the time spent paused.
    pub fn resume(&mut self) {
        if !self.pending_resume && self.status != Status::Paused {
            return;
        }
        self.pending_resume = false;
        if let Some(at) = self.paused_at.take() {
            self.paused_total += at.elapsed().as_secs_f64();
        }
        self.status = Status::Transcribing;
        self.stage_started = Instant::now();
        self.set_message("back on the tape...");
        self.toast("resumed", ToastKind::Success);
    }

    pub fn tick(&mut self, dt: f32) {
        self.tick += 1;
        self.animation_time += dt.max(0.0);
        self.progress_smooth = anim::approach(self.progress_smooth, self.progress as f32, dt, 9.0);
        self.scroll_smooth = anim::approach(self.scroll_smooth, self.scroll as f32, dt, 7.0);
        if self.scroll_home && self.segments.is_empty() {
            self.scroll_home = false;
            self.scroll = 0;
            self.scroll_smooth = 0.0;
        }
        for (sm, target) in self.envelope_smooth.iter_mut().zip(self.envelope.iter()) {
            *sm = anim::approach(*sm, *target, dt, 14.0);
        }
        self.toasts
            .retain(|t| t.created.elapsed().as_secs_f64() < 6.0);
    }

    pub fn recompute_search(&mut self) {
        self.matches.clear();
        if self.query.is_empty() {
            self.match_idx = 0;
            return;
        }
        let needle = self.query.to_lowercase();
        for (i, seg) in self.segments.iter().enumerate() {
            if seg.text.to_lowercase().contains(&needle) {
                self.matches.push(i);
            }
        }
        if self.match_idx >= self.matches.len() {
            self.match_idx = self.matches.len().saturating_sub(1);
        }
    }

    pub fn jump_match(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len();
        self.match_idx = if forward {
            (self.match_idx + 1) % len
        } else {
            (self.match_idx + len - 1) % len
        };
        self.selected = self.matches[self.match_idx];
        self.follow = false;
        self.scroll_home = false;
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.segments.is_empty() {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, self.segments.len() as isize - 1) as usize;
        self.follow = false;
        self.scroll_home = false;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        // scroll in visual-line units; the renderer clamps to valid range.
        let next = self.scroll as isize + delta;
        self.scroll = next.max(0) as usize;
        self.follow = false;
        self.scroll_home = false;
    }

    /// move the model-picker cursor and keep it inside the visible window,
    /// scrolling in both directions instead of only when it hits the bottom.
    pub fn move_picker(&mut self, delta: isize, visible: usize) {
        let len = self.download_models.len();
        if len == 0 {
            return;
        }
        let visible = visible.max(1).min(len);
        let next = (self.download_index as isize + delta).clamp(0, len as isize - 1) as usize;
        self.download_index = next;
        if next < self.picker_top {
            self.picker_top = next;
        } else if next >= self.picker_top + visible {
            self.picker_top = next + 1 - visible;
        }
    }

    /// accept the highlighted candidate. the whole value replaces the input,
    /// which is what makes a directory completion immediately drillable.
    pub fn accept_completion(&mut self) {
        if let Some(c) = self.completion.current() {
            self.input = c.value.clone();
        }
        self.completion = Completion::default();
    }

    /// accept only the portion all candidates share. with `foo` matching
    /// `foobar.wav` and `foobaz.wav`, first tab fills in `fooba` and keeps the
    /// list open; a second tab commits the highlighted one.
    pub fn accept_common_prefix(&mut self) -> bool {
        let shared = common_prefix(&self.completion.candidates);
        if shared.len() > self.input.len() && shared.starts_with(&self.input) {
            self.input = shared;
            self.update_completion();
            true
        } else {
            false
        }
    }

    /// what would be appended to the input if the user accepted right now.
    /// used to render the inline "ghost text" suggestion.
    pub fn ghost_suffix(&self) -> String {
        if self.focus != Focus::Input || self.completion.dismissed {
            return String::new();
        }
        let Some(c) = self.completion.current() else {
            return String::new();
        };
        if !c.value.starts_with(&self.input) {
            return String::new();
        }
        c.value[self.input.len()..].to_string()
    }

    pub fn dismiss_completion(&mut self) {
        self.completion.dismissed = true;
    }

    pub fn force_completion(&mut self) {
        self.completion.forced = true;
        self.completion.dismissed = false;
        self.update_completion();
    }

    /// rebuild the candidate list for the current input. called on every edit.
    pub fn update_completion(&mut self) {
        if self.focus != Focus::Input
            || self.input.is_empty()
            || is_url(&self.input)
            || self.completion.dismissed
        {
            if self.completion.dismissed {
                return;
            }
            self.completion = Completion::default();
            return;
        }

        // if nothing changed since the last scan, keep the session as-is.
        // this preserves the selected row across unrelated redraws.
        if self.completion.prefix == self.input && !self.completion.candidates.is_empty() {
            return;
        }

        let prefix = self.input.clone();
        let candidates = crate::util::complete_path(&prefix);
        let forced = self.completion.forced;
        // a stale session that was dismissed by moving focus should start over.
        if candidates.is_empty() {
            self.completion = Completion {
                prefix,
                forced,
                ..Completion::default()
            };
            return;
        }
        self.completion = Completion {
            candidates,
            selected: 0,
            prefix,
            forced,
            dismissed: false,
        };
    }

    pub fn export_formats() -> [Format; 5] {
        [
            Format::Txt,
            Format::Srt,
            Format::Vtt,
            Format::Md,
            Format::Json,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_labels() {
        assert_eq!(Status::Transcribing.label(), "transcribing");
        assert!(Status::Fetching.active());
        assert!(!Status::Ready.active());
    }

    fn candidate(value: &str) -> crate::util::CompletionCandidate {
        crate::util::CompletionCandidate {
            value: value.into(),
            label: value.into(),
            dir: String::new(),
            is_dir: false,
            score: 0,
        }
    }

    #[test]
    fn completion_navigation_wraps() {
        let mut c = Completion {
            candidates: vec![candidate("a.wav"), candidate("b.wav"), candidate("c.wav")],
            ..Completion::default()
        };
        c.move_by(-1);
        assert_eq!(c.selected, 2);
        c.move_by(1);
        assert_eq!(c.selected, 0);
        c.move_by(1);
        assert_eq!(c.selected, 1);
    }

    #[test]
    fn accepting_and_ghost_text() {
        let mut app = App::new(&Args::default());
        app.input = "/tmp/talk".into();
        app.completion.candidates = vec![candidate("/tmp/talk.wav")];
        app.completion.prefix = app.input.clone();
        assert_eq!(app.ghost_suffix(), ".wav");
        app.accept_completion();
        assert_eq!(app.input, "/tmp/talk.wav");
        assert!(app.ghost_suffix().is_empty());
    }

    #[test]
    fn finish_and_cancel_arm_the_smooth_scroll_home() {
        let mut app = App::new(&Args::default());
        app.scroll = 40;
        app.scroll_smooth = 40.0;
        app.follow = true;
        app.on_done();
        assert!(app.scroll_home);
        assert_eq!(app.scroll, 40); // the renderer drives it to 0 over time
        assert!(!app.follow);

        let mut app = App::new(&Args::default());
        app.scroll = 30;
        app.follow = true;
        app.on_cancelled();
        assert!(app.scroll_home);
        assert!(!app.follow);
    }

    #[test]
    fn scroll_home_stays_pinned_at_top() {
        let mut app = App::new(&Args::default());
        app.scroll = 50;
        app.scroll_smooth = 50.0;
        app.selected = 20;
        app.follow = true;
        app.on_done();
        assert!(app.scroll_home);
        // mid-glide: selection is already parked at the top
        app.scroll_smooth = 12.0;
        app.settle_scroll_home();
        assert!(app.scroll_home);
        assert_eq!(app.selected, 0);
        // glide finished: flag clears, scroll stays at zero
        app.scroll_smooth = 0.2;
        app.settle_scroll_home();
        assert!(!app.scroll_home);
        assert_eq!(app.scroll, 0);
        assert_eq!(app.scroll_smooth, 0.0);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn manual_scroll_disables_follow() {
        let mut app = App::new(&Args::default());
        assert!(app.follow);
        app.scroll_by(3);
        assert!(!app.follow);
        assert_eq!(app.scroll, 3);
        app.scroll_by(-100);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn enter_resumes_even_if_status_was_clobbered() {
        let mut app = App::new(&Args::default());
        app.status = Status::Transcribing;
        app.done_seconds = 42.0;
        app.progress = 0.4;
        app.pause();
        // pretend a straggler event flipped the status back
        app.status = Status::Transcribing;
        assert!(app.resume_from_enter());
        assert_eq!(app.done_seconds, 42.0);
        assert_eq!(app.progress, 0.4);
        assert!(!app.pending_resume);
    }

    #[test]
    fn queued_stage_events_do_not_clear_pause() {
        let mut app = App::new(&Args::default());
        app.status = Status::Transcribing;
        app.done_seconds = 42.0;
        app.pause();
        // a stage/progress event that was already in the channel when we paused
        app.on_stage(crate::worker::Stage::Transcribing);
        app.on_progress(43.0);
        assert_eq!(app.status, Status::Paused);
        assert_eq!(app.done_seconds, 42.0);
    }

    #[test]
    fn picker_scroll_follows_selection_both_ways() {
        let mut app = App::new(&Args::default());
        let n = app.download_models.len();
        assert!(n > 10);
        let visible = 8;

        // walk to the bottom; the viewport should slide down with us.
        for _ in 0..n {
            app.move_picker(1, visible);
        }
        assert_eq!(app.download_index, n - 1);
        assert!(app.picker_top + visible >= n);

        // walk back to the top; the viewport must slide back up too.
        for _ in 0..n {
            app.move_picker(-1, visible);
        }
        assert_eq!(app.download_index, 0);
        assert_eq!(app.picker_top, 0);

        // the selection is always inside the visible window.
        for _ in 0..n {
            app.move_picker(1, visible);
            assert!(app.download_index >= app.picker_top);
            assert!(app.download_index < app.picker_top + visible);
        }
    }

    #[test]
    fn pause_preserves_progress_and_resume_keeps_it() {
        let mut app = App::new(&Args::default());
        app.status = Status::Transcribing;
        app.started = Some(Instant::now() - std::time::Duration::from_secs(10));
        app.done_seconds = 42.0;
        app.progress = 0.4;
        app.segments.push(crate::export::Segment {
            start_cs: 0,
            end_cs: 100,
            text: "hello".into(),
            words: vec![],
        });
        app.pause();
        assert_eq!(app.status, Status::Paused);
        assert_eq!(app.done_seconds, 42.0);
        assert_eq!(app.progress, 0.4);
        assert_eq!(app.segments.len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        app.resume();
        assert_eq!(app.status, Status::Transcribing);
        assert_eq!(app.done_seconds, 42.0);
        assert_eq!(app.progress, 0.4);
        assert_eq!(app.segments.len(), 1);
    }

    #[test]
    fn dismiss_stays_until_edit() {
        let mut app = App::new(&Args::default());
        app.input = "/tmp/talk".into();
        app.completion.dismissed = true;
        app.update_completion();
        assert!(app.completion.candidates.is_empty());
    }

    #[test]
    fn eta_math() {
        let mut app = App::new(&Args::default());
        app.status = Status::Transcribing;
        app.started = Some(Instant::now() - std::time::Duration::from_secs(10));
        app.progress = 0.5;
        let eta = app.eta_s().unwrap();
        assert!((9.0..11.0).contains(&eta));
        let speed = app.speed_multiplier().unwrap();
        assert!((2.5..3.5).contains(&speed));
    }

    #[test]
    fn progress_never_moves_back_across_overlapping_windows() {
        let mut app = App::new(&Args::default());
        app.source_duration = Some(60.0);
        app.on_progress(30.0);
        app.on_progress(28.0);
        assert_eq!(app.done_seconds, 30.0);
        assert_eq!(app.progress, 0.5);
    }

    #[test]
    fn repeated_jobs_do_not_retain_stale_state() {
        let mut app = App::new(&Args::default());
        app.status = Status::Transcribing;
        app.on_segment(Segment::new(0, 100, "old".into()));
        app.done_seconds = 42.0;
        app.progress = 0.8;
        app.query = "old".into();
        app.recompute_search();
        app.finished_at = Some(Instant::now());

        app.start(42);
        assert_eq!(app.job_id, 42);
        assert!(app.segments.is_empty());
        assert_eq!(app.done_seconds, 0.0);
        assert_eq!(app.progress, 0.0);
        assert!(app.finished_at.is_none());
        assert!(app.follow);
        assert!(app.query.is_empty());
        assert!(app.matches.is_empty());
    }

    #[test]
    fn cancel_after_pause_settles_clock() {
        let mut app = App::new(&Args::default());
        app.status = Status::Transcribing;
        app.started = Some(Instant::now() - std::time::Duration::from_secs(5));
        app.pause();
        assert!(app.pending_resume);
        std::thread::sleep(std::time::Duration::from_millis(10));
        app.on_cancelled();
        assert!(!app.pending_resume);
        assert!(app.paused_at.is_none());
        assert_eq!(app.status, Status::Cancelled);
        assert!(app.elapsed_s() <= 5.5, "paused time should be excluded");
    }

    #[test]
    fn stale_job_events_are_rejected() {
        let mut app = App::new(&Args::default());
        app.start(10);
        let stale = WorkerEvent {
            job_id: 9,
            kind: WorkerEventKind::Segment(Segment::new(0, 100, "old".into())),
        };
        assert!(!app.accept_event(stale));
        assert!(app.segments.is_empty());
        let fresh = WorkerEvent {
            job_id: 10,
            kind: WorkerEventKind::Segment(Segment::new(0, 100, "new".into())),
        };
        assert!(app.accept_event(fresh));
        assert_eq!(app.segments.len(), 1);
    }

    #[test]
    fn stale_done_cannot_finish_a_new_job() {
        let mut app = App::new(&Args::default());
        app.start(11);
        let old_done = WorkerEvent {
            job_id: 10,
            kind: WorkerEventKind::Done,
        };
        assert!(!app.accept_event(old_done));
        assert_eq!(app.status, Status::Fetching);
        let done = WorkerEvent {
            job_id: 11,
            kind: WorkerEventKind::Done,
        };
        assert!(app.accept_event(done));
        assert_eq!(app.status, Status::Ready);
    }

    #[test]
    fn active_job_blocks_a_second_start() {
        let mut app = App::new(&Args::default());
        app.input = "https://example.com/a".into();
        app.start(12);
        assert!(!app.can_start());
        assert_eq!(app.job_id, 12);
    }

    #[test]
    fn incremental_search_matches_full_rescan() {
        let mut app = App::new(&Args::default());
        app.status = Status::Transcribing;
        app.query = "hello".into();
        app.on_segment(Segment::new(0, 100, "hello one".into()));
        app.on_segment(Segment::new(100, 200, "nothing".into()));
        app.on_segment(Segment::new(200, 300, "HELLO two".into()));
        let incremental = app.matches.clone();
        app.recompute_search();
        assert_eq!(incremental, app.matches);
        assert_eq!(app.matches, vec![0, 2]);
    }

    #[test]
    fn search_and_jump() {
        let mut app = App::new(&Args::default());
        app.on_segment(Segment::new(0, 100, "hello world".into()));
        app.on_segment(Segment::new(100, 200, "goodbye".into()));
        app.on_segment(Segment::new(200, 300, "hello again".into()));
        app.query = "hello".into();
        app.recompute_search();
        assert_eq!(app.matches, vec![0, 2]);
        app.jump_match(true);
        assert_eq!(app.selected, 2);
        app.jump_match(true);
        assert_eq!(app.selected, 0);
    }
}
