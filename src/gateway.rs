use crate::NodeStatus;
use serde::Serialize;
use serde_json;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, copy,
};
use tokio::net::{TcpListener, TcpStream};

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
    pub async fn run_server(self: Arc<Self>, listen_addr: String) -> std::io::Result<()> {
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
            return Err("Node failed to store file: did not receive OK".to_string().into());
        }

        tracing::info!(file = %filename, "File successfully pushed to ring");
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

    /// Connects to the ring and sends `NETMAP GET`.
    async fn fetch_node_map(
        &self,
    ) -> Result<HashMap<String, NodeStatus>, Box<dyn std::error::Error + Send + Sync>> {
        let mut stream = self.connect_to_ring().await?;
        stream.write_all(b"NETMAP GET\n").await?;

        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        let mut map = HashMap::new();

        while reader.read_line(&mut line).await? > 0 {
            if line.starts_with("OK") {
                break;
            }
            if let Some((port, status_str)) = line.trim().split_once('=') {
                let status = if status_str.eq_ignore_ascii_case("Dead") {
                    NodeStatus::Dead
                } else {
                    NodeStatus::Alive
                };
                map.insert(port.to_string(), status);
            }
            line.clear();
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
