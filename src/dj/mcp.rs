//! Spotifast's tools as an MCP server (streamable HTTP, JSON responses) for one `claude` child.
//!
//! Bound to 127.0.0.1 on a port the OS picks, for the life of that child only. Every request must
//! carry the bearer token generated for this child (it travels to the child in a private config
//! file, never on a command line), a `Host` naming this very address and no `Origin` header (a web
//! page cannot call it). A request failing any of that gets an answer without a tool being touched.
//!
//! The tool handler runs on the request's own thread: it may wait for the app to answer, and other
//! requests keep being served meanwhile.

use super::tools::Tool;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// What the handler gets: tool name and arguments. Its text is what the model reads; an `Err` is
/// a tool failure the model sees as such and reacts to.
pub type Handler = Arc<dyn Fn(&str, Value) -> Result<String, String> + Send + Sync>;

const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 2 * 1024 * 1024;
/// Connections served at once; the one `claude` child needs a handful.
const MAX_CONNECTIONS: usize = 16;
/// A request line that does not arrive in this time is dropped (a connection that never speaks).
const READ_TIMEOUT: Duration = Duration::from_secs(15);

pub struct McpServer {
    pub port: u16,
    pub token: String,
    stop: Arc<AtomicBool>,
}

impl McpServer {
    pub fn start(tools: Arc<Vec<Tool>>, handler: Handler) -> std::io::Result<McpServer> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let token = random_token();
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let tok = token.clone();
        let active = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("claude-mcp".into())
            .spawn(move || {
                while !st.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            // a flood of connections is dropped, not given a thread each
                            if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                                continue;
                            }
                            active.fetch_add(1, Ordering::SeqCst);
                            let (tools, handler, tok) =
                                (tools.clone(), handler.clone(), tok.clone());
                            let count = active.clone();
                            let started = std::thread::Builder::new()
                                .name("claude-mcp-conn".into())
                                .spawn(move || {
                                    serve(conn, port, &tok, &tools, &handler);
                                    count.fetch_sub(1, Ordering::SeqCst);
                                });
                            if started.is_err() {
                                active.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(40))
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(200)),
                    }
                }
            })?;
        Ok(McpServer { port, token, stop })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// Stops accepting. Calls already running finish (the turn's cancel flag ends their waits).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 256 bits from the OS, hex.
fn random_token() -> String {
    let mut b = [0u8; 32];
    // `rand::rng()` is a CSPRNG seeded from the OS
    rand::Rng::fill(&mut rand::rng(), &mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn same(a: &str, b: &str) -> bool {
    // constant time over the length of the shorter; the length itself is not secret
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn read_request(conn: &TcpStream) -> Result<Request, (u16, &'static str)> {
    // the head is read through a cap, so one endless line cannot grow a buffer before it is judged
    let mut r = BufReader::new(
        conn.try_clone()
            .map_err(|_| (500, "io"))?
            .take(MAX_HEADER as u64 + 1),
    );
    let mut line = String::new();
    r.read_line(&mut line).map_err(|_| (400, "bad request"))?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err((400, "bad request"));
    };
    let (method, path) = (method.to_string(), path.to_string());
    let mut headers = Vec::new();
    let mut total = line.len();
    loop {
        let mut h = String::new();
        let n = r.read_line(&mut h).map_err(|_| (400, "bad request"))?;
        total += n;
        if n == 0 || total > MAX_HEADER {
            return Err((431, "headers too large"));
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let req = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    if req.header("transfer-encoding").is_some() {
        return Err((411, "send a Content-Length"));
    }
    // the body has its own, larger cap
    r.get_mut().set_limit(MAX_BODY as u64);
    Ok(Request {
        body: read_body(&mut r, &req)?,
        ..req
    })
}

fn read_body(
    r: &mut BufReader<std::io::Take<TcpStream>>,
    req: &Request,
) -> Result<Vec<u8>, (u16, &'static str)> {
    let len: usize = req
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err((413, "body too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).map_err(|_| (400, "short body"))?;
    Ok(body)
}

fn write_response(conn: &mut TcpStream, status: u16, body: Option<&Value>) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let data = body
        .map(|b| serde_json::to_vec(b).expect("serializable"))
        .unwrap_or_default();
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n",
        data.len()
    );
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(&data);
    let _ = conn.flush();
    let _ = conn.shutdown(Shutdown::Both);
}

fn serve(mut conn: TcpStream, port: u16, token: &str, tools: &[Tool], handler: &Handler) {
    // Winsock gives an accepted socket the listener's non-blocking mode: reads would fail with
    // WouldBlock whenever the request is not there yet
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(READ_TIMEOUT));
    let req = match read_request(&conn) {
        Ok(r) => r,
        Err((code, _)) => return write_response(&mut conn, code, None),
    };
    if let Err(code) = authorize(&req.headers, port, token) {
        return write_response(&mut conn, code, None);
    }
    if req.path.split('?').next() != Some("/mcp") {
        return write_response(&mut conn, 404, None);
    }
    match req.method.as_str() {
        // no server-initiated stream: the client carries on without one
        "GET" => write_response(&mut conn, 405, None),
        "DELETE" => write_response(&mut conn, 200, None),
        "POST" => {
            // a tool call may wait for the user: the read timeout is for the request only
            let _ = conn.set_read_timeout(None);
            let msg: Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(_) => return write_response(&mut conn, 400, None),
            };
            match rpc(&msg, tools, handler) {
                Some(reply) => write_response(&mut conn, 200, Some(&reply)),
                None => write_response(&mut conn, 202, None),
            }
        }
        _ => write_response(&mut conn, 405, None),
    }
}

/// Whether a request may reach a tool: a `Host` naming this very address, no `Origin` (a browser
/// page cannot call it) and this child's bearer token. The error is the HTTP status to answer with.
/// `headers` have lowercase names.
pub(super) fn authorize(headers: &[(String, String)], port: u16, token: &str) -> Result<(), u16> {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let host_ok = get("host")
        .is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if get("origin").is_some() || !host_ok {
        return Err(403);
    }
    let authorized = get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| same(t, token));
    if authorized { Ok(()) } else { Err(401) }
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// One JSON-RPC message. None for a notification (nothing to answer).
pub(super) fn rpc(msg: &Value, tools: &[Tool], handler: &Handler) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let p = &msg["params"];
    let result = match method {
        "initialize" => {
            let v = p
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-03-26");
            json!({
                "protocolVersion": v,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "spotifast", "version": env!("CARGO_PKG_VERSION") },
            })
        }
        "ping" => json!({}),
        "tools/list" => {
            let list: Vec<Value> = tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.schema }))
                .collect();
            json!({ "tools": list })
        }
        "tools/call" => {
            let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = match p.get("arguments") {
                Some(a) if a.is_object() => a.clone(),
                _ => json!({}),
            };
            tool_result(&handler(name, args))
        }
        _ => return Some(error(&id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// A tool's outcome as an MCP `CallToolResult`. A failure is a result with `isError`, which the
/// model reads and reacts to.
pub fn tool_result(r: &Result<String, String>) -> Value {
    match r {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_request_needs_the_token_the_host_and_no_origin() {
        let ok = h(&[
            ("host", "127.0.0.1:4000"),
            ("authorization", "Bearer secret"),
        ]);
        assert_eq!(authorize(&ok, 4000, "secret"), Ok(()));
        let localhost = h(&[
            ("host", "localhost:4000"),
            ("authorization", "Bearer secret"),
        ]);
        assert_eq!(authorize(&localhost, 4000, "secret"), Ok(()));
        // wrong or missing token
        assert_eq!(
            authorize(
                &h(&[("host", "127.0.0.1:4000"), ("authorization", "Bearer nope")]),
                4000,
                "secret"
            ),
            Err(401)
        );
        assert_eq!(
            authorize(&h(&[("host", "127.0.0.1:4000")]), 4000, "secret"),
            Err(401)
        );
        assert_eq!(
            authorize(
                &h(&[("host", "127.0.0.1:4000"), ("authorization", "secret")]),
                4000,
                "secret"
            ),
            Err(401)
        );
        // DNS rebinding: another name for this address, or another port
        assert_eq!(
            authorize(
                &h(&[
                    ("host", "evil.example:4000"),
                    ("authorization", "Bearer secret")
                ]),
                4000,
                "secret"
            ),
            Err(403)
        );
        assert_eq!(
            authorize(
                &h(&[
                    ("host", "127.0.0.1:4001"),
                    ("authorization", "Bearer secret")
                ]),
                4000,
                "secret"
            ),
            Err(403)
        );
        assert_eq!(
            authorize(&h(&[("authorization", "Bearer secret")]), 4000, "secret"),
            Err(403)
        );
        // a web page always sends an Origin
        let page = h(&[
            ("host", "127.0.0.1:4000"),
            ("origin", "https://evil.example"),
            ("authorization", "Bearer secret"),
        ]);
        assert_eq!(authorize(&page, 4000, "secret"), Err(403));
    }

    fn tools() -> Arc<Vec<Tool>> {
        Arc::new(super::super::tools::registry())
    }

    fn echo() -> Handler {
        Arc::new(|name, args| {
            if name == "skip" {
                Ok(format!("ran {name} {args}"))
            } else {
                Err("no such tool".into())
            }
        })
    }

    #[test]
    fn rpc_lists_the_tools_and_runs_a_call() {
        let t = tools();
        let handler = echo();
        let init = rpc(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}), &t, &handler).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "spotifast");
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        let list = rpc(
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            &t,
            &handler,
        )
        .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x["name"].as_str())
            .collect();
        // the server lists exactly the registry, in its order
        let expected: Vec<String> = crate::dj::tools::registry()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names, expected);
        for name in [
            "search",
            "play",
            "add_to_playlist",
            "remember",
            "schedule_task",
        ] {
            assert!(names.contains(&name), "{name} is missing");
        }
        // nothing that deletes a playlist or the account's data is offered
        assert!(
            !names
                .iter()
                .any(|n| n.contains("delete") || n.contains("unfollow"))
        );
        let call = rpc(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"skip","arguments":{}}}), &t, &handler).unwrap();
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(call["result"]["content"][0]["text"], "ran skip {}");
        let bad = rpc(
            &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"rm"}}),
            &t,
            &handler,
        )
        .unwrap();
        assert_eq!(bad["result"]["isError"], true);
        // notifications get no answer; unknown methods get an error
        assert!(
            rpc(
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                &t,
                &handler
            )
            .is_none()
        );
        assert_eq!(
            rpc(
                &json!({"jsonrpc":"2.0","id":5,"method":"nope"}),
                &t,
                &handler
            )
            .unwrap()["error"]["code"],
            -32601
        );
    }

    fn post(port: u16, headers: &[(&str, String)], body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut req = format!("POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n", body.len());
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(body);
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        let status = out
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (status, out)
    }

    #[test]
    fn the_server_answers_only_with_its_token() {
        let server = McpServer::start(tools(), echo()).unwrap();
        let host = format!("127.0.0.1:{}", server.port);
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"skip","arguments":{}}}"#;
        let auth = format!("Bearer {}", server.token);
        let (code, text) = post(
            server.port,
            &[("Host", host.clone()), ("Authorization", auth.clone())],
            body,
        );
        assert_eq!(code, 200, "{text}");
        assert!(text.contains("ran skip"), "{text}");
        let (code, _) = post(server.port, &[("Host", host.clone())], body);
        assert_eq!(code, 401);
        let (code, text) = post(
            server.port,
            &[
                ("Host", host.clone()),
                ("Authorization", auth.clone()),
                ("Origin", "http://x".into()),
            ],
            body,
        );
        assert_eq!(code, 403);
        assert!(!text.contains("ran skip"));
        let (code, _) = post(
            server.port,
            &[("Host", "evil.example".into()), ("Authorization", auth)],
            body,
        );
        assert_eq!(code, 403);
        assert_eq!(server.token.len(), 64);
    }
}
