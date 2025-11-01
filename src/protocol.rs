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
//! IMPORTANT: the protocol is line-delimited. The WALK history is therefore
//! encoded on a **single line** using semicolons, e.g.
//!   7001->7002;7002->7003;7003->7001
//! Only when the start node replies to the client do we render it with \n.

/// Parsed representation of a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    // Existing verbs
    SetNext(String),                   // SET_NEXT <addr>
    Get,                               // GET
    Ring { ttl: u32, msg: String },   // RING <ttl> <message...>

    // WALK verbs
    WalkStart,                                             // "WALK"
    WalkHop { token: String, start_addr: String, history: String }, // "WALK HOP ..."
    WalkDone { token: String, history: String },           // "WALK DONE ..."
}

/// Parse one incoming line from the wire into a Command.
/// Returns an error string if the command is unknown or malformed.
pub fn parse_line(line: &str) -> Result<Command, String> {
    // 1) Trim typical line endings
    let trimmed = line.trim_end_matches(['\r', '\n']);

    // 2) SET_NEXT <addr>
    if let Some(rest) = trimmed.strip_prefix("SET_NEXT ") {
        let addr = rest.trim();
        if addr.is_empty() { return Err("missing address".into()); }
        return Ok(Command::SetNext(addr.to_string()));
    }

    // 3) GET
    if trimmed == "GET" {
        return Ok(Command::Get);
    }

    // 4) RING <ttl> <message...>
    if let Some(rest) = trimmed.strip_prefix("RING ") {
        let mut parts = rest.splitn(2, ' ');
        let ttl_str = parts.next().unwrap_or("");
        let msg = parts.next().unwrap_or("").trim().to_string();
        let ttl = ttl_str.parse::<u32>().map_err(|_| "invalid ttl")?;
        return Ok(Command::Ring { ttl, msg });
    }

    // 5) WALK (client -> start)
    if trimmed == "WALK" {
        return Ok(Command::WalkStart);
    }

    // 6) WALK HOP <token> <start_addr> <history>
    // Use splitn(3, ' ') to preserve spaces inside <history> (even though we use ';')
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

    // 7) WALK DONE <token> <history>
    if let Some(rest) = trimmed.strip_prefix("WALK DONE ") {
        let mut parts = rest.splitn(2, ' ');
        let token = parts.next().unwrap_or("").trim();
        let history = parts.next().unwrap_or("").to_string();
        if token.is_empty() {
            return Err("malformed WALK DONE".into());
        }
        return Ok(Command::WalkDone { token: token.to_string(), history });
    }

    // 8) Unknown verb
    Err("unknown command".into())
}
