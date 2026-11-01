//! `herdr mobile`: serves a phone page on the local network and forwards a
//! small allowlist of public API methods to the running server.
//!
//! The bridge is just another API client. It owns no runtime state, so it
//! stays outside the server and never touches the private TUI socket.

use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

use crate::api::schema::Request;

const PAGE: &str = include_str!("mobile.html");
const DEFAULT_PORT: u16 = 7781;
const MAX_BODY: usize = 64 * 1024;
const MAX_HEADER_LINES: usize = 64;
/// API methods a phone may call. Everything else is refused.
const ALLOWED_METHODS: &[&str] = &[
    "session.snapshot",
    "pane.read",
    "pane.send_text",
    "pane.send_keys",
    "git.status",
];

pub(super) fn run_mobile_command(args: &[String]) -> io::Result<i32> {
    let mut port = DEFAULT_PORT;
    let mut host: Option<IpAddr> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--port" => match rest.next().and_then(|value| value.parse().ok()) {
                Some(value) => port = value,
                None => return usage("--port needs a number"),
            },
            "--host" => match rest.next().and_then(|value| value.parse().ok()) {
                Some(value) => host = Some(value),
                None => return usage("--host needs an IP address"),
            },
            "help" | "--help" | "-h" => {
                print_help();
                return Ok(0);
            }
            other => return usage(&format!("unknown option: {other}")),
        }
    }

    // Fails early with the usual "server not running" guidance.
    let probe: Request = serde_json::from_value(serde_json::json!({
        "id": "cli:mobile:probe", "method": "session.snapshot", "params": {}
    }))
    .map_err(io::Error::other)?;
    let response = super::send_request(&probe)?;
    if response.get("error").is_some() {
        return super::print_response(&response);
    }

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
    let token = random_token();
    let host = host.unwrap_or_else(lan_address);
    let url = format!("http://{host}:{port}/#t={token}");

    println!("{}", qr_text(&url)?);
    println!("Scan to open herdr on your phone, or open:\n\n  {url}\n");
    println!("Your phone must be on the same network (or the same Tailscale/VPN).");
    println!("Anyone with this link can type into your terminals. Press Ctrl+C to stop.");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let token = token.clone();
        if let Err(error) = crate::thread_spawn::spawn_named("herdr-mobile-http", move || {
            if let Err(error) = serve(stream, &token) {
                tracing::debug!(%error, "mobile request failed");
            }
        }) {
            tracing::warn!(%error, "mobile bridge could not start a request thread");
        }
    }
    Ok(0)
}

fn usage(message: &str) -> io::Result<i32> {
    eprintln!("{message}");
    eprintln!("usage: herdr mobile [--port PORT] [--host IP]");
    Ok(2)
}

fn print_help() {
    println!(
        "Open herdr on your phone.\n\n\
         usage: herdr mobile [--port PORT] [--host IP]\n\n  \
         --port PORT  port to listen on (default {DEFAULT_PORT})\n  \
         --host IP    address to put in the link and QR code, e.g. a Tailscale IP\n               \
         (default: this machine's LAN address)"
    );
}

struct HttpRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: &'static str,
    content_type: &'static str,
    body: String,
}

impl HttpResponse {
    fn json(status: &'static str, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            body,
        }
    }

    fn error(status: &'static str, message: &str) -> Self {
        Self::json(
            status,
            serde_json::json!({ "error": { "message": message } }).to_string(),
        )
    }
}

fn serve(stream: TcpStream, token: &str) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let response = match read_request(&mut reader)? {
        Some(request) => route(&request, token, forward),
        None => HttpResponse::error("400 Bad Request", "malformed request"),
    };
    write_response(stream, &response)
}

fn read_request(reader: &mut impl BufRead) -> io::Result<Option<HttpRequest>> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, path) = (method.to_owned(), path.to_owned());

    let mut content_length = 0usize;
    let mut authorization = None;
    for _ in 0..MAX_HEADER_LINES {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(usize::MAX);
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = value.strip_prefix("Bearer ").map(str::to_owned);
        }
    }
    if content_length > MAX_BODY {
        return Ok(None);
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    Ok(Some(HttpRequest {
        method,
        path,
        authorization,
        body,
    }))
}

/// Decides the response for one request. `call` reaches the herdr server.
fn route(
    request: &HttpRequest,
    token: &str,
    call: impl FnOnce(&Request) -> Result<serde_json::Value, String>,
) -> HttpResponse {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => HttpResponse {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            body: PAGE.to_owned(),
        },
        ("POST", "/api") => {
            let authorized = request
                .authorization
                .as_deref()
                .is_some_and(|given| constant_time_eq(given.as_bytes(), token.as_bytes()));
            if !authorized {
                return HttpResponse::error(
                    "401 Unauthorized",
                    "link expired: scan the new QR code",
                );
            }
            let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
                return HttpResponse::error("400 Bad Request", "body is not JSON");
            };
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if !ALLOWED_METHODS.contains(&method) {
                return HttpResponse::error("403 Forbidden", "method not allowed from mobile");
            }
            let params = body.get("params").cloned().unwrap_or(serde_json::json!({}));
            let request = serde_json::from_value::<Request>(serde_json::json!({
                "id": "mobile", "method": method, "params": params
            }));
            match request {
                Ok(request) => match call(&request) {
                    Ok(value) => HttpResponse::json("200 OK", value.to_string()),
                    Err(message) => HttpResponse::error("502 Bad Gateway", &message),
                },
                Err(error) => HttpResponse::error("400 Bad Request", &error.to_string()),
            }
        }
        _ => HttpResponse::error("404 Not Found", "not found"),
    }
}

fn forward(request: &Request) -> Result<serde_json::Value, String> {
    let client = super::target::api_client().map_err(|error| error.to_string())?;
    client
        .request_value_with_timeout(request, Duration::from_secs(10))
        .map_err(|error| error.to_string())
}

fn write_response(mut stream: TcpStream, response: &HttpResponse) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    )?;
    stream.write_all(response.body.as_bytes())?;
    stream.flush()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 128 random bits as hex. `RandomState` keys come from the OS random source,
/// and SipHash with secret keys is unpredictable to anyone without them.
fn random_token() -> String {
    let word = |salt: u64| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.finish()
    };
    format!("{:016x}{:016x}", word(1), word(2))
}

/// The address other devices on this network reach us at. Connecting a UDP
/// socket sends nothing; it only asks the OS which interface it would use.
fn lan_address() -> IpAddr {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((Ipv4Addr::new(192, 0, 2, 1), 80))?;
            socket.local_addr()
        })
        .map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn qr_text(url: &str) -> io::Result<String> {
    let code = qrcode::QrCode::new(url.as_bytes()).map_err(io::Error::other)?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(auth: Option<&str>, body: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: "/api".into(),
            authorization: auth.map(str::to_owned),
            body: body.as_bytes().to_vec(),
        }
    }

    fn never_called(_: &Request) -> Result<serde_json::Value, String> {
        panic!("the server must not be reached")
    }

    #[test]
    fn page_is_served_without_a_token() {
        let request = HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            authorization: None,
            body: Vec::new(),
        };
        let response = route(&request, "secret", never_called);
        assert_eq!(response.status, "200 OK");
        assert!(response.body.contains("herdr"));
    }

    #[test]
    fn api_rejects_missing_or_wrong_token() {
        let body = r#"{"method":"session.snapshot"}"#;
        assert_eq!(
            route(&post(None, body), "secret", never_called).status,
            "401 Unauthorized"
        );
        assert_eq!(
            route(&post(Some("secreT"), body), "secret", never_called).status,
            "401 Unauthorized"
        );
    }

    #[test]
    fn api_refuses_methods_outside_the_allowlist() {
        let body = r#"{"method":"server.stop","params":{}}"#;
        let response = route(&post(Some("secret"), body), "secret", never_called);
        assert_eq!(response.status, "403 Forbidden");
    }

    #[test]
    fn api_forwards_allowed_methods() {
        let body = r#"{"method":"pane.send_text","params":{"pane_id":"w1:p1","text":"hi"}}"#;
        let response = route(&post(Some("secret"), body), "secret", |request| {
            assert_eq!(request.id, "mobile");
            Ok(serde_json::json!({ "result": { "type": "ok" } }))
        });
        assert_eq!(response.status, "200 OK");
        assert!(response.body.contains("\"ok\""));
    }

    #[test]
    fn oversized_body_is_refused() {
        let raw = format!(
            "POST /api HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(read_request(&mut raw.as_bytes()).unwrap().is_none());
    }

    #[test]
    fn request_parsing_reads_token_and_body() {
        let raw = "POST /api HTTP/1.1\r\nAuthorization: Bearer abc\r\nContent-Length: 2\r\n\r\n{}";
        let request = read_request(&mut raw.as_bytes()).unwrap().unwrap();
        assert_eq!(request.authorization.as_deref(), Some("abc"));
        assert_eq!(request.body, b"{}");
    }

    #[test]
    fn tokens_are_long_and_distinct() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
