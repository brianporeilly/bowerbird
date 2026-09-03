//! A blocking mock HTTP server, in about a hundred lines of std.
//!
//! Deliberately not a mocking crate: the adapter is blocking, so a blocking
//! listener on a thread is both simpler and closer to what actually happens
//! than pulling an async runtime into the test tree would be. It also keeps
//! the promise that the suite runs with no network, no model, and no key.

#![allow(dead_code, unreachable_pub)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// One scripted response.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    /// A 200 carrying `content` as the assistant's message, which is the shape
    /// the adapter digs through.
    pub fn assistant(content: &str) -> Self {
        Self {
            status: 200,
            body: serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": content } }]
            })
            .to_string(),
        }
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self { status, body: body.to_owned() }
    }
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The request body as JSON. Panics if it is not JSON, which in a test is
    /// the right response to the adapter sending something unparseable.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("request body should be JSON")
    }

    /// The text of every message sent, concatenated. Handy for asserting that
    /// something did *not* leak into the prompt.
    pub fn prompt_text(&self) -> String {
        self.json()["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

pub struct MockServer {
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    /// Serves `replies` in order. The last one repeats, so a test only scripts
    /// as many responses as it actually cares about.
    pub fn new(replies: Vec<Reply>) -> Self {
        assert!(!replies.is_empty(), "a mock server needs at least one reply");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener.local_addr().expect("local addr");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = {
            let recorded = Arc::clone(&recorded);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                for (served, stream) in listener.incoming().enumerate() {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { break };
                    let reply = replies.get(served).or_else(|| replies.last());
                    let Some(reply) = reply else { break };
                    serve(stream, reply, &recorded);
                }
            })
        };

        Self { addr, recorded, shutdown, handle: Some(handle) }
    }

    /// An endpoint in the shape the config expects.
    pub fn endpoint(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().expect("recorder lock").clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests().len()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock the accept loop so the thread can observe the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Reads one request and writes one response. `Connection: close` keeps each
/// exchange on its own socket, so the recorded order is the request order.
///
/// The request is recorded *before* the response is written. The other way
/// round is a race: the client can observe its reply and assert on
/// `request_count()` while this thread has not pushed yet.
fn serve(stream: TcpStream, reply: &Reply, recorded: &Mutex<Vec<Recorded>>) -> Option<Recorded> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (name.trim().to_owned(), value.trim().to_owned());
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    let request = Recorded { headers, body: String::from_utf8_lossy(&body).into_owned() };
    recorded.lock().expect("recorder lock").push(request.clone());

    let mut stream = stream;
    let response = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    stream.write_all(response.as_bytes()).ok()?;
    stream.flush().ok()?;

    Some(request)
}
