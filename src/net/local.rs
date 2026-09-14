//! Small loopback HTTP backend for local app/API resources.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};

const MAX_REQUEST: usize = 16 * 1024;

pub struct LocalServer {
    stop: Arc<AtomicBool>,
    address: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl LocalServer {
    pub fn spawn() -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("freeweb-local-backend".into())
            .spawn(move || serve(listener, worker_stop))?;
        Ok(Self { stop, address, thread: Some(thread) })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(listener: TcpListener, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if peer.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
                    continue;
                }
                let _ = handle(&mut stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn handle(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    let mut request = vec![0u8; MAX_REQUEST];
    let n = stream.read(&mut request)?;
    let request_text = String::from_utf8_lossy(&request[..n]);
    let line = request_text
        .lines()
        .next()
        .unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let (status, body) = match (method, path) {
        ("GET", "/") => (200, r#"{"name":"FreeWeb","status":"ok"}"#),
        ("GET", "/api/health") => (200, r#"{"ok":true}"#),
        _ => (404, r#"{"error":"not found"}"#),
    };
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Not Found" },
        body.len()
    );
    stream.write_all(response.as_bytes())
}
