//! Ask Agent integration: state on SlideApp, selection/document serialization, and undo grouping.

pub mod hub;
pub mod ui;

use std::sync::Arc;

use deckcraft_engine::DocState;
use egui::Rect;
use serde_json::{Value, json};

pub use hub::{AgentHub, HubError, Request, Snapshot, Status, Turn};
pub use ui::Action;

use crate::SlideApp;

/// The UI command.
pub const COMMAND: &str = "app.askAgent";
/// Shortcut to open Ask Agent.
pub const SHORTCUT: &str = "Cmd+Shift+O";

/// Canvas anchor kinds where the chip and bubble position themselves.
#[derive(Clone, Copy, Debug)]
pub enum Anchor {
    Shapes(Rect),
    Text { first: Rect, last: Rect },
    Caret(Rect),
}

impl Anchor {
    pub fn rect(&self) -> Rect {
        match *self {
            Anchor::Shapes(r) | Anchor::Caret(r) => r,
            Anchor::Text { first, last } => first.union(last),
        }
    }
}

/// One undo group preserving document state before any agent edits in the session.
#[derive(Clone, Debug)]
pub struct UndoGroup {
    pub uid: u64,
    pub before: Arc<deckcraft_model::Presentation>,
    pub selection: deckcraft_engine::Selection,
    pub after: Arc<deckcraft_model::Presentation>,
    pub undo_len: usize,
}

/// An open Ask Agent conversation in the UI.
pub struct Open {
    pub id: u64,
    pub anchor: Rect,
    pub undo_group: Option<UndoGroup>,
    pub turns: Vec<Turn>,
    pub instruction: String,
    commented: bool,
}

/// Ask Agent state stored on [`SlideApp`].
#[derive(Default)]
pub struct AskAgent {
    pub hub: Option<AgentHub>,
    pub open: Option<Open>,
    pub draft: String,
    pub picked: Option<String>,
    pub anchor: Option<Anchor>,
    /// Set while a command from an agent runs, so the hold does not refuse it.
    agent_turn: bool,
}

impl AskAgent {
    pub fn inspect(&self) -> Value {
        json!({
            "open": self.open.as_ref().map(|o| o.id),
            "agents": self.hub.as_ref().map(|h| h.agents()).unwrap_or_default(),
        })
    }
}

fn slide_json(st: &DocState, idx: usize) -> Value {
    let sl = st.doc.slides.get(idx);
    let id = sl.map(|s| s.id.0).unwrap_or(0);
    let title = sl.map(|s| s.title()).unwrap_or_else(|| format!("Slide {}", idx + 1));
    json!({"index": idx, "id": id, "title": title})
}

/// Serialize the current selection for the agent.
pub fn selection(st: &DocState) -> Value {
    let sel = &st.selection;
    let mut v = json!({"slide": slide_json(st, sel.slide)});
    if let Some(ts) = &sel.text {
        let (a, b) = ts.ordered();
        let text = deckcraft_engine::cmd::text::body_of(st, ts).map(|body| deckcraft_model::edit::text_range(body, a, b)).unwrap_or_default();
        v["kind"] = json!("text");
        v["text"] = json!({
            "shape": (!ts.notes).then_some(ts.shape),
            "anchor": [ts.anchor.0, ts.anchor.1],
            "caret": [ts.caret.0, ts.caret.1],
            "cell": ts.cell.map(|(r, c)| [r, c]),
            "notes": ts.notes,
            "text": text,
        });
    } else if !sel.shapes.is_empty() {
        let ctx = st.current_slide().and_then(|sl| deckcraft_model::resolve::Ctx::for_slide(&st.doc, sl));
        let shapes: Vec<Value> =
            sel.shapes.iter().filter_map(|id| st.shape(*id)).map(|sh| deckcraft_engine::cmd::inspect::shape_summary(ctx.as_ref(), sh, 0)).collect();
        v["kind"] = json!("shapes");
        v["shapes"] = json!(shapes);
    } else {
        v["kind"] = json!("slide");
        if sel.slides.len() > 1 {
            let slides: Vec<Value> = sel.slides.iter().filter_map(|id| st.doc.slide_index(*id)).map(|i| slide_json(st, i)).collect();
            v["slides"] = json!(slides);
        }
    }
    v
}

/// Summary of the active document for the agent.
pub fn document(st: &DocState) -> Value {
    let title =
        st.path.as_deref().and_then(|p| std::path::Path::new(p).file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| st.title());
    let lang = st.shapes().iter().find_map(|s| s.text.as_ref()?.paragraphs.iter().find_map(|p| p.runs.iter().find_map(|r| r.props.lang.clone())));
    json!({
        "id": st.uid,
        "title": title,
        "saved": st.path.is_some(),
        "slides": st.doc.slides.len(),
        "language": lang,
    })
}

/// Handle `app.askAgent` command invocation.
pub fn command(app: &mut SlideApp, p: &Value) -> Result<Value, String> {
    if p.get("close").and_then(Value::as_bool) == Some(true) {
        handle_action(app, Action::Close);
        return Ok(json!({"open": false}));
    }
    let anchor =
        app.ask_agent.anchor.map(|a| a.rect()).unwrap_or_else(|| egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(340.0, 200.0)));
    if let Some(instr) = p.get("instruction").and_then(Value::as_str) {
        let (sel, doc) = app.session.active().map(|st| (selection(st), document(st))).unwrap_or_default();
        let id = app.ask_agent.hub.as_ref().ok_or("no agent hub")?.ask(Request {
            id: 0,
            instruction: instr.to_string(),
            history: vec![],
            selection: sel,
            document: doc,
            agent: app.ask_agent.picked.clone(),
        });
        app.ask_agent.open = Some(Open { id, anchor, undo_group: None, turns: vec![], instruction: instr.to_string(), commented: false });
        return Ok(json!({"id": id}));
    }
    app.ask_agent.open = Some(Open { id: 0, anchor, undo_group: None, turns: vec![], instruction: String::new(), commented: false });
    Ok(json!({}))
}

/// Reconcile user typing outside the bubble.
pub fn typed_on_slide(app: &mut SlideApp, events: &[egui::Event]) {
    if app.ask_agent.open.is_none() {
        return;
    }
    if events.iter().any(|e| matches!(e, egui::Event::Text(_) | egui::Event::Paste(_))) {
        app.ask_agent.open = None;
    }
}

/// Handle Escape key to dismiss open bubble.
pub fn keyboard(app: &mut SlideApp, ctx: &egui::Context) {
    if app.ask_agent.open.is_some() && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        app.ask_agent.open = None;
    }
}

pub fn remember_focus(_app: &mut SlideApp, _ctx: &egui::Context) {}

/// Draw the chip or bubble and dispatch actions.
pub fn show(app: &mut SlideApp, ctx: &egui::Context) {
    let Some(hub) = app.ask_agent.hub.clone() else { return };
    let Some(open) = &app.ask_agent.open else {
        if let Some(anchor) = app.ask_agent.anchor.map(|a| a.rect())
            && !hub.agents().is_empty()
            && ui::chip(ctx, anchor)
        {
            app.ask_agent.open = Some(Open { id: 0, anchor, undo_group: None, turns: vec![], instruction: String::new(), commented: false });
        }
        return;
    };
    let (anchor, snapshot) = (open.anchor, hub.snapshot(open.id));
    if let Some(s) = snapshot.as_ref().filter(|s| s.status == Status::Answered) {
        comment_once(app, s);
    }
    let agents = hub.agents();
    let view = ui::View { snapshot: snapshot.as_ref(), agents: &agents };
    if let Some(action) = ui::bubble(ctx, anchor, &view, &mut app.ask_agent.draft, &mut app.ask_agent.picked) {
        handle_action(app, action);
    }
}

/// The agent's answer, once, as a comment on what it was asked about, authored like its edits.
fn comment_once(app: &mut SlideApp, s: &Snapshot) {
    let Some(open) = app.ask_agent.open.as_mut().filter(|o| !o.commented) else { return };
    open.commented = true;
    let (Some(answer), id) = (s.answer.clone(), open.id) else { return };
    let _ = run_for(app, id, "comment.add", json!({"text": answer}));
}

/// The shapes agents are working on, with each agent's colour.
pub fn held_shapes(app: &SlideApp) -> Vec<(u64, [u8; 3])> {
    let Some(hub) = &app.ask_agent.hub else { return vec![] };
    let ids = |sel: &Value| -> Vec<u64> {
        sel["shapes"].as_array().into_iter().flatten().filter_map(|s| s["id"].as_u64()).chain(sel["text"]["shape"].as_u64()).collect()
    };
    hub.held().into_iter().flat_map(|(sel, name)| ids(&sel).into_iter().map(move |id| (id, hub::colour(&name)))).collect()
}

/// Why the person may not run `command` now: it would change shapes an agent is working on.
pub fn held(app: &SlideApp, command: &str) -> Option<String> {
    let hub = app.ask_agent.hub.as_ref().filter(|_| !app.ask_agent.agent_turn)?;
    let spec = deckcraft_engine::cmd::find_command(command)?;
    let st = app.session.active()?;
    if !spec.undoable || command.starts_with("edit.select") {
        return None;
    }
    let mine: Vec<u64> = st.selection.shapes.iter().map(|s| u64::from(s.0)).chain(st.selection.text.as_ref().map(|t| u64::from(t.shape.0))).collect();
    hub.held().into_iter().find_map(|(sel, name)| {
        let ids = sel["shapes"].as_array().into_iter().flatten().filter_map(|s| s["id"].as_u64()).chain(sel["text"]["shape"].as_u64());
        let slide_held = sel["kind"] == "slide" && sel["slide"]["index"].as_u64() == Some(st.selection.slide as u64);
        (slide_held || ids.into_iter().any(|id| mine.contains(&id))).then(|| format!("Held by Agent ({name})"))
    })
}

fn handle_action(app: &mut SlideApp, action: Action) {
    let Some(hub) = app.ask_agent.hub.clone() else { return };
    match action {
        Action::Send(text) => {
            let (sel, doc) = app.session.active().map(|st| (selection(st), document(st))).unwrap_or_default();
            let agent = app.ask_agent.picked.clone();
            let Some(open) = &mut app.ask_agent.open else { return };
            // After an answer, this is a follow-up: the thread so far goes with it.
            if let Some(answer) = hub.snapshot(open.id).and_then(|s| s.answer) {
                open.turns.push(Turn { instruction: std::mem::take(&mut open.instruction), answer });
                hub.close(open.id);
            }
            open.instruction = text.clone();
            open.commented = false;
            open.id = hub.ask(Request { id: 0, instruction: text, history: open.turns.clone(), selection: sel, document: doc, agent });
        }
        Action::Undo => {
            let Some(open) = app.ask_agent.open.take() else { return };
            hub.close(open.id);
            if let Some(ug) = open.undo_group
                && let Some(st) = app.session.active_mut()
            {
                if st.uid == ug.uid && Arc::ptr_eq(&ug.after, &st.doc) {
                    st.doc = ug.before;
                    st.selection = ug.selection;
                    st.history.undo.truncate(ug.undo_len);
                    st.history.redo.clear();
                    st.revision += 1;
                } else {
                    app.set_status("The presentation changed since the agent's edits; use Undo in the Edit menu");
                }
            }
        }
        Action::Keep | Action::Close => {
            if let Some(open) = app.ask_agent.open.take() {
                hub.close(open.id);
            }
        }
    }
}

/// Execute an engine edit on behalf of a request, coalescing changes into one undo group.
pub fn run_for(app: &mut SlideApp, request: u64, id: &str, params: Value) -> Result<Value, String> {
    let spec = deckcraft_engine::cmd::find_command(id).ok_or("Ask Agent edits must use engine commands")?;
    if id.starts_with("file.") || matches!(id, "edit.undo" | "edit.redo" | "text.compose" | "text.commit") {
        return Err("file, history and composition commands cannot run for an Ask Agent request".into());
    }
    let st = app.session.active().ok_or("open the request's presentation first")?;
    if st.interaction.is_some() {
        return Err("finish the pointer gesture before the agent edits".into());
    }
    app.session.end_composition();
    let st = app.session.active().ok_or("open the request's presentation first")?;

    if let Some(open) = &app.ask_agent.open
        && open.id == request
        && let Some(ug) = &open.undo_group
        && (ug.uid != st.uid || !Arc::ptr_eq(&ug.after, &st.doc))
    {
        return Err("the presentation changed after the agent's edit; finish this request and ask again".into());
    }

    let (uid, before, selection, undo_len, limit) = (st.uid, st.doc.clone(), st.selection.clone(), st.history.undo.len(), st.history.limit);

    if let Some(st) = app.session.active_mut() {
        st.history.limit = limit.max(1).saturating_add(1);
    }
    let name = app.ask_agent.hub.as_ref().and_then(|h| h.snapshot(request)).and_then(|s| s.agent).unwrap_or_default();
    let author = std::mem::replace(&mut app.session.prefs.author, format!("Agent ({name})"));
    let was_agent = std::mem::replace(&mut app.ask_agent.agent_turn, true);
    let result = app.run(id, params);
    app.ask_agent.agent_turn = was_agent;
    app.session.prefs.author = author;
    if let Some(st) = app.session.active_mut() {
        st.history.limit = limit;
    }
    let result = result?;
    app.session.end_composition();
    let st = app.session.active_mut().ok_or("presentation closed")?;

    if !Arc::ptr_eq(&before, &st.doc) {
        if !spec.undoable {
            st.doc = before;
            st.selection = selection;
            st.revision += 1;
            return Err("this command changes the document without undo support".into());
        }
        let (ug_before, ug_sel, ug_undo_len) = if let Some(open) = &app.ask_agent.open
            && open.id == request
            && let Some(ug) = &open.undo_group
        {
            (ug.before.clone(), ug.selection.clone(), ug.undo_len)
        } else {
            (before, selection, undo_len)
        };

        st.history.undo.truncate(ug_undo_len);
        st.history.undo.push(deckcraft_engine::HistoryEntry { label: "Ask Agent".into(), doc: ug_before.clone(), selection: ug_sel.clone() });
        st.history.redo.clear();

        if let Some(open) = &mut app.ask_agent.open
            && open.id == request
        {
            open.undo_group = Some(UndoGroup { uid, before: ug_before, selection: ug_sel, after: st.doc.clone(), undo_len: ug_undo_len });
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use deckcraft_engine::Session;
    use std::time::Duration;

    #[test]
    fn test_bubble_flow_headless() {
        let hub = AgentHub::default();
        let mut app = SlideApp::new(Session::with_new(), crate::Services::default()).with_agent_hub(hub.clone());

        let res = command(&mut app, &json!({"instruction": "make title bigger"})).unwrap();
        let req_id = res["id"].as_u64().unwrap();
        assert!(app.ask_agent.open.is_some());
        assert_eq!(app.ask_agent.open.as_ref().unwrap().id, req_id);

        let acquired = hub.listen(1, Some("Codex"), Duration::from_millis(50), || true).unwrap();
        assert_eq!(acquired.id, req_id);
        assert_eq!(acquired.instruction, "make title bigger");

        hub.reply(1, req_id, "Processing…", false, None).unwrap();
        let snap = hub.snapshot(req_id).unwrap();
        assert_eq!(snap.notes, vec!["Processing…"]);

        let doc_before = app.session.active().unwrap().doc.clone();
        run_for(&mut app, req_id, "shape.insert", json!({"preset": "rect", "rect": [10.0, 10.0, 100.0, 50.0]})).unwrap();
        assert!(!Arc::ptr_eq(&doc_before, &app.session.active().unwrap().doc));

        assert!(held(&app, "shape.insert").is_some_and(|why| why.contains("Codex")), "the slide is held for the agent");
        hub.reply(1, req_id, "Done inserting shape", true, None).unwrap();
        comment_once(&mut app, &hub.snapshot(req_id).unwrap());
        let st = app.session.active().unwrap();
        assert_eq!(st.doc.slides[st.selection.slide].comments.last().unwrap().author, "Agent (Codex)");
        assert!(held(&app, "shape.insert").is_none(), "answered: the slide is free");

        handle_action(&mut app, Action::Send("now make it red".into()));
        let follow_id = app.ask_agent.open.as_ref().unwrap().id;
        assert_ne!(follow_id, req_id);
        assert_eq!(app.ask_agent.open.as_ref().unwrap().turns.len(), 1);

        let acquired2 = hub.listen(1, Some("Codex"), Duration::from_millis(50), || true).unwrap();
        assert_eq!(acquired2.id, follow_id);
        assert_eq!(acquired2.history.len(), 1);

        handle_action(&mut app, Action::Undo);
        assert!(Arc::ptr_eq(&doc_before, &app.session.active().unwrap().doc));
        assert!(app.ask_agent.open.is_none());
    }
}
