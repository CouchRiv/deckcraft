//! Loopback JSON-lines control server: one request per line, one reply per line.
//! This is the transport the MCP server (`deckcraft-cli mcp --connect`) wraps.
//!
//! The first request on a connection must be `{"method": "auth", "params": {"token": …}}` with
//! the token from `deckcraft_mcp::auth` (published to `auth::token_path(port)`); anything else,
//! including a line that isn't JSON (an HTTP request from a web page), gets an error reply and
//! closes the connection.
//!
//! Ask Agent's `agent.listen` and `agent.reply` are answered here, after that check, on the
//! connection's own thread, from the window's [`AgentHub`]: an agent waiting for the next request
//! never holds up the window or other connections.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use deckcraft_mcp::auth;
use deckcraft_ui_egui::ask_agent::{DEFAULT_LISTEN_MS, MAX_LISTEN_MS};
use deckcraft_ui_egui::{AgentHub, ControlRequest};
use serde_json::{Value, json};

const AUTH_FIRST: &str = "authenticate first: {\"method\":\"auth\",\"params\":{\"token\":…}}";

/// Start the control server on `port`; `None` when it can't (no token, port taken, token file
/// not written): the control channel, and with it Ask Agent, is then off. `agents` is the
/// window's Ask Agent queue.
pub fn start(port: u16, ctx: egui::Context, agents: AgentHub) -> Option<Receiver<ControlRequest>> {
    let token = match auth::env_token().map_or_else(auth::new_token, Ok) {
        Ok(t) => t,
        Err(e) => {
            log::error!("control server: no token: {e}");
            return None;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            log::error!("control server failed to bind 127.0.0.1:{port}: {e}");
            return None;
        }
    };
    // The port actually bound (`--control 0` picks a free one).
    let port = listener.local_addr().map_or(port, |a| a.port());
    let token_file = match auth::publish_token(port, &token) {
        Ok(path) => path,
        Err(e) => {
            log::error!("control server: writing the token file: {e}");
            return None;
        }
    };
    log::info!("control server listening on 127.0.0.1:{port} (token in {})", token_file.display());
    agents.set_port(port);
    Some(serve_on(listener, token, ctx, agents))
}

fn serve_on(listener: TcpListener, token: String, ctx: egui::Context, agents: AgentHub) -> Receiver<ControlRequest> {
    let (tx, rx) = channel::<ControlRequest>();
    let token = Arc::new(token);
    std::thread::spawn(move || {
        // Each connection's number, for Ask Agent's presence.
        let mut conn = 0u64;
        for stream in listener.incoming().flatten() {
            conn = conn.wrapping_add(1);
            let (tx, ctx, token, agents) = (tx.clone(), ctx.clone(), Arc::clone(&token), agents.clone());
            std::thread::spawn(move || serve(stream, &token, tx, ctx, &agents, conn));
        }
    });
    rx
}

/// Whether the client is still there (it hasn't closed its end of the connection): asked while
/// `agent.listen` waits and before it hands a request out, so a request never goes to a client
/// that has gone.
fn peer_open(s: &TcpStream) -> bool {
    if s.set_nonblocking(true).is_err() {
        return true;
    }
    let mut b = [0u8; 1];
    let r = s.peek(&mut b);
    let _ = s.set_nonblocking(false);
    match r {
        Ok(0) => false,
        Ok(_) => true,
        Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
    }
}

/// While an engine command waits for the UI, disconnects still release ownership promptly.
fn wait_for_ui(rx: &Receiver<Value>, probe: &TcpStream, agents: &AgentHub, conn: u64, owns_request: bool) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut input_closed = false;
    loop {
        if !input_closed && !peer_open(probe) {
            agents.disconnect(conn);
            if owns_request {
                return None;
            }
            // Generic clients may close only their write half and still await the response.
            input_closed = true;
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Some(json!({"ok": false, "error": "timeout"}));
        };
        match rx.recv_timeout(left.min(Duration::from_millis(250))) {
            Ok(reply) => return Some(reply),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Some(json!({"ok": false, "error": "control channel closed"})),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// `agent.listen {timeoutMs?}` and `agent.reply {id, text, done?}` (Ask Agent) for connection
/// `conn`: the reply, and the request handed out (to put back if the reply can't be written).
fn agent_method(agents: &AgentHub, conn: u64, method: &str, p: &Value, probe: &TcpStream) -> (Value, Option<u64>) {
    let err = |e: &str| json!({"ok": false, "error": e});
    if method == "agent.listen" {
        if agents.working_for(conn).is_some() {
            return (err("finish the current Ask Agent request before listening again"), None);
        }
        let ms = match p.get("timeoutMs") {
            None | Some(Value::Null) => DEFAULT_LISTEN_MS,
            Some(v) => match v.as_f64().filter(|ms| ms.is_finite() && *ms >= 0.0) {
                Some(ms) => (ms as u64).min(MAX_LISTEN_MS),
                None => {
                    return (err("`timeoutMs` is a number of milliseconds (at most 60000)"), None);
                }
            },
        };
        return match agents.listen(conn, Duration::from_millis(ms), || peer_open(probe)) {
            Some(r) => (json!({"ok": true, "result": r.to_json()}), Some(r.id)),
            None => (json!({"ok": true, "result": {"none": true}}), None),
        };
    }
    let Some(id) = p.get("id").and_then(Value::as_u64) else {
        return (err("missing `id` (the request's id from agent.listen)"), None);
    };
    let Some(text) = p.get("text").and_then(Value::as_str) else {
        return (err("missing `text` (what to show in the window)"), None);
    };
    let done = p.get("done").and_then(Value::as_bool).unwrap_or(true);
    match agents.reply(conn, id, text, done) {
        Ok(v) => (json!({"ok": true, "result": v}), None),
        Err(e) => (err(&e), None),
    }
}

/// The reply to a request line that arrives before the connection has authenticated, and whether
/// the connection is now authenticated (when not, it is closed).
fn authenticate(line: &str, token: &str) -> (Value, bool) {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return (json!({"ok": false, "error": AUTH_FIRST}), false);
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let is_auth = msg.get("method").and_then(Value::as_str) == Some("auth");
    let given = msg.get("params").and_then(|p| p.get("token")).and_then(Value::as_str);
    match given {
        Some(t) if is_auth && auth::tokens_match(t, token) => (json!({"id": id, "ok": true, "result": {"authenticated": true}}), true),
        _ if is_auth => (json!({"id": id, "ok": false, "error": "wrong token"}), false),
        _ => (json!({"id": id, "ok": false, "error": AUTH_FIRST}), false),
    }
}

fn serve(stream: TcpStream, token: &str, tx: Sender<ControlRequest>, ctx: egui::Context, agents: &AgentHub, conn: u64) {
    let Ok(read) = stream.try_clone() else { return };
    let Ok(probe) = stream.try_clone() else {
        return;
    };
    let mut out = stream;
    let mut authenticated = false;
    for line in BufReader::new(read).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if !authenticated {
            let (reply, ok) = authenticate(&line, token);
            if writeln!(out, "{reply}").is_err() || !ok {
                break;
            }
            authenticated = true;
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                // A repeated `auth` is answered here, never dispatched to the UI.
                if method == "auth" {
                    if writeln!(out, "{}", json!({"id": id, "ok": true, "result": {"authenticated": true}})).is_err() {
                        break;
                    }
                    continue;
                }
                let mut params = msg.get("params").cloned().unwrap_or(json!({}));
                // Ask Agent: answered here, never on the UI thread.
                if method == "agent.listen" || method == "agent.reply" {
                    let (mut r, taken) = agent_method(agents, conn, &method, &params, &probe);
                    if let Some(o) = r.as_object_mut() {
                        o.insert("id".into(), id);
                    }
                    if writeln!(out, "{r}").is_err() {
                        // The agent never got it: back in the queue for the next one.
                        if let Some(t) = taken {
                            agents.requeue(t);
                        }
                        break;
                    }
                    continue;
                }
                if let Some(request) = agents.working_for(conn) {
                    let why = if method == "engine.execute" {
                        if !params.is_object() {
                            Some("engine.execute params must be an object")
                        } else if params.get("request").and_then(Value::as_u64).is_some_and(|id| id != request) {
                            Some("command belongs to another Ask Agent request")
                        } else {
                            params["request"] = json!(request);
                            None
                        }
                    } else if matches!(method.as_str(), "engine.commands" | "document.inspect" | "ui.inspect" | "ui.render" | "ui.screenshot") {
                        None
                    } else {
                        Some("while handling Ask Agent, edit through engine.execute so one Undo covers the request")
                    };
                    if let Some(why) = why {
                        if writeln!(out, "{}", json!({"id": id, "ok": false, "error": why})).is_err() {
                            break;
                        }
                        continue;
                    }
                }
                if let Some(request) = params.get("request")
                    && !request.as_u64().is_some_and(|r| agents.owns(conn, r))
                {
                    if writeln!(
                        out,
                        "{}",
                        json!({"id": id, "ok": false, "error": "listen for this Ask Agent request on this connection before editing"})
                    )
                    .is_err()
                    {
                        break;
                    }
                    continue;
                }
                let owns_request = agents.working_for(conn).is_some();
                let agent_connection = params.get("request").is_some().then_some(conn);
                let (mut req, rrx) = ControlRequest::new(method, params);
                req.agent_connection = agent_connection;
                if tx.send(req).is_err() {
                    break;
                }
                ctx.request_repaint();
                let Some(mut r) = wait_for_ui(&rrx, &probe, agents, conn, owns_request) else {
                    break;
                };
                if let Some(o) = r.as_object_mut() {
                    o.insert("id".into(), id);
                }
                r
            }
            Err(e) => json!({"ok": false, "error": format!("bad JSON: {e}")}),
        };
        if writeln!(out, "{reply}").is_err() {
            break;
        }
    }
    // Gone: it no longer counts as an agent, and a request it took without answering goes back in
    // the queue.
    agents.disconnect(conn);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef";

    #[test]
    fn only_the_right_token_authenticates() {
        let (reply, ok) = authenticate(&json!({"id": 1, "method": "auth", "params": {"token": TOKEN}}).to_string(), TOKEN);
        assert!(ok);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["id"], 1);
        for line in [
            json!({"id": 1, "method": "auth", "params": {"token": "wrong"}}).to_string(),
            json!({"id": 1, "method": "auth", "params": {}}).to_string(),
            json!({"id": 1, "method": "auth"}).to_string(),
            json!({"id": 1, "method": "app.save", "params": {"path": "/tmp/x.pptx", "token": TOKEN}}).to_string(),
        ] {
            let (reply, ok) = authenticate(&line, TOKEN);
            assert!(!ok, "{line}");
            assert_eq!(reply["ok"], false, "{line}");
        }
    }

    #[test]
    fn an_http_request_is_refused() {
        // The first line a browser sends for `fetch("http://127.0.0.1:PORT", {method: "POST", body})`.
        let (reply, ok) = authenticate("POST / HTTP/1.1", TOKEN);
        assert!(!ok);
        assert_eq!(reply["ok"], false);
    }

    /// Over a real socket: an unauthenticated request is refused and the connection closed; after
    /// `auth`, requests reach the UI channel.
    #[test]
    fn requests_reach_the_ui_only_after_auth() {
        use std::io::Read as _;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = channel();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                serve(stream.unwrap(), TOKEN, tx.clone(), egui::Context::default(), &AgentHub::new(0, None), 1);
            }
        });

        // No auth: refused, closed, nothing dispatched.
        let mut s = TcpStream::connect(addr).unwrap();
        writeln!(s, "{}", json!({"id": 1, "method": "app.quit"})).unwrap();
        let mut rest = String::new();
        s.read_to_string(&mut rest).unwrap();
        assert!(rest.contains("authenticate first"), "{rest}");
        assert!(rx.try_recv().is_err());

        // Auth, then a request the UI answers.
        let s = TcpStream::connect(addr).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        writeln!(w, "{}", json!({"id": 1, "method": "auth", "params": {"token": TOKEN}})).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert!(line.contains("\"authenticated\":true"), "{line}");
        writeln!(w, "{}", json!({"id": 2, "method": "ui.inspect"})).unwrap();
        let req = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(req.method, "ui.inspect");
        let _ = req.reply.send(json!({"ok": true, "result": {}}));
        line.clear();
        r.read_line(&mut line).unwrap();
        assert!(line.contains("\"id\":2"), "{line}");
        drop((w, r));
        server.join().unwrap();
    }

    // Ask Agent.

    use deckcraft_ui_egui::ask_agent::State;
    use std::time::Instant;

    /// A server on a free port with its Ask Agent queue.
    fn boot() -> (u16, Receiver<ControlRequest>, AgentHub) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let agents = AgentHub::new(port, None);
        (port, serve_on(listener, TOKEN.to_string(), egui::Context::default(), agents.clone()), agents)
    }

    /// A client connection: one line out, one line back.
    struct Conn(BufReader<TcpStream>, TcpStream);

    impl Conn {
        fn open(port: u16) -> Self {
            let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            Conn(BufReader::new(s.try_clone().unwrap()), s)
        }
        fn authed(port: u16) -> Self {
            let mut c = Self::open(port);
            assert_eq!(c.call(&json!({"id": 0, "method": "auth", "params": {"token": TOKEN}}))["ok"], true);
            c
        }
        fn send(&mut self, v: &Value) {
            writeln!(self.1, "{v}").unwrap();
        }
        fn recv(&mut self) -> Value {
            let mut line = String::new();
            self.0.read_line(&mut line).unwrap();
            serde_json::from_str(&line).unwrap_or(Value::Null)
        }
        fn call(&mut self, v: &Value) -> Value {
            self.send(v);
            self.recv()
        }
        fn closed(&mut self) -> bool {
            let mut line = String::new();
            matches!(self.0.read_line(&mut line), Ok(0) | Err(_))
        }
    }

    fn shapes_sel() -> Value {
        json!({"kind": "shapes", "slide": {"index": 0, "id": 256, "title": "Kilns"}, "shapes": [{"id": 258, "kind": "shape", "text": "Kilns fire clay."}]})
    }

    fn until(f: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !f() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn agent_listen_hands_out_the_next_request_without_the_window() {
        let (port, rx, agents) = boot();
        let id = agents.ask("tighten the wording", shapes_sel(), json!({"title": "Kilns.pptx", "slides": 3})).unwrap();
        let mut c = Conn::authed(port);
        let r = c.call(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 1000}}));
        assert_eq!((r["ok"].clone(), r["id"].clone()), (json!(true), json!(1)), "{r}");
        assert_eq!(r["result"]["id"], id);
        assert_eq!(r["result"]["instruction"], "tighten the wording");
        assert_eq!(r["result"]["selection"]["shapes"][0]["id"], 258);
        assert_eq!(r["result"]["document"]["title"], "Kilns.pptx");
        assert!(rx.try_recv().is_err(), "agent.listen never reaches the UI thread");
        // Finish the one outstanding request, then `none` after the timeout.
        assert_eq!(c.call(&json!({"id": 9, "method": "agent.reply", "params": {"id": id, "text": "Done"}}))["ok"], true);
        let t0 = Instant::now();
        assert_eq!(c.call(&json!({"id": 2, "method": "agent.listen", "params": {"timeoutMs": 50}}))["result"], json!({"none": true}));
        assert!(t0.elapsed() < Duration::from_secs(3));
        // Junk timeouts are refused, not turned into a long wait.
        for bad in [json!("soon"), json!(-5), json!(true)] {
            let r = c.call(&json!({"id": 3, "method": "agent.listen", "params": {"timeoutMs": bad}}));
            assert_eq!(r["ok"], false, "{bad}: {r}");
        }
    }

    #[test]
    fn a_waiting_agent_is_woken_by_a_request_and_does_not_hold_up_the_window() {
        let (port, rx, agents) = boot();
        let mut listener = Conn::authed(port);
        listener.send(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 20000}}));
        until(|| agents.agent_present());
        assert!(agents.agent_present(), "the window sees a listening agent");
        // While it waits another connection is answered.
        let mut other = Conn::authed(port);
        other.send(&json!({"id": 2, "method": "document.inspect"}));
        let req = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let _ = req.reply.send(json!({"ok": true, "result": {}}));
        assert_eq!(other.recv()["ok"], true, "the window still answers while an agent waits");
        let t0 = Instant::now();
        let id = agents.ask("make this slide punchier", shapes_sel(), json!({})).unwrap();
        assert_eq!(listener.recv()["result"]["id"], id);
        assert!(t0.elapsed() < Duration::from_secs(2), "woken by the request, not by the timeout");
    }

    #[test]
    fn agent_reply_shows_in_the_window_and_reports_cancelled_requests() {
        let (port, _rx, agents) = boot();
        let id = agents.ask("explain this chart", shapes_sel(), json!({})).unwrap();
        let mut c = Conn::authed(port);
        c.call(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 1000}}));
        let r = c.call(&json!({"id": 2, "method": "agent.reply", "params": {"id": id, "text": "Reading the chart…", "done": false}}));
        assert_eq!(r["result"]["done"], false, "{r}");
        let r = c.call(&json!({"id": 3, "method": "agent.reply", "params": {"id": id, "text": "Sales doubled in Q3."}}));
        assert_eq!(r["result"]["shown"], true, "{r}");
        let req = agents.request(id).unwrap();
        assert_eq!((req.state, req.reply.as_str()), (State::Answered, "Sales doubled in Q3."));
        // Errors the agent can act on.
        let e = |c: &mut Conn, p: Value| c.call(&json!({"id": 4, "method": "agent.reply", "params": p}))["error"].as_str().unwrap_or("").to_string();
        assert!(e(&mut c, json!({"text": "x"})).contains("missing `id`"));
        assert!(e(&mut c, json!({"id": id})).contains("missing `text`"));
        assert!(e(&mut c, json!({"id": 424242, "text": "x"})).contains("no Ask Agent request"));
        // The person cancelled: the agent is told, so it can stop.
        let id = agents.ask("add speaker notes", shapes_sel(), json!({})).unwrap();
        c.call(&json!({"id": 5, "method": "agent.listen", "params": {"timeoutMs": 1000}}));
        agents.cancel(id);
        let r = c.call(&json!({"id": 6, "method": "agent.reply", "params": {"id": id, "text": "Done"}}));
        assert_eq!(r["result"]["cancelled"], true, "{r}");
    }

    #[test]
    fn agent_methods_need_the_token() {
        let (port, _rx, agents) = boot();
        let id = agents.ask("summarize", shapes_sel(), json!({})).unwrap();
        for line in [
            json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 10}}),
            json!({"id": 1, "method": "agent.reply", "params": {"id": id, "text": "x"}}),
        ] {
            let mut c = Conn::open(port);
            assert!(c.call(&line)["error"].as_str().unwrap_or("").contains("authenticate first"), "{line}");
            assert!(c.closed(), "{line}");
        }
        assert_eq!(agents.request(id).unwrap().state, State::Waiting, "nothing was handed out or answered");
        assert!(!agents.agent_present(), "an unauthenticated client doesn't count as an agent");
    }

    /// A client that hangs up stops counting at once, gets no request, and a request it took
    /// without answering goes back in the queue.
    #[test]
    fn a_client_that_hung_up_stops_counting_and_gives_back_its_request() {
        let (port, _rx, agents) = boot();
        let mut gone = Conn::authed(port);
        gone.send(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 20000}}));
        until(|| agents.agent_present());
        let _ = gone.1.shutdown(std::net::Shutdown::Both);
        drop(gone);
        until(|| !agents.agent_present());
        assert!(!agents.agent_present(), "a client that left while listening no longer counts");
        let id = agents.ask("fix the typos", shapes_sel(), json!({})).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(agents.request(id).unwrap().state, State::Waiting, "kept for the next agent");
        // An agent takes it, then its connection closes before it answers.
        let mut taker = Conn::authed(port);
        assert_eq!(taker.call(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 1000}}))["result"]["id"], id);
        assert_eq!(agents.request(id).unwrap().state, State::Working);
        assert!(agents.agent_present(), "it holds a request on an open connection");
        drop(taker);
        until(|| agents.request(id).is_some_and(|r| r.state == State::Waiting));
        assert_eq!(agents.request(id).unwrap().state, State::Waiting, "back in the queue");
        assert!(!agents.agent_present());
        let mut next = Conn::authed(port);
        assert_eq!(next.call(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 1000}}))["result"]["id"], id);
    }

    #[test]
    fn raw_agent_commands_are_attributed_and_cannot_bypass_undo() {
        let (port, rx, agents) = boot();
        let id = agents.ask("move", shapes_sel(), json!({})).unwrap();
        let mut c = Conn::authed(port);
        assert_eq!(c.call(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 1}}))["result"]["id"], id);
        assert_eq!(c.call(&json!({"id": 2, "method": "agent.listen", "params": {"timeoutMs": 1}}))["ok"], false);
        assert_eq!(c.call(&json!({"id": 3, "method": "ui.text", "params": {"text": "bypass"}}))["ok"], false);
        c.send(&json!({"id": 4, "method": "engine.execute", "params": {"command": "shape.move", "params": {"dx": 10}}}));
        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(request.params["request"], id);
        request.reply.send(json!({"ok": true, "result": {}})).unwrap();
        assert_eq!(c.recv()["ok"], true);
    }

    #[test]
    fn disconnect_while_waiting_for_ui_requeues_and_stale_command_is_refused() {
        let (port, rx, agents) = boot();
        let mut app = deckcraft_ui_egui::SlideApp::new(deckcraft_engine::Session::with_new(), deckcraft_ui_egui::Services::default())
            .with_agent_hub(agents.clone());
        let id = app.run("app.askAgent", json!({"instruction": "move"})).unwrap()["id"].as_u64().unwrap();
        let mut c = Conn::authed(port);
        c.call(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 1}}));
        c.send(&json!({"id": 2, "method": "engine.execute", "params": {"command": "slide.new", "params": {}}}));
        let stale = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(stale.agent_connection.is_some());
        drop(c);
        until(|| !agents.agent_present());
        assert!(!agents.agent_present(), "disconnect is detected while waiting for the UI, not after 60 seconds");
        assert_eq!(agents.request(id).unwrap().state, State::Waiting);
        let mut next = Conn::authed(port);
        assert_eq!(next.call(&json!({"id": 3, "method": "agent.listen", "params": {"timeoutMs": 1}}))["result"]["id"], id);
        let before = app.session.active().unwrap().doc.clone();
        match deckcraft_ui_egui::control::handle(&mut app, &egui::Context::default(), &stale) {
            deckcraft_ui_egui::control::Outcome::Done(reply) => assert_eq!(reply["ok"], false),
            _ => panic!("unexpected screenshot"),
        }
        assert_eq!(app.session.active().unwrap().doc, before, "a queued command from the lost owner cannot run");
    }
    #[test]
    fn generic_half_closed_clients_still_receive_their_reply() {
        let (port, rx, agents) = boot();
        let mut c = Conn::authed(port);
        assert_eq!(c.call(&json!({"id": 1, "method": "agent.listen", "params": {"timeoutMs": 0}}))["result"]["none"], true);
        assert!(agents.agent_present());
        c.send(&json!({"id": 2, "method": "engine.execute", "params": {"command": "slide.new", "params": {}}}));
        c.1.shutdown(std::net::Shutdown::Write).unwrap();
        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        until(|| !agents.agent_present());
        assert!(!agents.agent_present(), "closed passive agents stop counting too");
        request.reply.send(json!({"ok": true, "result": {"generic": true}})).unwrap();
        assert_eq!(c.recv()["result"]["generic"], true, "closing only the write half still allows reading the response");
    }
}
