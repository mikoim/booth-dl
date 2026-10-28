//! A tiny HTTP/1.1 server for tests. One connection per request.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug, Clone)]
pub struct Request {
    /// Path including the query string.
    pub path: String,
    /// Header names are lowercase.
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    cut_after: Option<usize>,
}

impl Reply {
    pub fn new(status: u16) -> Self {
        Reply {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            cut_after: None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// Announces the full `Content-Length` but closes the connection after `n` body bytes.
    pub fn cut_after(mut self, n: usize) -> Self {
        self.cut_after = Some(n);
        self
    }
}

pub struct TestServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl TestServer {
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let log = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = Arc::clone(&handler);
                let log = Arc::clone(&log);
                thread::spawn(move || serve(stream, handler.as_ref(), &log));
            }
        });
        TestServer { url, requests }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(stream: TcpStream, handler: &dyn Fn(&Request) -> Reply, log: &Mutex<Vec<Request>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_string());
        }
    }
    let request = Request { path, headers };
    log.lock().unwrap().push(request.clone());

    let reply = handler(&request);
    let mut out = stream;
    let mut head = format!("HTTP/1.1 {} Status\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    ));
    let body = match reply.cut_after {
        Some(n) => &reply.body[..n.min(reply.body.len())],
        None => &reply.body[..],
    };
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(body);
    let _ = out.flush();
}
