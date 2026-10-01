use crate::cache::{Cache, Source};
use crate::config::Config;
use crate::ratelimit::RateLimiter;
use crate::solver::{SolveError, SolveJob, SolverPool};
use std::env;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    remote_addr: String,
    authorization: Option<String>,
    body: String,
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: String,
}

pub fn serve(
    config: Arc<Config>,
    service: Arc<SolverPool>,
    limiter: Arc<RateLimiter>,
    cache: Arc<Cache>,
) -> Result<(), String> {
    let addr = format!("{}:{}", config.host, config.port);
    let ip: IpAddr = addr
        .split(':')
        .next()
        .unwrap_or("127.0.0.1")
        .parse()
        .map_err(|err| format!("invalid host ip: {}", err))?;
    let listener = TcpListener::bind((ip, config.port))
        .map_err(|err| format!("failed to bind HTTP server on {}: {}", addr, err))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to configure HTTP listener: {}", err))?;

    println!("[System] API listening on {} (internal, TLS handled by caddy)", addr);

    // periodic cleanup: rate-limit buckets + expired cache entries
    {
        let limiter = Arc::clone(&limiter);
        let cache = Arc::clone(&cache);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(600));
            limiter.sweep();
            cache.purge_expired();
        });
    }

    // nonblocking accept so we notice a shutdown request instead of parking
    while !crate::shutdown::is_requested() {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(err) = stream.set_nonblocking(false) {
                    eprintln!("[HTTP] failed to set blocking mode: {}", err);
                    continue;
                }
                let service = Arc::clone(&service);
                let limiter = Arc::clone(&limiter);
                let config = Arc::clone(&config);
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    if let Err(err) = handle_connection(stream, service, limiter, config, cache) {
                        eprintln!("[HTTP] {}", err);
                    }
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => eprintln!("[HTTP] connection failed: {}", err),
        }
    }

    println!("[System] Shutting down API service");
    Ok(())
}

impl HttpRequest {
    fn read_from(stream: &mut TcpStream, timeout: Duration) -> Result<Self, String> {
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|err| format!("failed to set read timeout: {}", err))?;

        let mut buffer = Vec::with_capacity(2048);
        let mut chunk = [0u8; 4096];
        let header_end;

        loop {
            let read = stream
                .read(&mut chunk)
                .map_err(|err| format!("failed to read request: {}", err))?;
            if read == 0 {
                return Err("connection closed before headers".to_string());
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
                header_end = index + 4;
                break;
            }
            if buffer.len() > 1024 * 1024 {
                return Err("request headers too large".to_string());
            }
        }

        let header_text = std::str::from_utf8(&buffer[..header_end])
            .map_err(|_| "request headers not utf-8".to_string())?;
        let mut lines = header_text.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| "missing request line".to_string())?;
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| "missing HTTP method".to_string())?
            .to_string();
        let target = parts
            .next()
            .ok_or_else(|| "missing HTTP target".to_string())?;
        // route on path, keep the query string for ?ip= lookups
        let mut target_parts = target.splitn(2, '?');
        let path = target_parts.next().unwrap_or(target).to_string();
        let query = target_parts.next().unwrap_or("").to_string();

        let mut content_length = 0usize;
        let mut authorization = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse::<usize>().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.to_string());
            }
        }

        while buffer.len() < header_end + content_length {
            let read = stream
                .read(&mut chunk)
                .map_err(|err| format!("failed to read request body: {}", err))?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > header_end + 10 * 1024 * 1024 {
                return Err("request body too large".to_string());
            }
        }

        // clamp in case the client over-sent past the declared length
        let body_end = buffer.len().min(header_end + content_length);
        let body = String::from_utf8(buffer[header_end..body_end].to_vec())
            .map_err(|_| "request body not utf-8".to_string())?;

        let remote_addr = stream
            .peer_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        Ok(Self {
            method,
            path,
            query,
            remote_addr,
            authorization,
            body,
        })
    }
}

impl HttpResponse {
    fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: body.to_string(),
        }
    }

    fn write_to(&self, stream: &mut TcpStream) -> Result<(), String> {
        let reason = reason_phrase(self.status);
        // strip crlf: an env-supplied header value can't smuggle in extra headers
        let dyno = sanitize_header_value(&env::var("DYNO").unwrap_or_else(|_| "local".to_string()));
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nX-Dyno: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.status,
            reason,
            self.content_type,
            dyno,
            self.body.len(),
            self.body
        );
        stream
            .write_all(response.as_bytes())
            .map_err(|err| format!("failed to write response: {}", err))
    }
}

fn handle_connection(
    mut stream: TcpStream,
    service: Arc<SolverPool>,
    limiter: Arc<RateLimiter>,
    config: Arc<Config>,
    cache: Arc<Cache>,
) -> Result<(), String> {
    let request = HttpRequest::read_from(&mut stream, service.timeout())?;
    let response = route(request, &service, &limiter, &config, &cache);
    // route receives &Arc<SolverPool> so /v1/ip can hand an owned handle to
    // background revalidation threads
    response.write_to(&mut stream)
}

fn route(
    request: HttpRequest,
    service: &Arc<SolverPool>,
    limiter: &RateLimiter,
    config: &Config,
    cache: &Arc<Cache>,
) -> HttpResponse {
    // /health stays open for caddy + uptime probes; carries cache diagnostics
    if request.path == "/health" {
        let body = health(service).body;
        let spliced = format!(
            "{},\"cache\":{}\n}}",
            body.trim_end().trim_end_matches('}'),
            cache.to_json()
        );
        return HttpResponse::json(200, &spliced);
    }

    // everything else: optional bearer token, then the token bucket
    if let Some(expected) = &config.api_token {
    let provided = request
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    if provided != expected.as_str() {
        return json_error(401, "unauthorized: missing or invalid bearer token");
    }
    }

    // one bucket per api token or, when unauthenticated, per client ip
    let client_key = request
        .authorization
        .clone()
        .unwrap_or_else(|| request.remote_addr.clone());
    if !limiter.check(&client_key) {
        return json_error(429, "Too Many Requests");
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/v1/solver") => solve_v1(request.body, service),
        ("POST" | "GET", "/v1/ip") => ip_lookup_v1(&request, Arc::clone(service), cache),

        // legacy routes, same behavior as the upstream solver
        ("POST", "/cloudflare") => solve_cloudflare(request.body, None, service),
        ("POST", "/turnstile") => solve_cloudflare(request.body, Some("turnstile"), service),
        ("POST", "/iuam") => solve_cloudflare(request.body, Some("iuam"), service),
        ("GET", "/config") => HttpResponse::json(200, &config.to_json()),
        _ => json_error(404, "Not Found"),
    }
}

fn health(service: &SolverPool) -> HttpResponse {
    let (capacity, available, active) = service.capacity_snapshot();
    HttpResponse::json(
        200,
        &json::object(&[
            ("status", json::string("ok")),
            (
                "dyno",
                json::string(&env::var("DYNO").unwrap_or_else(|_| "local".to_string())),
            ),
            ("capacity", capacity.to_string()),
            ("available", available.to_string()),
            ("active", active.to_string()),
        ]),
    )
}

// POST /v1/solver  {"url": "..."} -> clearance session json
fn solve_v1(body: String, service: &SolverPool) -> HttpResponse {
    let request = match SolveJob::from_json_with_mode(&body, Some("iuam")) {
        Ok(request) => request,
        Err(err) => return json_error(400, &err),
    };

    let url = request.url.clone();
    let started = crate::tui::now_hms();

    match service.solve(request) {
        Ok(result) => {
            let clearance = result.cf_clearance.clone().unwrap_or_default();
            crate::tui::log_done(&started, &url, "", result.elapsed_ms, &clearance);
            HttpResponse::json(200, &result.to_json())
        }
        Err(SolveError::TooManyRequests) => {
            crate::tui::log_fail(&started, &url, "", "too many requests");
            json_error(429, "Too Many Requests")
        }
        Err(SolveError::Internal(err)) => {
            crate::tui::log_fail(&started, &url, "", &err);
            json_error(500, &err)
        }
    }
}

// POST|GET /v1/ip?ip=1.2.3.4  (or body {"ip": "1.2.3.4"})
// two-tier cached: L1 memory -> L2 sqlite -> live solve+fetch (singleflight).
// caching is invisible to callers: stale entries are served instantly and
// refreshed in the background; the response never exposes cache mechanics
// beyond an informational "source" field for the operator.
fn ip_lookup_v1(request: &HttpRequest, service: Arc<SolverPool>, cache: &Arc<Cache>) -> HttpResponse {
    // accept ?ip= query param or JSON body {"ip": "..."}
    let ip = match json::find_string(&request.body, "ip") {
        Some(ip) => Some(ip),
        None => request_query_value(&request.query, "ip"),
    };
    let Some(ip) = ip.filter(|ip| parse_ip(ip).is_some()) else {
        return json_error(400, "missing or invalid ip parameter");
    };

    let url = format!("https://whatismyipaddress.com/ip/{}", ip);
    let started = crate::tui::now_hms();
    let key = Cache::key_for_ip(&ip);

    let fetch_start = std::time::Instant::now();
    let fetch_service = Arc::clone(&service);
    let fetch_url = url.clone();
    let (payload, source) = cache.get_or_fetch(&key, move || {
        match crate::solver::fetch_page_via_pool(&fetch_service, &fetch_url) {
            Ok(data) => {
                let details = crate::solver::parse_page_data_json(&data);
                Ok(details.to_json())
            }
            Err(SolveError::TooManyRequests) => Err("pool exhausted".to_string()),
            Err(SolveError::Internal(err)) => Err(err),
        }
    });
    let elapsed = fetch_start.elapsed().as_millis() as u64;

    if payload.is_empty() {
        crate::tui::log_fail(&started, &url, "", "lookup failed");
        return json_error(502, "upstream fetch failed");
    }

    crate::tui::log_done(&started, &url, "", elapsed as u128, &ip);
    cache.log_request("/v1/ip", &ip, "ok", source.as_str(), elapsed, &request.remote_addr);

    let mut response = payload.as_str().trim_end_matches('}').to_string();
    response.push_str(&format!(
        ",\"cached\":{},\"source\":{}}}",
        if source == Source::Miss { "false" } else { "true" },
        json::string(source.as_str())
    ));
    HttpResponse::json(200, &response)
}

// legacy routes, identical behavior to the upstream solver
fn solve_cloudflare(body: String, mode_override: Option<&str>, service: &SolverPool) -> HttpResponse {
    let request = match SolveJob::from_json_with_mode(&body, mode_override) {
        Ok(request) => request,
        Err(err) => return json_error(400, &err),
    };

    let url = request.url.clone();
    let sitekey = request.sitekey.first().cloned().unwrap_or_default();
    let started = crate::tui::now_hms();

    match service.solve(request) {
        Ok(result) => {
            let token = result
                .token
                .clone()
                .or_else(|| result.tokens.as_ref().and_then(|t| t.first().cloned()))
                .or_else(|| result.cf_clearance.clone())
                .unwrap_or_default();
            crate::tui::log_done(&started, &url, &sitekey, result.elapsed_ms, &token);
            HttpResponse::json(200, &result.to_json())
        }
        Err(SolveError::TooManyRequests) => {
            crate::tui::log_fail(&started, &url, &sitekey, "too many requests");
            json_error(429, "Too Many Requests")
        }
        Err(SolveError::Internal(err)) => {
            crate::tui::log_fail(&started, &url, &sitekey, &err);
            json_error(500, &err)
        }
    }
}

fn json_error(status: u16, message: &str) -> HttpResponse {
    HttpResponse::json(
        status,
        &json::object(&[
            ("success", "false".to_string()),
            ("message", json::string(message)),
        ]),
    )
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "OK",
    }
}

// minimal ipv4/ipv6 validation without pulling extra deps
fn parse_ip(input: &str) -> Option<std::net::IpAddr> {
    input.parse::<std::net::IpAddr>().ok()
}

// tiny `key=value&key=value` extractor for query strings
fn request_query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(key) {
            let raw = parts.next().unwrap_or("");
            let decoded = raw
                .replace("%2F", "/")
                .replace("%3A", ":")
                .replace("%2B", "+");
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) mod json {
    #[derive(Clone, Debug)]
    pub enum Value {
        Null,
        Bool(bool),
        Number(f64),
        String(String),
        Array(Vec<Value>),
        Object(Vec<(String, Value)>),
    }

    impl Value {
        pub fn get(&self, key: &str) -> Option<&Value> {
            let Value::Object(fields) = self else {
                return None;
            };
            fields
                .iter()
                .find_map(|(name, value)| if name == key { Some(value) } else { None })
        }

        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::String(value) => Some(value),
                _ => None,
            }
        }

        pub fn as_f64(&self) -> Option<f64> {
            match self {
                Value::Number(value) => Some(*value),
                _ => None,
            }
        }

        // json has no int type, so coerce from f64 — but reject negatives instead of wrapping
        pub fn as_u64(&self) -> Option<u64> {
            match self {
                Value::Number(value) if *value >= 0.0 => Some(*value as u64),
                _ => None,
            }
        }


        pub fn as_array(&self) -> Option<&[Value]> {
            match self {
                Value::Array(values) => Some(values),
                _ => None,
            }
        }

        pub fn is_object(&self) -> bool {
            matches!(self, Value::Object(_))
        }

        // whole-number floats print without a trailing .0
        pub fn stringify(&self) -> String {
            match self {
                Value::Null => "null".to_string(),
                Value::Bool(true) => "true".to_string(),
                Value::Bool(false) => "false".to_string(),
                Value::Number(value) => {
                    if value.fract() == 0.0 {
                        format!("{:.0}", value)
                    } else {
                        value.to_string()
                    }
                }
                Value::String(value) => string(value),
                Value::Array(values) => {
                    let inner = values
                        .iter()
                        .map(Value::stringify)
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("[{}]", inner)
                }
                Value::Object(fields) => {
                    let inner = fields
                        .iter()
                        .map(|(key, value)| format!("{}:{}", string(key), value.stringify()))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{{{}}}", inner)
                }
            }
        }
    }

    impl std::fmt::Display for Value {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.stringify())
        }
    }

    pub fn string(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for ch in value.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                ch if ch < ' ' => out.push_str(&format!("\\u{:04x}", ch as u32)),
                ch => out.push(ch),
            }
        }
        out.push('"');
        out
    }

    // values already stringified are passed through as-is
    pub fn object(fields: &[(&str, String)]) -> String {
        let inner = fields
            .iter()
            .map(|(key, value)| format!("{}:{}", string(key), value))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{{}}}", inner)
    }

    // key supports dotted paths (see value_at_path)
    pub fn find_string(input: &str, key: &str) -> Option<String> {
        let value = parse(input).ok()?;
        value_at_path(&value, key).and_then(value_to_string)
    }

    pub fn has_id(input: &str, expected_id: u64) -> bool {
        if let Ok(value) = parse(input) {
            if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                return id == expected_id;
            }
        }
        false
    }

    pub fn find_number(input: &str, key: &str) -> Option<f64> {
        let value = parse(input).ok()?;
        value_at_path(&value, key)?.as_f64()
    }

    // also accepts a lone string and wraps it as a one-item vec
    pub fn find_string_array(input: &str, key: &str) -> Option<Vec<String>> {
        let value = parse(input).ok()?;
        match value_at_path(&value, key)? {
            Value::Array(arr) => arr.iter().map(value_to_string).collect(),
            Value::String(s) => Some(vec![s.clone()]),
            _ => None,
        }
    }

    pub fn parse(input: &str) -> Result<Value, String> {
        let mut parser = Parser {
            input: input.as_bytes(),
            pos: 0,
        };
        let value = parser.parse_value()?;
        parser.skip_ws();
        if parser.pos != parser.input.len() {
            return Err("trailing JSON characters".to_string());
        }
        Ok(value)
    }

    // walk a dotted key like "a.b.c" down through nested objects
    fn value_at_path<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        let mut current = value;
        for part in key.split('.') {
            current = current.get(part)?;
        }
        Some(current)
    }

    fn value_to_string(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => {
                if n.fract() == 0.0 {
                    Some(format!("{:.0}", n))
                } else {
                    Some(n.to_string())
                }
            }
            Value::Bool(b) => Some(b.to_string()),
            Value::Null => None,
            _ => Some(value.stringify()),
        }
    }

    struct Parser<'a> {
        input: &'a [u8],
        pos: usize,
    }

    impl Parser<'_> {
        fn parse_value(&mut self) -> Result<Value, String> {
            self.skip_ws();
            match self.peek() {
                Some(b'"') => self.parse_string().map(Value::String),
                Some(b'{') => self.parse_object(),
                Some(b'[') => self.parse_array(),
                Some(b't') => {
                    self.expect_bytes(b"true")?;
                    Ok(Value::Bool(true))
                }
                Some(b'f') => {
                    self.expect_bytes(b"false")?;
                    Ok(Value::Bool(false))
                }
                Some(b'n') => {
                    self.expect_bytes(b"null")?;
                    Ok(Value::Null)
                }
                Some(b'-' | b'0'..=b'9') => self.parse_number().map(Value::Number),
                _ => Err("unexpected JSON value".to_string()),
            }
        }

        fn parse_object(&mut self) -> Result<Value, String> {
            self.expect(b'{')?;
            let mut fields = Vec::new();
            loop {
                self.skip_ws();
                if self.consume(b'}') {
                    break;
                }
                let key = self.parse_string()?;
                self.skip_ws();
                self.expect(b':')?;
                let value = self.parse_value()?;
                fields.push((key, value));
                self.skip_ws();
                if self.consume(b'}') {
                    break;
                }
                self.expect(b',')?;
            }
            Ok(Value::Object(fields))
        }

        fn parse_array(&mut self) -> Result<Value, String> {
            self.expect(b'[')?;
            let mut values = Vec::new();
            loop {
                self.skip_ws();
                if self.consume(b']') {
                    break;
                }
                values.push(self.parse_value()?);
                self.skip_ws();
                if self.consume(b']') {
                    break;
                }
                self.expect(b',')?;
            }
            Ok(Value::Array(values))
        }

        fn parse_string(&mut self) -> Result<String, String> {
            self.expect(b'"')?;
            let mut out = String::new();
            while let Some(byte) = self.next() {
                match byte {
                    b'"' => return Ok(out),
                    b'\\' => {
                        let escaped = self.next().ok_or_else(|| "bad JSON escape".to_string())?;
                        match escaped {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'/' => out.push('/'),
                            b'b' => out.push('\u{08}'),
                            b'f' => out.push('\u{0c}'),
                            b'n' => out.push('\n'),
                            b'r' => out.push('\r'),
                            b't' => out.push('\t'),
                            b'u' => {
                                let code = self.parse_hex4()?;
                                if let Some(ch) = char::from_u32(code) {
                                    out.push(ch);
                                }
                            }
                            _ => return Err("bad JSON escape".to_string()),
                        }
                    }
                    _ => out.push(byte as char),
                }
            }
            Err("unterminated JSON string".to_string())
        }

        // scan past the number's bytes then let std parse the slice
        fn parse_number(&mut self) -> Result<f64, String> {
            let start = self.pos;
            if self.peek() == Some(b'-') {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.peek() == Some(b'.') {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            if matches!(self.peek(), Some(b'e' | b'E')) {
                self.pos += 1;
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.pos += 1;
                }
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            std::str::from_utf8(&self.input[start..self.pos])
                .ok()
                .and_then(|text| text.parse::<f64>().ok())
                .ok_or_else(|| "bad JSON number".to_string())
        }

        // the 4 hex digits after a \u escape (note: doesn't pair up utf-16 surrogates)
        fn parse_hex4(&mut self) -> Result<u32, String> {
            if self.pos + 4 > self.input.len() {
                return Err("short JSON unicode escape".to_string());
            }
            let text = std::str::from_utf8(&self.input[self.pos..self.pos + 4])
                .map_err(|_| "bad JSON unicode escape".to_string())?;
            self.pos += 4;
            u32::from_str_radix(text, 16).map_err(|_| "bad JSON unicode escape".to_string())
        }

        fn skip_ws(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
                self.pos += 1;
            }
        }

        // expect_bytes matches a literal keyword (true/false/null); expect/consume work on one byte
        fn expect_bytes(&mut self, expected: &[u8]) -> Result<(), String> {
            if self.input.get(self.pos..self.pos + expected.len()) == Some(expected) {
                self.pos += expected.len();
                Ok(())
            } else {
                Err("unexpected JSON token".to_string())
            }
        }

        fn expect(&mut self, expected: u8) -> Result<(), String> {
            if self.consume(expected) {
                Ok(())
            } else {
                Err("unexpected JSON character".to_string())
            }
        }

        fn consume(&mut self, expected: u8) -> bool {
            if self.peek() == Some(expected) {
                self.pos += 1;
                true
            } else {
                false
            }
        }

        fn peek(&self) -> Option<u8> {
            self.input.get(self.pos).copied()
        }

        fn next(&mut self) -> Option<u8> {
            let byte = self.peek()?;
            self.pos += 1;
            Some(byte)
        }
    }
}