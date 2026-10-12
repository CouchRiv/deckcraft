//! Loopback JSON-lines control server: one request per line, one reply per line.
//! This is the transport the MCP server (`deckcraft-cli mcp --connect`) wraps.
//!
//! The first request on a connection must be `{"method": "auth", "params": {"token": …}}` with
//! the token from `deckcraft_mcp::auth` (published to `auth::token_path(port)`); anything else,
//! including a line that isn't JSON (an HTTP request from a web page), gets an error reply and
//! closes the connection.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use deckcraft_mcp::auth;
use deckcraft_ui_egui::ControlRequest;
use serde_json::{Value, json};

const AUTH_FIRST: &str = "authenticate first: {\"method\":\"auth\",\"params\":{\"token\":…}}";

pub fn start(port: u16, ctx: egui::Context) -> Receiver<ControlRequest> {
    let (tx, rx) = channel::<ControlRequest>();
    let token = match auth::env_token().map_or_else(auth::new_token, Ok) {
        Ok(t) => t,
        Err(e) => {
            log::error!("control server: no token: {e}");
            return rx;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            log::error!("control server failed to bind 127.0.0.1:{port}: {e}");
            return rx;
        }
    };
    // The port actually bound (`--control 0` picks a free one).
    let port = listener.local_addr().map_or(port, |a| a.port());
    let token_file = match auth::publish_token(port, &token) {
        Ok(path) => path,
        Err(e) => {
            log::error!("control server: writing the token file: {e}");
            return rx;
        }
    };
    log::info!("control server listening on 127.0.0.1:{port} (token in {})", token_file.display());
    let token = Arc::new(token);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            let ctx = ctx.clone();
            let token = Arc::clone(&token);
            std::thread::spawn(move || serve(stream, &token, tx, ctx));
        }
    });
    rx
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

fn serve(stream: TcpStream, token: &str, tx: Sender<ControlRequest>, ctx: egui::Context) {
    let Ok(read) = stream.try_clone() else { return };
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
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let (req, rrx) = ControlRequest::new(method, params);
                if tx.send(req).is_err() {
                    break;
                }
                ctx.request_repaint();
                let mut r = rrx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| json!({"ok": false, "error": "timeout"}));
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
                serve(stream.unwrap(), TOKEN, tx.clone(), egui::Context::default());
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
}
