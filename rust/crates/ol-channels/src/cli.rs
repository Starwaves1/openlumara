//! Port of channels/cli: the interactive terminal channel.
//!
//! python uses prompt_toolkit (input with history, auto-suggest and a bottom toolbar)
//! and rich (colours, progress bars, a "Thinking.." spinner). The port uses rustyline
//! and hand-written ANSI rendering:
//!
//! - line editing: rustyline in emacs mode (ctrl-r reverse search, case-insensitive),
//!   fish-style suggestions from history (accept with → / ctrl-f / ctrl-e), ctrl-z suspend
//! - history: `data/cli_history`, read and written in prompt_toolkit's FileHistory format
//! - output arriving while the prompt is shown is printed above it (patch_stdout)
//! - the status bar is drawn as a line *above* the prompt instead of a bottom toolbar
//!   (rustyline has no toolbar support)
//! - streamed text is printed as-is; rich's markup parsing / repr highlighting of the
//!   printed text is not reproduced
//! - prompt_toolkit's `enable_system_prompt` (meta-! shell commands) is not available

#[path = "../../../generated/channels/cli.rs"]
mod meta;

use futures::StreamExt;
use ol_core::channel::{Channel, ChannelBehavior, SendArgs, TextFormatOpts, TextStrings, TokenStream};
use ol_core::pyjson::{as_f64, py_str};
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::Mutex;
use rand::seq::SliceRandom;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::history::{DefaultHistory, History};
use rustyline::{Context as RlContext, ExternalPrinter};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

crate::descriptor_from_meta!("cli", meta::settings, factory);

fn factory() -> Arc<dyn ChannelBehavior> {
    Arc::new(Cli::default())
}

pub const BLURBS: &[&str] = &[
    "making AI agents easy for everyday people since 2026",
    "the AI framework that puts local AI first",
    "because AI agents should be for more than just coding",
    "what are we doing today?",
    "Rose22 says: i liek stawrbery",
    "Rose22 says: remember to drink water!",
    "the \"open\" stands for open source",
    "where local AI is the #1 priority",
    "your sanctuary in a sea of vibecoded noise",
    "cats have claws too, not just lobsters. meow! :3",
    "fast, lightweight, and modular",
    "the AI agent that literally can't wreck your computer",
    "remember to hydrate!",
    "em dashes are annoying — don't be an em dash.",
    "press A to jump",
    "press B to crouch",
    "press START to play",
    "insert theme song here",
    "initializing artificial intelligence",
    "now you're thinking with portals",
    "pairs well with Qwen or Gemma!",
    "because the average person doesn't have datacenter amounts of VRAM",
    "fully local, fully private",
    "your data stays where it belongs - on your hardware",
    "fully open source, fully local, fully private",
];

// ---------------------------------------------------------------------------------
// ANSI styling (a tiny subset of rich's style language)
// ---------------------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const RED: &str = "\x1b[31m";
const CLEAR_LINE: &str = "\r\x1b[2K";

/// rich colour name / `#rrggbb` / `rgb(r,g,b)` / `color(n)` → ANSI SGR parameters
pub fn color_params(color: &str, background: bool) -> Option<String> {
    let c = color.trim().to_lowercase();
    let (base, ext) = if background { (40, 48) } else { (30, 38) };
    if let Some(hex) = c.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            let p = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
            return Some(format!("{};2;{};{};{}", ext, p(0), p(2), p(4)));
        }
        return None;
    }
    if let Some(inner) = c.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<u8> = inner.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        if parts.len() == 3 {
            return Some(format!("{};2;{};{};{}", ext, parts[0], parts[1], parts[2]));
        }
        return None;
    }
    if let Some(n) = c.strip_prefix("color(").and_then(|s| s.strip_suffix(')')).and_then(|s| s.trim().parse::<u8>().ok()) {
        return Some(format!("{};5;{}", ext, n));
    }
    const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    if let Some(i) = NAMES.iter().position(|n| *n == c) {
        return Some(format!("{}", base + i));
    }
    if let Some(i) = c.strip_prefix("bright_").and_then(|n| NAMES.iter().position(|x| *x == n)) {
        return Some(format!("{}", base + 60 + i));
    }
    None
}

fn fg(color: &str) -> String {
    color_params(color, false).map(|p| format!("\x1b[{}m", p)).unwrap_or_default()
}

fn bg(color: &str) -> String {
    color_params(color, true).map(|p| format!("\x1b[{}m", p)).unwrap_or_default()
}

// ---------------------------------------------------------------------------------
// prompt_toolkit FileHistory format
// ---------------------------------------------------------------------------------

/// parse a prompt_toolkit history file into entries, oldest first
pub fn parse_history(text: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    let mut flush = |lines: &mut Vec<&str>| {
        if !lines.is_empty() {
            let joined: String = lines.concat();
            let mut s = joined.as_str();
            // "".join(lines)[:-1] drops the trailing newline
            if let Some(last) = s.chars().last() {
                s = &s[..s.len() - last.len_utf8()];
            }
            entries.push(s.to_string());
            lines.clear();
        }
    };
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix('+') {
            lines.push(rest);
        } else {
            flush(&mut lines);
        }
    }
    flush(&mut lines);
    entries
}

/// one prompt_toolkit history record (`str(datetime.now())` header + `+line`s)
pub fn format_history_entry(entry: &str, now: chrono::NaiveDateTime) -> String {
    let ts = if now.and_utc().timestamp_subsec_micros() == 0 {
        now.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        now.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
    };
    let mut out = format!("\n# {}\n", ts);
    for line in entry.split('\n') {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------------
// status bar / progress bar rendering
// ---------------------------------------------------------------------------------

/// prompt_toolkit's `bottom-toolbar` style: "#0A0A0A bg:#777777"
const TOOLBAR_STYLE: &str = "\x1b[38;2;10;10;10;48;2;119;119;119m";

pub struct StatusBar {
    pub plain: String,
    pub colored: String,
}

/// the status bar text (python Cli.bottom_bar), padded/truncated to `width` columns
#[allow(clippy::too_many_arguments)]
pub fn render_status_bar(
    chat_title: &str,
    model: &str,
    token_usage: i64,
    max_tokens: f64,
    api_url: &str,
    accent: &str,
    flags: [bool; 4],
    width: usize,
) -> StatusBar {
    let [show_title, show_model, show_tokens, show_url] = flags;
    let pct = if max_tokens > 0.0 { (token_usage as f64 / max_tokens).clamp(0.0, 1.0) } else { 0.0 };
    let filled = (pct * 20.0) as usize;
    let max_str = if max_tokens.fract() == 0.0 { format!("{}", max_tokens as i64) } else { format!("{}", max_tokens) };

    // segments of (text, is_token_fill)
    let mut parts: Vec<Vec<(String, bool)>> = Vec::new();
    if show_title {
        let n = chat_title.chars().count();
        let t: String = chat_title.chars().take(25).collect();
        parts.push(vec![(format!("{}{}", t, if n > 25 { ".." } else { "" }), false)]);
    }
    if show_model {
        parts.push(vec![(format!("▣ {}", model), false)]);
    }
    if show_tokens {
        parts.push(vec![
            ("◉ Tokens: ".to_string(), false),
            ("▓".repeat(filled), true),
            (format!("{} {}/{}", "░".repeat(20 - filled), token_usage, max_str), false),
        ]);
    }
    if show_url {
        parts.push(vec![(format!("⇄ {}", api_url), false)]);
    }

    let mut segs: Vec<(String, bool)> = Vec::new();
    for (i, p) in parts.into_iter().enumerate() {
        if i > 0 {
            segs.push((" | ".into(), false));
        }
        segs.extend(p);
    }

    // truncate / pad to the terminal width
    let mut plain = String::new();
    let mut colored = String::from(TOOLBAR_STYLE);
    let mut used = 0;
    for (text, fill) in segs {
        let take: String = text.chars().take(width.saturating_sub(used)).collect();
        used += take.chars().count();
        plain.push_str(&take);
        if fill && !take.is_empty() {
            colored.push_str(&bg(accent));
            colored.push_str(&take);
            colored.push_str(TOOLBAR_STYLE);
        } else {
            colored.push_str(&take);
        }
    }
    let pad = " ".repeat(width.saturating_sub(used));
    plain.push_str(&pad);
    colored.push_str(&pad);
    colored.push_str(RESET);
    StatusBar { plain, colored }
}

/// rich's default Progress columns: description, 40-wide bar, percentage, time remaining
pub fn render_progress(description: &str, accent: &str, completed: f64, remaining_secs: Option<f64>) -> String {
    const WIDTH: usize = 40;
    let completed = completed.clamp(0.0, 1.0);
    let complete_halves = (WIDTH as f64 * 2.0 * completed) as usize;
    let bar_count = complete_halves / 2;
    let half = complete_halves % 2;
    let done_style = if completed >= 1.0 { "\x1b[38;2;114;156;31m" } else { "\x1b[38;2;249;38;114m" };
    let back_style = "\x1b[38;5;237m";
    let mut bar = format!("{}{}", done_style, "━".repeat(bar_count));
    if half == 1 {
        bar.push('╸');
    }
    let remaining = WIDTH - bar_count - half;
    if remaining > 0 {
        bar.push_str(back_style);
        if half == 0 && bar_count > 0 {
            bar.push('╺');
            bar.push_str(&"━".repeat(remaining - 1));
        } else {
            bar.push_str(&"━".repeat(remaining));
        }
    }
    bar.push_str(RESET);
    let eta = match remaining_secs {
        Some(s) if s.is_finite() && s >= 0.0 => {
            let s = s.ceil() as i64;
            format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
        }
        _ => "-:--:--".to_string(),
    };
    format!("{}{}{} {} \x1b[35m{:>3.0}%{} \x1b[36m{}{}", fg(accent), description, RESET, bar, completed * 100.0, RESET, eta, RESET)
}

/// rich's "dots" spinner
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ---------------------------------------------------------------------------------
// process-wide terminal input (one rustyline editor, survives restarts)
// ---------------------------------------------------------------------------------

#[derive(Default)]
struct CliHelper {
    hinter: HistoryHinter,
    prompt_plain: String,
    prompt_colored: String,
}

impl rustyline::completion::Completer for CliHelper {
    type Candidate = String;
}
impl rustyline::validate::Validator for CliHelper {}
impl rustyline::Helper for CliHelper {}
impl Hinter for CliHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, ctx: &RlContext<'_>) -> Option<String> {
        self.hinter.hint(line, pos, ctx)
    }
}
impl Highlighter for CliHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, default: bool) -> Cow<'b, str> {
        if default && prompt == self.prompt_plain {
            Cow::Borrowed(self.prompt_colored.as_str())
        } else {
            Cow::Borrowed(prompt)
        }
    }
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        // prompt_toolkit's auto-suggestion colour
        Cow::Owned(format!("\x1b[38;2;102;102;102m{}{}", hint, RESET))
    }
}

type CliEditor = rustyline::Editor<CliHelper, DefaultHistory>;

enum ReadResult {
    Line(String),
    Interrupted,
    Eof,
    Error(String),
}

struct Input {
    editor: Arc<Mutex<Option<CliEditor>>>,
    printer: Mutex<Option<Box<dyn ExternalPrinter + Send>>>,
    history_path: Mutex<Option<String>>,
    in_progress: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::UnboundedSender<ReadResult>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ReadResult>>,
}

static INPUT: Lazy<Input> = Lazy::new(|| {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    Input {
        editor: Arc::new(Mutex::new(None)),
        printer: Mutex::new(None),
        history_path: Mutex::new(None),
        in_progress: Arc::new(AtomicBool::new(false)),
        tx,
        rx: tokio::sync::Mutex::new(rx),
    }
});

/// terminal attributes before rustyline switched to raw mode; restored at exit if the
/// process ends while a prompt is still open (the blocking read can't be interrupted)
static SAVED_TERMIOS: OnceCell<libc::termios> = OnceCell::new();
static PROMPTING: AtomicBool = AtomicBool::new(false);

extern "C" fn restore_terminal_at_exit() {
    if PROMPTING.load(Ordering::SeqCst) {
        if let Some(t) = SAVED_TERMIOS.get() {
            // SAFETY: plain libc call with a termios struct obtained from tcgetattr
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
        println!();
    }
}

impl Input {
    /// create the editor once per process (python: _setup_history + PromptSession)
    fn ensure_editor(&self) -> anyhow::Result<()> {
        let mut guard = match self.editor.try_lock() {
            Some(g) => g,
            // a read from before a restart is still running: the editor exists
            None => return Ok(()),
        };
        if guard.is_some() {
            return Ok(());
        }
        let config = rustyline::Config::builder()
            .auto_add_history(false)
            .max_history_size(100_000)?
            .history_ignore_dups(true)?
            .edit_mode(rustyline::EditMode::Emacs)
            .build();
        let mut editor = CliEditor::with_config(config)?;
        editor.set_helper(Some(CliHelper::default()));

        let history_file = std::path::Path::new(&ol_core::get_data_path(None)?).join("cli_history").to_string_lossy().to_string();
        if let Ok(bytes) = std::fs::read(&history_file) {
            for entry in parse_history(&String::from_utf8_lossy(&bytes)) {
                let _ = editor.history_mut().add(&entry);
            }
        }
        *self.history_path.lock() = Some(history_file);

        if let Ok(p) = editor.create_external_printer() {
            *self.printer.lock() = Some(Box::new(p));
        }

        if SAVED_TERMIOS.get().is_none() {
            // SAFETY: tcgetattr fills a zeroed termios struct for fd 0
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) == 0 {
                    let _ = SAVED_TERMIOS.set(t);
                    libc::atexit(restore_terminal_at_exit);
                }
            }
        }
        *guard = Some(editor);
        Ok(())
    }

    /// read one line. a read left running by a previous (restarted) cli instance is
    /// reused instead of starting a second reader on the terminal.
    async fn read(&self, prompt_plain: String, prompt_colored: String) -> ReadResult {
        if !self.in_progress.swap(true, Ordering::SeqCst) {
            let editor = self.editor.clone();
            let in_progress = self.in_progress.clone();
            let tx = self.tx.clone();
            let history_path = self.history_path.lock().clone();
            tokio::task::spawn_blocking(move || {
                let mut guard = editor.lock();
                let result = match guard.as_mut() {
                    None => ReadResult::Error("editor not initialized".into()),
                    Some(ed) => {
                        if let Some(h) = ed.helper_mut() {
                            h.prompt_plain = prompt_plain.clone();
                            h.prompt_colored = prompt_colored;
                        }
                        PROMPTING.store(true, Ordering::SeqCst);
                        let r = ed.readline(&prompt_plain);
                        PROMPTING.store(false, Ordering::SeqCst);
                        match r {
                            Ok(line) => {
                                if !line.is_empty() && ed.add_history_entry(line.as_str()).unwrap_or(false) {
                                    if let Some(p) = &history_path {
                                        append_history(p, &line);
                                    }
                                }
                                ReadResult::Line(line)
                            }
                            Err(rustyline::error::ReadlineError::Interrupted) => ReadResult::Interrupted,
                            Err(rustyline::error::ReadlineError::Eof) => ReadResult::Eof,
                            Err(e) => ReadResult::Error(e.to_string()),
                        }
                    }
                };
                drop(guard);
                in_progress.store(false, Ordering::SeqCst);
                let _ = tx.send(result);
            });
        }
        let mut rx = self.rx.lock().await;
        rx.recv().await.unwrap_or(ReadResult::Eof)
    }
}

fn append_history(path: &str, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
        let _ = f.write_all(format_history_entry(line, chrono::Local::now().naive_local()).as_bytes());
    }
}

// ---------------------------------------------------------------------------------
// the channel
// ---------------------------------------------------------------------------------

#[derive(Default)]
struct LiveState {
    /// None = nothing shown, Some(text) = current live line (spinner/progress)
    line: Option<String>,
    spinner: bool,
    frame: usize,
}

#[derive(Default)]
pub struct Cli {
    stopped: AtomicBool,
    console_ready: AtomicBool,
    token_usage: AtomicI64,
    live: Arc<Mutex<LiveState>>,
}

/// write to the terminal. while a prompt is open, output goes above it.
fn out(live: &Mutex<LiveState>, text: &str) {
    let live_active = live.lock().line.is_some();
    let text = if live_active { format!("{}{}", CLEAR_LINE, text) } else { text.to_string() };
    if PROMPTING.load(Ordering::SeqCst) {
        if let Some(p) = INPUT.printer.lock().as_mut() {
            if p.print(text.clone()).is_ok() {
                return;
            }
        }
    }
    crate::util::print_flush(&text);
}

impl Cli {
    fn accent(ch: &Channel) -> String {
        ch.config.get_str("accent_color").filter(|s| !s.is_empty()).unwrap_or_else(|| "white".into())
    }

    fn println(&self, text: &str) {
        out(&self.live, &format!("{}\n", text));
    }

    fn live_show(&self, text: String) {
        let mut l = self.live.lock();
        if l.line.is_none() {
            crate::util::print_flush("\x1b[?25l");
        }
        crate::util::print_flush(&format!("{}{}", CLEAR_LINE, text));
        l.line = Some(text);
    }

    fn live_hide(&self) {
        let mut l = self.live.lock();
        if l.line.take().is_some() {
            crate::util::print_flush(&format!("{}\x1b[?25h", CLEAR_LINE));
        }
        l.spinner = false;
    }

    /// rich.status.Status("Thinking..")
    fn spinner_start(&self) {
        {
            let mut l = self.live.lock();
            l.spinner = true;
            l.frame = 0;
        }
        self.live_show(format!("\x1b[32m{}{} Thinking..", SPINNER[0], RESET));
        let live = self.live.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(80)).await;
                let mut l = live.lock();
                if !l.spinner || l.line.is_none() {
                    break;
                }
                l.frame = (l.frame + 1) % SPINNER.len();
                let text = format!("\x1b[32m{}{} Thinking..", SPINNER[l.frame], RESET);
                crate::util::print_flush(&format!("{}{}", CLEAR_LINE, text));
                l.line = Some(text);
            }
        });
    }

    async fn update_token_usage(&self, ch: &Arc<Channel>) {
        self.token_usage.store(ch.context.get_total_tokens().await, Ordering::SeqCst);
    }

    fn status_bar(&self, ch: &Arc<Channel>, width: usize) -> StatusBar {
        let title = ch.chat.get_str("title").filter(|s| !s.is_empty()).unwrap_or_else(|| "New chat".into());
        let model = ch.manager().api().get_model().filter(|s| !s.is_empty()).unwrap_or_else(|| "model not set".into());
        let max_tokens = ol_core::config::get(&["api", "max_context"]).and_then(|v| as_f64(&v)).unwrap_or(0.0);
        let api_url = ol_core::config::get(&["api", "url"]).map(|v| py_str(&v)).unwrap_or_else(|| "None".into());
        let flags = [
            ch.config.get_bool("show_chat_title", true),
            ch.config.get_bool("show_model_name", true),
            ch.config.get_bool("show_token_usage", true),
            ch.config.get_bool("show_api_url", false),
        ];
        render_status_bar(&title, &model, self.token_usage.load(Ordering::SeqCst), max_tokens, &api_url, &Self::accent(ch), flags, width)
    }

    async fn render_stream(&self, ch: &Arc<Channel>, stream: TokenStream) -> bool {
        let accent = Self::accent(ch);
        let show_reasoning = ch.config.get_bool("show_reasoning", false);
        let strings = TextStrings {
            thinking_header: "Thinking:".into(),
            thinking_newline: "\n-> ".into(),
            conclusion_header: "".into(),
            separator: if show_reasoning { "-".repeat(8) } else { String::new() },
            tool_call_header: "🔧 calling tool {tool_name}".into(),
        };
        let opts = TextFormatOpts { chunk_size: None, use_markdown: false, strings: Some(strings), show_indicators: false };
        let mut tokens = ch.format_stream_for_text(stream, opts);

        let mut processing_prompt = false;
        let mut first_processing_prompt = true;
        let mut bar_mode: Option<&str> = None;
        let mut bar_desc = String::new();
        let mut bar_started = Instant::now();
        let mut reasoning_indicator_started = false;

        while let Some(token) = tokens.next().await {
            let ttype = token.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let content = token.get("content").cloned().unwrap_or(Value::Null);

            if ttype == ol_core::api::CANCELLED {
                self.live_hide();
                self.println(&format!("\n{}cancelled.{}", fg(&accent), RESET));
                return false;
            }
            if ttype == "error" {
                self.live_hide();
                self.println(&format!("{}{}ERROR:{}{} {}{}", RED, BOLD, RESET, RED, py_str(&content), RESET));
                continue;
            } else if ttype == "user_message" || ttype == "token_usage" {
                continue;
            }

            let mut show_bar = |mode: &'static str, description: String, this: &Self| {
                if !processing_prompt {
                    if !first_processing_prompt {
                        this.println("");
                    }
                    first_processing_prompt = false;
                    processing_prompt = true;
                    bar_started = Instant::now();
                    bar_desc = description;
                } else if bar_mode != Some(mode) {
                    bar_desc = description;
                }
                bar_mode = Some(mode);
            };

            if ttype == "prompt_progress" {
                show_bar("prompt", "Processing..".into(), self);
                let num = |k: &str| content.get(k).and_then(as_f64).unwrap_or(0.0);
                let cache = num("cache");
                let processed = num("processed") - cache;
                let total = num("total") - cache;
                let completed = if total > 0.0 { processed / total } else { 0.0 };
                self.live_show(render_progress(&bar_desc, &accent, completed, eta(bar_started, completed)));
            } else if ttype == "model_load_progress" {
                let model_name = content.get("model").map(py_str).unwrap_or_default().rsplit('/').next().unwrap_or("").to_string();
                let label = match content.get("status").and_then(|s| s.as_str()) {
                    Some("queued") => "Queued",
                    Some("downloading") => "Downloading",
                    _ => "Loading",
                };
                let desc = if model_name.is_empty() { format!("{} model", label) } else { format!("{} {}", label, model_name) };
                show_bar("model_load", desc, self);
                let completed = content.get("percent").and_then(as_f64).map(|p| p / 100.0).unwrap_or(0.0);
                self.live_show(render_progress(&bar_desc, &accent, completed, eta(bar_started, completed)));
            } else if processing_prompt {
                self.live_hide();
                processing_prompt = false;
                bar_mode = None;
            }

            if ttype == "reasoning" && !show_reasoning {
                if !reasoning_indicator_started {
                    self.spinner_start();
                    reasoning_indicator_started = true;
                }
            } else if ttype == "formatted" {
                if reasoning_indicator_started {
                    self.live_hide();
                    reasoning_indicator_started = false;
                }
                out(&self.live, &py_str(&content));
            }
        }
        self.live_hide();
        true
    }
}

fn eta(started: Instant, completed: f64) -> Option<f64> {
    let elapsed = started.elapsed().as_secs_f64();
    if completed > 0.0 && completed < 1.0 && elapsed > 0.0 {
        Some(elapsed / completed * (1.0 - completed))
    } else if completed >= 1.0 {
        Some(0.0)
    } else {
        None
    }
}

fn terminal_width() -> usize {
    terminal_size::terminal_size().map(|(w, _)| w.0 as usize).unwrap_or(80)
}

#[async_trait::async_trait]
impl ChannelBehavior for Cli {
    async fn on_ready(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        if !std::io::stdout().is_terminal() {
            return Ok(());
        }
        let accent = Self::accent(ch);
        self.console_ready.store(true, Ordering::SeqCst);
        self.println(&"-".repeat(40));
        self.println(&format!("{}Welcome to OpenLumara{}", fg(&accent), RESET));
        let blurb = BLURBS.choose(&mut rand::thread_rng()).copied().unwrap_or_default();
        self.println(&format!("{}{}{}", ITALIC, blurb, RESET));
        self.println("");

        if ol_core::globals::firstrun() {
            self.println(&"-".repeat(40));
            self.println(&format!("{}First start detected!{}", BOLD, RESET));
            self.println("Welcome to OpenLumara! Here is a quick guide on how to get started:");
            if ch.manager().channel("webui").is_some() {
                self.println("1. Open the WebUI at the link indicated below");
                self.println("2. Click the gear icon inside the webUI (in the header above the chat) to open the Settings Panel");
                self.println("3. Navigate to the API tab, then get your API url from your preferred local AI server (such as llamacpp, lemonade, koboldcpp) or your cloud API");
                self.println("4. Verify the connection is good, then navigate to the Models tab and select your model");
                self.println("5. You're done. Enjoy!");
            }
            self.println(&"-".repeat(40));
        }

        let p = ol_core::config::get(&["core", "cmd_prefix"]).map(|v| py_str(&v)).unwrap_or_else(|| "None".into());
        self.println(&format!("Type {p}new to start a new session, {p}help for help, {p}chats to see your chats"));
        self.println(&format!("Type {p}quit or {p}exit to quit"));
        self.println(&"-".repeat(40));

        self.update_token_usage(ch).await;
        Ok(())
    }

    async fn run(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        // auto disable when not run from a terminal
        if !std::io::stdout().is_terminal() {
            return Ok(());
        }
        INPUT.ensure_editor()?;

        while !self.stopped.load(Ordering::SeqCst) {
            let accent = Self::accent(ch);
            let prompt_text = ch.config.get1("prompt").map(|v| py_str(&v)).unwrap_or_else(|| "None".into());
            let (mut plain, mut colored) = (String::new(), String::new());
            if ch.config.get_bool("show_status_bar", true) {
                let bar = self.status_bar(ch, terminal_width().saturating_sub(1));
                plain = format!("{}\n", bar.plain);
                colored = format!("{}\n", bar.colored);
            }
            plain.push_str(&format!("{} ", prompt_text));
            colored.push_str(&format!("{}{}{} ", fg(&accent), prompt_text, RESET));

            let user_input = match INPUT.read(plain, colored).await {
                ReadResult::Line(l) => l,
                ReadResult::Interrupted | ReadResult::Eof => {
                    self.println("");
                    let m = ch.manager();
                    m.shutdown().await;
                    m.request_shutdown();
                    break;
                }
                ReadResult::Error(e) => anyhow::bail!(e),
            };
            if user_input.is_empty() {
                continue;
            }

            if let Some((_, cmd, _)) = ol_core::commands::extract_cmd(&user_input) {
                if cmd == "quit" || cmd == "exit" {
                    let m = ch.manager();
                    m.shutdown().await;
                    m.request_shutdown();
                    break;
                }
            }

            let stream = ch.send_stream(json!(user_input), SendArgs::authorized());
            ch.push_stream(stream).await;
        }
        Ok(())
    }

    async fn on_stream(&self, ch: &Arc<Channel>, stream: TokenStream) -> anyhow::Result<()> {
        let completed = self.render_stream(ch, stream).await;
        self.update_token_usage(ch).await;
        if completed {
            self.println("");
        }
        Ok(())
    }

    async fn on_shutdown(&self, _ch: &Arc<Channel>) -> anyhow::Result<()> {
        // stop the input loop so that run() exits cleanly
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn on_push(&self, ch: &Arc<Channel>, message: Value) -> anyhow::Result<()> {
        let accent = Self::accent(ch);
        self.println(&"-".repeat(40));
        self.println(&format!("{}{}{}{}", fg(&accent), BOLD, message.get("content").map(py_str).unwrap_or_else(|| "None".into()), RESET));
        self.println(&"-".repeat(40));
        self.update_token_usage(ch).await;
        Ok(())
    }

    fn on_log(&self, ch: &Arc<Channel>, category: &str, message: &str) {
        if category == "toolcall" || !self.console_ready.load(Ordering::SeqCst) {
            return;
        }
        let cat_str = if category.is_empty() { String::new() } else { format!("[{}] ", category.to_uppercase()) };
        self.println(&format!("{}{}{}{}{}", BOLD, fg(&Self::accent(ch)), cat_str, RESET, message));
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert_eq!(color_params("#E0B0FF", false).unwrap(), "38;2;224;176;255");
        assert_eq!(color_params("white", false).unwrap(), "37");
        assert_eq!(color_params("bright_red", true).unwrap(), "101");
        assert_eq!(color_params("color(5)", false).unwrap(), "38;5;5");
        assert!(color_params("nonsense", false).is_none());
    }

    #[test]
    fn history_roundtrip() {
        let t = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap().and_hms_micro_opt(3, 4, 5, 60).unwrap();
        let mut file = String::new();
        file.push_str(&format_history_entry("hello", t));
        file.push_str(&format_history_entry("multi\nline", t));
        assert!(file.starts_with("\n# 2026-01-02 03:04:05.000060\n+hello\n"));
        assert_eq!(parse_history(&file), vec!["hello".to_string(), "multi\nline".to_string()]);
        let t0 = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap().and_hms_opt(3, 4, 5).unwrap();
        assert_eq!(format_history_entry("x", t0), "\n# 2026-01-02 03:04:05\n+x\n");
        assert!(parse_history("").is_empty());
    }

    #[test]
    fn status_bar_text() {
        let b = render_status_bar("A very long chat title that keeps going", "qwen", 500, 1000.0, "http://x", "#E0B0FF", [true, true, true, false], 200);
        let text = b.plain.trim_end();
        assert_eq!(text, format!("A very long chat title th.. | ▣ qwen | ◉ Tokens: {}{} 500/1000", "▓".repeat(10), "░".repeat(10)));
        assert_eq!(b.plain.chars().count(), 200);
        let narrow = render_status_bar("t", "m", 0, 0.0, "u", "white", [true, true, false, true], 5);
        assert_eq!(narrow.plain, "t | ▣");
    }

    #[test]
    fn progress_bar() {
        let strip = |s: &str| regex::Regex::new("\x1b\\[[0-9;?]*[a-zA-Z]").unwrap().replace_all(s, "").to_string();
        let p = strip(&render_progress("Processing..", "white", 0.5, None));
        assert_eq!(p, format!("Processing.. {}╺{}  50% -:--:--", "━".repeat(20), "━".repeat(19)));
        let p = strip(&render_progress("x", "white", 0.0, Some(65.2)));
        assert_eq!(p, format!("x {}   0% 0:01:06", "━".repeat(40)));
    }
}
