//! A tiny HTTP server for tests: answers each request on 127.0.0.1 with
//! whatever the test's handler returns.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

/// A request as the handler sees it.
pub struct Request {
    pub method: String,
    pub path: String,
    /// Headers, names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The body as JSON (null if it is not).
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_default()
    }
}

/// A reply: status, extra headers and body.
pub type Reply = (u16, Vec<(&'static str, String)>, String);

/// Starts a server on a free port; returns its base URL (`http://127.0.0.1:port`).
pub fn serve(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = Arc::clone(&handler);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
                let mut headers = Vec::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
                    }
                }
                let length = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let request = Request { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() };
                let (status, extra, body) = handler(&request);
                let mut response = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                if !extra.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-type")) {
                    response.push_str("Content-Type: application/json\r\n");
                }
                for (name, value) in extra {
                    response.push_str(name);
                    response.push_str(": ");
                    response.push_str(&value);
                    response.push_str("\r\n");
                }
                response.push_str("\r\n");
                response.push_str(&body);
                let mut stream = stream;
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
    base
}
