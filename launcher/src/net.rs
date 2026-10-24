//! Minimal blocking HTTP/TCP probes — a launcher only needs status codes and
//! port liveness, so no async runtime or HTTP client dependency.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// GET `path`, return the HTTP status code, or None on any failure.
pub fn http_status(host: &str, port: u16, path: &str, timeout: Duration) -> Option<u16> {
    let addr: SocketAddr = format!("{host}:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut head = [0u8; 1024];
    let n = stream.read(&mut head).ok()?;
    String::from_utf8_lossy(&head[..n])
        .split_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()
}

/// TCP connect probe (dsh readiness).
pub fn port_open(host: &str, port: u16) -> bool {
    let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() else { return false };
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// GET `path`, return the response body with headers stripped, or None.
/// HTTP/1.0 keeps llama-server from answering with chunked encoding.
pub fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Option<String> {
    let addr: SocketAddr = format!("{host}:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let (_, body) = text.split_once("\r\n\r\n")?;
    Some(body.to_string())
}

/// Find the PID with a LISTENING socket on `port` via `netstat -ano`.
pub fn pid_listening_on(port: u16) -> Option<u32> {
    let out = std::process::Command::new("netstat")
        .arg("-ano")
        .output()
        .ok()?
        .stdout;
    let text = String::from_utf8_lossy(&out);
    let needle = format!(":{port} ");
    text.lines()
        .filter(|l| l.contains(&needle) && l.to_uppercase().contains("LISTENING"))
        .filter_map(|l| l.split_whitespace().last()?.parse::<u32>().ok())
        .next()
}
