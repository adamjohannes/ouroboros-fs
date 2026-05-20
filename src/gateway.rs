use crate::NodeStatus;
use crate::auth::AuthToken;
use crate::node::port_str;
use serde::Serialize;
use serde_json;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, copy,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Reject Content-Length above this ceiling before opening a ring connection.
/// 50 GB is generous enough that no legitimate upload hits it; an attacker
/// declaring `u64::MAX` short-circuits without ever allocating.
const MAX_REASONABLE_BYTES: u64 = 50 * 1024 * 1024 * 1024;

/// Tag used so `handle_http_request` can map oversized-body errors to HTTP 413
/// instead of the generic 500.
const OVERSIZED_ERR_TAG: &str = "oversized:";

#[derive(Debug)]
pub struct Gateway {
    /// Full addresses
    node_addrs: Vec<String>,
    /// Bearer token for HTTP clients AND PSK for ring outbound. The gateway
    /// is the bridge: HTTP clients authenticate to it via `Authorization:
    /// Bearer <hex>`, and the gateway uses the same secret to AUTH onto
    /// every ring node. (See `auth::AuthToken`.)
    auth_token: AuthToken,
}

/// HTTP Response Struct
#[derive(Serialize)]
struct FileInfo {
    name: String,
    start: u16,
    size: u64,
}

impl Gateway {
    pub fn new(node_addrs: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            node_addrs,
            auth_token: AuthToken::disabled(),
        })
    }

    pub fn with_auth(node_addrs: Vec<String>, auth_token: AuthToken) -> Arc<Self> {
        Arc::new(Self {
            node_addrs,
            auth_token,
        })
    }

    /// Runs the main TCP server to listen for clients
    pub async fn run_server(self: Arc<Self>, listen_addr: String) -> io::Result<()> {
        let listener = TcpListener::bind(&listen_addr).await?;
        tracing::info!(addr = %listen_addr, "Gateway listening (HTTP + TCP)");

        loop {
            let (client_stream, client_addr) = listener.accept().await?;
            let gateway_clone = Arc::clone(&self);

            tokio::spawn(async move {
                if let Err(e) = gateway_clone.handle_connection(client_stream).await {
                    tracing::warn!(client = %client_addr, error = ?e, "Gateway client error");
                }
            });
        }
    }

    /// An implementation of a protocol sniffer.
    async fn handle_connection(
        self: Arc<Self>,
        stream: TcpStream,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);

        // 1. Read the first line to sniff the protocol.
        let mut first_line = String::new();
        if let Err(e) = buf_reader.read_line(&mut first_line).await {
            tracing::debug!(error = ?e, "Client disconnected before sending data");
            return Ok(());
        }

        // 2. Check if the protocol is HTTP raw TCP
        if first_line.starts_with("GET /")
            || first_line.starts_with("POST /")
            || first_line.starts_with("OPTIONS /")
        {
            // Handle HTTP request
            tracing::debug!(line = %first_line.trim(), "Handling HTTP request");
            self.handle_http_request(&mut buf_reader, &mut writer, &first_line)
                .await?;
        } else {
            // Handle raw TCP
            tracing::debug!(line = %first_line.trim(), "Handling TCP proxy");
            self.handle_tcp_proxy(buf_reader, writer, &first_line)
                .await?;
        }
        Ok(())
    }

    // --- HTTP Handler

    async fn handle_http_request<R>(
        self: Arc<Self>,
        reader: &mut BufReader<R>,
        writer: &mut (impl AsyncWrite + Unpin),
        first_line: &str,
    ) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        let method = parts.first().cloned().unwrap_or("GET");
        let path = parts.get(1).cloned().unwrap_or("/");

        // Drain HTTP headers up front. Auth (and CORS preflight) decisions
        // happen here, before any handler-specific code runs. The body (if
        // any) is whatever is left in `reader` after the empty line.
        let headers = match Self::read_http_headers(reader).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = ?e, "Gateway: malformed HTTP headers");
                return Self::send_error_response(writer, 400, "Bad Request: malformed headers")
                    .await;
            }
        };

        // OPTIONS requests do NOT require auth (browsers send them as
        // preflight without credentials by design). Liveness/readiness
        // probes likewise don't carry credentials by default in
        // Kubernetes/Nomad — they short-circuit before the auth check.
        // All other methods require a valid bearer.
        if method == "GET" && (path == "/health" || path == "/ready") {
            return self.handle_health_request(writer, path).await;
        }
        // /metrics is bearer-protected (it leaks per-node activity counters
        // a casual scanner shouldn't see). Counterargument: most prom
        // scrapers can't easily carry a Bearer either. We follow the
        // /file/list convention and require auth — operators add the
        // token to the scrape config.
        if method != "OPTIONS"
            && !self
                .auth_token
                .verify_bearer(headers.get("authorization").map(String::as_str))
        {
            return Self::send_error_response(writer, 401, "Unauthorized").await;
        }

        if method == "GET" && path == "/metrics" {
            return self.handle_metrics_request(writer).await;
        }

        // Handle GET /file/pull/<filename>
        if method == "GET" && path.starts_with("/file/pull/") {
            // Reject empty filename (`/file/pull/` with nothing after the
            // last slash) at the gateway, before connecting to the ring.
            // Same for any traversal-shaped or otherwise-rejected name —
            // the ring would also reject it but we save the round-trip.
            // (NEXT_STEPS.md §3.2.)
            let filename = path.strip_prefix("/file/pull/").unwrap_or("");
            if filename.is_empty() {
                return Self::send_error_response(writer, 404, "Not Found: missing filename").await;
            }
            return match self.handle_file_pull(writer, filename).await {
                Ok(_) => Ok(()), // Full response was sent
                Err(e) => {
                    let msg = e.to_string();
                    let status = if msg.starts_with("not found:") {
                        404
                    } else {
                        500
                    };
                    Self::send_error_response(writer, status, &msg).await
                }
            };
        }

        match (method, path) {
            ("OPTIONS", _) => {
                // Handle CORS preflight requests
                Self::send_options_response(writer).await
            }
            ("GET", "/netmap/get") => match self.fetch_node_map().await {
                Ok(map) => Self::send_json_response(writer, &map).await,
                Err(e) => Self::send_error_response(writer, 500, &e.to_string()).await,
            },
            ("GET", "/file/list") => match self.fetch_file_list().await {
                Ok(list) => Self::send_json_response(writer, &list).await,
                Err(e) => Self::send_error_response(writer, 500, &e.to_string()).await,
            },
            ("POST", "/file/push") => match self.handle_file_upload(reader, &headers).await {
                Ok(_) => {
                    Self::send_json_response(writer, serde_json::json!({"status": "ok"})).await
                }
                Err(e) => {
                    let msg = e.to_string();
                    let (status, body) = if let Some(rest) = msg.strip_prefix(OVERSIZED_ERR_TAG) {
                        (413u16, rest.to_string())
                    } else {
                        (500u16, msg)
                    };
                    Self::send_error_response(writer, status, &body).await
                }
            },
            ("POST", "/network/heal") => match self.trigger_node_heal().await {
                Ok(msg) => {
                    Self::send_json_response(writer, serde_json::json!({ "message": msg })).await
                }
                Err(e) => Self::send_error_response(writer, 500, &e.to_string()).await,
            },
            // The kill endpoint was removed in Series C: it was a remote
            // RCE primitive (it ran `kill` on a PID derived from `lsof`)
            // that operators don't need (SSH + pkill exists). Return 404.
            _ => Self::send_error_response(writer, 404, "Not Found").await,
        }
    }

    /// `/health` — 200 always. Reaching this handler means the gateway's
    /// accept loop is responsive; liveness probes shouldn't ask for more.
    /// `/ready` — 200 if ≥ 1 ring node currently PONGs, 503 otherwise.
    /// Used by orchestrators to gate traffic until the gateway can serve
    /// at least one request. (NEXT_STEPS.md §4.4.)
    async fn handle_health_request(
        self: Arc<Self>,
        writer: &mut (impl AsyncWrite + Unpin),
        path: &str,
    ) -> io::Result<()> {
        if path == "/health" {
            return Self::send_json_response(writer, serde_json::json!({"status": "ok"})).await;
        }
        // /ready
        let map_result = self.fetch_node_map().await;
        let any_alive = match map_result {
            Ok(map) => map.values().any(|s| matches!(s, NodeStatus::Alive)),
            Err(_) => false,
        };
        if any_alive {
            Self::send_json_response(writer, serde_json::json!({"ready": true})).await
        } else {
            Self::send_error_response(writer, 503, "no ring nodes alive").await
        }
    }

    /// `/metrics` — Prometheus text format aggregated across all ring
    /// nodes. Each per-node counter is emitted with a `node="<port>"`
    /// label so scrapers can sum across the cluster while keeping the
    /// per-node breakdown. Unreachable nodes are skipped (their metrics
    /// just don't appear); the gateway never serves stale cached data.
    /// (NEXT_STEPS.md §4.2.)
    async fn handle_metrics_request(
        self: Arc<Self>,
        writer: &mut (impl AsyncWrite + Unpin),
    ) -> io::Result<()> {
        let per_node = self.fetch_metrics().await;
        let body = Self::render_prometheus(&per_node);
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain; version=0.0.4\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            body.len(),
            body
        );
        writer.write_all(response.as_bytes()).await
    }

    /// Hit every node concurrently with `NODE METRICS`; collect parsed
    /// `<key>=<value>` lines. Nodes that fail to respond are silently
    /// dropped — their absence in the output is the signal.
    async fn fetch_metrics(&self) -> Vec<(String, Vec<(String, u64)>)> {
        type ScrapeTask = JoinHandle<Option<(String, Vec<(String, u64)>)>>;
        let mut tasks: Vec<ScrapeTask> = Vec::new();
        for addr in self.node_addrs.clone() {
            let token = self.auth_token.clone();
            tasks.push(tokio::spawn(Self::scrape_node_metrics(addr, token)));
        }
        let mut out = Vec::new();
        for t in tasks {
            if let Ok(Some(entry)) = t.await {
                out.push(entry);
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    async fn scrape_node_metrics(
        addr: String,
        token: AuthToken,
    ) -> Option<(String, Vec<(String, u64)>)> {
        let timeout = Duration::from_millis(500);
        let port = port_str(&addr).to_string();
        let lines = tokio::time::timeout(timeout, async {
            let mut s = TcpStream::connect(&addr).await.ok()?;
            if let Some(line) = token.make_auth_line() {
                s.write_all(line.as_bytes()).await.ok()?;
            }
            s.write_all(b"NODE METRICS\n").await.ok()?;
            let (r, _w) = s.split();
            let mut reader = BufReader::new(r);
            let mut accum: Vec<(String, u64)> = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await.ok()?;
                if n == 0 {
                    break;
                }
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed == "OK" {
                    break;
                }
                if let Some((k, v)) = trimmed.split_once('=')
                    && let Ok(val) = v.parse::<u64>()
                {
                    accum.push((k.to_string(), val));
                }
            }
            Some(accum)
        })
        .await
        .ok()
        .flatten()?;
        Some((port, lines))
    }

    /// line. Returns lowercased keys → trimmed values. The reader is left
    /// positioned at the first byte of the body (if any).
    async fn read_http_headers<R>(reader: &mut BufReader<R>) -> io::Result<HashMap<String, String>>
    where
        R: AsyncRead + Unpin,
    {
        const MAX_HEADERS: usize = 64;
        const MAX_LINE_LEN: usize = 8 * 1024;
        let mut headers = HashMap::new();
        let mut line = String::new();
        for _ in 0..MAX_HEADERS {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break;
            }
            if n > MAX_LINE_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "header line too long",
                ));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                return Ok(headers);
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many or unterminated headers",
        ))
    }

    /// Handles the `POST /file/push` request. Headers have been pre-parsed
    /// by `handle_http_request`; the reader is positioned at the body.
    async fn handle_file_upload<R>(
        self: Arc<Self>,
        reader: &mut BufReader<R>,
        headers: &HashMap<String, String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        R: AsyncRead + Unpin,
    {
        let content_length: u64 = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let filename = headers.get("x-filename").map(|raw| {
            raw.replace(
                |c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-',
                "_",
            )
        });

        let Some(filename) = filename else {
            return Err("Missing X-Filename header".into());
        };
        if content_length == 0 {
            return Err("Missing Content-Length header".into());
        }

        // Reject absurd Content-Length before allocating *or* opening a ring
        // connection. The ring node also rejects oversized PUSH (server-side
        // bound), but stopping here saves an allocation and a hop.
        if content_length > MAX_REASONABLE_BYTES {
            return Err(format!(
                "{}Content-Length {} exceeds {} bytes",
                OVERSIZED_ERR_TAG, content_length, MAX_REASONABLE_BYTES
            )
            .into());
        }

        let size = content_length;

        tracing::info!(file = %filename, bytes = size, "Receiving file from HTTP POST");

        // 2. Connect to the ring before reading the body, so we don't buffer
        //    a 50 GB upload in RAM if the ring is already unreachable.
        let mut node_stream = self.connect_to_ring().await?;

        // 3. Send the FILE PUSH command.
        let header = format!("FILE PUSH {} {}\n", size, filename);
        node_stream.write_all(header.as_bytes()).await?;

        // 4. Stream the body straight from the HTTP reader to the node.
        //    `tokio::io::copy` uses a fixed internal buffer; no O(size) alloc.
        let mut limited = (&mut *reader).take(size);
        copy(&mut limited, &mut node_stream).await?;

        // 5. Wait for the "OK" from the node to confirm success
        let mut node_reader = BufReader::new(node_stream);
        let mut node_response = String::new();
        let mut found_ok = false;

        // Read lines until we get an "OK" or the stream ends
        while node_reader.read_line(&mut node_response).await? > 0 {
            if node_response.starts_with("OK") {
                found_ok = true;
                break;
            }
            node_response.clear(); // Clear for next line
        }

        if !found_ok {
            return Err("Node failed to store file: did not receive OK"
                .to_string()
                .into());
        }

        tracing::info!(file = %filename, "File successfully pushed to ring");
        Ok(())
    }

    /// Connects to the ring and streams a file back to an HTTP client.
    ///
    /// Detects the §3.1 truncation trailer (`\nERR truncated …\n`) at EOF
    /// of the ring response. The trailer is *appended* to whatever body
    /// bytes the ring could recover, so we maintain a small lookbehind
    /// buffer (max trailer length) and only emit older bytes; on EOF we
    /// inspect the lookbehind and strip the trailer before flushing the
    /// remainder. Aware HTTP clients can detect truncation via the
    /// `X-Ouroboros-Truncated` *body suffix* — we can't add a real HTTP
    /// trailer header without chunked encoding, and we can't change the
    /// status code after `send_file_response_headers` already wrote 200.
    /// Logging the failure server-side is the most actionable signal here.
    async fn handle_file_pull(
        self: Arc<Self>,
        writer: &mut (impl AsyncWrite + Unpin),
        filename: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // 1. Connect to a node in the ring
        let mut node_stream = self.connect_to_ring().await?;
        let (mut node_read, mut node_write) = node_stream.split();

        // 2. Send TCP FILE PULL to the node
        let header = format!("FILE PULL {}\n", filename);
        node_write.write_all(header.as_bytes()).await?;
        node_write.shutdown().await?;

        // 3. Sniff the first few bytes from the ring. If it leads with
        //    `ERR `, treat as a structured error (404 for "file not
        //    found", 500 for everything else) and DO NOT emit 200
        //    headers. Otherwise the bytes are body content; we write
        //    them through and stream the rest. (NEXT_STEPS.md §3.2.)
        let mut prefix = [0u8; 4];
        let n = Self::read_exact_or_eof(&mut node_read, &mut prefix).await?;
        if n >= 4 && &prefix == b"ERR " {
            let mut rest = String::new();
            tokio::io::AsyncBufReadExt::read_line(
                &mut tokio::io::BufReader::new(&mut node_read),
                &mut rest,
            )
            .await?;
            let msg = rest.trim_end_matches(['\r', '\n']);
            let combined = format!("ERR {msg}");
            return if combined.contains("file not found") {
                Err(format!("not found: {combined}").into())
            } else {
                Err(combined.into())
            };
        }

        // 4. Send the HTTP 200 OK and file headers to the browser
        Self::send_file_response_headers(writer, filename).await?;

        // 5. Flush the sniffed prefix, then stream the rest with a
        //    lookbehind window for the truncation trailer at EOF.
        if n > 0 {
            writer.write_all(&prefix[..n]).await?;
        }
        let truncated = Self::stream_with_truncation_detection(&mut node_read, writer).await?;
        if truncated {
            tracing::error!(
                file = %filename,
                "Gateway: detected PULL truncation trailer; HTTP body is short"
            );
        }
        Ok(())
    }

    /// Read up to `buf.len()` bytes; return how many were filled. Differs
    /// from `read_exact` in that EOF before filling is not an error — the
    /// caller decides. Used for the FILE PULL response sniff.
    async fn read_exact_or_eof<R: AsyncRead + Unpin>(
        src: &mut R,
        buf: &mut [u8],
    ) -> std::io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = src.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(filled)
    }

    /// Render a Prometheus 0.0.4 text-format response from per-node metric
    /// pairs. Each `<key>=<value>` line in the input becomes a labeled
    /// metric; the *_total naming is preserved (Prometheus convention for
    /// counters). The gauge `ouroboros_dead_nodes` gets a `# TYPE gauge`
    /// hint; everything else is a counter.
    fn render_prometheus(per_node: &[(String, Vec<(String, u64)>)]) -> String {
        use std::collections::BTreeMap;

        // Group by metric name across nodes for stable HELP/TYPE emission.
        let mut by_metric: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
        for (port, kvs) in per_node {
            for (k, v) in kvs {
                if k == "port" {
                    continue;
                }
                by_metric
                    .entry(k.clone())
                    .or_default()
                    .push((port.clone(), *v));
            }
        }

        let mut out = String::new();
        for (name, samples) in &by_metric {
            let metric_name = format!("ouroboros_{name}");
            let mtype = if name.ends_with("_total") {
                "counter"
            } else {
                "gauge"
            };
            out.push_str(&format!("# HELP {metric_name} OuroborosFS {name}\n"));
            out.push_str(&format!("# TYPE {metric_name} {mtype}\n"));
            for (port, val) in samples {
                out.push_str(&format!("{metric_name}{{node=\"{port}\"}} {val}\n"));
            }
        }
        out
    }

    /// `\nERR truncated expected=<u64> got=<u64>\n`. Two u64s in decimal cap
    /// at 20 digits each; the literal text adds 32 bytes; round up to 96.
    const MAX_TRAILER_LEN: usize = 96;

    /// Marker prefix the trailer always starts with.
    const TRAILER_MARKER: &[u8] = b"\nERR truncated ";

    /// Stream `src` to `dst` while keeping a lookbehind window large enough to
    /// strip a trailing `\nERR truncated …\n` line if present. Returns `true`
    /// if a trailer was detected and stripped (caller logs/surfaces the
    /// failure), `false` for a clean stream.
    ///
    /// The implementation is deliberately simple: append into a `Vec`, flush
    /// everything except the last MAX_TRAILER_LEN bytes after each read, and
    /// on EOF inspect the tail. The held-back window is constant-bounded so
    /// memory is O(1) regardless of file size; the I/O pattern is the same as
    /// `tokio::io::copy` modulo the small final flush.
    async fn stream_with_truncation_detection<R, W>(
        src: &mut R,
        dst: &mut W,
    ) -> std::io::Result<bool>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut tail: Vec<u8> = Vec::with_capacity(Self::MAX_TRAILER_LEN * 2);
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = src.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > Self::MAX_TRAILER_LEN {
                let flush_until = tail.len() - Self::MAX_TRAILER_LEN;
                dst.write_all(&tail[..flush_until]).await?;
                tail.drain(..flush_until);
            }
        }
        // EOF. Inspect the lookbehind for a trailer.
        if let Some(pos) = tail
            .windows(Self::TRAILER_MARKER.len())
            .rposition(|w| w == Self::TRAILER_MARKER)
        {
            // Trailer present; flush only the body bytes before it.
            dst.write_all(&tail[..pos]).await?;
            Ok(true)
        } else {
            // No trailer; flush whatever's left.
            dst.write_all(&tail).await?;
            Ok(false)
        }
    }

    async fn handle_tcp_proxy<R>(
        self: Arc<Self>,
        mut client_reader: BufReader<R>,
        mut client_writer: impl AsyncWrite + Unpin,
        first_line: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        R: AsyncRead + Unpin,
    {
        // 1. Connect to node (with AUTH already sent by connect_to_ring).
        let mut node_stream = self.connect_to_ring().await?;
        tracing::debug!(addr = ?node_stream.peer_addr(), "Gateway connected to ring node");

        // 2. Send the first line (the request the client sent us).
        node_stream.write_all(first_line.as_bytes()).await?;

        // 3. Bidirectional copy with explicit half-shutdown.
        //
        // The original deadlock (NEXT_STEPS.md §3.3): both halves of
        // `try_join!(client→server, server→client)` only return on EOF
        // of their reader. The ring's `handle_client` is a loop — it
        // doesn't close after one command — so it only EOFs when the
        // gateway shuts down the write half. The fix: as soon as the
        // client→server copy completes (the client shut down its write
        // half, signaling "request done"), explicitly shut down our
        // node_write half. The ring then sees EOF, exits its read loop,
        // closes its write half, and the server→client copy returns.
        let (mut node_read, mut node_write) = node_stream.split();
        let client_to_server = async {
            let r = copy(&mut client_reader, &mut node_write).await;
            // Shut down the write half regardless of copy result so the
            // ring sees a clean EOF and stops looping. The shutdown can
            // race the copy itself (both write through node_write), but
            // tokio's AsyncWriteExt::shutdown is idempotent w.r.t. error
            // semantics — failure is logged-and-swallowed.
            let _ = node_write.shutdown().await;
            r
        };
        let server_to_client = copy(&mut node_read, &mut client_writer);

        match tokio::try_join!(client_to_server, server_to_client) {
            Ok(_) => {
                tracing::debug!("TCP proxy finished successfully.");
            }
            Err(e) => {
                tracing::debug!(error = ?e, "TCP proxy finished with error");
            }
        }

        Ok(())
    }

    // --- API Data Fetchers

    /// Sends a "NODE PING" to a single address and returns its status.
    ///
    /// This is a lightweight, best-effort check with a short timeout.
    async fn ping_node(addr: String, token: AuthToken) -> (String, NodeStatus) {
        let port = port_str(&addr).to_string();
        let timeout = Duration::from_millis(500);

        type AnyErr = Box<dyn std::error::Error + Send + Sync>;

        let check = async {
            // Connect with timeout
            let mut stream = tokio::time::timeout(timeout, TcpStream::connect(&addr)).await??;

            // Authenticate before any protocol command (no-op when disabled).
            if let Some(line) = token.make_auth_line() {
                stream.write_all(line.as_bytes()).await?;
            }

            // Send the PING command
            stream.write_all(b"NODE PING\n").await?;

            // Read response with timeout
            let mut reader = BufReader::new(stream);
            let mut buf = String::new();
            tokio::time::timeout(timeout, reader.read_line(&mut buf)).await??;

            if buf.trim().eq_ignore_ascii_case("PONG") {
                Ok::<NodeStatus, AnyErr>(NodeStatus::Alive)
            } else {
                // Got a response, but it wasn't a valid PONG
                Ok::<NodeStatus, AnyErr>(NodeStatus::Dead)
            }
        };

        // Any error (timeout, connection refused, read fail...) means the node is Dead
        match check.await {
            Ok(status) => (port, status),
            Err(_) => (port, NodeStatus::Dead),
        }
    }

    /// Checks the real-time status of all nodes by pinging them concurrently.
    async fn fetch_node_map(
        &self,
    ) -> Result<HashMap<String, NodeStatus>, Box<dyn std::error::Error + Send + Sync>> {
        let mut tasks: Vec<JoinHandle<(String, NodeStatus)>> = Vec::new();

        // 1. Spawn a concurrent ping task for every node address we know
        for addr in self.node_addrs.clone() {
            let token = self.auth_token.clone();
            tasks.push(tokio::spawn(Self::ping_node(addr, token)));
        }

        let mut map = HashMap::new();

        // 2. Collect the results from all completed tasks
        for task in tasks {
            match task.await {
                Ok((port, status)) => {
                    map.insert(port, status);
                }
                Err(e) => {
                    tracing::error!(error = ?e, "A gateway ping task failed (panicked)");
                }
            }
        }

        Ok(map)
    }

    /// Connects to the ring and sends `FILE LIST`.
    async fn fetch_file_list(
        &self,
    ) -> Result<Vec<FileInfo>, Box<dyn std::error::Error + Send + Sync>> {
        let mut stream = self.connect_to_ring().await?;
        stream.write_all(b"FILE LIST\n").await?;

        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        let mut files = Vec::new();

        // Skip CSV header
        let _ = reader.read_line(&mut line).await?;
        line.clear();

        while reader.read_line(&mut line).await? > 0 {
            if line.trim().is_empty() {
                break;
            }
            let parts: Vec<&str> = line.trim().splitn(3, ',').collect();
            if parts.len() == 3 {
                // Handle CSV escaping
                let name = parts[0].trim_matches('\"');

                files.push(FileInfo {
                    name: name.to_string(),
                    start: parts[1].parse().unwrap_or(0),
                    size: parts[2].parse().unwrap_or(0),
                });
            }
            line.clear();
        }
        Ok(files)
    }

    /// Connects to the ring, sends "NODE HEAL", and waits for the full response.
    async fn trigger_node_heal(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // 1. Connect to a node in the ring
        let mut stream = self.connect_to_ring().await?;
        tracing::info!("Gateway: Sending NODE HEAL to ring");

        // 2. Send the TCP NODE HEAL command
        stream.write_all(b"NODE HEAL\n").await?;

        // 3. Read the response
        // `handle_node_heal` in server.rs can take up to 60s
        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        let gateway_timeout = Duration::from_secs(65);

        match tokio::time::timeout(gateway_timeout, reader.read_line(&mut response_line)).await {
            Ok(Ok(0)) => Err("Node disconnected without response".into()),
            Ok(Ok(_)) => Ok(response_line.trim().to_string()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err("Gateway timed out waiting for NODE HEAL response".into()),
        }
    }

    // --- TCP Helpers

    /// Tries all node addresses and returns a stream to the first one that
    /// connects, having already sent the wire-protocol AUTH line on it.
    async fn connect_to_ring(&self) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
        for addr in &self.node_addrs {
            if let Ok(mut stream) = TcpStream::connect(addr).await {
                if let Some(line) = self.auth_token.make_auth_line()
                    && let Err(e) = stream.write_all(line.as_bytes()).await
                {
                    tracing::warn!(node = %addr, error = ?e, "Gateway: failed to send AUTH; trying next node");
                    continue;
                }
                return Ok(stream);
            }
        }
        Err("Could not connect to any node in the ring".into())
    }

    // --- HTTP Helpers
    //
    // For an internal-only deployment we DO NOT emit
    // `Access-Control-Allow-Origin: *`. Browsers should refuse cross-origin
    // reads of the gateway. If a future dashboard needs CORS, it can be
    // re-added with a specific allowed origin (not wildcard).

    /// Sends a 204 No Content response for OPTIONS preflight requests
    async fn send_options_response(writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let response = "HTTP/1.1 204 No Content\r\n\
                        Allow: POST, GET, OPTIONS\r\n\
                        Connection: close\r\n\
                        \r\n";
        writer.write_all(response.as_bytes()).await
    }

    /// Sends HTTP headers for a file pull.
    async fn send_file_response_headers(
        writer: &mut (impl AsyncWrite + Unpin),
        filename: &str,
    ) -> io::Result<()> {
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"{}\"\r\n\
             Connection: close\r\n\
             \r\n",
            filename
        );
        writer.write_all(response.as_bytes()).await
    }

    async fn send_json_response<T: Serialize>(
        writer: &mut (impl AsyncWrite + Unpin),
        data: T,
    ) -> io::Result<()> {
        let json = serde_json::to_string(&data).unwrap_or("{}".to_string());
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            json.len(),
            json
        );
        writer.write_all(response.as_bytes()).await
    }

    async fn send_error_response(
        writer: &mut (impl AsyncWrite + Unpin),
        status: u16,
        message: &str,
    ) -> io::Result<()> {
        let response = format!(
            "HTTP/1.1 {} {}\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            status,
            message,
            message.len(),
            message
        );
        writer.write_all(response.as_bytes()).await
    }
}
