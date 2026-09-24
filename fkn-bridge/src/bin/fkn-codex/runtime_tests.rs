use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;

fn serve_status(status: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let status = status.to_string();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 256];
        let _ = stream.read(&mut request).unwrap();
        let response = format!("HTTP/1.1 {status}\r\nContent-Length: 5\r\n\r\nready");
        stream.write_all(response.as_bytes()).unwrap();
    });
    format!("http://{address}")
}

#[test]
fn tunnel_readiness_requires_a_successful_readyz_response() {
    assert!(tunnel_is_ready(&serve_status("200 OK")));
    assert!(!tunnel_is_ready(&serve_status("503 Service Unavailable")));
    assert!(!tunnel_is_ready("not-a-health-url"));
}

#[test]
fn normalizes_plain_and_protocol_specific_proxies() {
    assert_eq!(
        normalize_proxy("127.0.0.1:10808".to_string()),
        Some("http://127.0.0.1:10808".to_string())
    );
    assert_eq!(
        normalize_proxy("http=127.0.0.1:8080;https=127.0.0.1:10808".to_string()),
        Some("http://127.0.0.1:10808".to_string())
    );
    assert_eq!(normalize_proxy("  ".to_string()), None);
}
