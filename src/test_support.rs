//! Shared test support: a minimal HTTP mock server plus wire-format JSON
//! fixtures. Compiled only for tests (`#[cfg(test)] mod test_support;`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// The answer to one mock request.
pub struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::with_content_type(status, "application/json", body)
    }

    pub fn plain(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::with_content_type(status, "text/plain", body)
    }

    fn with_content_type(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), content_type.to_owned())],
            body: body.into(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn write_to(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            reason(self.status),
            self.body.len()
        );
        for (name, value) in &self.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(&self.body)?;
        stream.flush()
    }
}

/// Either a canned response or a dropped connection (to exercise retries).
pub enum Action {
    Respond(Response),
    Close,
}

/// Routes on the request target (path + query, without the `/api/v1` base
/// path) and the number of requests served so far, so a handler can vary its
/// answer per call.
type Handler = Box<dyn Fn(usize, &str) -> Action + Send + Sync>;

/// A throwaway HTTP server on 127.0.0.1 serving one test.
pub struct MockServer {
    /// Base URL for `Client::with_base`, e.g. `http://127.0.0.1:49152/api/v1`.
    pub base: String,
    handler: Mutex<Option<Handler>>,
    calls: Mutex<Vec<String>>,
}

impl MockServer {
    pub fn start() -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
        let server = Arc::new(Self {
            base: format!("http://127.0.0.1:{port}/api/v1"),
            handler: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
        });
        let accept_loop = Arc::clone(&server);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let accept_loop = Arc::clone(&accept_loop);
                thread::spawn(move || accept_loop.serve(stream));
            }
        });
        server
    }

    pub fn set_handler(&self, handler: impl Fn(usize, &str) -> Action + Send + Sync + 'static) {
        *self.handler.lock().expect("handler lock") = Some(Box::new(handler));
    }

    /// Targets served so far, in request order, e.g.
    /// `["/packs/by-path/community/x?page=1", "/puzzles/11"]`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }

    fn serve(&self, stream: TcpStream) {
        let Ok(reader_stream) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reader_stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let Some(target) = request_line.split_whitespace().nth(1) else {
            return;
        };
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        if content_length > 0 {
            let mut body = vec![0; content_length];
            let _ = reader.read_exact(&mut body);
        }
        let (call, target) = {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.push(target.strip_prefix("/api/v1").unwrap_or(target).to_owned());
            (calls.len() - 1, calls.last().expect("just pushed").clone())
        };

        let action = match self.handler.lock().expect("handler lock").as_ref() {
            Some(handler) => handler(call, &target),
            None => Action::Respond(Response::plain(404, "no mock handler")),
        };
        if let Action::Respond(response) = action {
            let mut stream = stream;
            let _ = response.write_to(&mut stream);
            let _ = stream.shutdown(Shutdown::Both);
        }
        // Action::Close: no response, so the client sees a dropped connection.
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// A full success envelope wrapping `data`.
pub fn wire_detail(id: i64, puzzle_type: &str, design: &str, guest_locked: bool) -> String {
    let data = serde_json::json!({
        "puzzleid": id,
        "name": format!("p{id}"),
        "path": format!("/play/community/t1024/{id}"),
        "puzzletype": puzzle_type,
        "design": design,
        "guestlocked": guest_locked,
        "publisheddate": "2023-08-18T12:34:56Z"
    });
    serde_json::json!({ "status": "success", "meta": { "total": 0 }, "data": data }).to_string()
}

/// A pack object with `name` also used as slug/path tail.
pub fn wire_pack(name: &str) -> serde_json::Value {
    serde_json::json!({
        "packid": 1,
        "name": name,
        "fullslug": format!("community/{name}"),
        "path": format!("/play/community/{name}"),
        "numpuzzles": 7
    })
}

/// One puzzle entry of a listing.
pub fn wire_puzzle_item(id: i64) -> serde_json::Value {
    serde_json::json!({ "puzzleid": id, "name": format!("p{id}"), "position": 1 })
}

/// A pack object whose `numpuzzles` counter is set explicitly (the counter
/// includes levels that belong to no sub-pack).
pub fn wire_pack_counted(name: &str, num_puzzles: u64) -> serde_json::Value {
    let mut pack = wire_pack(name);
    pack["numpuzzles"] = serde_json::json!(num_puzzles);
    pack
}

/// One sub-pack entry of a listing.
pub fn wire_pack_item(fullslug: &str) -> serde_json::Value {
    let name = fullslug.rsplit('/').next().unwrap_or(fullslug);
    serde_json::json!({
        "packid": 2,
        "name": name,
        "fullslug": fullslug,
        "path": format!("/play/{fullslug}"),
        "numpuzzles": 3
    })
}

/// A full success envelope for one listing page.
pub fn wire_listing(
    pack: Option<serde_json::Value>,
    items: Vec<serde_json::Value>,
    total: u64,
) -> String {
    serde_json::json!({
        "status": "success",
        "meta": { "total": total },
        "data": { "pack": pack, "items": items }
    })
    .to_string()
}
