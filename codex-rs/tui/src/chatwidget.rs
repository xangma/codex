use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use codex_apply_patch::ApplyPatchFileChange;
use codex_apply_patch::maybe_parse_apply_patch_verified;
use codex_core::codex_wrapper::CodexConversation;
use codex_core::codex_wrapper::init_codex;
use codex_core::config::Config;
use codex_core::protocol::AgentMessageDeltaEvent;
use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::AgentReasoningDeltaEvent;
use codex_core::protocol::AgentReasoningEvent;
use codex_core::protocol::AgentReasoningRawContentDeltaEvent;
use codex_core::protocol::AgentReasoningRawContentEvent;
use codex_core::protocol::ApplyPatchApprovalRequestEvent;
use codex_core::protocol::BackgroundEventEvent;
use codex_core::protocol::ErrorEvent;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExecApprovalRequestEvent;
use codex_core::protocol::ExecCommandBeginEvent;
use codex_core::protocol::ExecCommandEndEvent;
use codex_core::protocol::InputItem;
use codex_core::protocol::McpToolCallBeginEvent;
use codex_core::protocol::McpToolCallEndEvent;
use codex_core::protocol::Op;
use codex_core::protocol::PatchApplyBeginEvent;
use codex_core::protocol::TaskCompleteEvent;
use codex_core::protocol::TokenUsage;
use codex_core::protocol::TurnDiffEvent;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tracing::info;

use crate::app_event::AppEvent;
use crate::app_event::VscodeSelectionInfo;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::InputResult;
use crate::history_cell::CommandOutput;
use crate::history_cell::HistoryCell;
use crate::history_cell::PatchEventType;
use crate::live_wrap::RowBuilder;
use crate::user_approval_widget::ApprovalRequest;
use codex_file_search::FileMatch;
use ratatui::style::Stylize;
use std::collections::VecDeque;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::process::Stdio;
#[cfg(windows)]
use tokio::io::AsyncBufReadExt;
#[cfg(windows)]
use tokio::io::AsyncWriteExt;
#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;
#[cfg(windows)]
use tokio::runtime::Runtime;

struct RunningCommand {
    command: Vec<String>,
    #[allow(dead_code)]
    cwd: PathBuf,
}

pub(crate) struct ChatWidget<'a> {
    pub(crate) app_event_tx: AppEventSender,
    codex_op_tx: UnboundedSender<Op>,
    pub(crate) bottom_pane: BottomPane<'a>,
    active_history_cell: Option<HistoryCell>,
    config: Config,
    initial_user_message: Option<UserMessage>,
    total_token_usage: TokenUsage,
    last_token_usage: TokenUsage,
    reasoning_buffer: String,
    content_buffer: String,
    // Buffer for streaming assistant answer text; we do not surface partial
    // We wait for the final AgentMessage event and then emit the full text
    // at once into scrollback so the history contains a single message.
    answer_buffer: String,
    running_commands: HashMap<String, RunningCommand>,
    live_builder: RowBuilder,
    current_stream: Option<StreamKind>,
    stream_header_emitted: bool,
    live_max_rows: u16,
    vscode_selection: Option<VscodeSelectionInfo>,
    ipc_tx: Option<std::sync::mpsc::Sender<String>>,
}

// --------------------- IPC helpers (platform-agnostic wrappers) ---------------------

fn selection_from_json(v: &serde_json::Value) -> Option<VscodeSelectionInfo> {
    let lines = v.get("lines").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let characters = v.get("characters").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let text = v
        .get("text")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let rel_path = v
        .get("path")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let language_id = v
        .get("languageId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let start_line = v.get("startLine").and_then(|x| x.as_u64());
    let end_line = v.get("endLine").and_then(|x| x.as_u64());
    Some(VscodeSelectionInfo {
        lines,
        characters,
        text,
        rel_path,
        language_id,
        start_line,
        end_line,
    })
}

fn dispatch_selection_from_str(app_event_tx: &crate::app_event_sender::AppEventSender, data: &str) {
    let evt = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(v) => {
            if v.get("type").and_then(|t| t.as_str()) == Some("selection") {
                Some(crate::app_event::AppEvent::VscodeSelectionUpdate(
                    selection_from_json(&v),
                ))
            } else {
                Some(crate::app_event::AppEvent::VscodeSelectionUpdate(
                    selection_from_json(&v),
                ))
            }
        }
        Err(_) => Some(crate::app_event::AppEvent::VscodeSelectionUpdate(None)),
    };
    if let Some(evt) = evt {
        app_event_tx.send(evt);
    }
}

#[cfg(unix)]
fn start_ipc_reader(path: &str, app_event_tx: crate::app_event_sender::AppEventSender) -> bool {
    let p = path.to_string();
    std::thread::spawn(move || {
        loop {
            match UnixStream::connect(&p) {
                Ok(stream) => {
                    // Set footer hint: IDE connected
                    app_event_tx.send(AppEvent::LatestLog(String::from("__SET_IDE_CONNECTED__")));
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        let n = reader.read_line(&mut line).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        let trimmed = line.trim_end_matches(['\n', '\r']);
                        if trimmed.is_empty() {
                            continue;
                        }
                        dispatch_selection_from_str(&app_event_tx, trimmed);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    });
    true
}

#[cfg(windows)]
fn start_ipc_reader(path: &str, app_event_tx: crate::app_event_sender::AppEventSender) -> bool {
    let pipe_path = path.to_string();
    std::thread::spawn(move || {
        let rt = Runtime::new().expect("tokio rt");
        rt.block_on(async move {
            loop {
                match ClientOptions::new().open(&pipe_path) {
                    Ok(client) => {
                        // Set footer hint: IDE connected
                        app_event_tx
                            .send(AppEvent::LatestLog(String::from("__SET_IDE_CONNECTED__")));
                        let mut reader = tokio::io::BufReader::new(client);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            let n = reader.read_line(&mut line).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            let trimmed = line.trim_end_matches(['\n', '\r']);
                            if trimmed.is_empty() {
                                continue;
                            }
                            dispatch_selection_from_str(&app_event_tx, trimmed);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        });
    });
    true
}

#[cfg(unix)]
fn make_ipc_writer(path: &str) -> Option<std::sync::mpsc::Sender<String>> {
    let p = path.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut backlog: VecDeque<String> = VecDeque::new();
        let mut stream: Option<UnixStream> = None;
        loop {
            if backlog.is_empty() {
                match rx.recv() {
                    Ok(line) => backlog.push_back(line),
                    Err(_) => break,
                }
            }
            while stream.is_none() {
                match UnixStream::connect(&p) {
                    Ok(s) => stream = Some(s),
                    Err(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                }
            }
            while let Some(line) = backlog.pop_front() {
                if let Some(s) = stream.as_mut() {
                    if s.write_all(line.as_bytes()).is_err()
                        || s.write_all(b"\n").is_err()
                        || s.flush().is_err()
                    {
                        stream = None;
                        backlog.push_front(line);
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        break;
                    }
                }
            }
        }
    });
    Some(tx)
}

#[cfg(windows)]
fn make_ipc_writer(path: &str) -> Option<std::sync::mpsc::Sender<String>> {
    let pipe_path = path.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let rt = Runtime::new().expect("tokio rt");
        rt.block_on(async move {
            let mut backlog: VecDeque<String> = VecDeque::new();
            let mut client: Option<tokio::net::windows::named_pipe::NamedPipeClient> = None;
            loop {
                if backlog.is_empty() {
                    match rx.recv() {
                        Ok(line) => backlog.push_back(line),
                        Err(_) => break,
                    }
                }
                while client.is_none() {
                    match ClientOptions::new().open(&pipe_path) {
                        Ok(c) => client = Some(c),
                        Err(_) => {
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        }
                    }
                }
                while let Some(line) = backlog.pop_front() {
                    if let Some(c) = client.as_mut() {
                        if c.write_all(line.as_bytes()).await.is_err()
                            || c.write_all(b"\n").await.is_err()
                            || c.flush().await.is_err()
                        {
                            client = None;
                            backlog.push_front(line);
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            break;
                        }
                    }
                }
            }
        });
    });
    Some(tx)
}

struct UserMessage {
    text: String,
    image_paths: Vec<PathBuf>,
    extra_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Answer,
    Reasoning,
}

impl From<String> for UserMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            image_paths: Vec::new(),
            extra_text: None,
        }
    }
}

fn create_initial_user_message(text: String, image_paths: Vec<PathBuf>) -> Option<UserMessage> {
    if text.is_empty() && image_paths.is_empty() {
        None
    } else {
        Some(UserMessage {
            text,
            image_paths,
            extra_text: None,
        })
    }
}

impl ChatWidget<'_> {
    fn vscode_extension_active(&self) -> bool {
        let marker = self.config.codex_home.join("ide").join("extension.json");
        marker.exists()
    }

    /// Best-effort: if running inside a VS Code terminal, attempt to
    /// install or update the Codex VS Code extension so IPC can work.
    /// This runs non-blocking in a background thread and is safe to call
    /// during UI startup. Failures are logged but ignored.
    fn maybe_autoinstall_vscode_extension(&self) {
        // Only try inside VS Code terminal to avoid surprising installs.
        let in_vscode_term = std::env::var("TERM_PROGRAM")
            .map(|v| v == "vscode")
            .unwrap_or(false)
            || std::env::var("VSCODE_PID").is_ok();
        if !in_vscode_term {
            return;
        }

        // If the extension already looks active (marker present), skip.
        if self.vscode_extension_active() {
            return;
        }

        // Spawn a detached worker so we never block the UI thread.
        std::thread::spawn(move || {
            // Prefer the VS Code stable CLI; fall back to insiders.
            let candidates: &[&str] = if cfg!(windows) {
                &[
                    "code.cmd",
                    "code-insiders.cmd",
                    "code.exe",
                    "code-insiders.exe",
                ]
            } else {
                &["code", "code-insiders"]
            };

            // Helper to check if invoking the CLI succeeds for a quick no-op command.
            let mut chosen: Option<&str> = None;
            for c in candidates {
                let ok = Command::new(c)
                    .arg("--version")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    chosen = Some(*c);
                    break;
                }
            }
            let Some(cli) = chosen else { return };

            // Check if extension exists and version via `--list-extensions --show-versions`.
            // If missing or outdated, install/update with --force.
            let list_out = Command::new(cli)
                .args(["--list-extensions", "--show-versions"])
                .output();
            let need_install_or_update = match list_out {
                Ok(out) => {
                    let s = String::from_utf8_lossy(&out.stdout);
                    // Example lines: "openai.codex-vscode@0.0.1"
                    let mut found: Option<String> = None;
                    for line in s.lines() {
                        if let Some(rest) = line.strip_prefix("openai.codex-vscode@") {
                            found = Some(rest.trim().to_string());
                            break;
                        }
                        if line.trim() == "openai.codex-vscode" {
                            found = Some(String::from("0.0.0"));
                            break;
                        }
                    }
                    match found {
                        None => true,
                        Some(installed_ver) => {
                            // Minimal semver compare against the required version.
                            // Bump this when the extension requires a newer protocol.
                            const REQUIRED: &str = "0.0.1";
                            fn parse(v: &str) -> (u64, u64, u64) {
                                let mut it = v.split('.');
                                let a = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
                                let b = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
                                let c = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
                                (a, b, c)
                            }
                            parse(&installed_ver) < parse(REQUIRED)
                        }
                    }
                }
                Err(_) => true,
            };

            if need_install_or_update {
                let _ = Command::new(cli)
                    .args(["--install-extension", "openai.codex-vscode", "--force"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
        });
    }

    fn write_proposed_patch_json(&self, payload: serde_json::Value) {
        if let Some(tx) = &self.ipc_tx {
            let _ = tx.send(payload.to_string());
        }
    }
    fn interrupt_running_task(&mut self) {
        if self.bottom_pane.is_task_running() {
            self.active_history_cell = None;
            self.bottom_pane.clear_ctrl_c_quit_hint();
            self.submit_op(Op::Interrupt);
            self.bottom_pane.set_task_running(false);
            self.bottom_pane.clear_live_ring();
            self.live_builder = RowBuilder::new(self.live_builder.width());
            self.current_stream = None;
            self.stream_header_emitted = false;
            self.answer_buffer.clear();
            self.reasoning_buffer.clear();
            self.content_buffer.clear();
            self.request_redraw();
        }
    }
    fn layout_areas(&self, area: Rect) -> [Rect; 2] {
        Layout::vertical([
            Constraint::Max(
                self.active_history_cell
                    .as_ref()
                    .map_or(0, |c| c.desired_height(area.width)),
            ),
            Constraint::Min(self.bottom_pane.desired_height(area.width)),
        ])
        .areas(area)
    }
    fn emit_stream_header(&mut self, kind: StreamKind) {
        use ratatui::text::Line as RLine;
        if self.stream_header_emitted {
            return;
        }
        let header = match kind {
            StreamKind::Reasoning => RLine::from("thinking".magenta().italic()),
            StreamKind::Answer => RLine::from("codex".magenta().bold()),
        };
        self.app_event_tx
            .send(AppEvent::InsertHistory(vec![header]));
        self.stream_header_emitted = true;
    }
    fn finalize_active_stream(&mut self) {
        if let Some(kind) = self.current_stream {
            self.finalize_stream(kind);
        }
    }
    pub(crate) fn new(
        config: Config,
        app_event_tx: AppEventSender,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
        enhanced_keys_supported: bool,
    ) -> Self {
        let (codex_op_tx, mut codex_op_rx) = unbounded_channel::<Op>();

        let app_event_tx_clone = app_event_tx.clone();
        // Create the Codex asynchronously so the UI loads as quickly as possible.
        let config_for_agent_loop = config.clone();
        tokio::spawn(async move {
            let CodexConversation {
                codex,
                session_configured,
                ..
            } = match init_codex(config_for_agent_loop).await {
                Ok(vals) => vals,
                Err(e) => {
                    // TODO: surface this error to the user.
                    tracing::error!("failed to initialize codex: {e}");
                    return;
                }
            };

            // Forward the captured `SessionInitialized` event that was consumed
            // inside `init_codex()` so it can be rendered in the UI.
            app_event_tx_clone.send(AppEvent::CodexEvent(session_configured.clone()));
            let codex = Arc::new(codex);
            let codex_clone = codex.clone();
            tokio::spawn(async move {
                while let Some(op) = codex_op_rx.recv().await {
                    let id = codex_clone.submit(op).await;
                    if let Err(e) = id {
                        tracing::error!("failed to submit op: {e}");
                    }
                }
            });

            while let Ok(event) = codex.next_event().await {
                app_event_tx_clone.send(AppEvent::CodexEvent(event));
            }
        });

        // Set up VS Code selection integration
        {
            let ide_dir = config.codex_home.join("ide");
            let _selection_file = ide_dir.join("selection.json");
            let _ = std::fs::create_dir_all(&ide_dir);
            let ext_marker = ide_dir.join("extension.json");
            let _app_evt_for_fallback = app_event_tx.clone();
            let _started_ipc = false;
            let ipc_path = std::fs::read_to_string(&ext_marker)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| {
                    v.get("ipc_path")
                        .and_then(|x| x.as_str().map(|s| s.to_string()))
                });
            if let Some(path) = ipc_path.as_deref() {
                if start_ipc_reader(path, app_event_tx.clone()) {
                    let _ = true; // mark connected in this branch
                }
            }
        }

        let mut s = Self {
            app_event_tx: app_event_tx.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
            }),
            active_history_cell: None,
            config: config.clone(),
            initial_user_message: create_initial_user_message(
                initial_prompt.unwrap_or_default(),
                initial_images,
            ),
            total_token_usage: TokenUsage::default(),
            last_token_usage: TokenUsage::default(),
            reasoning_buffer: String::new(),
            content_buffer: String::new(),
            answer_buffer: String::new(),
            running_commands: HashMap::new(),
            live_builder: RowBuilder::new(80),
            current_stream: None,
            stream_header_emitted: false,
            live_max_rows: 3,
            vscode_selection: None,
            ipc_tx: {
                let ide_dir2 = config.codex_home.clone().join("ide");
                let ext_marker2 = ide_dir2.join("extension.json");
                std::fs::read_to_string(&ext_marker2)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| {
                        v.get("ipc_path")
                            .and_then(|x| x.as_str().map(|s| s.to_string()))
                    })
                    .and_then(|path| make_ipc_writer(&path))
            },
        };
        if s.vscode_extension_active() {
            s.bottom_pane.set_ide_detected(true);
        }
        // Fire-and-forget autoinstall if running in VS Code terminal
        // and the extension doesn't appear active yet.
        s.maybe_autoinstall_vscode_extension();
        s
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.bottom_pane.desired_height(width)
            + self
                .active_history_cell
                .as_ref()
                .map_or(0, |c| c.desired_height(width))
    }

    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Press {
            self.bottom_pane.clear_ctrl_c_quit_hint();
        }

        match self.bottom_pane.handle_key_event(key_event) {
            InputResult::Submitted(text) => {
                // Prepare extra, invisible context from VS Code selection without
                // showing it in the user's visible message/history.
                let mut extra: Option<String> = None;
                if let Some(sel) = &self.vscode_selection {
                    if sel.lines > 0 {
                        let lang = sel
                            .language_id
                            .as_ref()
                            .map(|s| s.as_str())
                            .filter(|s| {
                                !s.is_empty()
                                    && s.chars()
                                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                            })
                            .unwrap_or("");
                        let header = if let Some(p) = &sel.rel_path {
                            match (sel.start_line, sel.end_line) {
                                (Some(s), Some(e)) => format!("From {}:L{}-L{}\n", p, s, e),
                                _ => format!("From {}\n", p),
                            }
                        } else {
                            "Selected code:\n".to_string()
                        };
                        let code = sel
                            .text
                            .as_deref()
                            .unwrap_or("")
                            .replace("```", "\u{0060}\u{0060}\u{0060}");
                        let block = format!("```{}\n{}\n```\n", lang, code);
                        extra = Some(format!("{}{}", header, block));
                    }
                }
                let mut um: UserMessage = text.into();
                um.extra_text = extra;
                self.submit_user_message(um);
            }
            InputResult::None => {}
        }
    }

    pub(crate) fn handle_paste(&mut self, text: String) {
        self.bottom_pane.handle_paste(text);
    }

    pub(crate) fn set_vscode_selection(&mut self, sel: Option<VscodeSelectionInfo>) {
        let lines = sel.as_ref().map(|s| s.lines).filter(|v| *v > 0);
        let rel_path = sel.as_ref().and_then(|s| s.rel_path.clone());
        self.bottom_pane.set_vscode_selection_hint(lines, rel_path);
        self.vscode_selection = sel;
    }

    fn add_to_history(&mut self, cell: HistoryCell) {
        self.app_event_tx
            .send(AppEvent::InsertHistory(cell.plain_lines()));
    }

    fn submit_user_message(&mut self, user_message: UserMessage) {
        let UserMessage {
            text,
            image_paths,
            extra_text,
        } = user_message;
        let mut items: Vec<InputItem> = Vec::new();

        if !text.is_empty() {
            items.push(InputItem::Text { text: text.clone() });
        }

        if let Some(extra) = extra_text.as_ref() {
            if !extra.is_empty() {
                items.push(InputItem::Text {
                    text: extra.clone(),
                });
            }
        }

        for path in image_paths {
            items.push(InputItem::LocalImage { path });
        }

        if items.is_empty() {
            return;
        }

        self.codex_op_tx
            .send(Op::UserInput { items })
            .unwrap_or_else(|e| {
                tracing::error!("failed to send message: {e}");
            });

        // Persist the text to cross-session message history.
        if !text.is_empty() {
            self.codex_op_tx
                .send(Op::AddToHistory { text: text.clone() })
                .unwrap_or_else(|e| {
                    tracing::error!("failed to send AddHistory op: {e}");
                });
        }

        // Only show text portion in conversation history for now.
        if !text.is_empty() {
            self.add_to_history(HistoryCell::new_user_prompt(text.clone()));
        }
    }

    pub(crate) fn handle_codex_event(&mut self, event: Event) {
        let Event { id, msg } = event;
        match msg {
            EventMsg::SessionConfigured(event) => {
                self.bottom_pane
                    .set_history_metadata(event.history_log_id, event.history_entry_count);
                // Record session information at the top of the conversation.
                self.add_to_history(HistoryCell::new_session_info(&self.config, event, true));

                if let Some(user_message) = self.initial_user_message.take() {
                    // If the user provided an initial message, add it to the
                    // conversation history.
                    self.submit_user_message(user_message);
                }

                self.request_redraw();
            }
            EventMsg::AgentMessage(AgentMessageEvent { message: _ }) => {
                // Final assistant answer: commit all remaining rows and close with
                // a blank line. Use the final text if provided, otherwise rely on
                // streamed deltas already in the builder.
                self.finalize_stream(StreamKind::Answer);
                self.request_redraw();
            }
            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta }) => {
                self.begin_stream(StreamKind::Answer);
                self.answer_buffer.push_str(&delta);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent { delta }) => {
                // Stream CoT into the live pane; keep input visible and commit
                // overflow rows incrementally to scrollback.
                self.begin_stream(StreamKind::Reasoning);
                self.reasoning_buffer.push_str(&delta);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text: _ }) => {
                // Final reasoning: commit remaining rows and close with a blank.
                self.finalize_stream(StreamKind::Reasoning);
                self.request_redraw();
            }
            EventMsg::AgentReasoningRawContentDelta(AgentReasoningRawContentDeltaEvent {
                delta,
            }) => {
                // Treat raw reasoning content the same as summarized reasoning for UI flow.
                self.begin_stream(StreamKind::Reasoning);
                self.reasoning_buffer.push_str(&delta);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text: _ }) => {
                // Finalize the raw reasoning stream just like the summarized reasoning event.
                self.finalize_stream(StreamKind::Reasoning);
                self.request_redraw();
            }
            EventMsg::TaskStarted => {
                self.bottom_pane.clear_ctrl_c_quit_hint();
                self.bottom_pane.set_task_running(true);
                // Replace composer with single-line spinner while waiting.
                self.bottom_pane
                    .update_status_text("waiting for model".to_string());
                self.request_redraw();
            }
            EventMsg::TaskComplete(TaskCompleteEvent {
                last_agent_message: _,
            }) => {
                self.bottom_pane.set_task_running(false);
                self.bottom_pane.clear_live_ring();
                self.request_redraw();
            }
            EventMsg::TokenCount(token_usage) => {
                self.total_token_usage = add_token_usage(&self.total_token_usage, &token_usage);
                self.last_token_usage = token_usage;
                self.bottom_pane.set_token_usage(
                    self.total_token_usage.clone(),
                    self.last_token_usage.clone(),
                    self.config.model_context_window,
                );
            }
            EventMsg::Error(ErrorEvent { message }) => {
                self.add_to_history(HistoryCell::new_error_event(message.clone()));
                self.bottom_pane.set_task_running(false);
                self.bottom_pane.clear_live_ring();
                self.live_builder = RowBuilder::new(self.live_builder.width());
                self.current_stream = None;
                self.stream_header_emitted = false;
                self.answer_buffer.clear();
                self.reasoning_buffer.clear();
                self.content_buffer.clear();
                self.request_redraw();
            }
            EventMsg::PlanUpdate(update) => {
                // Commit plan updates directly to history (no status-line preview).
                self.add_to_history(HistoryCell::new_plan_update(update));
            }
            EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                call_id: _,
                command,
                cwd,
                reason,
            }) => {
                self.finalize_active_stream();
                // If this exec is an apply_patch invocation, surface a structured
                // apply_patch payload so the VS Code extension can show diffs while
                // the user decides to approve or deny execution.
                if self.vscode_extension_active() {
                    let detected = match maybe_parse_apply_patch_verified(&command, &cwd) {
                        codex_apply_patch::MaybeApplyPatchVerified::Body(action) => Some(action),
                        _ => {
                            // Fallback for codex --codex-run-as-apply-patch '<patch>' shape
                            if command.len() >= 3
                                && command.get(1).map(|s| s.as_str())
                                    == Some(codex_core::CODEX_APPLY_PATCH_ARG1)
                            {
                                let shim = vec!["apply_patch".to_string(), command[2].clone()];
                                match maybe_parse_apply_patch_verified(&shim, &cwd) {
                                    codex_apply_patch::MaybeApplyPatchVerified::Body(action) => {
                                        Some(action)
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(action) = detected {
                        let mut changes: HashMap<
                            std::path::PathBuf,
                            codex_core::protocol::FileChange,
                        > = HashMap::new();
                        for (p, ch) in action.changes() {
                            let v = match ch {
                                ApplyPatchFileChange::Add { content } => {
                                    codex_core::protocol::FileChange::Add {
                                        content: content.clone(),
                                    }
                                }
                                ApplyPatchFileChange::Delete => {
                                    codex_core::protocol::FileChange::Delete
                                }
                                ApplyPatchFileChange::Update {
                                    unified_diff,
                                    move_path,
                                    new_content: _,
                                } => codex_core::protocol::FileChange::Update {
                                    unified_diff: unified_diff.clone(),
                                    move_path: move_path.clone(),
                                },
                            };
                            changes.insert(p.clone(), v);
                        }
                        let payload = serde_json::json!({
                            "type": "apply_patch",
                            "changes": changes,
                            "title": "Proposed Patch",
                            "cwd": cwd,
                        });
                        self.write_proposed_patch_json(payload);
                    }
                }
                let request = ApprovalRequest::Exec {
                    id,
                    command,
                    cwd,
                    reason,
                };
                self.bottom_pane.push_approval_request(request);
                self.request_redraw();
            }
            EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                call_id,
                changes,
                reason,
                grant_root,
            }) => {
                self.finalize_active_stream();
                if self.vscode_extension_active() {
                    // Mirror payload for the VS Code extension
                    // Provide a helpful title; extension will also show per-file diffs for updates.
                    let payload = serde_json::json!({
                        "type": "apply_patch",
                        "call_id": call_id,
                        "changes": changes,
                        "reason": reason,
                        "grant_root": grant_root,
                        "title": "Proposed Patch",
                    });
                    self.write_proposed_patch_json(payload);
                }
                // ------------------------------------------------------------------
                // Before we even prompt the user for approval we surface the patch
                // summary in the main conversation so that the dialog appears in a
                // sensible chronological order:
                //   (1) codex → proposes patch (HistoryCell::PendingPatch)
                //   (2) UI → asks for approval (BottomPane)
                // This mirrors how command execution is shown (command begins →
                // approval dialog) and avoids surprising the user with a modal
                // prompt before they have seen *what* is being requested.
                // ------------------------------------------------------------------
                self.add_to_history(HistoryCell::new_patch_event(
                    PatchEventType::ApprovalRequest,
                    changes,
                ));

                // Now surface the approval request in the BottomPane as before.
                let request = ApprovalRequest::ApplyPatch {
                    id,
                    reason,
                    grant_root,
                };
                self.bottom_pane.push_approval_request(request);
                self.request_redraw();
            }
            EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                call_id,
                command,
                cwd,
            }) => {
                self.finalize_active_stream();
                // Ensure the status indicator is visible while the command runs.
                self.bottom_pane
                    .update_status_text("running command".to_string());
                self.running_commands.insert(
                    call_id,
                    RunningCommand {
                        command: command.clone(),
                        cwd: cwd.clone(),
                    },
                );
                self.active_history_cell = Some(HistoryCell::new_active_exec_command(command));
            }
            EventMsg::ExecCommandOutputDelta(_) => {
                // TODO
            }
            EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: _,
                auto_approved,
                changes,
            }) => {
                self.add_to_history(HistoryCell::new_patch_event(
                    PatchEventType::ApplyBegin { auto_approved },
                    changes,
                ));
            }
            EventMsg::PatchApplyEnd(event) => {
                if !event.success {
                    self.add_to_history(HistoryCell::new_patch_apply_failure(event.stderr));
                }
                // Notify VS Code extension about patch status
                if self.vscode_extension_active() {
                    let payload = serde_json::json!({
                        "type": "patch_applied",
                        "success": event.success,
                        "call_id": event.call_id,
                    });
                    if let Some(tx) = &self.ipc_tx {
                        let _ = tx.send(payload.to_string());
                    }
                }
            }
            EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id,
                exit_code,
                duration: _,
                stdout,
                stderr,
            }) => {
                // Compute summary before moving stdout into the history cell.
                let cmd = self.running_commands.remove(&call_id);
                self.active_history_cell = None;
                self.add_to_history(HistoryCell::new_completed_exec_command(
                    cmd.map(|cmd| cmd.command).unwrap_or_else(|| vec![call_id]),
                    CommandOutput {
                        exit_code,
                        stdout,
                        stderr,
                    },
                ));
            }
            EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id: _,
                invocation,
            }) => {
                self.finalize_active_stream();
                self.add_to_history(HistoryCell::new_active_mcp_tool_call(invocation));
            }
            EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id: _,
                duration,
                invocation,
                result,
            }) => {
                self.add_to_history(HistoryCell::new_completed_mcp_tool_call(
                    80,
                    invocation,
                    duration,
                    result
                        .as_ref()
                        .map(|r| r.is_error.unwrap_or(false))
                        .unwrap_or(false),
                    result,
                ));
            }
            EventMsg::GetHistoryEntryResponse(event) => {
                let codex_core::protocol::GetHistoryEntryResponseEvent {
                    offset,
                    log_id,
                    entry,
                } = event;

                // Inform bottom pane / composer.
                self.bottom_pane
                    .on_history_entry_response(log_id, offset, entry.map(|e| e.text));
            }
            EventMsg::ShutdownComplete => {
                self.app_event_tx.send(AppEvent::ExitRequest);
            }
            EventMsg::TurnDiff(TurnDiffEvent { unified_diff }) => {
                if self.vscode_extension_active() {
                    let payload = serde_json::json!({
                        "type": "turn_diff",
                        "unified_diff": unified_diff,
                        "title": "Turn Diff",
                    });
                    self.write_proposed_patch_json(payload);
                } else {
                    info!("TurnDiffEvent: {unified_diff}");
                }
            }
            EventMsg::BackgroundEvent(BackgroundEventEvent { message }) => {
                info!("BackgroundEvent: {message}");
            }
        }
    }

    /// Update the live log preview while a task is running.
    pub(crate) fn update_latest_log(&mut self, line: String) {
        if self.bottom_pane.is_task_running() {
            self.bottom_pane.update_status_text(line);
        }
    }

    fn request_redraw(&mut self) {
        self.app_event_tx.send(AppEvent::RequestRedraw);
    }

    pub(crate) fn add_diff_output(&mut self, diff_output: String) {
        self.add_to_history(HistoryCell::new_diff_output(diff_output.clone()));
    }

    pub(crate) fn add_status_output(&mut self) {
        self.add_to_history(HistoryCell::new_status_output(
            &self.config,
            &self.total_token_usage,
        ));
    }

    pub(crate) fn add_prompts_output(&mut self) {
        self.add_to_history(HistoryCell::new_prompts_output());
    }

    /// Forward file-search results to the bottom pane.
    pub(crate) fn apply_file_search_result(&mut self, query: String, matches: Vec<FileMatch>) {
        self.bottom_pane.on_file_search_result(query, matches);
    }

    pub(crate) fn on_esc(&mut self) -> bool {
        if self.bottom_pane.is_task_running() {
            self.interrupt_running_task();
            return true;
        }
        false
    }

    /// Handle Ctrl-C key press.
    /// Returns CancellationEvent::Handled if the event was consumed by the UI, or
    /// CancellationEvent::Ignored if the caller should handle it (e.g. exit).
    pub(crate) fn on_ctrl_c(&mut self) -> CancellationEvent {
        match self.bottom_pane.on_ctrl_c() {
            CancellationEvent::Handled => return CancellationEvent::Handled,
            CancellationEvent::Ignored => {}
        }
        if self.bottom_pane.is_task_running() {
            self.interrupt_running_task();
            CancellationEvent::Ignored
        } else if self.bottom_pane.ctrl_c_quit_hint_visible() {
            self.submit_op(Op::Shutdown);
            CancellationEvent::Handled
        } else {
            self.bottom_pane.show_ctrl_c_quit_hint();
            CancellationEvent::Ignored
        }
    }

    pub(crate) fn on_ctrl_z(&mut self) {
        self.interrupt_running_task();
    }

    pub(crate) fn composer_is_empty(&self) -> bool {
        self.bottom_pane.composer_is_empty()
    }

    /// Forward an `Op` directly to codex.
    pub(crate) fn submit_op(&self, op: Op) {
        if let Err(e) = self.codex_op_tx.send(op) {
            tracing::error!("failed to submit op: {e}");
        }
    }

    /// Programmatically submit a user text message as if typed in the
    /// composer. The text will be added to conversation history and sent to
    /// the agent.
    pub(crate) fn submit_text_message(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.submit_user_message(text.into());
    }

    pub(crate) fn token_usage(&self) -> &TokenUsage {
        &self.total_token_usage
    }

    pub(crate) fn clear_token_usage(&mut self) {
        self.total_token_usage = TokenUsage::default();
        self.bottom_pane.set_token_usage(
            self.total_token_usage.clone(),
            self.last_token_usage.clone(),
            self.config.model_context_window,
        );
    }

    pub fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let [_, bottom_pane_area] = self.layout_areas(area);
        self.bottom_pane.cursor_pos(bottom_pane_area)
    }
}

impl ChatWidget<'_> {
    fn begin_stream(&mut self, kind: StreamKind) {
        if let Some(current) = self.current_stream {
            if current != kind {
                self.finalize_stream(current);
            }
        }

        if self.current_stream != Some(kind) {
            self.current_stream = Some(kind);
            self.stream_header_emitted = false;
            // Clear any previous live content; we're starting a new stream.
            self.live_builder = RowBuilder::new(self.live_builder.width());
            // Ensure the waiting status is visible (composer replaced).
            self.bottom_pane
                .update_status_text("waiting for model".to_string());
            self.emit_stream_header(kind);
        }
    }

    fn stream_push_and_maybe_commit(&mut self, delta: &str) {
        self.live_builder.push_fragment(delta);

        // Commit overflow rows (small batches) while keeping the last N rows visible.
        let drained = self
            .live_builder
            .drain_commit_ready(self.live_max_rows as usize);
        if !drained.is_empty() {
            let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
            if !self.stream_header_emitted {
                match self.current_stream {
                    Some(StreamKind::Reasoning) => {
                        lines.push(ratatui::text::Line::from("thinking".magenta().italic()));
                    }
                    Some(StreamKind::Answer) => {
                        lines.push(ratatui::text::Line::from("codex".magenta().bold()));
                    }
                    None => {}
                }
                self.stream_header_emitted = true;
            }
            for r in drained {
                lines.push(ratatui::text::Line::from(r.text));
            }
            self.app_event_tx.send(AppEvent::InsertHistory(lines));
        }

        // Update the live ring overlay lines (text-only, newest at bottom).
        let rows = self
            .live_builder
            .display_rows()
            .into_iter()
            .map(|r| ratatui::text::Line::from(r.text))
            .collect::<Vec<_>>();
        self.bottom_pane
            .set_live_ring_rows(self.live_max_rows, rows);
    }

    fn finalize_stream(&mut self, kind: StreamKind) {
        if self.current_stream != Some(kind) {
            // Nothing to do; either already finalized or not the active stream.
            return;
        }
        // Flush any partial line as a full row, then drain all remaining rows.
        self.live_builder.end_line();
        let remaining = self.live_builder.drain_rows();
        // TODO: Re-add markdown rendering for assistant answers and reasoning.
        // When finalizing, pass the accumulated text through `markdown::append_markdown`
        // to build styled `Line<'static>` entries instead of raw plain text lines.
        if !remaining.is_empty() || !self.stream_header_emitted {
            let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
            if !self.stream_header_emitted {
                match kind {
                    StreamKind::Reasoning => {
                        lines.push(ratatui::text::Line::from("thinking".magenta().italic()));
                    }
                    StreamKind::Answer => {
                        lines.push(ratatui::text::Line::from("codex".magenta().bold()));
                    }
                }
                self.stream_header_emitted = true;
            }
            for r in remaining {
                lines.push(ratatui::text::Line::from(r.text));
            }
            // Close the block with a blank line for readability.
            lines.push(ratatui::text::Line::from(""));
            self.app_event_tx.send(AppEvent::InsertHistory(lines));
        }

        // Clear the live overlay and reset state for the next stream.
        self.live_builder = RowBuilder::new(self.live_builder.width());
        self.bottom_pane.clear_live_ring();
        self.current_stream = None;
        self.stream_header_emitted = false;
    }
}

impl WidgetRef for &ChatWidget<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let [active_cell_area, bottom_pane_area] = self.layout_areas(area);
        (&self.bottom_pane).render(bottom_pane_area, buf);
        if let Some(cell) = &self.active_history_cell {
            cell.render_ref(active_cell_area, buf);
        }
    }
}

fn add_token_usage(current_usage: &TokenUsage, new_usage: &TokenUsage) -> TokenUsage {
    let cached_input_tokens = match (
        current_usage.cached_input_tokens,
        new_usage.cached_input_tokens,
    ) {
        (Some(current), Some(new)) => Some(current + new),
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(new),
        (None, None) => None,
    };
    let reasoning_output_tokens = match (
        current_usage.reasoning_output_tokens,
        new_usage.reasoning_output_tokens,
    ) {
        (Some(current), Some(new)) => Some(current + new),
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(new),
        (None, None) => None,
    };
    TokenUsage {
        input_tokens: current_usage.input_tokens + new_usage.input_tokens,
        cached_input_tokens,
        output_tokens: current_usage.output_tokens + new_usage.output_tokens,
        reasoning_output_tokens,
        total_tokens: current_usage.total_tokens + new_usage.total_tokens,
    }
}
