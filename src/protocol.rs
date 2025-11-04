//! Line-based text protocol for the ring server.
//!
//! Commands (one per line, newline-terminated):
//!   - "SET_NEXT <addr>"
//!   - "GET"
//!   - "RING <ttl> <message...>"
//!   - "WALK"                              (client -> start node)
//!   - "WALK HOP <token> <start> <hist>"   (node -> node; single line)
//!   - "WALK DONE <token> <hist>"          (last node -> start)
//!
//! Discovery / netmap (line-only, csv-ish payload):
//!   - "INVESTIGATE"                                        (client/tool -> start)
//!   - "INVEST HOP <token> <start_addr> <entries>"          (node -> node)
//!   - "INVEST DONE <token> <entries>"                      (last node -> start)
//!   - "NETMAP SET <entries>"                               (start -> every node)
//!   - "NETMAP GET"                                         (client -> any node; prints map)
//!
//! File transfer (line header + exact number of binary bytes):
//!   - "PUSH_FILE <size> <name>"                         (client -> start)
//!   - "FILE HOP <token> <start_addr> <size> <name>"     (node -> node)
//!   - "FILE PIPE HOP <token> <start_addr> <file_size> <parts> <index> <name>"
//!
//! File listing:
//!   - "LIST_FILES"  (client -> any; returns CSV from in-memory tags)
//!
//! File retrieval (client orchestrated via node-to-node chunk fetch):
//!   - "GET_FILE <name>"                 (client -> any node)
//!   - "CHUNK GET <name>"                (node -> node)
//!   - "CHUNK RESP <next_addr> <size> <name>"  (header line before <size> raw bytes)
//!
//! IMPORTANT: the protocol is line-delimited. Any binary payload *follows*
//! the header line and is exactly <size> bytes long.

/// Parsed representation of a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    // Core verbs
    SetNext(String), // SET_NEXT <addr>
    Get,             // GET
    Ring { ttl: u32, msg: String }, // RING <ttl> <message...>

    // WALK verbs
    WalkStart, // "WALK"
    WalkHop { token: String, start_addr: String, history: String },
    WalkDone { token: String, history: String },

    // INVESTIGATION verbs
    InvestigateStart, // "INVESTIGATE"
    InvestigateHop { token: String, start_addr: String, entries: String },
    InvestigateDone { token: String, entries: String },

    // Broadcast full map to all nodes
    NetmapSet { entries: String }, // "NETMAP SET <entries>"
    NetmapGet,                     // "NETMAP GET"

    // FILE verbs
    PushFile { size: u64, name: String }, // client -> start
    FileHop { token: String, start_addr: String, size: u64, name: String }, // node -> node
    FilePipeHop {
        token: String,
        start_addr: String,
        file_size: u64,
        parts: u32,
        index: u32,
        name: String,
    },

    // LIST (local)
    ListFiles, // "LIST_FILES"

    // FILE retrieval
    GetFile { name: String }, // "GET_FILE <name>" (client -> any)
    ChunkGet { name: String }, // "CHUNK GET <name>" (node -> node)
}

/// Parse one incoming line from the wire into a Command.
pub fn parse_line(line: &str) -> Result<Command, String> {
    let trimmed = line.trim_end_matches(['\r', '\n']);

    // SET_NEXT <addr>
    if let Ok(cmd) = set_next(trimmed) { return Ok(cmd); }

    // GET
    if trimmed.eq_ignore_ascii_case("GET") {
        return Ok(Command::Get);
    }

    // RING <ttl> <msg>
    if let Ok(cmd) = ring(trimmed) { return Ok(cmd); }

    // WALK
    if let Ok(cmd) = walk(trimmed) { return Ok(cmd); }

    // INVESTIGATE / NETMAP
    if let Ok(cmd) = investigate(trimmed) { return Ok(cmd); }

    // FILE PUSH / HOP
    if let Ok(cmd) = file_push(trimmed) { return Ok(cmd); }

    // LIST_FILES
    if trimmed.eq_ignore_ascii_case("LIST_FILES") {
        return Ok(Command::ListFiles);
    }

    // FILE retrieval
    if let Ok(cmd) = file_retrieval(trimmed) { return Ok(cmd); }

    Err("unknown command".into())
}

/* --- helpers --- */

fn set_next(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("SET_NEXT ") {
        let addr = rest.trim();
        if addr.is_empty() {
            return Err("missing address".into());
        }
        return Ok(Command::SetNext(addr.to_string()));
    }
    Err("wrong command".into())
}

fn ring(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("RING ") {
        let mut parts = rest.splitn(2, ' ');
        let ttl_str = parts.next().unwrap_or("").trim();
        let msg = parts.next().unwrap_or("").to_string();
        let ttl = ttl_str.parse::<u32>().map_err(|_| "invalid ttl")?;
        return Ok(Command::Ring { ttl, msg });
    }
    Err("wrong command".into())
}

fn walk(trimmed: &str) -> Result<Command, String> {
    if trimmed.eq_ignore_ascii_case("WALK") {
        return Ok(Command::WalkStart);
    }
    if let Ok(cmd) = walk_hop(trimmed) { return Ok(cmd); }
    if let Ok(cmd) = walk_done(trimmed) { return Ok(cmd); }
    Err("wrong command".into())
}

fn walk_hop(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("WALK HOP ") {
        let mut parts = rest.splitn(3, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        let history = parts.next().unwrap_or("").to_string();
        if token.is_empty() || start_addr.is_empty() {
            return Err("malformed WALK HOP".into());
        }
        return Ok(Command::WalkHop {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
            history,
        });
    }
    Err("wrong command".into())
}

fn walk_done(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("WALK DONE ") {
        let mut parts = rest.splitn(2, ' ');
        let token = parts.next().unwrap_or("").trim();
        let history = parts.next().unwrap_or("").to_string();
        if token.is_empty() {
            return Err("malformed WALK DONE".into());
        }
        return Ok(Command::WalkDone {
            token: token.to_string(),
            history,
        });
    }
    Err("wrong command".into())
}

fn investigate(trimmed: &str) -> Result<Command, String> {
    if trimmed.eq_ignore_ascii_case("INVESTIGATE") {
        return Ok(Command::InvestigateStart);
    }
    if let Ok(cmd) = invest_hop(trimmed) { return Ok(cmd); }
    if let Ok(cmd) = invest_done(trimmed) { return Ok(cmd); }
    if let Some(rest) = trimmed.strip_prefix("NETMAP SET ") {
        return Ok(Command::NetmapSet { entries: rest.trim().to_string() });
    }
    if trimmed.eq_ignore_ascii_case("NETMAP GET") {
        return Ok(Command::NetmapGet);
    }
    Err("wrong command".into())
}

fn invest_hop(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("INVEST HOP ") {
        let mut parts = rest.splitn(3, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        let entries = parts.next().unwrap_or("").to_string();
        if token.is_empty() || start_addr.is_empty() {
            return Err("malformed INVEST HOP".into());
        }
        return Ok(Command::InvestigateHop {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
            entries,
        });
    }
    Err("wrong command".into())
}

fn invest_done(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("INVEST DONE ") {
        let mut parts = rest.splitn(2, ' ');
        let token = parts.next().unwrap_or("").trim();
        let entries = parts.next().unwrap_or("").to_string();
        if token.is_empty() {
            return Err("malformed INVEST DONE".into());
        }
        return Ok(Command::InvestigateDone {
            token: token.to_string(),
            entries,
        });
    }
    Err("wrong command".into())
}

fn file_push(trimmed: &str) -> Result<Command, String> {
    if let Ok(cmd) = push_file(trimmed) { return Ok(cmd); }
    if let Ok(cmd) = file_hop(trimmed) { return Ok(cmd); }
    if let Ok(cmd) = file_pipe_hop(trimmed) { return Ok(cmd); }
    Err("wrong command".into())
}

fn push_file(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("PUSH_FILE ") {
        let mut parts = rest.splitn(2, ' ');
        let size_str = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").to_string();
        if name.is_empty() {
            return Err("missing file name".into());
        }
        let size = size_str.parse::<u64>().map_err(|_| "invalid size")?;
        return Ok(Command::PushFile { size, name });
    }
    Err("wrong command".into())
}

fn file_retrieval(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("GET_FILE ") {
        let name = rest.to_string();
        if name.trim().is_empty() { return Err("missing file name".into()); }
        return Ok(Command::GetFile { name });
    }
    if let Some(rest) = trimmed.strip_prefix("CHUNK GET ") {
        let name = rest.to_string();
        if name.trim().is_empty() { return Err("missing file name".into()); }
        return Ok(Command::ChunkGet { name });
    }
    Err("wrong command".into())
}

fn file_hop(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("FILE HOP ") {
        let mut parts = rest.splitn(4, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        let size_str = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").to_string();
        if token.is_empty() || start_addr.is_empty() || name.is_empty() {
            return Err("malformed FILE HOP".into());
        }
        let size = size_str.parse::<u64>().map_err(|_| "invalid size")?;
        return Ok(Command::FileHop {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
            size,
            name,
        });
    }
    Err("wrong command".into())
}

fn file_pipe_hop(trimmed: &str) -> Result<Command, String> {
    if let Some(rest) = trimmed.strip_prefix("FILE PIPE HOP ") {
        let mut parts = rest.splitn(6, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        let file_size_str = parts.next().unwrap_or("").trim();
        let total_parts_str = parts.next().unwrap_or("").trim();
        let index_str = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").to_string();
        if token.is_empty() || start_addr.is_empty() || name.is_empty() {
            return Err("malformed FILE PIPE HOP".into());
        }
        let file_size = file_size_str.parse::<u64>().map_err(|_| "invalid file_size")?;
        let parts_u = total_parts_str.parse::<u32>().map_err(|_| "invalid parts")?;
        let index = index_str.parse::<u32>().map_err(|_| "invalid index")?;
        return Ok(Command::FilePipeHop {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
            file_size,
            parts: parts_u,
            index,
            name,
        });
    }
    Err("wrong command".into())
}
