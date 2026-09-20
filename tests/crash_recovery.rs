#![cfg(not(windows))]
mod common;
use std::io::Write;
use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::Duration;

use common::*;

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn send(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if response.windows(4).any(|w| w == b"\r\n\r\n") {
                    if let Some(idx) = response.windows(2).position(|w| w == b"\r\n") {
                        let _ = idx;
                    }
                    if headers_complete(&response) {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&response).to_string()
}

use std::io::Read;
fn headers_complete(bytes: &[u8]) -> bool {
    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
        let head = String::from_utf8_lossy(&bytes[..pos]);
        for line in head.lines() {
            if line.to_lowercase().starts_with("content-length:") {
                if let Some(n) = line.split(':').nth(1).and_then(|v| v.trim().parse::<usize>().ok()) {
                    return bytes.len() >= pos + 4 + n;
                }
            }
        }
    }
    false
}

fn wait_ready(port: u16) {
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            std::thread::sleep(Duration::from_millis(50));
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("server never came up on {port}");
}

fn start(port: u16, dir: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_server"))
        .args(["--listen", &format!("127.0.0.1:{port}"), "--data-dir", dir])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn state_survives_hard_kill() {
    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "bfw-crash-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let mut child = start(port, dir.to_str().unwrap());
    wait_ready(port);
    let health = send(port, "GET /api/health HTTP/1.0\r\n\r\n");
    assert!(health.contains("200"));
    let seeded = send(port, "GET /api/formats HTTP/1.0\r\n\r\n");
    assert!(seeded.contains("img"));

    let put = format!(
        "PUT /api/samples HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nIdempotency-Key: crash-key\r\nConnection: close\r\n\r\n{}",
        sample_json().len(),
        sample_json()
    );
    let saved = send(port, &put);
    assert!(saved.contains("201"));

    child.kill().unwrap();
    child.wait().unwrap();

    let mut child2 = start(port, dir.to_str().unwrap());
    wait_ready(port);
    let samples = send(port, "GET /api/samples HTTP/1.0\r\n\r\n");
    assert!(samples.contains("crash-sample"), "sample missing after kill");
    let list = send(port, "GET /api/plans HTTP/1.0\r\n\r\n");
    assert!(list.contains("200"));
    child2.kill().unwrap();
    child2.wait().unwrap();
}

fn sample_json() -> String {
    let bytes = sample_bytes();
    let hex = bfw::util::to_hex(&bytes);
    format!(
        r#"{{"id":"crash-sample","rev":0,"format":{{"id":"img","version":1}},"name":"crash","hex":"{}","note":"","derived_from":null}}"#,
        hex
    )
}
