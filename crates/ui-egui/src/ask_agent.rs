//! Ask Agent: the person selects shapes, text in a shape, or nothing (the slide), picks **Ask
//! Agent** (the chip that floats beside the selection while an agent listens, Home › Ask Agent,
//! the slide's context menu, Ctrl+Shift+O / ⌘⇧O) and types an instruction ("tighten the wording
//! of this slide"). An agent they already run on the same computer, connected through the control
//! channel (`deckcraft-cli mcp --connect`), takes the request with `agent.listen`, does the work
//! with the usual commands and answers with `agent.reply`; the answer shows in a bubble next to
//! the selection. DeckCraft calls no model and holds no provider key: the "provider" is whatever agent the
//! person connected.
//!
//! [`AgentHub`] is the queue between the window and the control channel. The control server's
//! connection threads share it, so `agent.listen` waits there, never on the UI thread; the window
//! reads it when it changes. See `docs/control-protocol.md` (Ask Agent).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use deckcraft_engine::{DocState, Target};
use egui::{Align2, Key, Modifiers, Order, Pos2, Rect, Vec2, pos2, vec2};
use serde_json::{Value, json};

use crate::SlideApp;
use crate::icons::{self, Icon};
use crate::theme::{self, Tokens};

/// The UI command.
pub const COMMAND: &str = "app.askAgent";
/// Its shortcut (Ctrl+Shift+O; ⌘⇧O on macOS): nothing else in DeckCraft uses it.
pub const SHORTCUT: &str = "Cmd+Shift+O";
/// An agent that listened or replied this recently still counts as there, between two
/// `agent.listen`s.
pub const PRESENCE_MS: f64 = 60_000.0;
/// The longest one `agent.listen` waits, whatever it asks for (below the MCP bridge's 90 s read
/// timeout).
pub const MAX_LISTEN_MS: u64 = 60_000;
/// What `agent.listen` waits without a `timeoutMs`.
pub const DEFAULT_LISTEN_MS: u64 = 30_000;
/// How often a waiting `agent.listen` checks that its client is still connected.
#[cfg(not(target_arch = "wasm32"))]
const POLL_MS: u64 = 250;
/// Longest instruction and reply kept, in characters; longer ones are cut with "…".
pub const MAX_TEXT_CHARS: usize = 4000;
/// Longest selected text sent with a request, in bytes (`truncated` says when it was cut). The
/// shapes' summaries are cut to ids, names, kinds and boxes past the same size.
pub const MAX_SELECTION_BYTES: usize = 100_000;
/// Selected shapes described in a request.
const MAX_SHAPES: usize = 50;
/// Requests kept: closed ones are dropped first; open ones only past twice this.
const KEEP: usize = 20;
/// Agent connections remembered for presence.
const MAX_AGENTS: usize = 64;
/// Requests shown in the corner card at once (the newest).
const SHOWN: usize = 3;
/// Width of the bubble and the requests card.
const WIDTH: f32 = 340.0;
/// Width for a long answer, which wraps rather than being cut.
const WIDE: f32 = 440.0;
/// Space between the selection and the chip or bubble.
const GAP: f32 = 6.0;
/// Room for the selection handles drawn on the selection's edge.
const HANDLES: f32 = 6.0;
const CHIP_H: f32 = 24.0;

/// Where a request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// In the queue: no agent has it.
    Waiting,
    /// An agent took it (`agent.listen`) and hasn't closed it.
    Working,
    /// The agent's reply closed it.
    Answered,
    /// The person cancelled it.
    Cancelled,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Working => "working",
            State::Answered => "answered",
            State::Cancelled => "cancelled",
        }
    }
    pub fn open(self) -> bool {
        matches!(self, State::Waiting | State::Working)
    }
}

/// One request from the window.
#[derive(Clone, Debug)]
pub struct Request {
    pub id: u64,
    pub instruction: String,
    /// What was selected when the person asked (see [`selection`]).
    pub selection: Value,
    /// `{title, saved, slides, language}`.
    pub document: Value,
    pub action: Value,
    pub apply: &'static str,
    pub review: Option<&'static str>,
    pub state: State,
    /// The agent's latest text: its answer, or a progress note while it works.
    pub reply: String,
    /// The control-channel connection working on it.
    taken_by: Option<u64>,
    /// When the person asked (`now_ms`), for the time shown while it runs.
    asked_ms: f64,
}

impl Request {
    /// What `agent.listen` hands the agent.
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "instruction": self.instruction, "selection": self.selection, "document": self.document, "action": self.action, "apply": self.apply})
    }
    fn summary(&self) -> Summary {
        Summary {
            id: self.id,
            instruction: self.instruction.clone(),
            state: self.state,
            reply: self.reply.clone(),
            asked_ms: self.asked_ms,
            document: self.document.clone(),
            action: self.action.clone(),
            apply: self.apply,
            review: self.review,
        }
    }
}

/// A request as the window shows it: without the selection, which can be large.
#[derive(Clone, Debug)]
pub struct Summary {
    pub id: u64,
    pub instruction: String,
    pub state: State,
    pub reply: String,
    asked_ms: f64,
    pub document: Value,
    pub action: Value,
    pub apply: &'static str,
    pub review: Option<&'static str>,
}

struct Inner {
    requests: VecDeque<Request>,
    next_id: u64,
    /// `agent.listen` calls waiting right now.
    listeners: usize,
    /// Open connections that have listened or replied, and when they last did (`now_ms`).
    agents: Vec<(u64, f64)>,
    /// Bumped on every change, so the window reads the requests again only when they change.
    version: u64,
    /// The control channel's port, for the "how to connect" message.
    port: u16,
}

impl Inner {
    fn seen(&mut self, conn: u64) {
        let now = crate::now_ms();
        match self.agents.iter_mut().find(|(c, _)| *c == conn) {
            Some(a) => a.1 = now,
            None => {
                if self.agents.len() >= MAX_AGENTS
                    && let Some(i) = self.agents.iter().enumerate().min_by(|a, b| a.1.1.total_cmp(&b.1.1)).map(|(i, _)| i)
                {
                    self.agents.remove(i);
                }
                self.agents.push((conn, now));
            }
        }
    }

    /// Connection `conn` is gone: it no longer counts, and what it was working on goes back in
    /// the queue.
    fn gone(&mut self, conn: u64) {
        self.agents.retain(|(c, _)| *c != conn);
        for r in self.requests.iter_mut().filter(|r| r.state == State::Working && r.taken_by == Some(conn)) {
            r.state = State::Waiting;
            r.taken_by = None;
        }
    }
}

/// The Ask Agent queue, shared by the window and the control channel's connection threads.
/// Clones share one queue. A poisoned lock is taken over, never a panic.
#[derive(Clone)]
pub struct AgentHub {
    shared: Arc<(Mutex<Inner>, Condvar)>,
    repaint: Option<egui::Context>,
}

impl AgentHub {
    /// The queue of the control channel on `port`; `repaint` wakes the window when it changes.
    pub fn new(port: u16, repaint: Option<egui::Context>) -> Self {
        let inner = Inner { requests: VecDeque::new(), next_id: 1, listeners: 0, agents: Vec::new(), version: 0, port };
        Self { shared: Arc::new((Mutex::new(inner), Condvar::new())), repaint }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.shared.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Something changed: wake waiting listeners and the window.
    fn changed(&self, mut s: MutexGuard<'_, Inner>) {
        s.version = s.version.wrapping_add(1);
        drop(s);
        self.shared.1.notify_all();
        if let Some(ctx) = &self.repaint {
            ctx.request_repaint();
        }
    }

    pub fn port(&self) -> u16 {
        self.lock().port
    }

    /// The port actually bound (`--control 0` picks one).
    pub fn set_port(&self, port: u16) {
        self.lock().port = port;
    }

    /// Changes with every change to the requests or to who listens.
    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// Queue a request; its id. An instruction that is empty once trimmed is refused.
    pub fn ask(&self, instruction: &str, selection: Value, document: Value) -> Result<u64, String> {
        self.ask_with_action(instruction, selection, document, Value::Null, "auto")
    }

    pub fn ask_with_action(&self, instruction: &str, selection: Value, document: Value, action: Value, apply: &'static str) -> Result<u64, String> {
        let instruction = cap_chars(instruction.trim(), MAX_TEXT_CHARS);
        if instruction.is_empty() {
            return Err("type an instruction for the agent first".into());
        }
        let mut s = self.lock();
        let id = s.next_id;
        s.next_id = s.next_id.saturating_add(1);
        s.requests.push_back(Request {
            id,
            instruction,
            selection,
            document,
            action,
            apply,
            review: None,
            state: State::Waiting,
            reply: String::new(),
            taken_by: None,
            asked_ms: crate::now_ms(),
        });
        // Drop the oldest closed requests, then (only past twice the limit) the oldest.
        while s.requests.len() > KEEP {
            match s.requests.iter().position(|r| !r.state.open()) {
                Some(i) => drop(s.requests.remove(i)),
                None if s.requests.len() > 2 * KEEP => drop(s.requests.pop_front()),
                None => break,
            }
        }
        self.changed(s);
        Ok(id)
    }

    /// Cancel request `id` (waiting or being worked on); false when it was already closed or
    /// isn't there.
    pub fn cancel(&self, id: u64) -> bool {
        let mut s = self.lock();
        let Some(r) = s.requests.iter_mut().find(|r| r.id == id && r.state.open()) else { return false };
        r.state = State::Cancelled;
        r.taken_by = None;
        self.changed(s);
        true
    }

    /// Forget a closed request (the person put it away).
    pub fn dismiss(&self, id: u64) {
        let mut s = self.lock();
        s.requests.retain(|r| r.id != id || r.state.open());
        self.changed(s);
    }

    /// Wait up to `timeout` for the oldest waiting request and hand it to connection `conn` (it
    /// becomes `Working`). `alive` is asked before handing one out and every 250 ms while waiting:
    /// a client that has gone gets nothing and stops counting as an agent at once, and the request
    /// stays for the next one. Runs on a control-channel connection thread.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn listen(&self, conn: u64, timeout: std::time::Duration, alive: impl Fn() -> bool) -> Option<Request> {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + timeout.min(Duration::from_millis(MAX_LISTEN_MS));
        let mut s = self.lock();
        s.listeners = s.listeners.saturating_add(1);
        s.seen(conn);
        // The window shows that an agent listens.
        self.changed(s);
        let mut s = self.lock();
        let mut left_us = false;
        let taken = loop {
            if !alive() {
                left_us = true;
                break None;
            }
            if let Some(r) = s.requests.iter_mut().find(|r| r.state == State::Waiting) {
                r.state = State::Working;
                r.taken_by = Some(conn);
                break Some(r.clone());
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { break None };
            let wait = left.min(Duration::from_millis(POLL_MS));
            s = self.shared.1.wait_timeout(s, wait).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
        };
        s.listeners = s.listeners.saturating_sub(1);
        if left_us {
            s.gone(conn);
        } else {
            s.seen(conn);
        }
        self.changed(s);
        taken
    }

    /// Put a request an agent took back in the queue (the reply carrying it never reached the
    /// agent).
    pub fn requeue(&self, id: u64) {
        let mut s = self.lock();
        if let Some(r) = s.requests.iter_mut().find(|r| r.id == id && r.state == State::Working) {
            r.state = State::Waiting;
            r.taken_by = None;
        }
        self.changed(s);
    }

    /// Connection `conn` closed: it no longer counts as an agent, and a request it took and didn't
    /// answer goes back in the queue for the next `agent.listen`.
    pub fn disconnect(&self, conn: u64) {
        let mut s = self.lock();
        let had = s.agents.iter().any(|(c, _)| *c == conn) || s.requests.iter().any(|r| r.taken_by == Some(conn));
        if !had {
            return;
        }
        s.gone(conn);
        self.changed(s);
    }

    /// The agent's answer to request `id`, from connection `conn`: shown in the window. `done`
    /// closes the request; otherwise the text is a progress note and the request stays open.
    /// Only the connection that currently owns the request may answer. A request the person cancelled answers
    /// `{"cancelled": true}` so the agent can stop.
    pub fn reply(&self, conn: u64, id: u64, text: &str, done: bool) -> Result<Value, String> {
        let text = cap_chars(text.trim(), MAX_TEXT_CHARS);
        let mut s = self.lock();
        let r = match s.requests.iter_mut().find(|r| r.id == id) {
            None => Err(format!("no Ask Agent request {id}: it was never made, or the person dismissed it")),
            Some(r) if r.state == State::Cancelled => Ok(json!({"id": id, "cancelled": true})),
            Some(r) if r.state == State::Answered => Err(format!("Ask Agent request {id} is already answered")),
            Some(r) if r.taken_by != Some(conn) => Err("listen for this request before replying; another connection may own it".into()),
            Some(r) => {
                r.reply = text;
                r.state = if done { State::Answered } else { State::Working };
                r.taken_by = (!done).then_some(conn);
                Ok(json!({"id": id, "shown": true, "done": done}))
            }
        };
        let r = r?;
        s.seen(conn);
        self.changed(s);
        Ok(r)
    }

    /// The requests as the window shows them, oldest first.
    pub fn summaries(&self) -> Vec<Summary> {
        self.lock().requests.iter().map(Request::summary).collect()
    }

    /// Request `id`, whole.
    pub fn request(&self, id: u64) -> Option<Request> {
        self.lock().requests.iter().find(|r| r.id == id).cloned()
    }

    pub fn owns(&self, conn: u64, id: u64) -> bool {
        self.lock().requests.iter().any(|r| r.id == id && r.state == State::Working && r.taken_by == Some(conn))
    }

    pub fn working_for(&self, conn: u64) -> Option<u64> {
        self.lock().requests.iter().find(|r| r.state == State::Working && r.taken_by == Some(conn)).map(|r| r.id)
    }

    fn reviewed(&self, id: u64, review: &'static str) {
        let mut s = self.lock();
        if let Some(r) = s.requests.iter_mut().find(|r| r.id == id && !r.state.open()) {
            r.review = Some(review);
        }
        self.changed(s);
    }

    /// Whether an agent is there: one waits in `agent.listen`, an open connection listened or
    /// replied in the last [`PRESENCE_MS`], or an open connection holds a request it took and
    /// hasn't answered. A connection that closes stops counting at once.
    pub fn agent_present(&self) -> bool {
        let now = crate::now_ms();
        let s = self.lock();
        s.listeners > 0
            || s.agents.iter().any(|(_, seen)| now - seen < PRESENCE_MS)
            || s.requests.iter().any(|r| r.state == State::Working && r.taken_by.is_some())
    }

    /// Idle presence expires without pointer input. Waiting listeners and owned work repaint
    /// through changed(); only the recent-connection grace period needs a timed refresh.
    pub fn presence_refresh_after(&self) -> Option<std::time::Duration> {
        let s = self.lock();
        if s.listeners > 0 || s.requests.iter().any(|r| r.state == State::Working && r.taken_by.is_some()) {
            return None;
        }
        let newest = s.agents.iter().map(|(_, seen)| *seen).max_by(f64::total_cmp)?;
        let left = newest + PRESENCE_MS - crate::now_ms();
        (left > 0.0).then(|| std::time::Duration::from_millis(left.ceil().clamp(1.0, PRESENCE_MS) as u64))
    }
}

/// `text` cut to at most `max` characters, with "…" when it was longer.
fn cap_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", text.get(..i).unwrap_or(text)),
        None => text.to_string(),
    }
}

/// `text` cut to at most `max` bytes at a character boundary; whether it was cut.
fn cap_bytes(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// Why Ask Agent can't be used right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The web app has no control channel.
    Web,
    /// The app was started without `--control`.
    NoControlChannel,
    /// No agent listens on the control channel on this port.
    NoAgent(u16),
}

impl Unavailable {
    pub fn message(self) -> String {
        match self {
            Unavailable::Web => "Ask Agent isn't available in the web app: it needs the desktop app's control channel".into(),
            Unavailable::NoControlChannel => {
                "Ask Agent needs the control channel: start DeckCraft with `--control 7979`, then connect your agent with `deckcraft-cli mcp --connect 7979`"
                    .into()
            }
            Unavailable::NoAgent(port) => {
                format!("No agent is listening: run `deckcraft-cli mcp --connect {port}` in your agent and ask it to wait for DeckCraft requests")
            }
        }
    }
}

/// Where the selection is on screen this frame, set by the canvas: the chip floats beside it and
/// the bubble opens under it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    /// Selected text: the first and last selected line.
    Text { first: Rect, last: Rect },
    /// Selected shapes: their bounding box (rotation included).
    Shapes(Rect),
    /// A caret in text: no chip (nothing is selected), but a bubble opened from the shortcut or a
    /// menu goes under it.
    Caret(Rect),
}

impl Anchor {
    pub fn rect(&self) -> Rect {
        match self {
            Anchor::Text { first, last } => first.union(*last),
            Anchor::Shapes(r) | Anchor::Caret(r) => *r,
        }
    }
    /// Whether the chip shows for it.
    pub fn takes_chip(&self) -> bool {
        !matches!(self, Anchor::Caret(_))
    }
}

/// The window's side of Ask Agent.
#[derive(Default)]
pub struct AskAgent {
    /// The control channel's queue: `None` without a control channel (and on the web).
    pub hub: Option<AgentHub>,
    /// The bubble at the selection: the instruction box, then the request it sent.
    pub bubble: Option<Bubble>,
    /// The person's recent instructions, newest first. Kept for this session only: they can
    /// quote the presentation, so they aren't written to the preferences.
    pub history: Vec<String>,
    /// The selection on screen this frame (the canvas sets it; `None` when nothing that takes
    /// the chip is selected or the canvas isn't shown).
    pub anchor: Option<Anchor>,
    /// The requests as last read, and the hub's version then.
    seen: Option<(u64, Vec<Summary>)>,
    document_uid: Option<u64>,
    histories: HashMap<u64, Vec<String>>,
    edits: HashMap<u64, AgentEdit>,
    live_documents: Vec<u64>,
    edits_version: Option<u64>,
    /// egui clears focus at the start of an Escape frame, before widgets get that key.
    focused_box: bool,
}

/// A request owns exactly one history entry. Interleaved edits are refused rather than
/// absorbing the person's work into the agent's undo step.
struct AgentEdit {
    uid: u64,
    before: Arc<deckcraft_model::Presentation>,
    selection: deckcraft_engine::Selection,
    after: Arc<deckcraft_model::Presentation>,
    undo_len: usize,
}

pub fn sync_document(app: &mut SlideApp) {
    let uid = app.session.active().map(|d| d.uid);
    let open: Vec<u64> = app.session.documents().iter().map(|d| d.uid).collect();
    if let Some(hub) = &app.ask_agent.hub
        && (app.ask_agent.edits_version != Some(hub.version()) || open != app.ask_agent.live_documents)
    {
        let requests = hub.summaries();
        app.ask_agent.edits.retain(|id, e| open.contains(&e.uid) && requests.iter().any(|r| r.id == *id));
        app.ask_agent.edits_version = Some(hub.version());
    }
    if uid == app.ask_agent.document_uid && open == app.ask_agent.live_documents {
        return;
    }
    if uid != app.ask_agent.document_uid {
        if let Some(old) = app.ask_agent.document_uid {
            app.ask_agent.histories.insert(old, std::mem::take(&mut app.ask_agent.history));
        }
        app.ask_agent.history = uid.and_then(|id| app.ask_agent.histories.remove(&id)).unwrap_or_default();
        app.ask_agent.document_uid = uid;
        app.ask_agent.bubble = None;
    }
    app.ask_agent.live_documents.clone_from(&open);
    app.ask_agent.histories.retain(|id, _| open.contains(id));
    if let Some(hub) = &app.ask_agent.hub {
        for r in hub.summaries() {
            if r.document["id"].as_u64().is_some_and(|id| !open.contains(&id)) {
                hub.cancel(r.id);
            }
        }
    }
}

/// Execute for an open request, on its original document, grouping all mutations into one
/// undo step. UI/file/history commands cannot be attributed to a request.
pub fn run_for(app: &mut SlideApp, request: u64, id: &str, params: Value) -> Result<Value, String> {
    sync_document(app);
    let r = app.ask_agent.hub.as_ref().and_then(|h| h.request(request)).ok_or("unknown Ask Agent request")?;
    if !r.state.open() {
        return Err("Ask Agent request is closed or cancelled".into());
    }
    let spec = deckcraft_engine::cmd::find_command(id).ok_or("Ask Agent edits must use engine commands")?;
    if r.apply == "replyOnly" && spec.undoable {
        return Err("this Ask Agent request is reply-only; do not change the presentation".into());
    }
    if id.starts_with("file.") || matches!(id, "edit.undo" | "edit.redo" | "text.compose" | "text.commit") {
        return Err("file, history and composition commands cannot run for an Ask Agent request".into());
    }
    let st = app.session.active().ok_or("open the request's presentation first")?;
    if r.document["id"].as_u64() != Some(st.uid) {
        return Err("switch back to the presentation this request belongs to".into());
    }
    if st.interaction.is_some() {
        return Err("finish the pointer gesture before the agent edits".into());
    }
    app.session.end_composition();
    let st = app.session.active().ok_or("open the request's presentation first")?;
    if let Some(e) = app.ask_agent.edits.get(&request)
        && (e.uid != st.uid || !Arc::ptr_eq(&e.after, &st.doc) || st.history.undo.len() != e.undo_len + 1)
    {
        return Err("the presentation changed after the agent's edit; finish this request and ask again".into());
    }
    let (uid, before, selection, undo_len, limit) = (st.uid, st.doc.clone(), st.selection.clone(), st.history.undo.len(), st.history.limit);
    // Reserve space for the temporary engine entry. Coalescing below applies the real cap once,
    // so later commands for this request never evict human history again.
    if let Some(st) = app.session.active_mut() {
        st.history.limit = limit.max(1).saturating_add(1);
    }
    let result = app.run(id, params);
    if let Some(st) = app.session.active_mut() {
        st.history.limit = limit;
    }
    let result = result?;
    app.session.end_composition();
    let st = app.session.active_mut().ok_or("presentation closed")?;
    if !Arc::ptr_eq(&before, &st.doc) {
        if !spec.undoable {
            // No engine mutation may silently evade the request's history entry.
            st.doc = before;
            st.selection = selection;
            st.revision += 1;
            return Err("this command changes the document without undo support".into());
        }
        let e = app.ask_agent.edits.entry(request).or_insert_with(|| AgentEdit { uid, before, selection, after: st.doc.clone(), undo_len });
        st.history.undo.truncate(e.undo_len);
        st.history.undo.push(deckcraft_engine::HistoryEntry { label: "Ask Agent".into(), doc: e.before.clone(), selection: e.selection.clone() });
        st.history.redo.clear();
        let excess = st.history.undo.len().saturating_sub(limit.max(1));
        if excess > 0 {
            st.history.undo.drain(..excess);
            e.undo_len = e.undo_len.saturating_sub(excess);
        }
        e.after = st.doc.clone();
    }
    Ok(result)
}

/// The bubble at the selection.
pub struct Bubble {
    /// Where it opened, in slide points on that slide: it follows zoom and scrolling. `None`:
    /// nothing was selected on the slide.
    pub at: Option<(usize, Rect)>,
    /// What the person types.
    pub text: String,
    /// The request sent from it; `None` while the person types.
    pub request: Option<u64>,
    /// What the agent gets, in words, worked out when it opened.
    pub about: String,
    /// `at` was worked out (in the first frame after the canvas drew the selection).
    placed: bool,
    /// The box has taken the keyboard once.
    focused: bool,
    selection: Value,
    document: Value,
    scope: &'static str,
}

impl AskAgent {
    /// Why the person can't ask now, if they can't.
    pub fn unavailable(&self) -> Option<Unavailable> {
        match &self.hub {
            None if cfg!(target_arch = "wasm32") => Some(Unavailable::Web),
            None => Some(Unavailable::NoControlChannel),
            Some(h) if !h.agent_present() => Some(Unavailable::NoAgent(h.port())),
            Some(_) => None,
        }
    }

    /// The requests, read again from the hub only when they changed.
    pub fn requests(&mut self) -> &[Summary] {
        let Some(hub) = &self.hub else { return &[] };
        let v = hub.version();
        if self.seen.as_ref().is_none_or(|(seen, _)| *seen != v) {
            self.seen = Some((v, hub.summaries()));
        }
        self.seen.as_ref().map(|(_, r)| r.as_slice()).unwrap_or(&[])
    }

    /// For `ui.inspect`.
    pub fn inspect(&self) -> Value {
        let why = self.unavailable();
        let requests: Vec<Value> = self
            .hub
            .iter()
            .flat_map(AgentHub::summaries)
            .map(|r| json!({"id": r.id, "state": r.state.name(), "instruction": r.instruction, "reply": r.reply, "document": r.document, "action": r.action, "apply": r.apply, "review": r.review}))
            .collect();
        json!({
            "available": why.is_none(),
            "reason": why.map(Unavailable::message),
            "agentListening": self.hub.as_ref().is_some_and(AgentHub::agent_present),
            "bubble": self.bubble.as_ref().map(|b| json!({"request": b.request})),
            "history": self.history,
            "requests": requests,
            "actions": crate::agent_actions::ACTIONS.iter().map(|a| json!({"id": a.id, "label": a.label, "menu": a.menu, "scope": a.scope, "instruction": a.instruction, "apply": a.apply})).collect::<Vec<_>>(),
        })
    }
}

/// Recent instructions kept, and shown in the bubble.
const HISTORY: usize = 8;
const HISTORY_SHOWN: usize = 5;

fn remember(history: &mut Vec<String>, text: &str) {
    let text = cap_chars(text.trim(), 300);
    if text.is_empty() {
        return;
    }
    history.retain(|h| *h != text);
    history.insert(0, text);
    history.truncate(HISTORY);
}

fn slide_json(st: &DocState, i: usize) -> Value {
    match st.doc.slides.get(i) {
        Some(s) => json!({"index": i, "id": s.id, "title": s.title()}),
        None => Value::Null,
    }
}

/// The selection as an agent gets it, from what is selected now:
///
/// - text being edited: `{kind: "text", slide, shapes: [the shape], text: {shape, anchor, caret,
///   cell?, notes, text, truncated}}` (positions are `[paragraph, character]`, as `text.select`
///   takes them; `notes` is the slide's speaker notes, with no shape);
/// - shapes: `{kind: "shapes", slide, shapes: [summaries as slide.inspect gives them], truncated}`;
/// - nothing: `{kind: "slide", slide, slides?: [the selected slides when there are several]}`.
///
/// `slide` is `{index, id, title}`; `target` is added in Slide Master view (shape ids then refer
/// to the master or layout).
pub fn selection(st: &DocState) -> Value {
    let sel = &st.selection;
    let mut v = json!({"slide": slide_json(st, sel.slide)});
    let summaries = |ids: &[deckcraft_model::ShapeId]| -> (Vec<Value>, bool) {
        let ctx = match sel.target {
            Target::Slides => st.current_slide().and_then(|sl| deckcraft_model::resolve::Ctx::for_slide(&st.doc, sl)),
            _ => None,
        };
        let shapes: Vec<&deckcraft_model::Shape> = ids.iter().filter_map(|id| st.shape(*id)).collect();
        let mut out: Vec<Value> =
            shapes.iter().take(MAX_SHAPES).map(|sh| deckcraft_engine::cmd::inspect::shape_summary(ctx.as_ref(), sh, 0)).collect();
        let mut truncated = shapes.len() > MAX_SHAPES;
        if out.iter().map(|s| s.to_string().len()).sum::<usize>() > MAX_SELECTION_BYTES {
            out = out.iter().map(|s| json!({"id": s["id"], "name": s["name"], "kind": s["kind"], "box": s["box"]})).collect();
            truncated = true;
        }
        (out, truncated)
    };
    if let Some(ts) = &sel.text {
        let (a, b) = ts.ordered();
        let mut text = deckcraft_engine::cmd::text::body_of(st, ts).map(|body| deckcraft_model::edit::text_range(body, a, b)).unwrap_or_default();
        let truncated = cap_bytes(&mut text, MAX_SELECTION_BYTES);
        v["kind"] = json!("text");
        v["text"] = json!({
            "shape": (!ts.notes).then_some(ts.shape),
            "anchor": [ts.anchor.0, ts.anchor.1],
            "caret": [ts.caret.0, ts.caret.1],
            "cell": ts.cell.map(|(r, c)| [r, c]),
            "notes": ts.notes,
            "text": text,
            "truncated": truncated,
        });
        if !ts.notes {
            v["shapes"] = json!(summaries(&[ts.shape]).0);
        }
    } else if !sel.shapes.is_empty() {
        let (shapes, truncated) = summaries(&sel.shapes);
        v["kind"] = json!("shapes");
        v["shapes"] = json!(shapes);
        v["truncated"] = json!(truncated);
    } else {
        v["kind"] = json!("slide");
        if sel.slides.len() > 1 {
            let slides: Vec<Value> = sel.slides.iter().filter_map(|id| st.doc.slide_index(*id)).take(MAX_SHAPES).map(|i| slide_json(st, i)).collect();
            v["slides"] = json!(slides);
        }
    }
    if sel.target != Target::Slides {
        v["target"] = serde_json::to_value(sel.target).unwrap_or_default();
    }
    v
}

/// What the agent gets, in words, for the bubble.
fn about(st: &DocState) -> String {
    let sel = &st.selection;
    if let Some(ts) = &sel.text {
        let chars = deckcraft_engine::cmd::text::body_of(st, ts)
            .map(|body| {
                let (a, b) = ts.ordered();
                deckcraft_model::edit::text_range(body, a, b).chars().count()
            })
            .unwrap_or(0);
        let place = if ts.notes { "the speaker notes" } else { "this shape" };
        return if chars == 0 {
            format!("No text is selected: the agent gets the insertion point in {place}.")
        } else {
            format!("The agent gets the selected text ({chars} characters) in {place}.")
        };
    }
    match sel.shapes.len() {
        0 if sel.slides.len() > 1 => format!("The agent gets the {} selected slides.", sel.slides.len()),
        0 => format!("Nothing is selected: the agent gets slide {}.", sel.slide + 1),
        1 => "The agent gets the selected shape.".into(),
        n => format!("The agent gets the {n} selected shapes."),
    }
}

/// The language of the selected text, else of the slide's first text that names one.
fn language(st: &DocState) -> Option<String> {
    let lang_of = |body: &deckcraft_model::TextBody| body.paragraphs.iter().flat_map(|p| p.runs.iter()).find_map(|r| r.props.lang.clone());
    if let Some(ts) = &st.selection.text
        && let Some(l) = deckcraft_engine::cmd::text::body_of(st, ts).and_then(lang_of)
    {
        return Some(l);
    }
    let selected = st.selected_shapes();
    let shapes: Vec<&deckcraft_model::Shape> = if selected.is_empty() { st.shapes().iter().collect() } else { selected };
    shapes.iter().filter_map(|s| s.text.as_ref()).find_map(lang_of)
}

/// The presentation as an agent gets it: its file name (else the window's name for a new one,
/// `Presentation1`), whether it has a file, its slide count and the selection's language tag.
pub fn document(st: &DocState) -> Value {
    let title =
        st.path.as_deref().and_then(|p| std::path::Path::new(p).file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| st.title());
    json!({"id": st.uid, "title": title, "saved": st.path.is_some(), "slides": st.doc.slides.len(), "language": language(st)})
}

fn action_enabled(action: &crate::agent_actions::Action, selection: &Value) -> bool {
    if action.id == "animation.buildBullets"
        && selection["kind"] == "text"
        && (selection["text"]["notes"] == true || !selection["text"]["cell"].is_null())
    {
        return false;
    }
    match action.scope {
        "deck" => true,
        "slide" => !selection["slide"].is_null(),
        "shape" => selection["shapes"].as_array().is_some_and(|s| s.len() == 1),
        "text" => {
            selection["kind"] == "text"
                || selection["shapes"].as_array().is_some_and(|s| s.iter().any(|sh| sh.get("text").is_some_and(|t| !t.is_null())))
        }
        _ => false,
    }
}

fn queue_action(app: &mut SlideApp, id: &str, origin: Option<(Value, Value)>) -> Result<u64, String> {
    let action = crate::agent_actions::find(id).ok_or_else(|| format!("unknown Ask Agent action `{id}`"))?;
    let st = app.session.active().ok_or("open a presentation first")?;
    let (selected, doc) = origin.unwrap_or_else(|| (selection(st), document(st)));
    if doc["id"].as_u64() != Some(st.uid) {
        return Err("switch back to the presentation this action belongs to".into());
    }
    if !action_enabled(action, &selected) {
        return Err(format!("select {} content before choosing {}", action.scope, action.label));
    }
    let selected = match action.scope {
        "deck" => {
            json!({"kind": "deck", "slides": st.doc.slides.iter().enumerate().take(MAX_SHAPES).map(|(i, _)| slide_json(st, i)).collect::<Vec<_>>(), "truncated": st.doc.slides.len() > MAX_SHAPES})
        }
        "slide" => json!({"kind": "slide", "slide": selected["slide"]}),
        _ => selected,
    };
    let hub = app.ask_agent.hub.as_ref().ok_or("Ask Agent needs the desktop control channel")?;
    let id =
        hub.ask_with_action(action.instruction, selected, doc, json!({"id": action.id, "label": action.label, "scope": action.scope}), action.apply)?;
    remember(&mut app.ask_agent.history, action.label);
    Ok(id)
}

fn review(app: &mut SlideApp, id: u64, undo: bool) -> Result<Value, String> {
    let hub = app.ask_agent.hub.clone().ok_or("Ask Agent needs the desktop control channel")?;
    let r = hub.request(id).ok_or("unknown Ask Agent request")?;
    if r.state.open() {
        return Err("wait for the request to finish, or cancel it before reviewing".into());
    }
    if undo {
        let e = app.ask_agent.edits.get(&id).ok_or("this request has no edits left to undo")?;
        let st = app.session.active().ok_or("open the request's presentation first")?;
        if st.uid != e.uid
            || !Arc::ptr_eq(&st.doc, &e.after)
            || st.history.undo.len() != e.undo_len + 1
            || st.history.undo.last().is_none_or(|entry| entry.label != "Ask Agent" || !Arc::ptr_eq(&entry.doc, &e.before))
            || st.composition.is_some()
            || st.interaction.is_some()
        {
            return Err("the presentation changed after this request; Undo would affect unrelated work".into());
        }
        app.run("edit.undo", json!({}))?;
    }
    app.ask_agent.edits.remove(&id);
    hub.reviewed(id, if undo { "undone" } else { "kept" });
    Ok(json!({"id": id, "review": if undo { "undone" } else { "kept" }}))
}

/// `app.askAgent`. No params: open the bubble at the selection (refused, with the reason, when
/// no agent is listening). `instruction`: send it without the bubble. `cancel`: cancel that
/// request. `close`: close the bubble.
pub fn command(app: &mut SlideApp, p: &Value) -> Result<Value, String> {
    sync_document(app);
    for (key, undo) in [("keep", false), ("undo", true)] {
        if let Some(id) = p.get(key) {
            return review(app, id.as_u64().ok_or("review takes a request id")?, undo);
        }
    }
    if let Some(id) = p.get("cancel") {
        let id = id.as_u64().ok_or("`cancel` is a request id")?;
        let cancelled = app.ask_agent.hub.as_ref().is_some_and(|h| h.cancel(id));
        return Ok(json!({"cancelled": cancelled}));
    }
    if p.get("close").and_then(Value::as_bool) == Some(true) {
        close_bubble(app);
        return Ok(json!({}));
    }
    let Some(hub) = app.ask_agent.hub.clone() else {
        return Err(app.ask_agent.unavailable().unwrap_or(Unavailable::NoControlChannel).message());
    };
    let Some(st) = app.session.active() else { return Err("open a presentation first".into()) };
    if let Some(action) = p.get("action") {
        return queue_action(app, action.as_str().ok_or("action must be an action id")?, None).map(|id| json!({"id": id}));
    }
    if let Some(text) = p.get("instruction") {
        let text = text.as_str().ok_or("`instruction` is text")?;
        let selected = match p.get("scope").and_then(Value::as_str) {
            Some("deck") => {
                json!({"kind": "deck", "slides": st.doc.slides.iter().enumerate().take(MAX_SHAPES).map(|(i, _)| slide_json(st, i)).collect::<Vec<_>>(), "truncated": st.doc.slides.len() > MAX_SHAPES})
            }
            Some("slide") => json!({"kind": "slide", "slide": slide_json(st, st.selection.slide)}),
            None => selection(st),
            Some(_) => return Err("scope must be slide or deck; omit it for the current selection".into()),
        };
        let id = hub.ask(text, selected, document(st))?;
        remember(&mut app.ask_agent.history, text);
        return Ok(json!({"id": id}));
    }
    if let Some(why) = app.ask_agent.unavailable() {
        return Err(why.message());
    }
    let about = about(st);
    app.ask_agent.bubble = Some(Bubble {
        at: None,
        text: String::new(),
        request: None,
        about,
        placed: false,
        focused: false,
        selection: selection(st),
        document: document(st),
        scope: "selection",
    });
    Ok(json!({"pending": "askAgent"}))
}

/// The selection's anchor in slide points, from where the canvas drew it.
fn anchor_on_slide(app: &SlideApp) -> Option<Rect> {
    let (r, slide) = (app.ask_agent.anchor?.rect(), app.slide_rect?);
    let k = app.canvas_scale;
    if !(k.is_finite() && k > 0.0) {
        return None;
    }
    Some(Rect::from_min_max(((r.min - slide.min) / k).to_pos2(), ((r.max - slide.min) / k).to_pos2()))
}

/// Close only the bubble. Requests and results remain in the corner card until dismissed.
fn close_bubble(app: &mut SlideApp) {
    app.ask_agent.bubble = None;
}

/// The person typed on the slide: the bubble gives way (a request it sent goes on in the corner
/// card). Text, Delete, Backspace, Enter, arrows and Tab count; shortcuts and Escape don't.
pub fn typed_on_slide(app: &mut SlideApp, events: &[egui::Event]) {
    if app.ask_agent.bubble.is_none() {
        return;
    }
    let typed = events.iter().any(|e| match e {
        egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Ime(egui::ImeEvent::Commit(_) | egui::ImeEvent::Preedit { .. }) => true,
        egui::Event::Key { key, pressed: true, modifiers, .. } => *key != Key::Escape && !(modifiers.command || modifiers.ctrl || modifiers.alt),
        _ => false,
    });
    if typed {
        close_bubble(app);
    }
}

/// Handle the bubble's Escape before other widgets can change keyboard focus this frame.
pub fn keyboard(app: &mut SlideApp, ctx: &egui::Context) {
    let edit = egui::Id::new("dc_ask_agent_text");
    if app.ask_agent.bubble.is_some()
        && app.dialog.is_none()
        && app.palette.is_none()
        && !egui::Popup::is_any_open(ctx)
        && app.ask_agent.focused_box
        && ctx.memory(|m| m.focused().is_none() || m.has_focus(edit))
        && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    {
        close_bubble(app);
        ctx.memory_mut(|m| m.surrender_focus(edit));
        app.ask_agent.focused_box = false;
    }
}

pub fn remember_focus(app: &mut SlideApp, ctx: &egui::Context) {
    app.ask_agent.focused_box = app.dialog.is_none()
        && app.palette.is_none()
        && !egui::Popup::is_any_open(ctx)
        && ctx.memory(|m| m.has_focus(egui::Id::new("dc_ask_agent_text")));
}

/// Send what is in the box; the bubble then follows the request.
fn send(app: &mut SlideApp, ctx: &egui::Context) {
    sync_document(app);
    let (Some(b), Some(hub)) = (&mut app.ask_agent.bubble, &app.ask_agent.hub) else { return };
    let selected = match b.scope {
        "deck" => app.session.active().map(|st| json!({"kind": "deck", "slides": st.doc.slides.iter().enumerate().take(MAX_SHAPES).map(|(i, _)| slide_json(st, i)).collect::<Vec<_>>(), "truncated": st.doc.slides.len() > MAX_SHAPES})).unwrap_or_default(),
        "slide" => json!({"kind": "slide", "slide": b.selection["slide"]}),
        _ => b.selection.clone(),
    };
    match hub.ask(&b.text, selected, b.document.clone()) {
        Ok(id) => {
            remember(&mut app.ask_agent.history, &b.text);
            b.request = Some(id);
            b.text.clear();
            ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("dc_ask_agent_text")));
        }
        Err(e) => app.set_status(e),
    }
}

fn send_action(app: &mut SlideApp, ctx: &egui::Context, action: &'static str) {
    let origin = app.ask_agent.bubble.as_ref().map(|b| (b.selection.clone(), b.document.clone()));
    match queue_action(app, action, origin) {
        Ok(id) => {
            if let Some(b) = app.ask_agent.bubble.as_mut() {
                b.request = Some(id);
                b.text.clear();
            }
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
        Err(e) => app.set_status(e),
    }
}

fn quick_actions(ui: &mut egui::Ui, selection: &Value) -> Option<&'static str> {
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        for menu in ["Rewrite", "Text", "Translate", "Design", "Images", "Animations", "Deck"] {
            ui.menu_button(menu, |ui| {
                for a in crate::agent_actions::ACTIONS.iter().filter(|a| a.menu == menu) {
                    if ui.add_enabled(action_enabled(a, selection), egui::Button::new(a.label)).clicked() {
                        clicked = Some(a.id);
                        ui.close();
                    }
                }
            });
        }
    });
    clicked
}

fn instruction_label(r: &Summary) -> &str {
    r.action.get("label").and_then(Value::as_str).unwrap_or(&r.instruction)
}

/// Where the bubble goes: below `anchor`, inside the canvas; above it when there is no room
/// below; the canvas's top-left corner without an anchor on screen. Pure geometry; `None` for a
/// degenerate canvas.
pub fn bubble_pos(anchor: Option<Rect>, size: Vec2, viewport: Rect) -> Option<Pos2> {
    if !viewport.is_finite() || !viewport.is_positive() {
        return None;
    }
    let corner = Rect::from_min_size(viewport.min + vec2(24.0, 24.0), Vec2::ZERO);
    let at = anchor.filter(|r| r.is_finite() && viewport.intersects(*r)).unwrap_or(corner);
    let x = at.left().clamp(viewport.left(), (viewport.right() - size.x).max(viewport.left()));
    let below = at.bottom() + GAP + HANDLES;
    let y = if below + size.y <= viewport.bottom() { below } else { (at.top() - size.y - GAP - HANDLES).max(viewport.top()) };
    Some(pos2(x, y))
}

/// Where the chip goes: for text, above the first selected line, or under the last one when
/// there is no room above (sliding along the line to stay in the canvas); for shapes, beside their
/// box at its top (right, else left), else under it, else above it, else just inside its top-right
/// corner. Always inside the canvas. Pure geometry; `None` for degenerate input.
pub fn chip_pos(anchor: Anchor, size: Vec2, viewport: Rect) -> Option<Pos2> {
    if !viewport.is_finite() || !viewport.is_positive() || !anchor.rect().is_finite() {
        return None;
    }
    let clamp = |p: Pos2| {
        pos2(
            p.x.clamp(viewport.left(), (viewport.right() - size.x).max(viewport.left())),
            p.y.clamp(viewport.top(), (viewport.bottom() - size.y).max(viewport.top())),
        )
    };
    let p = match anchor {
        Anchor::Text { first, last } => {
            let above = pos2(first.left(), first.top() - GAP - size.y);
            let below = pos2(last.left(), last.bottom() + GAP);
            if above.y >= viewport.top() { above } else { below }
        }
        Anchor::Shapes(r) | Anchor::Caret(r) => {
            let fits = |p: &Pos2| viewport.contains_rect(Rect::from_min_size(*p, size));
            [
                pos2(r.right() + GAP + HANDLES, r.top()),
                pos2(r.left() - GAP - HANDLES - size.x, r.top()),
                pos2(r.left(), r.bottom() + GAP + HANDLES),
                pos2(r.left(), r.top() - GAP - HANDLES - size.y),
            ]
            .into_iter()
            .find(fits)
            .unwrap_or(pos2(r.right() - size.x - GAP, r.top() + GAP))
        }
    };
    Some(clamp(p))
}

/// Where the chip was drawn last frame, kept in the context's memory for tests.
pub fn chip_id() -> egui::Id {
    egui::Id::new("dc_ask_agent_chip").with("shown")
}

/// Whether the chip shows: on selected shapes or selected text in the canvas, while an agent
/// listens (nothing floats otherwise), the bubble is closed, and nothing else is going on: no
/// drag or drawing, no dialog, palette, menu or start screen.
pub fn chip_visible(app: &SlideApp, ctx: &egui::Context) -> bool {
    app.ask_agent.anchor.is_some_and(|a| a.takes_chip())
        && app.ask_agent.bubble.is_none()
        && app.dialog.is_none()
        && app.palette.is_none()
        && app.show.is_none()
        && !app.home_open()
        && !app.session.tool.dragging()
        && app.session.tool.marquee.is_none()
        && !app.session.tool.drawing_freeform()
        && !egui::Popup::is_any_open(ctx)
        && app.ask_agent.unavailable().is_none()
}

/// The chip on the selection, the bubble, and the card with the other requests in the canvas's
/// bottom-right corner. Drawn after the canvas, which sets the anchor.
pub fn show(app: &mut SlideApp, ctx: &egui::Context) {
    sync_document(app);
    if let Some(delay) = app.ask_agent.hub.as_ref().and_then(AgentHub::presence_refresh_after) {
        ctx.request_repaint_after(delay);
    }
    let Some(viewport) = app.canvas_rect.filter(|v| v.is_finite() && v.is_positive()) else { return };
    // The bubble belongs to the canvas: gone with it (another view, the start screen, a show).
    if app.show.is_some() || app.home_open() || app.session.active().is_none() {
        return;
    }
    show_chip(app, ctx, viewport);
    let shown = show_bubble(app, ctx, viewport);
    show_requests(app, ctx, viewport, shown);
}

fn show_chip(app: &mut SlideApp, ctx: &egui::Context, viewport: Rect) {
    let anchor = match app.ask_agent.anchor {
        Some(a) if a.takes_chip() && chip_visible(app, ctx) => a,
        _ => {
            ctx.data_mut(|d| d.remove::<Rect>(chip_id()));
            return;
        }
    };
    let t = Tokens::get(ctx);
    let label = "Ask Agent";
    let font = theme::font(12.0);
    let text_w = ctx.fonts_mut(|f| f.layout_no_wrap(label.to_string(), font.clone(), t.text).size().x);
    let size = vec2(text_w + 34.0, CHIP_H);
    let Some(pos) = chip_pos(anchor, size, viewport) else { return };
    let mut clicked = false;
    // chip_pos already constrains the actual chip size. Area's default first-frame size would
    // otherwise move the chip toward the middle of the slide before it knows that size.
    egui::Area::new(egui::Id::new("dc_ask_agent_chip")).order(Order::Middle).constrain(false).fixed_pos(pos).show(ctx, |ui| {
        let (r, resp) = ui.allocate_exact_size(size, egui::Sense::click());
        let p = ui.painter();
        p.rect_filled(r.translate(vec2(0.0, 1.5)), egui::CornerRadius::same(6), t.shadow);
        p.rect_filled(r, egui::CornerRadius::same(6), if resp.hovered() { t.hover } else { t.card });
        p.rect_stroke(r, egui::CornerRadius::same(6), egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
        let icon = Rect::from_center_size(pos2(r.min.x + 14.0, r.center().y), vec2(16.0, 16.0));
        icons::paint(p, icon, Icon::Sparkle, t.text, false);
        p.text(pos2(r.min.x + 26.0, r.center().y), Align2::LEFT_CENTER, label, font.clone(), t.text);
        let resp = resp.on_hover_text(format!("{label} ({})", crate::ribbon::pretty_shortcut(SHORTCUT)));
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
        clicked = resp.clicked();
        ctx.data_mut(|d| d.insert_temp(chip_id(), r));
    });
    if clicked {
        let _ = app.run(COMMAND, json!({}));
    }
}

/// The bubble; the request it shows, if any.
fn show_bubble(app: &mut SlideApp, ctx: &egui::Context, viewport: Rect) -> Option<u64> {
    // Placed once, under the selection the canvas drew this frame (the bubble may have been
    // opened from the ribbon or a shortcut, before the canvas was drawn).
    if app.ask_agent.bubble.as_ref().is_some_and(|b| !b.placed) {
        let slide = app.session.active().filter(|d| d.selection.target == Target::Slides).map(|d| d.selection.slide);
        let at = slide.zip(anchor_on_slide(app));
        if let Some(b) = app.ask_agent.bubble.as_mut() {
            b.at = at;
            b.placed = true;
        }
    }
    let b = app.ask_agent.bubble.as_ref()?;
    let (at, request, about) = (b.at, b.request, b.about.clone());
    let current = request.and_then(|id| app.ask_agent.requests().iter().find(|r| r.id == id).cloned());
    // Its request was dismissed (from the corner card, or by a script): nothing left to show.
    if request.is_some() && current.is_none() {
        app.ask_agent.bubble = None;
        return None;
    }
    let on_screen = match (at, app.slide_rect, app.session.active()) {
        (Some((slide, r)), Some(sr), Some(d)) if d.selection.target == Target::Slides && d.selection.slide == slide => {
            let k = app.canvas_scale;
            Some(Rect::from_min_max(sr.min + r.min.to_vec2() * k, sr.min + r.max.to_vec2() * k))
        }
        (None, Some(sr), _) => Some(Rect::from_min_size(sr.min + vec2(24.0, 24.0), Vec2::ZERO)),
        _ => None,
    };
    let width = if current.as_ref().is_some_and(|r| r.reply.len() > 160) { WIDE } else { WIDTH };
    let pos = bubble_pos(on_screen, vec2(width, 170.0), viewport)?;
    let present = app.ask_agent.hub.as_ref().is_some_and(AgentHub::agent_present);
    let edit_id = egui::Id::new("dc_ask_agent_text");
    // Read before the box handles its keys: Escape closes the bubble only while it has the focus.
    let typing = ctx.memory(|m| m.has_focus(edit_id));
    let escape =
        typing && app.dialog.is_none() && app.palette.is_none() && !egui::Popup::is_any_open(ctx) && ctx.input(|i| i.key_pressed(Key::Escape));
    let mut act = BubbleAct::None;
    let t = Tokens::get(ctx);
    let history = app.ask_agent.history.clone();
    let shown = egui::Area::new(egui::Id::new("dc_ask_agent"))
        .order(Order::Foreground)
        .default_size(vec2(width, 300.0))
        .constrain_to(viewport)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            card(&t).show(ui, |ui| {
                ui.set_width(width);
                if header(ui, &t) {
                    act = BubbleAct::Close;
                }
                match &current {
                    None => {
                        let mut scope_changed = false;
                        if let Some(b) = app.ask_agent.bubble.as_mut() {
                            egui::ComboBox::from_id_salt("dc_agent_scope")
                                .selected_text(match b.scope {
                                    "deck" => "Entire presentation",
                                    "slide" => "This slide",
                                    _ => "Current selection",
                                })
                                .show_ui(ui, |ui| {
                                    scope_changed |= ui.selectable_value(&mut b.scope, "selection", "Current selection").changed();
                                    scope_changed |= ui.selectable_value(&mut b.scope, "slide", "This slide").changed();
                                    scope_changed |= ui.selectable_value(&mut b.scope, "deck", "Entire presentation").changed();
                                });
                            let description = match b.scope {
                                "deck" => "The agent gets the entire presentation.",
                                "slide" => "The agent gets this slide.",
                                _ => &about,
                            };
                            ui.label(egui::RichText::new(description).small().color(t.text_dim));
                        }
                        // Enter sends, Shift+Enter starts a new line, Up in an empty box brings back
                        // the last instruction.
                        let enter = typing && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                        let Some(b) = app.ask_agent.bubble.as_mut() else { return };
                        let up = typing && b.text.is_empty() && !history.is_empty() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp));
                        if let (true, Some(last)) = (up, history.first()) {
                            b.text = last.clone();
                        }
                        let edit = ui.add(
                            egui::TextEdit::multiline(&mut b.text)
                                .id(edit_id)
                                .hint_text("Ask agent to…")
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                        if !b.focused || scope_changed {
                            edit.request_focus();
                            b.focused = true;
                        }
                        if up {
                            // The caret at the end of the recalled text.
                            let end = b.text.chars().count();
                            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit_id) {
                                state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(end))));
                                state.store(ui.ctx(), edit_id);
                            }
                        }
                        ui.label(egui::RichText::new("Enter to send, Shift+Enter for a new line").small().color(t.text_faint));
                        let has_text = !b.text.trim().is_empty();
                        if !has_text && !history.is_empty() {
                            ui.label(egui::RichText::new("Recent").small().color(t.text_dim));
                            for h in history.iter().take(HISTORY_SHOWN) {
                                // A recent instruction fills the box, to send as it is or change first.
                                if ui.add(egui::Label::new(h.as_str()).truncate().sense(egui::Sense::click())).on_hover_text(h.as_str()).clicked() {
                                    b.text = h.clone();
                                    edit.request_focus();
                                }
                            }
                        }
                        ui.horizontal(|ui| {
                            if ui.add_enabled(has_text, egui::Button::new("Send")).clicked() || (enter && has_text) {
                                act = BubbleAct::Send;
                            }
                            if ui.button("Cancel").clicked() {
                                act = BubbleAct::Close;
                            }
                        });
                        if let Some(action) = quick_actions(ui, &b.selection) {
                            act = BubbleAct::Action(action);
                        }
                    }
                    Some(r) => {
                        // The instruction, in grey, then where it stands.
                        ui.add(egui::Label::new(egui::RichText::new(instruction_label(r)).small().color(t.text_dim)).truncate());
                        status(ui, r, present, &t);
                        ui.horizontal(|ui| {
                            if r.state.open() {
                                if ui.button("Cancel").clicked() {
                                    act = BubbleAct::Cancel(r.id);
                                }
                            } else {
                                if app.ask_agent.edits.contains_key(&r.id) {
                                    if ui.button("Keep").clicked() {
                                        act = BubbleAct::Keep(r.id);
                                    }
                                    if ui.button("Undo").clicked() {
                                        act = BubbleAct::Undo(r.id);
                                    }
                                }
                                if ui.button("Close").clicked() {
                                    act = BubbleAct::Close;
                                }
                            }
                        });
                    }
                }
            });
        });
    // Escape in the box, or a press in the canvas outside the bubble, closes it.
    let rect = shown.response.rect;
    let press = ctx.input(|i| if i.pointer.any_pressed() { i.pointer.press_origin() } else { None });
    let elsewhere =
        press.is_some_and(|p| viewport.contains(p) && !rect.contains(p) && !ctx.layer_id_at(p).is_some_and(|l| l.order >= Order::Foreground));
    if matches!(act, BubbleAct::None) && (escape || elsewhere) {
        act = BubbleAct::Close;
    }
    match act {
        BubbleAct::None => {}
        BubbleAct::Send => send(app, ctx),
        BubbleAct::Action(action) => send_action(app, ctx, action),
        BubbleAct::Keep(id) => {
            let _ = app.run(COMMAND, json!({"keep": id}));
        }
        BubbleAct::Undo(id) => {
            let _ = app.run(COMMAND, json!({"undo": id}));
        }
        BubbleAct::Close => close_bubble(app),
        BubbleAct::Cancel(id) => {
            if let Some(h) = &app.ask_agent.hub {
                h.cancel(id);
            }
        }
    }
    request
}

enum BubbleAct {
    None,
    Send,
    Close,
    Cancel(u64),
    Action(&'static str),
    Keep(u64),
    Undo(u64),
}

/// A request's state, live: "Sent to your agent" the moment it is sent, "Your agent is working"
/// as soon as the agent has it, with a spinner and the time since it was sent; the agent's latest
/// progress note; then its answer, whole (it wraps, and scrolls when long), or Cancelled.
fn status(ui: &mut egui::Ui, r: &Summary, present: bool, t: &Tokens) {
    if let Some(review) = r.review {
        ui.label(match review {
            "undone" => "Changes undone",
            _ => "Changes kept",
        });
    }
    let secs = ((crate::now_ms() - r.asked_ms) / 1000.0).max(0.0) as u64;
    let busy = |ui: &mut egui::Ui, text: &str| {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0).color(t.accent));
            ui.label(text);
            ui.label(egui::RichText::new(format!("{secs} s")).small().color(t.text_dim));
        });
    };
    match r.state {
        State::Waiting if present => busy(ui, "Sent to your agent…"),
        State::Waiting => {
            busy(ui, "Waiting for an agent…");
            ui.label(egui::RichText::new("No agent is listening right now.").small().color(t.text_dim));
        }
        State::Working => {
            busy(ui, "Your agent is working…");
            if !r.reply.is_empty() {
                ui.add(egui::Label::new(r.reply.as_str()).wrap());
            }
        }
        // The agent's own words: shown whole, as it wrote them, selectable to copy.
        State::Answered if r.reply.is_empty() => {
            ui.label("Done.");
        }
        State::Answered => {
            egui::ScrollArea::vertical().id_salt(("dc_ask_agent_reply", r.id)).max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.add(egui::Label::new(r.reply.as_str()).selectable(true).wrap());
            });
        }
        State::Cancelled => {
            ui.label(egui::RichText::new("Cancelled").color(t.text_dim));
        }
    }
}

/// The bubble's and the card's look: a popover card (card fill and border, 6 pt corners, a soft
/// shadow), with room inside.
fn card(t: &Tokens) -> egui::Frame {
    egui::Frame::NONE
        .fill(t.card)
        .stroke(egui::Stroke::new(1.0, t.card_border))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(10))
        .shadow(egui::epaint::Shadow { offset: [0, 2], blur: 8, spread: 0, color: t.shadow })
}

/// The title row: the Ask Agent icon and name, and a close button. True when it was clicked.
fn header(ui: &mut egui::Ui, t: &Tokens) -> bool {
    ui.horizontal(|ui| {
        let (icon, _) = ui.allocate_exact_size(vec2(16.0, 16.0), egui::Sense::hover());
        icons::paint(ui.painter(), icon, Icon::Sparkle, t.text, false);
        ui.label(egui::RichText::new("Ask Agent").font(theme::bold(12.5)).color(t.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| close_button(ui, t)).inner
    })
    .inner
}

/// A small × like the toast's.
fn close_button(ui: &mut egui::Ui, t: &Tokens) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, egui::CornerRadius::same(4), t.hover);
    }
    icons::paint(ui.painter(), r.shrink(3.0), Icon::Close, t.text_dim, false);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Close Ask Agent"));
    resp.on_hover_text("Close").clicked()
}

/// The requests the bubble doesn't show, newest last, in the canvas's bottom-right corner.
fn show_requests(app: &mut SlideApp, ctx: &egui::Context, viewport: Rect, in_bubble: Option<u64>) {
    let Some(hub) = app.ask_agent.hub.clone() else { return };
    let uid = app.session.active().map(|d| d.uid);
    let requests: Vec<Summary> =
        app.ask_agent.requests().iter().filter(|r| Some(r.id) != in_bubble && r.document["id"].as_u64() == uid).cloned().collect();
    if requests.is_empty() {
        return;
    }
    let present = hub.agent_present();
    let t = Tokens::get(ctx);
    let mut review_action = None;
    egui::Area::new(egui::Id::new("dc_ask_agent_requests"))
        .order(Order::Foreground)
        .pivot(Align2::RIGHT_BOTTOM)
        .fixed_pos(viewport.right_bottom() - vec2(16.0, 16.0))
        .show(ctx, |ui| {
            card(&t).show(ui, |ui| {
                ui.set_width(WIDTH);
                let close = header(ui, &t);
                for r in requests.iter().rev().take(SHOWN).rev() {
                    ui.separator();
                    ui.add(egui::Label::new(egui::RichText::new(instruction_label(r)).small().color(t.text_dim)).truncate());
                    status(ui, r, present, &t);
                    if r.state.open() {
                        if ui.small_button("Cancel").clicked() {
                            hub.cancel(r.id);
                        }
                    } else {
                        ui.horizontal(|ui| {
                            if app.ask_agent.edits.contains_key(&r.id) {
                                if ui.small_button("Keep").clicked() {
                                    review_action = Some((r.id, false));
                                }
                                if ui.small_button("Undo").clicked() {
                                    review_action = Some((r.id, true));
                                }
                            }
                            if ui.small_button("Dismiss").clicked() {
                                hub.dismiss(r.id);
                            }
                        });
                    }
                }
                // Closing the card puts away the answered and cancelled requests; open ones stay.
                if close {
                    for r in &requests {
                        hub.dismiss(r.id);
                    }
                }
            });
        });
    if let Some((id, undo)) = review_action {
        let _ = app.run(COMMAND, if undo { json!({"undo": id}) } else { json!({"keep": id}) });
    }
}

#[cfg(test)]
#[path = "ask_agent_tests.rs"]
mod tests;
