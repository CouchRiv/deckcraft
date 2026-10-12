//! Where MCP tool calls end up: a control-channel method call.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::{Value, json};

/// Something that answers control-channel methods (`engine.execute`, `document.inspect`,
/// `ui.pointer`, `ui.render`, …). See `deckcraft_ui_egui::control` for the full list.
pub trait Backend {
    /// Call one method. `Ok` carries the `result`, `Err` the error message.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
    /// True when a real UI is attached (`ui.screenshot`, `ui.click`, dialogs… work).
    fn has_ui(&self) -> bool;
    /// Short human description ("headless", "connected to 127.0.0.1:7979").
    fn describe(&self) -> String;
}

/// A running DeckCraft app, reached through its loopback control port.
pub struct Remote {
    addr: String,
    /// Presented first on every connection (see [`crate::auth`]).
    token: Option<String>,
    /// The token was looked up rather than given: look it up again on every reconnect, so a
    /// restarted app (with a new token) is still reached.
    lookup_token: bool,
    conn: Option<(BufReader<TcpStream>, TcpStream)>,
    next_id: u64,
}

impl Remote {
    /// Connect to `addr` (`127.0.0.1:7979`), failing fast when nothing is listening. The token
    /// comes from [`crate::auth::token_for`]; without one the app refuses every request.
    pub fn connect(addr: &str) -> std::io::Result<Self> {
        let mut r = Self { addr: addr.to_string(), token: None, lookup_token: true, conn: None, next_id: 1 };
        r.reconnect()?;
        Ok(r)
    }

    /// [`Remote::connect`] with an explicit token (`None`: don't authenticate).
    pub fn connect_with_token(addr: &str, token: Option<String>) -> std::io::Result<Self> {
        let mut r = Self { addr: addr.to_string(), token, lookup_token: false, conn: None, next_id: 1 };
        r.reconnect()?;
        Ok(r)
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn reconnect(&mut self) -> std::io::Result<()> {
        self.conn = None;
        if self.lookup_token {
            self.token = crate::auth::token_for(&self.addr);
        }
        let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, format!("cannot resolve {}", self.addr));
        for sa in self.addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&sa, Duration::from_millis(800)) {
                Ok(s) => {
                    s.set_nodelay(true).ok();
                    // The app answers within 60 s (its own timeout); leave headroom.
                    s.set_read_timeout(Some(Duration::from_secs(90))).ok();
                    let read = s.try_clone()?;
                    self.conn = Some((BufReader::new(read), s));
                    if let Err(e) = self.authenticate() {
                        self.conn = None;
                        return Err(e);
                    }
                    return Ok(());
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Send `auth` with the token on a fresh connection; a refusal is a `PermissionDenied` error.
    fn authenticate(&mut self) -> std::io::Result<()> {
        let Some(token) = self.token.clone() else { return Ok(()) };
        let line = json!({"id": 0, "method": "auth", "params": {"token": token}}).to_string();
        let reply = self.exchange(&line)?;
        let v: Value = serde_json::from_str(reply.trim()).map_err(std::io::Error::other)?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            let why = v.get("error").and_then(Value::as_str).unwrap_or("refused");
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("authentication failed: {why}")))
        }
    }

    fn roundtrip(&mut self, line: &str) -> std::io::Result<String> {
        if self.conn.is_none() {
            self.reconnect()?;
        }
        self.exchange(line)
    }

    /// One line out, one line back, on the current connection.
    fn exchange(&mut self, line: &str) -> std::io::Result<String> {
        let Some((reader, writer)) = self.conn.as_mut() else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected"));
        };
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        let mut reply = String::new();
        if reader.read_line(&mut reply)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "control channel closed"));
        }
        Ok(reply)
    }
}

impl Backend for Remote {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        // One retry with a fresh connection (the app may have restarted).
        let reply = match self.roundtrip(&line) {
            Ok(r) => r,
            Err(_) => {
                self.conn = None;
                self.roundtrip(&line).map_err(|e| {
                    self.conn = None;
                    format!("DeckCraft app at {} is not reachable: {e}", self.addr)
                })?
            }
        };
        let v: Value = serde_json::from_str(reply.trim()).map_err(|e| format!("bad reply from app: {e}"))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(v.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(v.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_string())
        }
    }

    fn has_ui(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        format!("connected to the DeckCraft app at {}", self.addr)
    }
}
