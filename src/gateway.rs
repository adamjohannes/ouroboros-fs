use crate::NodeStatus;
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
use tokio::process::Command;
use tokio::task::JoinHandle;

#[derive(Debug)]
pub struct Gateway {
    /// Full addresses
    node_addrs: Vec<String>,
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
        Arc::new(Self { node_addrs })
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

    // --- HTTP HANDLER ---

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
        let method = parts.get(0).cloned().unwrap_or("GET");
        let path = parts.get(1).cloned().unwrap_or("/");

        // Handle GET /file/pull/<filename>
        if method == "GET" && path.starts_with("/file/pull/") {
            return if let Some(filename) = path.strip_prefix("/file/pull/") {
                match self.handle_file_pull(writer, filename).await {
                    Ok(_) => Ok(()), // Full response was sent
                    Err(e) => Self::send_error_response(writer, 500, &e.to_string()).await,
                }
            } else {
                Self::send_error_response(writer, 400, "Bad Request: Missing filename").await
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
            ("POST", "/file/push") => match self.handle_file_upload(reader).await {
                Ok(_) => {
                    Self::send_json_response(writer, serde_json::json!({"status": "ok"})).await
                }
                Err(e) => Self::send_error_response(writer, 500, &e.to_string()).await,
            },
            ("POST", "/network/heal") => match self.trigger_node_heal().await {
                Ok(msg) => {
                    Self::send_json_response(writer, serde_json::json!({ "message": msg })).await
                }
                Err(e) => Self::send_error_response(writer, 500, &e.to_string()).await,
            },

            (method, path)
                if method == "POST" && path.starts_with("/node/") && path.ends_with("/kill") =>
            {
                // Handle POST /node/<port>/kill
                if let Some(port_str) = path
                    .strip_prefix("/node/")
                    .and_then(|p| p.strip_suffix("/kill"))
                {
                    match self.trigger_node_kill(port_str).await {
                        Ok(msg) => {
                            Self::send_json_response(writer, serde_json::json!({ "message": msg }))
                                .await
                        }
                        Err(e) => Self::send_error_response(writer, 500, &e.to_string()).await,
                    }
                } else {
                    Self::send_error_response(writer, 400, "Bad Request: Malformed kill URL").await
                }
            }
            _ => Self::send_error_response(writer, 404, "Not Found").await,
        }
    }

    /// Handles the `POST /api/upload` request
    async fn handle_file_upload<R>(
        self: Arc<Self>,
        reader: &mut BufReader<R>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        R: AsyncRead + Unpin,
    {
        // 1. Read headers to find Content-Length and X-Filename
        let mut content_length: u64 = 0;
        let mut filename: Option<String> = None;
        let mut line = String::new();

        loop {
            line.clear();
            if reader.read_line(&mut line).await? == 0 {
                break; // Premature end
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break; // End of headers
            }

            if let Some((key, value)) = trimmed.split_once(':') {
                let key_lower = key.to_ascii_lowercase();
                let value_trimmed = value.trim();

                if key_lower == "content-length" {
                    content_length = value_trimmed.parse().unwrap_or(0);
                }
                if key_lower == "x-filename" {
                    // Sanitize filename
                    let safe_name = value_trimmed.replace(
                        |c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-',
                        "_",
                    );
                    filename = Some(safe_name);
                }
            }
        }

        if content_length == 0 || filename.is_none() {
            return Err("Missing Content-Length or X-Filename header".into());
        }

        let filename = filename.unwrap();
        let size = content_length;

        tracing::info!(file = %filename, bytes = size, "Receiving file from HTTP POST");

        // 2. Read the raw body
        let mut file_body = vec![0; size as usize];
        reader.read_exact(&mut file_body).await?;

        // 3. Connect to the ring
        let mut node_stream = self.connect_to_ring().await?;

        // 4. Send the FILE PUSH command
        let header = format!("FILE PUSH {} {}\n", size, filename);
        node_stream.write_all(header.as_bytes()).await?;

        // 5. Stream the file body to the node
        node_stream.write_all(&file_body).await?;

        // 6. Wait for the "OK" from the node to confirm success
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

        // 3. Send the HTTP 200 OK and file headers to the browser
        Self::send_file_response_headers(writer, filename).await?;

        // 4. Stream the raw file data from the node directly to the browser
        copy(&mut node_read, writer).await?;

        Ok(())
    }

    // --- TCP PROXY HANDLER ---

    /// This is the proxy for all TCP commands
    async fn handle_tcp_proxy<R>(
        self: Arc<Self>,
        mut client_reader: BufReader<R>,
        mut client_writer: impl AsyncWrite + Unpin,
        first_line: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        R: AsyncRead + Unpin,
    {
        // 1. Connect to node
        let mut node_stream = self.connect_to_ring().await?;
        tracing::debug!(addr = ?node_stream.peer_addr(), "Gateway connected to ring node");

        // 2. Send the first line
        node_stream.write_all(first_line.as_bytes()).await?;

        // 3. Proxy all remaining data in both directions
        let (mut node_read, mut node_write) = node_stream.split();

        // `client_reader` is the BufReader, which will empty its
        // internal buffer first before reading from the underlying stream.
        let client_to_server = copy(&mut client_reader, &mut node_write);
        let server_to_client = copy(&mut node_read, &mut client_writer);

        // Use `try_join!` to wait for both halves to complete.
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

    // --- API DATA FETCHERS ---

    /// Sends a "NODE PING" to a single address and returns its status.
    ///
    /// This is a lightweight, best-effort check with a short timeout.
    async fn ping_node(addr: String) -> (String, NodeStatus) {
        let port = port_str(&addr).to_string();
        let timeout = Duration::from_millis(500);

        type AnyErr = Box<dyn std::error::Error + Send + Sync>;

        let check = async {
            // Connect with timeout
            let mut stream = tokio::time::timeout(timeout, TcpStream::connect(&addr)).await??;

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
            tasks.push(tokio::spawn(Self::ping_node(addr)));
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

    /// Finds a process by port and kills it. (Unix-specific)
    async fn trigger_node_kill(
        &self,
        port: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        tracing::info!(port = %port, "Gateway: Received request to kill node");

        // 1. Find PID using lsof
        let lsof_output = Command::new("lsof")
            .arg(format!("-iTCP:{}", port))
            .arg("-sTCP:LISTEN")
            .arg("-n")
            .arg("-P")
            .arg("-t")
            .output()
            .await?;

        if !lsof_output.status.success() {
            tracing::warn!(port = %port, error = ?String::from_utf8_lossy(&lsof_output.stderr), "lsof command failed");
            return Err(format!(
                "lsof failed to find port {}: {}",
                port,
                String::from_utf8_lossy(&lsof_output.stderr)
            )
            .into());
        }

        let pid_str = String::from_utf8(lsof_output.stdout)?.trim().to_string();
        if pid_str.is_empty() {
            tracing::warn!(port = %port, "No process found listening on port");
            return Err(format!("No process found on port {}", port).into());
        }

        // lsof might return multiple PIDs, just use the first line
        let pid = pid_str.lines().next().unwrap_or("").trim();
        if pid.is_empty() {
            return Err(format!("No PID found on port {}", port).into());
        }

        // 2. Kill the PID
        let kill_output = Command::new("kill")
            .arg(pid) // Send SIGTERM
            .output()
            .await?;

        if !kill_output.status.success() {
            tracing::error!(port = %port, pid = %pid, error = ?String::from_utf8_lossy(&kill_output.stderr), "kill command failed");
            return Err(format!(
                "kill failed for PID {}: {}",
                pid,
                String::from_utf8_lossy(&kill_output.stderr)
            )
            .into());
        }

        tracing::info!(port = %port, pid = %pid, "Successfully sent kill signal to node");
        Ok(format!("Killed node on port {} (PID: {})", port, pid))
    }

    // --- TCP HELPERS ---

    /// Tries all node addresses and returns a stream to the first one that connects.
    async fn connect_to_ring(&self) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
        for addr in &self.node_addrs {
            if let Ok(stream) = TcpStream::connect(addr).await {
                return Ok(stream);
            }
        }
        Err("Could not connect to any node in the ring".into())
    }

    // --- HTTP HELPERS ---

    /// Sends a 204 No Content response for OPTIONS preflight requests
    async fn send_options_response(writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let response = "HTTP/1.1 204 No Content\r\n\
                        Access-Control-Allow-Origin: *\r\n\
                        Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
                        Access-Control-Allow-Headers: Content-Type, X-Filename\r\n\
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
             Access-Control-Allow-Origin: *\r\n\
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
             Access-Control-Allow-Origin: *\r\n\
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
             Access-Control-Allow-Origin: *\r\n\
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
