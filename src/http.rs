//! HTTP server: serves the embedded noVNC client and upgrades `/websockify`
//! to a WebSocket carrying the RFB stream.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use include_dir::{include_dir, Dir};

use crate::rfb::{self, Config};
use crate::screen::Hub;
use crate::ws::{accept_key, WsSink, WsSource};
use crate::x11::Input;

static WEB: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web");

const MAX_HEADER_BYTES: usize = 16 * 1024;

pub struct Server {
    pub hub: Arc<Hub>,
    pub input: Arc<Mutex<Input>>,
    pub cfg: Arc<Config>,
    pub extras: rfb::Extras,
}

pub fn run(listener: TcpListener, server: Arc<Server>) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let server = server.clone();
        std::thread::spawn(move || {
            let peer = stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "?".into());
            if let Err(e) = handle(stream, &server) {
                if e.kind() != io::ErrorKind::UnexpectedEof
                    && e.kind() != io::ErrorKind::BrokenPipe
                    && e.kind() != io::ErrorKind::ConnectionReset
                {
                    log::warn(&format!("{peer}: {e}"));
                }
            }
        });
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_request<R: BufRead>(reader: &mut R) -> io::Result<Request> {
    let mut line = String::new();
    let mut total = 0;
    reader.read_line(&mut line)?;
    total += line.len();
    let mut parts = line.trim_end().split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed request line"));
    }

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        total += n;
        if n == 0 || total > MAX_HEADER_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "headers too long"));
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Request { method, path, headers })
}

fn handle(stream: TcpStream, server: &Server) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let req = read_request(&mut reader)?;

    let upgrade = req
        .header("upgrade")
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if upgrade {
        return upgrade_websocket(stream, reader, req, server);
    }
    if req.method != "GET" && req.method != "HEAD" {
        return respond(stream, 405, "text/plain", b"method not allowed");
    }
    serve_static(stream, &req)
}

/// Reject cross-origin upgrades: without this, any web page the user visits
/// could open a socket to a passwordless server on their own machine.
///
/// Behind a reverse proxy the browser's `Origin` names the public host while
/// `Host` may name the backend, so `X-Forwarded-Host` counts too. A browser
/// cannot set that header on a WebSocket handshake, only a proxy can, so
/// honouring it does not weaken the check against drive-by pages.
fn origin_allowed(req: &Request, allowed: &[String]) -> bool {
    let Some(origin) = req.header("origin") else {
        return true; // native clients and curl send no Origin
    };
    let origin_host = host_of(origin.split_once("://").map_or(origin, |(_, rest)| rest));

    if allowed.iter().any(|a| host_of(a) == origin_host) {
        return true;
    }
    [req.header("host"), req.header("x-forwarded-host")]
        .into_iter()
        .flatten()
        // A proxy chain can leave a list here; the first entry is the client's.
        .flat_map(|value| value.split(','))
        .any(|candidate| host_of(candidate.trim()) == origin_host)
}

/// Strip the port from a `host:port` pair, leaving IPv6 literals intact.
fn host_of(value: &str) -> &str {
    if value.starts_with('[') {
        return match value.find(']') {
            Some(end) => &value[..=end],
            None => value,
        };
    }
    value.split(':').next().unwrap_or(value)
}

fn upgrade_websocket(
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    req: Request,
    server: &Server,
) -> io::Result<()> {
    if !origin_allowed(&req, &server.cfg.allowed_origins) {
        log::warn(&format!(
            "rejected websocket with Origin {:?} (Host {:?}); pass --allow-origin HOST if this is your proxy",
            req.header("origin"),
            req.header("host"),
        ));
        return respond(stream, 403, "text/plain", b"cross-origin websocket rejected");
    }
    let Some(key) = req.header("sec-websocket-key") else {
        return respond(stream, 400, "text/plain", b"missing Sec-WebSocket-Key");
    };

    let mut response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n",
        accept_key(key)
    );
    // noVNC offers "binary" for compatibility with old proxies; echo it back.
    if let Some(protocols) = req.header("sec-websocket-protocol") {
        if protocols.split(',').any(|p| p.trim() == "binary") {
            response.push_str("Sec-WebSocket-Protocol: binary\r\n");
        }
    }
    response.push_str("\r\n");

    let mut out = stream.try_clone()?;
    out.write_all(response.as_bytes())?;
    out.flush()?;

    let sink = Arc::new(Mutex::new(WsSink::new(out)));
    let source = WsSource::new(reader, sink.clone());
    log::info("client connected");
    let result = rfb::serve(
        source,
        sink.clone(),
        server.hub.clone(),
        server.input.clone(),
        server.cfg.clone(),
        server.extras.clone(),
    );
    log::info("client disconnected");
    let _ = sink.lock().unwrap().close();
    result
}

fn serve_static(stream: TcpStream, req: &Request) -> io::Result<()> {
    let path = req.path.split(['?', '#']).next().unwrap_or("/");
    let rel = if path == "/" { "index.html" } else { path.trim_start_matches('/') };
    if rel.contains("..") {
        return respond(stream, 400, "text/plain", b"bad path");
    }
    match WEB.get_file(rel) {
        Some(file) => respond(stream, 200, content_type(rel), file.contents()),
        None => respond(stream, 404, "text/plain", b"not found"),
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn respond(mut stream: TcpStream, status: u16, ctype: &str, body: &[u8]) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Tiny stderr logger; the whole program only needs three levels.
pub mod log {
    use std::sync::atomic::{AtomicBool, Ordering};

    static VERBOSE: AtomicBool = AtomicBool::new(false);

    pub fn set_verbose(on: bool) {
        VERBOSE.store(on, Ordering::Relaxed);
    }

    pub fn info(msg: &str) {
        eprintln!("rvnc: {msg}");
    }

    pub fn warn(msg: &str) {
        eprintln!("rvnc: warning: {msg}");
    }

    pub fn debug(msg: &str) {
        if VERBOSE.load(Ordering::Relaxed) {
            eprintln!("rvnc: {msg}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Request {
        read_request(&mut BufReader::new(io::Cursor::new(raw.as_bytes()))).unwrap()
    }

    #[test]
    fn parses_request_and_headers() {
        let req = parse("GET /websockify HTTP/1.1\r\nHost: box:6080\r\nUpgrade: websocket\r\n\r\n");
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/websockify");
        assert_eq!(req.header("host"), Some("box:6080"));
        assert_eq!(req.header("UPGRADE"), Some("websocket"));
    }

    #[test]
    fn same_origin_allowed_cross_origin_rejected() {
        let none: &[String] = &[];
        assert!(origin_allowed(&parse(
            "GET / HTTP/1.1\r\nHost: box:6080\r\nOrigin: http://box:6080\r\n\r\n"
        ), none));
        assert!(origin_allowed(&parse("GET / HTTP/1.1\r\nHost: box:6080\r\n\r\n"), none));
        assert!(!origin_allowed(&parse(
            "GET / HTTP/1.1\r\nHost: box:6080\r\nOrigin: http://evil.example\r\n\r\n"
        ), none));
    }

    #[test]
    fn reverse_proxy_origins_are_accepted() {
        let none: &[String] = &[];
        // Caddy keeps the public Host: straightforward match.
        assert!(origin_allowed(&parse(
            "GET / HTTP/1.1\r\nHost: br.example.com\r\nOrigin: https://br.example.com\r\n\r\n"
        ), none));
        // A proxy that rewrites Host to the backend still forwards the real one.
        assert!(origin_allowed(&parse(
            "GET / HTTP/1.1\r\nHost: 10.0.3.138:8080\r\n\
             X-Forwarded-Host: br.example.com\r\nOrigin: https://br.example.com\r\n\r\n"
        ), none));
        // Neither header matches: still refused.
        assert!(!origin_allowed(&parse(
            "GET / HTTP/1.1\r\nHost: 10.0.3.138:8080\r\n\
             X-Forwarded-Host: br.example.com\r\nOrigin: https://evil.example\r\n\r\n"
        ), none));
    }

    #[test]
    fn allow_origin_overrides_the_check() {
        let allowed = vec!["br.example.com".to_string()];
        assert!(origin_allowed(&parse(
            "GET / HTTP/1.1\r\nHost: 10.0.3.138:8080\r\nOrigin: https://br.example.com\r\n\r\n"
        ), &allowed));
        assert!(!origin_allowed(&parse(
            "GET / HTTP/1.1\r\nHost: 10.0.3.138:8080\r\nOrigin: https://other.example\r\n\r\n"
        ), &allowed));
    }

    #[test]
    fn ipv6_literals_keep_their_address() {
        assert_eq!(host_of("[fc42::1]:8080"), "[fc42::1]");
        assert_eq!(host_of("box:6080"), "box");
        assert_eq!(host_of("box"), "box");
    }

    #[test]
    fn client_assets_are_embedded() {
        assert!(WEB.get_file("index.html").is_some());
        assert!(WEB.get_file("novnc/core/rfb.js").is_some());
        assert!(WEB.get_file("../../etc/passwd").is_none());
    }

    #[test]
    fn content_types_cover_the_client() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("novnc/core/rfb.js"), "text/javascript; charset=utf-8");
    }
}
