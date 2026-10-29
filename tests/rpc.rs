use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    thread,
};

use base64::{engine::general_purpose::STANDARD, Engine};
use electrum_lite::{rpc::Core, Chain};
use serde_json::{json, Value};
use tempfile::tempdir;

fn serve(body: String, status: &str) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let status = status.to_owned();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let n = stream.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let size: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + size {
                    break;
                }
            }
        }
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        // The client may stop reading as soon as its response-size guard trips.
        let _ = stream.write_all(response.as_bytes());
        String::from_utf8(bytes).unwrap()
    });
    (url, handle)
}

#[test]
fn cookie_auth_and_request_are_correct() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".cookie");
    fs::write(&path, "__cookie__:secret").unwrap();
    let (url, server) = serve(
        json!({"id":"electrum-lite","result":42,"error":null}).to_string(),
        "200 OK",
    );
    let core = Core::new(&url, &path).unwrap();
    assert_eq!(core.call::<u32>("getblockcount", json!([])).unwrap(), 42);
    let request = server.join().unwrap();
    assert!(request.contains(&format!("Basic {}", STANDARD.encode("__cookie__:secret"))));
    assert!(request.contains("getblockcount"));
}

#[test]
fn batched_block_fetch_rejects_duplicate_missing_and_out_of_range_ids() {
    let dir = tempdir().unwrap();
    let cookie = dir.path().join("cookie");
    fs::write(&cookie, "user:secret").unwrap();
    for body in [
        json!([{"id":0,"result":"00"},{"id":0,"result":"00"}]),
        json!([{"id":0,"result":"00"}]),
        json!([{"id":0,"result":"00"},{"id":2,"result":"00"}]),
    ] {
        let (url, server) = serve(body.to_string(), "200 OK");
        let error = Core::new(&url, &cookie)
            .unwrap()
            .block_group(1, 2)
            .unwrap_err();
        assert!(format!("{error:#}").contains("RPC batch"));
        let request = server.join().unwrap();
        assert!(request.contains("getblockhash"));
    }
}

#[test]
fn rpc_errors_do_not_echo_server_secrets() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".cookie");
    fs::write(&path, "user:secret").unwrap();
    for (body, status) in [
        (
            json!({"id":"electrum-lite","error":{"code":-5,"message":"secret"},"result":null})
                .to_string(),
            "500 Internal Server Error",
        ),
        (json!({"id":"wrong-id","result":42}).to_string(), "200 OK"),
        ("unauthorized secret".to_owned(), "401 Unauthorized"),
    ] {
        let (url, server) = serve(body, status);
        let error = Core::new(&url, &path)
            .unwrap()
            .call::<Value>("getblock", json!([]))
            .unwrap_err();
        assert!(!format!("{error:#}").contains("secret"));
        server.join().unwrap();
    }
}

#[test]
fn wrong_core_network_and_oversized_response_fail() {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".cookie");
    fs::write(&path, "user:secret").unwrap();
    let (url, server) = serve(
        json!({"id":"electrum-lite","result":{"chain":"main"},"error":null}).to_string(),
        "200 OK",
    );
    assert!(Core::new(&url, &path)
        .unwrap()
        .verify(Chain::Testnet4)
        .is_err());
    server.join().unwrap();
    let (url, server) = serve(" ".repeat(8_100_000), "200 OK");
    let error = Core::new(&url, &path)
        .unwrap()
        .call::<Value>("getblock", json!([]))
        .unwrap_err();
    assert!(error.to_string().contains("size limit"));
    server.join().unwrap();
}
