//! Line-based text protocol for the ring server.
//!
//! Commands are namespaced: <NOUN> <VERB> [params...]
//!
//! NODE
//!   - "NODE NEXT <addr>" (client -> any node)
//!   - "NODE STATUS"      (client -> any node)
//!   - "NODE PING"        (node -> node)
//!   - "NODE HEAL"        (client -> any node)
//!   - "NODE HEAL-HOP <token> <start_addr>" (node -> node)
//!   - "NODE HEAL-DONE <token>"             (last node -> start node)
//!
//! RING
//!   - "RING FORWARD <ttl> <message...>"
//!
//! TOPOLOGY
//!   - "TOPOLOGY WALK"                       (client -> start node)
//!   - "TOPOLOGY HOP <token> <start> <hist>" (node -> node; single line)
//!   - "TOPOLOGY DONE <token> <hist>"        (last node -> start node)
//!   - "TOPOLOGY SET <hist>"                 (node -> all nodes)
//!
//! NETMAP
//!   - "NETMAP DISCOVER"                           (client -> start node)
//!   - "NETMAP HOP <token> <start_addr> <entries>" (node -> node)
//!   - "NETMAP DONE <token> <entries>"             (last node -> start node)
//!   - "NETMAP SET <entries>"                      (start node -> every node)
//!   - "NETMAP GET"                                (client -> any node)
//!
//! FILE
//!   - "FILE PUSH <size> <name>" (client -> start)
//!   - "FILE PULL <name>"        (client -> any node)
//!   - "FILE LIST"               (client -> any)
//!   - "FILE TAGS-SET <entries>" (node -> node)
//!
//! FILE (internal)
//!   - "FILE RELAY-BLOB <token> <start_addr> <size> <name>"
//!   - "FILE RELAY-STREAM <token> <start> <file_size> <parts> <index> <name>"
//!   - "FILE GET-CHUNK <name>"                (node -> node)
//!   - "FILE RESP-CHUNK <next_addr> <size> <name>"
//!
//! FILE (backup)
//!   - "FILE NOTIFY-CHUNK-SAVED <name>"   (node -> predecessor node)
//!   - "FILE GET-CHUNK-FOR-BACKUP <name>" (predecessor node -> node)
//!   - "FILE GET-BACKUP-CHUNK <name>"     (node -> node, for PULL failover)
//!
//! IMPORTANT: the protocol is line-delimited. Any binary payload *follows*
//! the header line and is exactly <size> bytes long.

/// Parsed representation of a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    // NODE
    NodeNext(String), // NODE NEXT <addr>
    NodeStatus,       // NODE STATUS
    NodePing,         // NODE PING
    NodeHeal,         // "NODE HEAL" (client)
    NodeHealHop {
        token: String,
        start_addr: String,
    }, // "NODE HEAL-HOP <token> <start>" (internal)
    NodeHealDone {
        token: String,
    }, // "NODE HEAL-DONE <token>" (internal)

    // RING
    RingForward {
        ttl: u32,
        msg: String,
    }, // RING FORWARD <ttl> <message...>

    // TOPOLOGY
    TopologyWalk, // "TOPOLOGY WALK"
    TopologyHop {
        token: String,
        start_addr: String,
        history: String,
    },
    TopologyDone {
        token: String,
        history: String,
    },
    TopologySet {
        history: String,
    },

    // NETMAP
    NetmapDiscover, // "NETMAP DISCOVER"
    NetmapHop {
        token: String,
        start_addr: String,
        entries: String,
    },
    NetmapDone {
        token: String,
        entries: String,
    },
    NetmapSet {
        entries: String,
    }, // "NETMAP SET <entries>"
    NetmapGet, // "NETMAP GET"

    // FILE
    FilePush {
        size: u64,
        name: String,
    }, // "FILE PUSH <size> <name>"
    FilePull {
        name: String,
    }, // "FILE PULL <name>"
    FileList, // "FILE LIST"
    FileTagsSet {
        entries: String,
    },

    // FILE (internal)
    FileRelayBlob {
        token: String,
        start_addr: String,
        size: u64,
        name: String,
    },
    FileRelayStream {
        token: String,
        start_addr: String,
        file_size: u64,
        parts: u32,
        index: u32,
        name: String,
    },
    FileGetChunk {
        name: String,
    }, // "FILE GET-CHUNK <name>"

    // FILE (backup)
    FileNotifyChunkSaved {
        name: String,
    }, // "FILE NOTIFY-CHUNK-SAVED <name>"
    FileGetChunkForBackup {
        name: String,
    }, // "FILE GET-CHUNK-FOR-BACKUP <name>"
    FileGetBackupChunk {
        name: String,
    }, // "FILE GET-BACKUP-CHUNK <name>"
}

/// Parse one incoming line from the wire into a Command.
pub fn parse_line(line: &str) -> Result<Command, String> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.splitn(2, ' ');
    let noun = parts.next().unwrap_or("").to_ascii_uppercase();
    let rest = parts.next().unwrap_or("");

    match noun.as_str() {
        "NODE" => parse_node_cmd(rest),
        "RING" => parse_ring_cmd(rest),
        "TOPOLOGY" => parse_topology_cmd(rest),
        "NETMAP" => parse_netmap_cmd(rest),
        "FILE" => parse_file_cmd(rest),
        _ => Err(format!("unknown command namespace: '{}'", noun)),
    }
}

// --- Noun parsers

fn parse_node_cmd(rest: &str) -> Result<Command, String> {
    if let Some(addr) = rest.strip_prefix("NEXT ") {
        let addr = addr.trim();
        if addr.is_empty() {
            return Err("missing address for NODE NEXT".into());
        }
        return Ok(Command::NodeNext(addr.to_string()));
    }
    if rest.eq_ignore_ascii_case("STATUS") {
        return Ok(Command::NodeStatus);
    }
    if rest.eq_ignore_ascii_case("PING") {
        return Ok(Command::NodePing);
    }
    if rest.eq_ignore_ascii_case("HEAL") {
        return Ok(Command::NodeHeal);
    }
    if let Some(rest) = rest.strip_prefix("HEAL-HOP ") {
        let mut parts = rest.splitn(2, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        if token.is_empty() || start_addr.is_empty() {
            return Err("malformed NODE HEAL-HOP".into());
        }
        return Ok(Command::NodeHealHop {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
        });
    }
    if let Some(token) = rest.strip_prefix("HEAL-DONE ") {
        let token = token.trim();
        if token.is_empty() {
            return Err("malformed NODE HEAL-DONE".into());
        }
        return Ok(Command::NodeHealDone {
            token: token.to_string(),
        });
    }

    Err("unknown NODE command".into())
}

fn parse_ring_cmd(rest: &str) -> Result<Command, String> {
    if let Some(rest) = rest.strip_prefix("FORWARD ") {
        let mut parts = rest.splitn(2, ' ');
        let ttl_str = parts.next().unwrap_or("").trim();
        let msg = parts.next().unwrap_or("").to_string();
        let ttl = ttl_str
            .parse::<u32>()
            .map_err(|_| "invalid ttl for RING FORWARD")?;
        return Ok(Command::RingForward { ttl, msg });
    }
    Err("unknown RING command".into())
}

fn parse_topology_cmd(rest: &str) -> Result<Command, String> {
    if rest.eq_ignore_ascii_case("WALK") {
        return Ok(Command::TopologyWalk);
    }
    if let Some(rest) = rest.strip_prefix("HOP ") {
        let mut parts = rest.splitn(3, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        let history = parts.next().unwrap_or("").to_string();
        if token.is_empty() || start_addr.is_empty() {
            return Err("malformed TOPOLOGY HOP".into());
        }
        return Ok(Command::TopologyHop {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
            history,
        });
    }
    if let Some(rest) = rest.strip_prefix("DONE ") {
        let mut parts = rest.splitn(2, ' ');
        let token = parts.next().unwrap_or("").trim();
        let history = parts.next().unwrap_or("").to_string();
        if token.is_empty() {
            return Err("malformed TOPOLOGY DONE".into());
        }
        return Ok(Command::TopologyDone {
            token: token.to_string(),
            history,
        });
    }
    if let Some(rest) = rest.strip_prefix("SET ") {
        return Ok(Command::TopologySet {
            history: rest.to_string(),
        });
    }
    Err("unknown TOPOLOGY command".into())
}

fn parse_netmap_cmd(rest: &str) -> Result<Command, String> {
    if rest.eq_ignore_ascii_case("DISCOVER") {
        return Ok(Command::NetmapDiscover);
    }
    if let Some(rest) = rest.strip_prefix("HOP ") {
        let mut parts = rest.splitn(3, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        let entries = parts.next().unwrap_or("").to_string();
        if token.is_empty() || start_addr.is_empty() {
            return Err("malformed NETMAP HOP".into());
        }
        return Ok(Command::NetmapHop {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
            entries,
        });
    }
    if let Some(rest) = rest.strip_prefix("DONE ") {
        let mut parts = rest.splitn(2, ' ');
        let token = parts.next().unwrap_or("").trim();
        let entries = parts.next().unwrap_or("").to_string();
        if token.is_empty() {
            return Err("malformed NETMAP DONE".into());
        }
        return Ok(Command::NetmapDone {
            token: token.to_string(),
            entries,
        });
    }
    if let Some(rest) = rest.strip_prefix("SET ") {
        return Ok(Command::NetmapSet {
            entries: rest.trim().to_string(),
        });
    }
    if rest.eq_ignore_ascii_case("GET") {
        return Ok(Command::NetmapGet);
    }
    Err("unknown NETMAP command".into())
}

fn parse_file_cmd(rest: &str) -> Result<Command, String> {
    // PUSH
    if let Some(rest) = rest.strip_prefix("PUSH ") {
        let mut parts = rest.splitn(2, ' ');
        let size_str = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").to_string();
        if name.is_empty() {
            return Err("missing file name for FILE PUSH".into());
        }
        let size = size_str
            .parse::<u64>()
            .map_err(|_| "invalid size for FILE PUSH")?;
        return Ok(Command::FilePush { size, name });
    }

    // PULL
    if let Some(rest) = rest.strip_prefix("PULL ") {
        let name = rest.to_string();
        if name.trim().is_empty() {
            return Err("missing file name for FILE PULL".into());
        }
        return Ok(Command::FilePull { name });
    }

    // LIST
    if rest.eq_ignore_ascii_case("LIST") {
        return Ok(Command::FileList);
    }

    // TAGS-SET
    if let Some(rest) = rest.strip_prefix("TAGS-SET ") {
        return Ok(Command::FileTagsSet {
            entries: rest.to_string(),
        });
    }

    // GET-CHUNK
    if let Some(rest) = rest.strip_prefix("GET-CHUNK ") {
        let name = rest.to_string();
        if name.trim().is_empty() {
            return Err("missing file name for FILE GET-CHUNK".into());
        }
        return Ok(Command::FileGetChunk { name });
    }

    // NOTIFY-CHUNK-SAVED
    if let Some(rest) = rest.strip_prefix("NOTIFY-CHUNK-SAVED ") {
        let name = rest.to_string();
        if name.trim().is_empty() {
            return Err("missing file name for FILE NOTIFY-CHUNK-SAVED".into());
        }
        return Ok(Command::FileNotifyChunkSaved { name });
    }

    // GET-CHUNK-FOR-BACKUP
    if let Some(rest) = rest.strip_prefix("GET-CHUNK-FOR-BACKUP ") {
        let name = rest.to_string();
        if name.trim().is_empty() {
            return Err("missing file name for FILE GET-CHUNK-FOR-BACKUP".into());
        }
        return Ok(Command::FileGetChunkForBackup { name });
    }

    // GET-BACKUP-CHUNK
    if let Some(rest) = rest.strip_prefix("GET-BACKUP-CHUNK ") {
        let name = rest.to_string();
        if name.trim().is_empty() {
            return Err("missing file name for FILE GET-BACKUP-CHUNK".into());
        }
        return Ok(Command::FileGetBackupChunk { name });
    }

    // RELAY-BLOB
    if let Some(rest) = rest.strip_prefix("RELAY-BLOB ") {
        let mut parts = rest.splitn(4, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        let size_str = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").to_string();
        if token.is_empty() || start_addr.is_empty() || name.is_empty() {
            return Err("malformed FILE RELAY-BLOB".into());
        }
        let size = size_str
            .parse::<u64>()
            .map_err(|_| "invalid size for FILE RELAY-BLOB")?;
        return Ok(Command::FileRelayBlob {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
            size,
            name,
        });
    }

    // RELAY-STREAM
    if let Some(rest) = rest.strip_prefix("RELAY-STREAM ") {
        let mut parts = rest.splitn(6, ' ');
        let token = parts.next().unwrap_or("").trim();
        let start_addr = parts.next().unwrap_or("").trim();
        let file_size_str = parts.next().unwrap_or("").trim();
        let total_parts_str = parts.next().unwrap_or("").trim();
        let index_str = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").to_string();
        if token.is_empty() || start_addr.is_empty() || name.is_empty() {
            return Err("malformed FILE RELAY-STREAM".into());
        }
        let file_size = file_size_str
            .parse::<u64>()
            .map_err(|_| "invalid file_size for FILE RELAY-STREAM")?;
        let parts_u = total_parts_str
            .parse::<u32>()
            .map_err(|_| "invalid parts for FILE RELAY-STREAM")?;
        let index = index_str
            .parse::<u32>()
            .map_err(|_| "invalid index for FILE RELAY-STREAM")?;
        return Ok(Command::FileRelayStream {
            token: token.to_string(),
            start_addr: start_addr.to_string(),
            file_size,
            parts: parts_u,
            index,
            name,
        });
    }

    Err("unknown FILE command".into())
}
