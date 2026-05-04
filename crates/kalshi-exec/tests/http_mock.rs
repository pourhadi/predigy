//! Minimal HTTP/1.1 mock server for executor integration tests.
//!
//! One request per connection; sends `Connection: close` so reqwest
//! re-dials for the next request. That's plenty for testing — we
//! never push more than ~10 requests through a test.
//!
//! Routes are matched on `(method, path-prefix)` via [`MockRoute`]
//! and respond with a registered status + JSON body. The
//! [`MockServer`] runs in a background tokio task; the test gets
//! back the bound URL.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub struct MockRoute {
    pub method: String,
    pub path_prefix: String,
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone, Default)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    #[allow(dead_code)] // exposed for tests that want to assert on query strings
    pub query: String,
    #[allow(dead_code)] // exposed for tests that want to assert on bodies
    pub body: String,
}

#[derive(Debug)]
pub struct MockServer {
    pub base_url: String,
    pub recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    /// Routes consulted in order; the first prefix-match wins.
    pub routes: Arc<Mutex<VecDeque<MockRoute>>>,
    _task: JoinHandle<()>,
}

impl MockServer {
    pub async fn start(routes: Vec<MockRoute>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let routes: Arc<Mutex<VecDeque<MockRoute>>> =
            Arc::new(Mutex::new(routes.into_iter().collect()));
        let recorded_clone = recorded.clone();
        let routes_clone = routes.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded_clone.clone();
                let routes = routes_clone.clone();
                tokio::spawn(async move {
                    handle_one_request(sock, recorded, routes).await;
                });
            }
        });
        Self {
            base_url,
            recorded,
            routes,
            _task: task,
        }
    }

    /// Replace the route table at runtime.
    pub fn set_routes(&self, routes: Vec<MockRoute>) {
        let mut g = self.routes.lock().unwrap();
        g.clear();
        g.extend(routes);
    }

    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().unwrap().clone()
    }
}

async fn handle_one_request(
    mut sock: tokio::net::TcpStream,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    routes: Arc<Mutex<VecDeque<MockRoute>>>,
) {
    // Read until we have headers + (content-length) body.
    let mut buf = Vec::with_capacity(4096);
    let mut header_end: Option<usize> = None;
    let mut tmp = [0u8; 1024];
    while header_end.is_none() {
        let n = match sock.read(&mut tmp).await {
            // 0 bytes = EOF, Err = peer closed; both terminate the request.
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_header_end(&buf) {
            header_end = Some(idx);
        }
    }
    let header_end = header_end.unwrap();
    let header_str = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_str.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path_and_query = parts.next().unwrap_or("");
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (path_and_query.to_string(), String::new()),
    };

    let mut content_length: usize = 0;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("Content-Length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    // Body might already be in `buf` past header_end.
    let body_start = header_end;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = sock.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    let body_str = String::from_utf8_lossy(&body).to_string();

    recorded.lock().unwrap().push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        query,
        body: body_str,
    });

    let route = {
        let g = routes.lock().unwrap();
        g.iter()
            .find(|r| r.method.eq_ignore_ascii_case(&method) && path.starts_with(&r.path_prefix))
            .cloned()
    };
    let (status, body) = match route {
        Some(r) => (r.status, r.body),
        None => (
            404,
            format!(r#"{{"error":"no route for {method} {path}"}}"#),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status} {phrase}\r\nContent-Length: {len}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
        phrase = status_phrase(status),
        len = body.len(),
        body = body
    );
    let _ = sock.write_all(response.as_bytes()).await;
    let _ = sock.shutdown().await;
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

fn status_phrase(status: u16) -> &'static str {
    match status {
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        // 200 OK and any unmapped status default to OK — we never
        // assert on the reason phrase, only the numeric status.
        _ => "OK",
    }
}
