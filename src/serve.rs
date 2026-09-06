//! Minimal HTTP server exposing the ductile API for the web frontend.
//!
//! std-only: TcpListener + hand-rolled request parsing (single-threaded,
//! sufficient for a local operator console — not a public-facing server).
//! GET /            → embedded web UI
//! GET /api/stats   → library stats
//! GET /api/scripts → registered script contracts
//! GET /api/procs?query= → proc library (empty query = all)
//! GET /api/pipeline?path= → structural JSON of one .pipeline file
//! GET /api/runs?proc=NAME&limit=N → recent runs
//! POST /api/run    {"path","topic","params"{}, "policy"} → execution JSON
//! POST /api/script_call {"name","args"{}} → script call JSON

use crate::api;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn http_response(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Extract the first query param value: `/api/procs?query=x` → Some("x").
pub fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(v.replace("%20", " ").replace("%22", "").replace("%27", ""));
        }
    }
    None
}

/// Minimal JSON object walker for POST bodies: top-level `"key": "value"`,
/// `"key": number`, and one nested object (`params` / `args`) of string values.
pub fn json_get_str(body: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\"", key);
    let i = body.find(&pat)?;
    let rest = &body[i + pat.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    let end = rest
        .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
        .unwrap_or(rest.len());
    let num = rest[..end].trim();
    if num.is_empty() {
        None
    } else {
        Some(num.to_string())
    }
}

/// Nested-object getter: returns k=v pairs of `"params": {"a":"b"}`.
pub fn json_get_object(body: &str, key: &str) -> Option<Vec<(String, String)>> {
    let pat = format!("\"{}\"", key);
    let i = body.find(&pat)?;
    let rest = &body[i + pat.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('{')?;
    let close = rest.find('}')?;
    let inner = &rest[..close];
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in inner.chars() {
        match c {
            '{' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ']' | '}' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                if let Some((k, v)) = parse_kv(&cur) {
                    out.push((k, v));
                }
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    if let Some((k, v)) = parse_kv(&cur) {
        out.push((k, v));
    }
    Some(out)
}

fn parse_kv(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let colon = s.find(':')?;
    let k = s[..colon].trim().trim_matches('"').to_string();
    let v = s[colon + 1..].trim().trim_matches('"').to_string();
    if k.is_empty() {
        None
    } else {
        Some((k, v))
    }
}

fn route(req: &str, web_dir: Option<&std::path::Path>) -> Vec<u8> {
    let first_line = req.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");

    let (req_path, body) = split_body(req);

    match (method, path) {
        ("GET", "/") => {
            let html = match web_dir {
                Some(d) => std::fs::read_to_string(d.join("index.html")).unwrap_or_else(|_| {
                    "<html><body><h1>ductile</h1><p>web/index.html not found</p></body></html>"
                        .into()
                }),
                None => include_str!("../web/index.html").into(),
            };
            http_response("200 OK", "text/html; charset=utf-8", &html)
        }
        ("GET", "/api/stats") => {
            let body = api::db_stats_json_core();
            http_response("200 OK", "application/json", &body)
        }
        ("GET", "/api/scripts") => {
            let body = api::scripts_json_core();
            http_response("200 OK", "application/json", &body)
        }
        ("GET", "/api/procs") => {
            let q = query_param(&req_path, "query").unwrap_or_default();
            let body = api::procs_json_core(&q);
            http_response("200 OK", "application/json", &body)
        }
        ("GET", "/api/pipeline") => match query_param(&req_path, "path") {
            Some(p) => match api::pipeline_json_core(&p) {
                Ok(b) => http_response("200 OK", "application/json", &b),
                Err(e) => http_response(
                    "400 Bad Request",
                    "application/json",
                    &format!("{{\"error\":\"{}\"}}", e),
                ),
            },
            None => http_response(
                "400 Bad Request",
                "application/json",
                "{\"error\":\"missing path\"}",
            ),
        },
        ("GET", "/api/runs") => {
            let proc = query_param(&req_path, "proc").unwrap_or_default();
            let limit = query_param(&req_path, "limit")
                .and_then(|l| l.parse::<usize>().ok())
                .unwrap_or(10);
            let body = api::runs_json_core(&proc, limit);
            http_response("200 OK", "application/json", &body)
        }
        ("POST", "/api/run") => {
            let path_v = json_get_str(&body, "path").unwrap_or_default();
            let topic = json_get_str(&body, "topic").unwrap_or_default();
            let params = json_get_object(&body, "params").unwrap_or_default();
            let policy = json_get_str(&body, "policy");
            match api::run_json_core(
                &path_v,
                &topic,
                if params.is_empty() {
                    None
                } else {
                    Some(params.into_iter().collect())
                },
                policy.as_deref(),
            ) {
                Ok(b) => http_response("200 OK", "application/json", &b),
                Err(e) => http_response(
                    "400 Bad Request",
                    "application/json",
                    &format!("{{\"error\":\"{}\"}}", e),
                ),
            }
        }
        ("POST", "/api/script_call") => {
            let name = json_get_str(&body, "name").unwrap_or_default();
            let args = json_get_object(&body, "args").unwrap_or_default();
            match api::script_call_json_core(
                &name,
                if args.is_empty() {
                    None
                } else {
                    Some(args.into_iter().collect())
                },
            ) {
                Ok(b) => http_response("200 OK", "application/json", &b),
                Err(e) => http_response(
                    "400 Bad Request",
                    "application/json",
                    &format!("{{\"error\":\"{}\"}}", e),
                ),
            }
        }
        _ => http_response(
            "404 Not Found",
            "application/json",
            "{\"error\":\"not found\"}",
        ),
    }
}

fn split_body(req: &str) -> (String, String) {
    match req.split_once("\r\n\r\n") {
        Some((head, body)) => (
            head.split_whitespace().nth(1).unwrap_or("/").to_string(),
            body.to_string(),
        ),
        None => (
            req.split_whitespace().nth(1).unwrap_or("/").to_string(),
            String::new(),
        ),
    }
}

fn handle(mut stream: TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    // read until headers complete (+ short body wait)
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let s = String::from_utf8_lossy(&buf);
                if let Some(cl) = content_length(&s) {
                    if let Some(pos) = s.find("\r\n\r\n") {
                        if buf.len() >= pos + 4 + cl {
                            break;
                        }
                    }
                } else if s.contains("\r\n\r\n") {
                    break;
                }
                if buf.len() > 1_048_576 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let req = String::from_utf8_lossy(&buf).into_owned();
    let resp = route(
        &req,
        std::env::var("DUCTILE_WEB_DIR")
            .ok()
            .map(std::path::PathBuf::from)
            .as_deref(),
    );
    let _ = stream.write_all(&resp);
    let _ = stream.flush();
}

fn content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let l = line.to_ascii_lowercase();
        if l.starts_with("content-length:") {
            return l["content-length:".len()..].trim().parse().ok();
        }
    }
    None
}

/// Serve on addr (e.g. "127.0.0.1:7878"). Blocks forever.
pub fn serve(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("ductile serving on http://{}", addr);
    for stream in listener.incoming() {
        if let Ok(s) = stream {
            handle(s);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_extracts() {
        assert_eq!(
            query_param("/api/procs?query=web", "query"),
            Some("web".into())
        );
        assert_eq!(
            query_param("/api/runs?proc=gen&limit=5", "proc"),
            Some("gen".into())
        );
        assert_eq!(
            query_param("/api/runs?proc=gen&limit=5", "limit"),
            Some("5".into())
        );
        assert_eq!(query_param("/api/procs", "query"), None);
    }

    #[test]
    fn json_get_str_forms() {
        assert_eq!(
            json_get_str("{\"path\":\"a.pipeline\",\"topic\":\"x\"}", "path"),
            Some("a.pipeline".into())
        );
        assert_eq!(json_get_str("{\"limit\":10}", "limit"), Some("10".into()));
        assert_eq!(
            json_get_str("{\"topic\":\"中文 topic\"}", "topic"),
            Some("中文 topic".into())
        );
        assert_eq!(json_get_str("{\"a\":1}", "missing"), None);
    }

    #[test]
    fn json_get_object_pairs() {
        let body = "{\"path\":\"p\",\"params\":{\"mode\":\"fast\",\"n\":\"3\"}}";
        let obj = json_get_object(body, "params").unwrap();
        assert!(obj.contains(&("mode".into(), "fast".into())));
        assert!(obj.contains(&("n".into(), "3".into())));
        assert_eq!(json_get_object(body, "nope"), None);
    }

    #[test]
    fn parse_kv_quotes() {
        assert_eq!(parse_kv("\"k\": \"v\""), Some(("k".into(), "v".into())));
        assert_eq!(parse_kv("\"k\":v"), Some(("k".into(), "v".into())));
        assert_eq!(parse_kv("junk"), None);
    }

    #[test]
    fn route_get_api_scripts() {
        let resp = route("GET /api/scripts HTTP/1.1\r\nHost: x\r\n\r\n", None);
        let s = String::from_utf8(resp).unwrap();
        assert!(s.starts_with("HTTP/1.1 200"));
        assert!(s.contains("application/json"));
    }

    #[test]
    fn route_404_and_index() {
        let resp = route("GET /nope HTTP/1.1\r\n\r\n", None);
        assert!(String::from_utf8(resp).unwrap().starts_with("HTTP/1.1 404"));
        let resp = route("GET / HTTP/1.1\r\n\r\n", None);
        assert!(String::from_utf8(resp).unwrap().starts_with("HTTP/1.1 200"));
    }
}
