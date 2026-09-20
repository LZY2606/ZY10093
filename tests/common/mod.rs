#![allow(dead_code)]
use std::io::{Read, Write};
use std::net::TcpStream;

pub fn http(method: &str, port: u16, path: &str, body: Option<&str>, idem: Option<&str>) -> (u16, String, Vec<u8>) {
    let mut last_err = None;
    let mut stream = None;
    for _attempt in 0..20 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => { stream = Some(s); break; }
            Err(e) => { last_err = Some(e); std::thread::sleep(std::time::Duration::from_millis(50)); }
        }
    }
    let mut stream = stream.unwrap_or_else(|| panic!("connect to {port} failed: {:?}", last_err));
    let body = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if let Some(k) = idem {
        req.push_str(&format!("Idempotency-Key: {k}\r\n"));
    }
    req.push_str("Content-Type: application/json\r\n\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status: u16 = head.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let payload = raw[split + 4..].to_vec();
    let payload = if head.to_lowercase().contains("transfer-encoding: chunked") {
        dechunk(&payload)
    } else {
        payload
    };
    let text = String::from_utf8_lossy(&payload).to_string();
    (status, text, payload)
}

fn dechunk(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let line_end = input[i..].windows(2).position(|w| w == b"\r\n").map(|p| i + p);
        let le = match line_end { Some(v) => v, None => break };
        let size_str = String::from_utf8_lossy(&input[i..le]);
        let size = usize::from_str_radix(size_str.trim(), 16).unwrap_or(0);
        if size == 0 { break; }
        out.extend_from_slice(&input[le + 2..le + 2 + size]);
        i = le + 2 + size + 2;
    }
    out
}

pub fn spawn_server() -> (u16, std::process::Child) {
    let port = pick_port();
    let db = std::env::temp_dir().join(format!("wbench_test_{}_{}.db", std::process::id(), port));
    let db = db.to_string_lossy().to_string();
    let exe = env!("CARGO_BIN_EXE_server");
    let mut child = std::process::Command::new(exe)
        .args(["--listen", &format!("127.0.0.1:{port}"), "--db", &db])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn server");
    for _ in 0..100 {
        if http("GET", port, "/api/defs", None, None).0 == 200 {
            return (port, child);
        }
        if let Ok(Some(_)) = child.try_wait() {
            let mut err = String::new();
            use std::io::Read;
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            panic!("server exited early: {err}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("server did not start");
}

fn pick_port() -> u16 {
    use std::net::TcpListener;
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
