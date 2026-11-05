//! http:<url> 공급자 왕복 — 미니 목 서버로 자가 호스팅 규약 검증.

use std::io::{Read, Write};
use std::net::TcpListener;
use vgraph_vec::providers::embedder_from_spec;

fn read_request(s: &mut std::net::TcpStream) {
    // 헤더 끝(\r\n\r\n)까지 읽고 Content-Length만큼 body 소진 (flake 방지)
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end;
    loop {
        let n = s.read(&mut tmp).unwrap();
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let content_len: usize = headers
        .lines()
        .find_map(|l| {
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap())
        })
        .unwrap_or(0);
    let mut have = buf.len() - header_end;
    while have < content_len {
        let n = s.read(&mut tmp).unwrap();
        have += n;
    }
}

#[test]
fn http_provider_roundtrip() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        read_request(&mut s);
        let body = r#"{"vectors":[[1.0,0.0],[0.0,1.0]]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        s.write_all(resp.as_bytes()).unwrap();
    });

    let e = embedder_from_spec(&format!("http://{addr}/embed")).unwrap();
    assert!(e.model_id().starts_with("http:"));
    let vs = e.embed_batch(&["하나".into(), "둘".into()]).unwrap();
    assert_eq!(vs, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    server.join().unwrap();
}
