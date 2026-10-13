//! Ask Agent: the queue, the request's contents, the geometry, and the window driven headless.

use std::time::Duration;

use deckcraft_engine::Session;
use egui::{Key, Modifiers, Pos2, Rect, pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use super::*;
use crate::{Services, SlideApp};

fn sel(text: &str) -> Value {
    json!({"kind": "text", "text": {"text": text}})
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn requests_go_out_oldest_first_and_once() {
    let hub = AgentHub::new(7979, None);
    assert!(hub.listen(1, ms(10), || true).is_none(), "nothing queued");
    let a = hub.ask("  shorten this  ", sel("Kilns fire clay."), json!({})).unwrap();
    let b = hub.ask("translate", sel(""), json!({})).unwrap();
    assert!(b > a);
    let got = hub.listen(1, ms(10), || true).unwrap();
    assert_eq!((got.id, got.instruction.as_str(), got.state), (a, "shorten this", State::Working));
    assert_eq!(got.to_json()["selection"]["text"]["text"], "Kilns fire clay.");
    assert_eq!(hub.listen(2, ms(10), || true).unwrap().id, b);
    assert!(hub.listen(1, ms(10), || true).is_none(), "each request goes to one agent");
}

#[test]
fn listen_waits_on_its_own_thread_until_a_request_arrives() {
    let hub = AgentHub::new(7979, None);
    let agent = hub.clone();
    let waiting = std::thread::spawn(move || {
        let r = agent.listen(1, Duration::from_secs(10), || true);
        (r, std::time::Instant::now())
    });
    // The listener counts as an agent while it waits.
    let t0 = std::time::Instant::now();
    while !hub.agent_present() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(ms(5));
    }
    assert!(hub.agent_present());
    std::thread::sleep(ms(50));
    let asked = std::time::Instant::now();
    let id = hub.ask("make it formal", sel("hey"), json!({})).unwrap();
    let (got, at) = waiting.join().unwrap();
    assert_eq!(got.expect("woken by the request").id, id);
    let lag = at.duration_since(asked);
    assert!(lag < ms(500), "woken in {lag:?}, not by polling the window");
}

#[test]
fn a_client_that_has_gone_gets_nothing_stops_counting_and_the_request_stays() {
    let hub = AgentHub::new(7979, None);
    let id = hub.ask("fix the typos", sel("teh"), json!({})).unwrap();
    assert!(hub.listen(1, ms(10), || false).is_none());
    assert!(!hub.agent_present(), "a client that left doesn't count, even for the 60 s after it listened");
    assert_eq!(hub.request(id).unwrap().state, State::Waiting);
    assert_eq!(hub.listen(2, ms(10), || true).unwrap().id, id);
    // A reply that never reached the agent: back in the queue.
    hub.requeue(id);
    assert_eq!(hub.listen(2, ms(10), || true).unwrap().id, id);
    // The agent's connection closes before it answers: back in the queue, and it no longer
    // counts.
    assert!(hub.agent_present(), "an open connection working on a request");
    hub.disconnect(2);
    assert_eq!(hub.request(id).unwrap().state, State::Waiting);
    assert!(!hub.agent_present());
    // A new connection must take the requeued request before replying.
    assert!(hub.reply(3, id, "Fixed two typos.", true).is_err());
    hub.listen(3, ms(10), || true).unwrap();
    assert_eq!(hub.reply(3, id, "Fixed two typos.", true).unwrap()["shown"], true);
    assert_eq!(hub.request(id).unwrap().state, State::Answered);
}

#[test]
fn presence_lasts_a_minute_after_the_last_listen() {
    let hub = AgentHub::new(7985, None);
    let app = AskAgent { hub: Some(hub.clone()), ..Default::default() };
    assert_eq!(app.unavailable(), Some(Unavailable::NoAgent(7985)));
    assert!(app.unavailable().unwrap().message().contains("deckcraft-cli mcp --connect 7985"));
    hub.listen(1, ms(1), || true);
    assert_eq!(app.unavailable(), None, "listened a moment ago");
    // 61 s later, with no request in hand: gone.
    for a in hub.lock().agents.iter_mut() {
        a.1 -= PRESENCE_MS + 1000.0;
    }
    assert!(!hub.agent_present());
    // Unless it holds a request on its open connection.
    let id = hub.ask("go", sel(""), json!({})).unwrap();
    hub.listen(1, ms(1), || true);
    for a in hub.lock().agents.iter_mut() {
        a.1 -= PRESENCE_MS + 1000.0;
    }
    assert!(hub.agent_present(), "working on request {id}");
    hub.reply(1, id, "Done", true).unwrap();
    for a in hub.lock().agents.iter_mut() {
        a.1 -= PRESENCE_MS + 1000.0;
    }
    assert!(!hub.agent_present());
    assert_eq!(AskAgent::default().unavailable(), Some(Unavailable::NoControlChannel));
}

#[test]
fn replies_show_and_close_cancelled_requests_say_so() {
    let hub = AgentHub::new(7979, None);
    let id = hub.ask("shorten", sel("a b c"), json!({})).unwrap();
    hub.listen(1, ms(10), || true).unwrap();
    assert_eq!(hub.reply(1, id, "Working on it", false).unwrap()["done"], false);
    assert_eq!(hub.request(id).unwrap().state, State::Working);
    assert_eq!(hub.reply(1, id, "Shortened to one line.", true).unwrap()["shown"], true);
    let r = hub.request(id).unwrap();
    assert_eq!((r.state, r.reply.as_str()), (State::Answered, "Shortened to one line."));
    assert!(hub.reply(1, id, "again", true).unwrap_err().contains("already answered"));
    assert!(hub.reply(1, 999, "?", true).unwrap_err().contains("no Ask Agent request 999"));
    // Cancelled while the agent works: its reply learns it.
    let id = hub.ask("translate", sel("x"), json!({})).unwrap();
    hub.listen(1, ms(10), || true).unwrap();
    assert!(hub.cancel(id));
    assert!(!hub.cancel(id), "only once");
    assert_eq!(hub.reply(1, id, "Done", true).unwrap()["cancelled"], true);
    // A cancelled request is never handed out.
    let id = hub.ask("later", sel(""), json!({})).unwrap();
    hub.cancel(id);
    assert!(hub.listen(1, ms(10), || true).is_none());
    hub.dismiss(id);
    assert!(hub.request(id).is_none());
}

#[test]
fn hostile_text_is_capped_and_empty_instructions_refused() {
    let hub = AgentHub::new(7979, None);
    assert!(hub.ask("   ", sel(""), json!({})).is_err());
    let id = hub.ask(&"é".repeat(MAX_TEXT_CHARS + 50), sel(""), json!({})).unwrap();
    assert_eq!(hub.request(id).unwrap().instruction.chars().count(), MAX_TEXT_CHARS + 1, "cut, with …");
    hub.listen(1, ms(10), || true).unwrap();
    hub.reply(1, id, &"ü".repeat(MAX_TEXT_CHARS * 3), true).unwrap();
    assert_eq!(hub.request(id).unwrap().reply.chars().count(), MAX_TEXT_CHARS + 1);
    let mut s = "ab€".repeat(10);
    assert!(cap_bytes(&mut s, 4));
    assert_eq!(s, "ab", "cut at a character boundary");
    // The queue stays bounded however many requests come.
    for i in 0..200 {
        let id = hub.ask(&format!("r{i}"), sel(""), json!({})).unwrap();
        hub.cancel(id);
    }
    assert!(hub.summaries().len() <= KEEP);
    for i in 0..200 {
        hub.ask(&format!("open {i}"), sel(""), json!({})).unwrap();
    }
    assert!(hub.summaries().len() <= 2 * KEEP);
    // So does the list of agent connections.
    for c in 0..500 {
        hub.reply(c, 1, "x", false).ok();
    }
    assert!(hub.lock().agents.len() <= MAX_AGENTS);
}

#[test]
fn the_window_reads_the_requests_only_when_they_change() {
    let hub = AgentHub::new(7979, None);
    let mut a = AskAgent { hub: Some(hub.clone()), ..Default::default() };
    assert!(a.requests().is_empty());
    let v = hub.version();
    let id = hub.ask("go", sel(&"x".repeat(MAX_SELECTION_BYTES)), json!({})).unwrap();
    assert_ne!(hub.version(), v);
    assert_eq!(a.requests()[0].id, id);
    let seen = a.seen.as_ref().map(|(v, _)| *v);
    assert_eq!(a.requests().len(), 1);
    assert_eq!(a.seen.as_ref().map(|(v, _)| *v), seen, "no change, no new read");
}

#[test]
fn recent_instructions_are_few_unique_and_newest_first() {
    let mut h = Vec::new();
    for i in 0..20 {
        remember(&mut h, &format!("  step {i} "));
    }
    remember(&mut h, "step 15");
    remember(&mut h, "   ");
    assert_eq!(h.len(), HISTORY);
    assert_eq!(h[0], "step 15");
    assert_eq!(h.iter().filter(|x| *x == "step 15").count(), 1);
}

#[test]
fn the_bubble_and_chip_stay_inside_the_canvas() {
    let view = Rect::from_min_size(pos2(0.0, 100.0), vec2(800.0, 600.0));
    let size = vec2(WIDTH, 150.0);
    // Below the selection.
    let r = Rect::from_min_size(pos2(100.0, 200.0), vec2(200.0, 20.0));
    let p = bubble_pos(Some(r), size, view).unwrap();
    assert!(p.y >= 220.0 && p.x == 100.0, "{p:?}");
    // Near the bottom: above it; near the right edge: pulled in.
    let p = bubble_pos(Some(Rect::from_min_size(pos2(790.0, 660.0), vec2(5.0, 20.0))), size, view).unwrap();
    assert!(p.y + size.y <= 660.0 && p.x + size.x <= 800.0, "{p:?}");
    // No anchor on screen, or junk: the canvas's corner, never NaN.
    assert!(bubble_pos(None, size, view).is_some_and(|p| view.contains(p)));
    assert!(bubble_pos(Some(Rect::from_min_size(pos2(f32::NAN, 1.0), vec2(1.0, 1.0))), size, view).is_some_and(|p| p.is_finite()));
    assert!(bubble_pos(None, size, Rect::NOTHING).is_none());

    let chip = vec2(100.0, CHIP_H);
    let inside = |p: Pos2| view.contains_rect(Rect::from_min_size(p, chip));
    // Shapes: to the right of the box, at its top; left of it at the canvas's right edge; under
    // it when it spans the canvas.
    let shape = Rect::from_min_size(pos2(200.0, 300.0), vec2(200.0, 100.0));
    assert_eq!(chip_pos(Anchor::Shapes(shape), chip, view), Some(pos2(400.0 + GAP + HANDLES, 300.0)));
    let right = Rect::from_min_size(pos2(600.0, 300.0), vec2(190.0, 100.0));
    assert_eq!(chip_pos(Anchor::Shapes(right), chip, view), Some(pos2(600.0 - GAP - HANDLES - 100.0, 300.0)));
    let wide = Rect::from_min_size(pos2(0.0, 300.0), vec2(800.0, 100.0));
    let p = chip_pos(Anchor::Shapes(wide), chip, view).unwrap();
    assert!(p.y >= 400.0 && inside(p), "{p:?}");
    let all = view;
    assert!(chip_pos(Anchor::Shapes(all), chip, view).is_some_and(inside), "a box filling the canvas: inside its corner");
    // Text: above the first line; under the last one at the canvas's top.
    let first = Rect::from_min_size(pos2(120.0, 300.0), vec2(200.0, 18.0));
    let last = Rect::from_min_size(pos2(100.0, 340.0), vec2(150.0, 18.0));
    assert_eq!(chip_pos(Anchor::Text { first, last }, chip, view), Some(pos2(120.0, 300.0 - GAP - CHIP_H)));
    let top = Rect::from_min_size(pos2(120.0, 104.0), vec2(200.0, 18.0));
    let p = chip_pos(Anchor::Text { first: top, last: top }, chip, view).unwrap();
    assert!(p.y >= 122.0 && inside(p), "{p:?}");
    assert!(chip_pos(Anchor::Shapes(Rect::NAN), chip, view).is_none());
}

// The request's contents.

fn deck() -> (Session, u32) {
    let mut s = Session::with_new();
    s.prefs.autocorrect = false;
    s.execute("slide.new", &json!({"layout": "blank"})).unwrap();
    let id = s.execute("shape.insert", &json!({"preset": "rect", "rect": [300, 200, 300, 120], "text": "Kilns fire clay slowly"})).unwrap()["id"]
        .as_u64()
        .unwrap() as u32;
    s.execute("text.exit", &json!({})).ok();
    s.execute("edit.select", &json!({"ids": []})).unwrap();
    (s, id)
}

#[test]
fn the_request_carries_the_shapes_the_text_or_the_slide() {
    let (mut s, id) = deck();
    // Nothing selected: the slide.
    let st = s.active().unwrap();
    let v = selection(st);
    assert_eq!(v["kind"], "slide");
    assert_eq!(v["slide"]["index"], 1);
    assert!(v.get("target").is_none());
    assert_eq!(about(st), "Nothing is selected: the agent gets slide 2.");
    let d = document(st);
    assert_eq!((d["title"].clone(), d["saved"].clone(), d["slides"].clone()), (json!("Presentation1"), json!(false), json!(2)), "{d}");
    // A shape: its summary, as slide.inspect gives it.
    s.execute("edit.select", &json!({"ids": [id]})).unwrap();
    let st = s.active().unwrap();
    let v = selection(st);
    assert_eq!(v["kind"], "shapes");
    assert_eq!(v["shapes"][0]["id"], id);
    assert_eq!(v["shapes"][0]["text"], "Kilns fire clay slowly");
    assert_eq!(v["shapes"][0]["box"], json!([300.0, 200.0, 300.0, 120.0]));
    assert_eq!(v["truncated"], false);
    assert_eq!(about(st), "The agent gets the selected shape.");
    // Selected text: the text, where it is, and its shape.
    s.execute("text.edit", &json!({"id": id})).unwrap();
    s.execute("text.select", &json!({"anchor": [0, 6], "caret": [0, 10]})).unwrap();
    let st = s.active().unwrap();
    let v = selection(st);
    assert_eq!(v["kind"], "text");
    assert_eq!(v["text"]["text"], "fire");
    assert_eq!((v["text"]["anchor"].clone(), v["text"]["caret"].clone(), v["text"]["shape"].clone()), (json!([0, 6]), json!([0, 10]), json!(id)));
    assert_eq!(v["shapes"][0]["id"], id);
    assert_eq!(about(st), "The agent gets the selected text (4 characters) in this shape.");
    // The positions select the same text again.
    s.execute("text.select", &json!({"anchor": v["text"]["anchor"], "caret": v["text"]["caret"]})).unwrap();
    assert_eq!(selection(s.active().unwrap())["text"]["text"], "fire");
    // A huge selection is cut, and says so.
    let big = "word ".repeat(30_000);
    s.execute("text.set", &json!({"id": id, "text": big})).unwrap();
    s.execute("text.edit", &json!({"id": id})).unwrap();
    s.execute("text.selectAll", &json!({})).unwrap();
    let v = selection(s.active().unwrap());
    assert!(v["text"]["text"].as_str().unwrap().len() <= MAX_SELECTION_BYTES && v["text"]["truncated"] == true);
}

// The whole window, headless.

fn window(hub: Option<AgentHub>) -> (Harness<'static, SlideApp>, u32) {
    let (s, id) = deck();
    let mut app = SlideApp::new(s, Services::default());
    if let Some(hub) = hub {
        app = app.with_agent_hub(hub);
    }
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    (h, id)
}

fn steps(h: &mut Harness<'static, SlideApp>) {
    h.run_steps(4);
}

fn chip(h: &Harness<'static, SlideApp>) -> Option<Rect> {
    let shown = h.ctx.memory(|m| m.areas().visible_layer_ids().into_iter().any(|l| l.id == egui::Id::new("dc_ask_agent_chip")));
    shown.then(|| h.ctx.data(|d| d.get_temp::<Rect>(chip_id()))).flatten()
}

fn click(h: &mut Harness<'static, SlideApp>, at: Pos2) {
    h.event(egui::Event::PointerMoved(at));
    h.step();
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() });
        h.step();
    }
    steps(h);
}

fn key(h: &mut Harness<'static, SlideApp>, key: Key, modifiers: Modifiers) {
    h.key_press_modifiers(modifiers, key);
    steps(h);
}

fn listening() -> AgentHub {
    let hub = AgentHub::new(7985, None);
    hub.listen(1, ms(1), || true);
    hub
}

/// The chip shows on selected shapes and on selected text, only while an agent listens, in the
/// first frame after the selection, and not on a caret.
#[test]
fn the_chip_shows_on_a_selection_while_an_agent_listens() {
    // No control channel: nothing floats.
    let (mut h, id) = window(None);
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    steps(&mut h);
    assert!(chip(&h).is_none(), "no control channel, no chip");
    // A control channel, no agent yet: still nothing.
    let hub = AgentHub::new(7985, None);
    let (mut h, id) = window(Some(hub.clone()));
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    steps(&mut h);
    assert!(chip(&h).is_none(), "no agent listening, no chip");
    // An agent listens: the chip is beside the shape, inside the canvas.
    hub.listen(1, ms(1), || true);
    h.state_mut().run("edit.select", json!({"ids": []})).unwrap();
    steps(&mut h);
    assert!(chip(&h).is_none(), "nothing selected, no chip");
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    h.step();
    let at = chip(&h).expect("the chip shows in the first frame after the selection");
    let (canvas, slide, k) = (h.state().canvas_rect.unwrap(), h.state().slide_rect.unwrap(), h.state().canvas_scale);
    assert!(canvas.contains_rect(at), "{at:?} inside {canvas:?}");
    let shape_right = slide.min.x + 600.0 * k;
    assert!(at.left() > shape_right && (at.top() - (slide.min.y + 200.0 * k)).abs() < 2.0, "beside the shape: {at:?}");
    // A caret in the text: no chip. Selected text: the chip above it.
    h.state_mut().run("text.edit", json!({"id": id})).unwrap();
    steps(&mut h);
    assert!(chip(&h).is_none(), "no chip on a caret");
    h.state_mut().run("text.select", json!({"anchor": [0, 0], "caret": [0, 5]})).unwrap();
    steps(&mut h);
    let at = chip(&h).expect("the chip shows on selected text");
    assert!(at.bottom() <= slide.min.y + 260.0 * k, "above the text: {at:?}");
    // The context menu has Ask Agent too; while a menu is open the chip steps aside.
    assert!(h.state().ask_agent.anchor.is_some());
}

/// Chip → bubble → instruction → the agent gets the selection; its notes and answer show.
#[test]
fn the_chip_opens_the_bubble_and_the_request_reaches_the_agent() {
    let hub = listening();
    let (mut h, id) = window(Some(hub.clone()));
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    steps(&mut h);
    let at = chip(&h).expect("chip").center();
    click(&mut h, at);
    assert!(h.state().ask_agent.bubble.is_some(), "the chip opened the bubble");
    assert!(chip(&h).is_none(), "the chip gives way to the bubble");
    assert!(h.query_by_label_contains("The agent gets the selected shape").is_some(), "the bubble says what the agent gets");
    h.event(egui::Event::Text("make it punchier".into()));
    steps(&mut h);
    key(&mut h, Key::Enter, Modifiers::NONE);
    // Sent: the bubble says so at once, before any agent has it.
    assert!(h.query_by_label_contains("Sent to your agent").is_some(), "immediate feedback");
    let r = hub.listen(1, ms(10), || true).expect("the request is queued");
    assert_eq!(r.instruction, "make it punchier");
    assert_eq!(r.selection["kind"], "shapes");
    assert_eq!(r.selection["shapes"][0]["id"], id);
    assert_eq!(r.document["title"], "Presentation1");
    assert_eq!(h.state().ask_agent.history, vec!["make it punchier".to_string()]);
    steps(&mut h);
    assert!(h.query_by_label_contains("Your agent is working").is_some(), "the bubble says the agent has it");
    hub.reply(1, r.id, "Rewriting the text…", false).unwrap();
    steps(&mut h);
    assert!(h.query_by_label_contains("Rewriting the text…").is_some(), "the latest progress note");
    hub.reply(1, r.id, "Cut it to three words.", true).unwrap();
    steps(&mut h);
    assert!(h.query_by_label_contains("Cut it to three words.").is_some(), "the answer shows in the bubble");
    // The slide is the agent's to change, through the usual commands; nothing here.
    let st = h.state().session.active().unwrap();
    assert_eq!(st.shape(deckcraft_model::ShapeId(id)).and_then(|s| s.text.as_ref()).map(|t| t.text()).as_deref(), Some("Kilns fire clay slowly"));
    // Close puts the answered request away.
    h.get_by_label("Close").click();
    steps(&mut h);
    assert!(h.state().ask_agent.bubble.is_none());
    assert_eq!(hub.summaries().len(), 1, "closing keeps the answered request in history");
}

/// Escape closes the bubble only while it has the focus; typing on the slide closes it (the
/// request goes on in the corner card); a press in the canvas outside it closes it too.
#[test]
fn escape_typing_and_clicks_close_the_bubble() {
    let hub = listening();
    let (mut h, id) = window(Some(hub.clone()));
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    steps(&mut h);
    // The box has the focus: Escape closes the bubble, nothing is sent.
    key(&mut h, Key::O, Modifiers::COMMAND | Modifiers::SHIFT);
    assert!(h.state().ask_agent.bubble.is_some(), "Ctrl+Shift+O opened the bubble");
    key(&mut h, Key::Escape, Modifiers::NONE);
    assert!(h.state().ask_agent.bubble.is_none() && hub.summaries().is_empty(), "Escape in the box sends nothing");
    // After sending the box is gone and the slide has the keyboard: Escape is the slide's.
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    h.event(egui::Event::Text("add speaker notes".into()));
    steps(&mut h);
    key(&mut h, Key::Enter, Modifiers::NONE);
    assert_eq!(hub.summaries().len(), 1);
    key(&mut h, Key::Escape, Modifiers::NONE);
    assert!(h.state().ask_agent.bubble.is_some(), "Escape on the slide leaves the bubble");
    // Typing on the slide: the bubble gives way, the request goes on in the corner card.
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    steps(&mut h);
    key(&mut h, Key::ArrowRight, Modifiers::NONE);
    assert!(h.state().ask_agent.bubble.is_none(), "typing on the slide closes the bubble");
    assert_eq!(hub.summaries()[0].state, State::Waiting, "the request goes on");
    assert!(h.query_by_label_contains("add speaker notes").is_some(), "in the corner card");
    // A press in the canvas away from the bubble closes it.
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    assert!(h.state().ask_agent.bubble.is_some());
    let slide = h.state().slide_rect.unwrap();
    click(&mut h, slide.min + vec2(10.0, 10.0));
    assert!(h.state().ask_agent.bubble.is_none(), "a click elsewhere closes it");
}

/// With no agent the shortcut says why in the status bar; Cancel in the bubble reaches the agent.
#[test]
fn cancel_and_no_agent() {
    let hub = AgentHub::new(7985, None);
    let (mut h, id) = window(Some(hub.clone()));
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    steps(&mut h);
    key(&mut h, Key::O, Modifiers::COMMAND | Modifiers::SHIFT);
    assert!(h.state().ask_agent.bubble.is_none(), "no agent: no bubble");
    let msg = h.state().status.clone().map(|m| m.0).unwrap_or_default();
    assert!(msg.contains("No agent is listening") && msg.contains("--connect 7985"), "{msg}");
    hub.listen(1, ms(1), || true);
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    h.event(egui::Event::Text("translate into French".into()));
    steps(&mut h);
    key(&mut h, Key::Enter, Modifiers::NONE);
    let r = hub.listen(1, ms(10), || true).expect("queued");
    steps(&mut h);
    h.get_by_label("Cancel").click();
    steps(&mut h);
    assert_eq!(hub.reply(1, r.id, "Traduit.", true).unwrap()["cancelled"], true, "the agent learns it was cancelled");
}

/// Without a control channel `app.askAgent` says how to get one; scripts can queue requests and
/// read them in `ui.inspect`.
#[test]
fn scripts_and_no_control_channel() {
    let (s, id) = deck();
    let mut a = SlideApp::new(s, Services::default());
    let e = a.run(COMMAND, json!({})).unwrap_err();
    assert!(e.contains("--control"), "{e}");
    let hub = AgentHub::new(7979, None);
    let mut a = a.with_agent_hub(hub.clone());
    a.run("edit.select", json!({"ids": [id]})).unwrap();
    let rid = a.run(COMMAND, json!({"instruction": "explain this shape"})).unwrap()["id"].as_u64().unwrap();
    assert_eq!(hub.request(rid).unwrap().selection["shapes"][0]["id"], id);
    let v = crate::control::inspect(&a, &egui::Context::default());
    assert_eq!(v["askAgent"]["requests"][0]["state"], "waiting");
    assert_eq!(v["askAgent"]["agentListening"], false);
    assert!(v["askAgent"]["reason"].as_str().unwrap().contains("No agent is listening"));
    assert_eq!(a.run(COMMAND, json!({"cancel": rid})).unwrap()["cancelled"], true);
    assert!(a.run(COMMAND, json!({"instruction": 5})).is_err());
    assert!(a.run(COMMAND, json!({"cancel": "x"})).is_err());
    // Listed with the other commands, with its shortcut.
    let all = crate::control::all_commands(&a);
    assert!(all.as_array().unwrap().iter().any(|c| c["id"] == COMMAND && c["shortcut"] == SHORTCUT));
}

/// Up in the empty box brings back the last instruction.
#[test]
fn up_recalls_the_last_instruction() {
    let hub = listening();
    let (mut h, id) = window(Some(hub.clone()));
    h.state_mut().ask_agent.history = vec!["make it punchier".into(), "older".into()];
    h.state_mut().run("edit.select", json!({"ids": [id]})).unwrap();
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    key(&mut h, Key::ArrowUp, Modifiers::NONE);
    assert_eq!(h.state().ask_agent.bubble.as_ref().map(|b| b.text.as_str()), Some("make it punchier"));
    key(&mut h, Key::Enter, Modifiers::NONE);
    assert_eq!(hub.listen(1, ms(10), || true).map(|r| r.instruction), Some("make it punchier".to_string()));
}

fn request_app() -> (SlideApp, AgentHub, u32, u64) {
    let (s, shape) = deck();
    let hub = listening();
    let mut app = SlideApp::new(s, Services::default()).with_agent_hub(hub.clone());
    let id = app.run(COMMAND, json!({"instruction": "move and recolor"})).unwrap()["id"].as_u64().unwrap();
    hub.listen(1, ms(1), || true).unwrap();
    (app, hub, shape, id)
}

#[test]
fn request_edits_are_one_undo_step_and_person_typing_is_separate() {
    let (mut app, hub, shape, id) = request_app();
    let original = app.session.active().unwrap().doc.clone();
    let count = app.session.active().unwrap().history.undo.len();
    run_for(&mut app, id, "shape.move", json!({"ids": [shape], "dx": 25})).unwrap();
    run_for(&mut app, id, "shape.fill", json!({"ids": [shape], "color": "#123456"})).unwrap();
    assert_eq!(app.session.active().unwrap().history.undo.len(), count + 1);
    let edited = app.session.active().unwrap().doc.clone();
    hub.reply(1, id, "Done", true).unwrap();
    assert!(run_for(&mut app, id, "shape.move", json!({"ids": [shape], "dx": 1})).is_err());
    app.run("text.edit", json!({"id": shape, "end": true})).unwrap();
    app.run("text.insert", json!({"text": " (fired)"})).unwrap();
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(app.session.active().unwrap().doc, edited);
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(app.session.active().unwrap().doc, original);
    app.run("edit.redo", json!({})).unwrap();
    assert_eq!(app.session.active().unwrap().doc, edited);
}

#[test]
fn interleaving_or_wrong_document_never_joins_an_agent_undo_step() {
    let (mut app, _, shape, id) = request_app();
    run_for(&mut app, id, "shape.move", json!({"ids": [shape], "dx": 25})).unwrap();
    app.run("shape.move", json!({"ids": [shape], "dy": 10})).unwrap();
    let after_person = app.session.active().unwrap().doc.clone();
    assert!(run_for(&mut app, id, "shape.move", json!({"ids": [shape], "dx": 1})).unwrap_err().contains("changed"));
    assert_eq!(app.session.active().unwrap().doc, after_person);
    app.run("file.new", json!({})).unwrap();
    assert!(run_for(&mut app, id, "slide.new", json!({})).unwrap_err().contains("switch back"));
}

#[test]
fn explicit_deck_scope_and_document_history_are_isolated() {
    let (mut app, hub, _, _) = request_app();
    let uid = app.session.active().unwrap().uid;
    let r = app.run(COMMAND, json!({"instruction": "restyle", "scope": "deck"})).unwrap()["id"].as_u64().unwrap();
    assert_eq!(hub.request(r).unwrap().selection["kind"], "deck");
    assert_eq!(hub.request(r).unwrap().document["id"], uid);
    app.run(COMMAND, json!({})).unwrap();
    app.run("file.new", json!({})).unwrap();
    sync_document(&mut app);
    assert!(app.ask_agent.bubble.is_none());
    assert!(app.ask_agent.history.is_empty());
    app.session.set_active(0);
    sync_document(&mut app);
    assert_eq!(app.ask_agent.history[0], "restyle");
    app.session.close_document(0);
    sync_document(&mut app);
    assert_eq!(hub.request(r).unwrap().state, State::Cancelled);
}

#[test]
fn a_late_reply_cannot_steal_a_reassigned_request() {
    let hub = listening();
    let id = hub.ask("move", sel(""), json!({})).unwrap();
    hub.listen(1, ms(1), || true).unwrap();
    hub.disconnect(1);
    hub.listen(2, ms(1), || true).unwrap();
    assert!(hub.reply(3, id, "Late", true).is_err());
    assert!(hub.owns(2, id));
    hub.reply(2, id, "Done", true).unwrap();
}

#[test]
fn grouped_undo_obeys_a_saturated_history_limit() {
    let (mut app, _, shape, id) = request_app();
    app.session.active_mut().unwrap().history.limit = 1;
    app.session.active_mut().unwrap().history.undo.clear();
    app.run("shape.move", json!({"ids": [shape], "dy": 10})).unwrap();
    let before = app.session.active().unwrap().doc.clone();
    run_for(&mut app, id, "shape.move", json!({"ids": [shape], "dx": 10})).unwrap();
    run_for(&mut app, id, "shape.fill", json!({"ids": [shape], "color": "#123456"})).unwrap();
    assert_eq!(app.session.active().unwrap().history.undo.len(), 1);
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(app.session.active().unwrap().doc, before);
    assert!(app.session.active().unwrap().history.undo.is_empty());
}

#[test]
fn forgotten_requests_release_their_snapshots_without_switching_documents() {
    let (mut app, hub, shape, first) = request_app();
    for n in 0..60 {
        let id = if n == 0 {
            first
        } else {
            let id = app.run(COMMAND, json!({"instruction": "move"})).unwrap()["id"].as_u64().unwrap();
            hub.listen(1, ms(1), || true).unwrap();
            id
        };
        run_for(&mut app, id, "shape.move", json!({"ids": [shape], "dx": 1})).unwrap();
        hub.reply(1, id, "Done", true).unwrap();
        hub.dismiss(id);
        sync_document(&mut app);
        assert!(app.ask_agent.edits.is_empty());
    }
}

#[test]
fn escape_for_a_dialog_and_ime_typing_never_cancel_a_request() {
    let hub = listening();
    let (mut h, shape) = window(Some(hub.clone()));
    h.state_mut().run("edit.select", json!({"ids": [shape]})).unwrap();
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    h.state_mut().dialog = Some(crate::dialogs::Dialog::new("quit"));
    key(&mut h, Key::Escape, Modifiers::NONE);
    assert!(h.state().ask_agent.bubble.is_some(), "Escape for the dialog leaves the bubble");
    h.state_mut().dialog = None;
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    h.event(egui::Event::Text("rewrite".into()));
    steps(&mut h);
    key(&mut h, Key::Enter, Modifiers::NONE);
    let request = hub.summaries()[0].id;
    typed_on_slide(h.state_mut(), &[egui::Event::Ime(egui::ImeEvent::Commit("clay".into()))]);
    assert!(h.state().ask_agent.bubble.is_none());
    assert_eq!(hub.request(request).unwrap().state, State::Waiting);
}

#[test]
fn actions_carry_metadata_and_their_declared_scope() {
    let (mut app, hub, shape, _) = request_app();
    app.run("edit.select", json!({"ids": [shape]})).unwrap();
    for a in crate::agent_actions::ACTIONS {
        let id = app.run(COMMAND, json!({"action": a.id})).unwrap()["id"].as_u64().unwrap();
        let r = hub.request(id).unwrap();
        assert_eq!(r.action["id"], a.id);
        assert_eq!(r.action["label"], a.label);
        assert_eq!(r.apply, a.apply);
        assert_eq!(r.instruction, a.instruction);
        assert_eq!(
            r.selection["kind"],
            match a.scope {
                "deck" => "deck",
                "slide" => "slide",
                _ => "shapes",
            }
        );
        hub.cancel(id);
    }
    let inspected = app.ask_agent.inspect();
    assert_eq!(inspected["actions"].as_array().unwrap().len(), 32);
    app.run("edit.select", json!({"ids": []})).unwrap();
    assert!(app.run(COMMAND, json!({"action": "text.shorten"})).is_err());
    assert!(app.run(COMMAND, json!({"action": "missing"})).is_err());
}

#[test]
fn keep_and_undo_review_only_the_requests_own_edits() {
    let (mut app, hub, shape, id) = request_app();
    let before = app.session.active().unwrap().doc.clone();
    run_for(&mut app, id, "shape.move", json!({"ids": [shape], "dx": 5})).unwrap();
    run_for(&mut app, id, "shape.fill", json!({"ids": [shape], "color": "#123456"})).unwrap();
    assert!(app.run(COMMAND, json!({"undo": id})).is_err(), "cannot review an open request");
    hub.reply(1, id, "Done", true).unwrap();
    app.run(COMMAND, json!({"undo": id})).unwrap();
    assert_eq!(app.session.active().unwrap().doc, before);
    assert_eq!(hub.request(id).unwrap().review, Some("undone"));
    app.run("edit.redo", json!({})).unwrap();
    let edited = app.session.active().unwrap().doc.clone();
    let next = app.run(COMMAND, json!({"instruction": "move"})).unwrap()["id"].as_u64().unwrap();
    hub.listen(1, ms(1), || true).unwrap();
    run_for(&mut app, next, "shape.move", json!({"ids": [shape], "dy": 5})).unwrap();
    hub.reply(1, next, "Done", true).unwrap();
    let kept = app.session.active().unwrap().doc.clone();
    app.run(COMMAND, json!({"keep": next})).unwrap();
    assert_eq!(app.session.active().unwrap().doc, kept);
    assert_eq!(hub.request(next).unwrap().review, Some("kept"));
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(app.session.active().unwrap().doc, edited, "regular Undo still works after Keep");
}

#[test]
fn result_undo_cannot_erase_later_human_work_and_reply_only_cannot_edit() {
    let (mut app, hub, shape, id) = request_app();
    run_for(&mut app, id, "shape.move", json!({"ids": [shape], "dx": 5})).unwrap();
    hub.reply(1, id, "Done", true).unwrap();
    app.run("shape.move", json!({"ids": [shape], "dy": 5})).unwrap();
    let after_person = app.session.active().unwrap().doc.clone();
    assert!(app.run(COMMAND, json!({"undo": id})).unwrap_err().contains("unrelated"));
    assert_eq!(app.session.active().unwrap().doc, after_person);
    let st = app.session.active().unwrap();
    let reply_only = hub.ask_with_action("explain", selection(st), document(st), Value::Null, "replyOnly").unwrap();
    hub.listen(1, ms(1), || true).unwrap();
    assert!(run_for(&mut app, reply_only, "shape.move", json!({"ids": [shape], "dx": 5})).unwrap_err().contains("reply-only"));
    run_for(&mut app, reply_only, "slide.inspect", json!({})).unwrap();
    assert_eq!(app.session.active().unwrap().doc, after_person);
}

#[test]
fn bubble_menus_send_actions_and_keep_undo_buttons_work_headless() {
    let hub = listening();
    let (mut h, shape) = window(Some(hub.clone()));
    h.state_mut().run("edit.select", json!({"ids": [shape]})).unwrap();
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    h.get_by_label("Text").click();
    steps(&mut h);
    h.get_by_label("Shorten").click();
    steps(&mut h);
    let r = hub.listen(1, ms(1), || true).unwrap();
    assert_eq!(r.action["id"], "text.shorten");
    assert_eq!(r.selection["shapes"][0]["id"], shape);
    run_for(h.state_mut(), r.id, "shape.move", json!({"ids": [shape], "dx": 5})).unwrap();
    hub.reply(1, r.id, "Done", true).unwrap();
    steps(&mut h);
    assert!(h.query_by_label("Keep").is_some());
    h.get_by_label("Undo").click();
    steps(&mut h);
    assert_eq!(hub.request(r.id).unwrap().review, Some("undone"));
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    h.get_by_label("Translate").click();
    steps(&mut h);
    h.get_by_label("French").click();
    steps(&mut h);
    assert_eq!(hub.listen(1, ms(1), || true).unwrap().action["id"], "translate.fr");
}

#[test]
fn rewrite_notes_and_table_cells_restore_the_captured_range_as_one_undo() {
    let (mut app, hub, _, first) = request_app();
    hub.reply(1, first, "No edits", true).unwrap();
    app.run("slide.notes", json!({"text": "Before notes"})).unwrap();
    let table = app.run("insert.table", json!({"rows": 1, "cols": 1})).unwrap()["id"].as_u64().unwrap();
    app.run("text.set", json!({"id": table, "cell": [0, 0], "text": "Before cell"})).unwrap();
    for notes in [true, false] {
        app.run("text.edit", if notes { json!({"notes": true}) } else { json!({"id": table, "cell": [0, 0]}) }).unwrap();
        app.run("text.select", json!({"anchor": [0, 0], "caret": [0, 6]})).unwrap();
        let id = app.run(COMMAND, json!({"action": "text.shorten"})).unwrap()["id"].as_u64().unwrap();
        let r = hub.listen(1, ms(1), || true).unwrap();
        assert_eq!(r.id, id);
        assert_eq!(r.selection["text"]["text"], "Before");
        assert_eq!(r.selection["text"]["notes"], notes);
        assert!(r.instruction.contains("text.get` cannot read notes or table cells"));
        let before = app.session.active().unwrap().doc.clone();
        let history = app.session.active().unwrap().history.undo.len();
        run_for(
            &mut app,
            id,
            "text.edit",
            if notes { json!({"notes": true}) } else { json!({"id": r.selection["text"]["shape"], "cell": r.selection["text"]["cell"]}) },
        )
        .unwrap();
        run_for(&mut app, id, "text.select", json!({"anchor": r.selection["text"]["anchor"], "caret": r.selection["text"]["caret"]})).unwrap();
        run_for(&mut app, id, "text.insert", json!({"text": "After"})).unwrap();
        assert_eq!(app.session.active().unwrap().history.undo.len(), history + 1);
        hub.reply(1, id, "Done", true).unwrap();
        app.run(COMMAND, json!({"undo": id})).unwrap();
        assert_eq!(app.session.active().unwrap().doc, before);
    }
}

#[test]
fn idle_presence_has_an_expiry_repaint_and_owned_work_survives_record_eviction() {
    let hub = AgentHub::new(7979, None);
    assert!(hub.presence_refresh_after().is_none());
    hub.listen(1, ms(0), || true);
    let delay = hub.presence_refresh_after().unwrap();
    assert!(delay > Duration::ZERO && delay <= ms(PRESENCE_MS as u64));
    let id = hub.ask("work", sel(""), json!({})).unwrap();
    hub.listen(1, ms(0), || true).unwrap();
    assert!(hub.presence_refresh_after().is_none(), "owned work waits for connection events, not a grace timer");
    for conn in 2..MAX_AGENTS as u64 + 4 {
        hub.listen(conn, ms(0), || true);
    }
    assert!(!hub.lock().agents.iter().any(|(conn, _)| *conn == 1), "the bounded recent-connection record was evicted");
    for (_, seen) in &mut hub.lock().agents {
        *seen -= PRESENCE_MS + 1_000.0;
    }
    assert!(hub.agent_present(), "the still-open owner remains present even after its recent record was evicted");
    hub.disconnect(1);
    assert_eq!(hub.request(id).unwrap().state, State::Waiting);
    assert!(!hub.agent_present());
    assert!(hub.presence_refresh_after().is_none());
}

#[test]
fn choosing_deck_scope_returns_typing_to_the_instruction_box() {
    let hub = listening();
    let (mut h, shape) = window(Some(hub.clone()));
    h.state_mut().run("edit.select", json!({"ids": [shape]})).unwrap();
    h.state_mut().run(COMMAND, json!({})).unwrap();
    steps(&mut h);
    h.get_by_value("Current selection").click();
    steps(&mut h);
    h.get_by_label("Entire presentation").click();
    steps(&mut h);
    let before = h.state().session.active().unwrap().doc.clone();
    h.event(egui::Event::Text("Restyle the whole deck".into()));
    steps(&mut h);
    key(&mut h, Key::Enter, Modifiers::NONE);
    let request = hub.listen(1, ms(1), || true).expect("scope choice must keep typing in the instruction box");
    assert_eq!(request.selection["kind"], "deck");
    assert_eq!(request.instruction, "Restyle the whole deck");
    assert_eq!(h.state().session.active().unwrap().doc, before, "typing in Ask Agent does not edit the slide");
}
