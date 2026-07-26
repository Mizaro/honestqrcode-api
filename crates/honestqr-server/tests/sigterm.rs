#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn sigterm_drains_an_in_flight_request() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_honestqr-server"))
        .args(["--host", "127.0.0.1", "--port", "0", "--json-logs"])
        .env("RUST_LOG", "honestqr_server=info")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start server");
    let stdout = child.stdout.take().expect("server stdout");
    let (address_tx, address_rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut address_tx = Some(address_tx);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(address) = event
                .pointer("/fields/address")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            if let (Some(sender), Ok(address)) = (address_tx.take(), address.parse::<SocketAddr>())
            {
                let _ = sender.send(address);
            }
        }
    });
    let mut child = ChildGuard(child);
    let address = address_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server did not report its bound address");
    wait_until_ready(address, &mut child.0);

    let body = br#"{"data":{"kind":"text","value":"sigterm"},"render":{"format":"svg"}}"#;
    let split = body.len() / 2;
    let mut stream = TcpStream::connect(address).expect("connect render request");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    write!(
        stream,
        "POST /v1/qr HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("write request headers");
    stream.flush().expect("flush request headers");
    let interim = read_headers(&mut stream);
    assert!(
        interim.starts_with("HTTP/1.1 100"),
        "server did not admit the in-flight request: {interim}"
    );
    stream
        .write_all(&body[..split])
        .expect("write partial body");
    stream.flush().expect("flush partial request");

    let status = Command::new("kill")
        .args(["-TERM", &child.0.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success(), "kill command failed");

    stream
        .write_all(&body[split..])
        .expect("finish request after SIGTERM");
    stream.flush().expect("flush completed request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read drained response");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "in-flight request was not drained: {response}"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().expect("poll server") {
            assert!(status.success(), "server exited unsuccessfully: {status}");
            break;
        }
        assert!(Instant::now() < deadline, "server did not exit after drain");
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_headers(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("read response header");
        bytes.push(byte[0]);
        assert!(bytes.len() <= 16 * 1024, "response header is too large");
    }
    String::from_utf8(bytes).expect("HTTP header is UTF-8")
}

fn wait_until_ready(address: SocketAddr, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("poll startup") {
            panic!("server exited during startup: {status}");
        }
        if let Ok(mut stream) = TcpStream::connect(address) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
            let _ = stream.write_all(
                b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            );
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            if response.starts_with("HTTP/1.1 200") {
                return;
            }
        }
        assert!(Instant::now() < deadline, "server did not become ready");
        thread::sleep(Duration::from_millis(20));
    }
}
