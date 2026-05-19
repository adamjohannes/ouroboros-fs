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
//!   - "FILE PUSH-CHUNK <name> <chunk_size> <file_size> <parts> <index> <start_port>"
//!   - "FILE GET-CHUNK <name>"                (node -> node)
//!   - "FILE RESP-CHUNK <next_addr> <size> <name>"
//!
//! FILE (backup)
//!   - "FILE BACKUP-PUSH <name> <size>"  (node -> predecessor; raw bytes follow)
//!   - "FILE GET-BACKUP-CHUNK <name>"    (PULL failover: backup-holder serves)
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
    /// Start node fans this command out to each chunk's owner concurrently.
    /// Replaces the older RELAY-STREAM chain (one connection per node along
    /// the ring). Receiver reads exactly `chunk_size` bytes from the
    /// connection and saves them as `<name>.part-<index+1>-of-<parts>`.
    /// `start_port` lets the receiver tag the file locally with the right
    /// origin port for FILE LIST / file_tags propagation.
    FilePushChunk {
        name: String,
        chunk_size: u64,
        file_size: u64,
        parts: u32,
        index: u32,
        start_port: u16,
    },
    FileGetChunk {
        name: String,
    }, // "FILE GET-CHUNK <name>"

    // FILE (backup)
    /// Saving node pushes its just-saved chunk to its predecessor.
    /// Replaces an older notify-then-pull dance (NOTIFY-CHUNK-SAVED →
    /// GET-CHUNK-FOR-BACKUP) with a single push, halving the round trips
    /// per saved chunk.
    FileBackupPush {
        name: String,
        size: u64,
    }, // "FILE BACKUP-PUSH <name> <size>"
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

    // BACKUP-PUSH (saving node → predecessor; raw bytes follow)
    if let Some(rest) = rest.strip_prefix("BACKUP-PUSH ") {
        let mut parts = rest.splitn(2, ' ');
        let name = parts.next().unwrap_or("").trim().to_string();
        let size_str = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            return Err("missing file name for FILE BACKUP-PUSH".into());
        }
        let size = size_str
            .parse::<u64>()
            .map_err(|_| "invalid size for FILE BACKUP-PUSH")?;
        return Ok(Command::FileBackupPush { name, size });
    }

    // GET-BACKUP-CHUNK
    if let Some(rest) = rest.strip_prefix("GET-BACKUP-CHUNK ") {
        let name = rest.to_string();
        if name.trim().is_empty() {
            return Err("missing file name for FILE GET-BACKUP-CHUNK".into());
        }
        return Ok(Command::FileGetBackupChunk { name });
    }

    // PUSH-CHUNK (start node fans out to each chunk owner)
    if let Some(rest) = rest.strip_prefix("PUSH-CHUNK ") {
        let mut parts = rest.splitn(6, ' ');
        let name = parts.next().unwrap_or("").trim().to_string();
        let chunk_size_str = parts.next().unwrap_or("").trim();
        let file_size_str = parts.next().unwrap_or("").trim();
        let total_parts_str = parts.next().unwrap_or("").trim();
        let index_str = parts.next().unwrap_or("").trim();
        let start_port_str = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            return Err("missing name for FILE PUSH-CHUNK".into());
        }
        let chunk_size = chunk_size_str
            .parse::<u64>()
            .map_err(|_| "invalid chunk_size for FILE PUSH-CHUNK")?;
        let file_size = file_size_str
            .parse::<u64>()
            .map_err(|_| "invalid file_size for FILE PUSH-CHUNK")?;
        let parts_u = total_parts_str
            .parse::<u32>()
            .map_err(|_| "invalid parts for FILE PUSH-CHUNK")?;
        let index = index_str
            .parse::<u32>()
            .map_err(|_| "invalid index for FILE PUSH-CHUNK")?;
        let start_port = start_port_str
            .parse::<u16>()
            .map_err(|_| "invalid start_port for FILE PUSH-CHUNK")?;
        return Ok(Command::FilePushChunk {
            name,
            chunk_size,
            file_size,
            parts: parts_u,
            index,
            start_port,
        });
    }

    Err("unknown FILE command".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_next() {
        assert_eq!(
            parse_line("NODE NEXT 127.0.0.1:7001\n").unwrap(),
            Command::NodeNext("127.0.0.1:7001".into())
        );
    }

    #[test]
    fn node_simple_verbs() {
        assert_eq!(parse_line("NODE STATUS").unwrap(), Command::NodeStatus);
        assert_eq!(parse_line("NODE PING").unwrap(), Command::NodePing);
        assert_eq!(parse_line("NODE HEAL").unwrap(), Command::NodeHeal);
    }

    #[test]
    fn node_heal_hop_and_done() {
        assert_eq!(
            parse_line("NODE HEAL-HOP tok 127.0.0.1:7000").unwrap(),
            Command::NodeHealHop {
                token: "tok".into(),
                start_addr: "127.0.0.1:7000".into()
            }
        );
        assert_eq!(
            parse_line("NODE HEAL-DONE tok").unwrap(),
            Command::NodeHealDone { token: "tok".into() }
        );
    }

    #[test]
    fn ring_forward() {
        let cmd = parse_line("RING FORWARD 5 hello world").unwrap();
        match cmd {
            Command::RingForward { ttl, msg } => {
                assert_eq!(ttl, 5);
                assert_eq!(msg, "hello world");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn ring_forward_bad_ttl() {
        assert!(parse_line("RING FORWARD abc msg").is_err());
    }

    #[test]
    fn topology_walk_hop_done_set() {
        assert_eq!(parse_line("TOPOLOGY WALK").unwrap(), Command::TopologyWalk);
        match parse_line("TOPOLOGY HOP tok 127.0.0.1:7000 a->b").unwrap() {
            Command::TopologyHop {
                token,
                start_addr,
                history,
            } => {
                assert_eq!(token, "tok");
                assert_eq!(start_addr, "127.0.0.1:7000");
                assert_eq!(history, "a->b");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse_line("TOPOLOGY DONE tok a->b").unwrap() {
            Command::TopologyDone { token, history } => {
                assert_eq!(token, "tok");
                assert_eq!(history, "a->b");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse_line("TOPOLOGY SET a->b;b->c").unwrap() {
            Command::TopologySet { history } => assert_eq!(history, "a->b;b->c"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn netmap_variants() {
        assert_eq!(
            parse_line("NETMAP DISCOVER").unwrap(),
            Command::NetmapDiscover
        );
        assert_eq!(parse_line("NETMAP GET").unwrap(), Command::NetmapGet);
        match parse_line("NETMAP HOP tok 127.0.0.1:7000 7000=Alive").unwrap() {
            Command::NetmapHop {
                token,
                start_addr,
                entries,
            } => {
                assert_eq!(token, "tok");
                assert_eq!(start_addr, "127.0.0.1:7000");
                assert_eq!(entries, "7000=Alive");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse_line("NETMAP DONE tok 7000=Alive").unwrap() {
            Command::NetmapDone { token, entries } => {
                assert_eq!(token, "tok");
                assert_eq!(entries, "7000=Alive");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse_line("NETMAP SET 7000=Alive").unwrap() {
            Command::NetmapSet { entries } => assert_eq!(entries, "7000=Alive"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn file_push_and_pull() {
        match parse_line("FILE PUSH 1024 myfile").unwrap() {
            Command::FilePush { size, name } => {
                assert_eq!(size, 1024);
                assert_eq!(name, "myfile");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match parse_line("FILE PULL myfile").unwrap() {
            Command::FilePull { name } => assert_eq!(name, "myfile"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn file_chunk_commands() {
        match parse_line("FILE GET-CHUNK myfile.part-001-of-003").unwrap() {
            Command::FileGetChunk { name } => assert_eq!(name, "myfile.part-001-of-003"),
            other => panic!("unexpected: {other:?}"),
        }
        match parse_line("FILE GET-BACKUP-CHUNK foo").unwrap() {
            Command::FileGetBackupChunk { name } => assert_eq!(name, "foo"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn file_backup_push() {
        match parse_line("FILE BACKUP-PUSH chunk.part-001-of-003 4096").unwrap() {
            Command::FileBackupPush { name, size } => {
                assert_eq!(name, "chunk.part-001-of-003");
                assert_eq!(size, 4096);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Bad size
        assert!(parse_line("FILE BACKUP-PUSH foo notanumber").is_err());
        // Missing name
        assert!(parse_line("FILE BACKUP-PUSH").is_err());
    }

    #[test]
    fn file_push_chunk() {
        // 6-field shape: name chunk_size file_size parts index start_port
        match parse_line("FILE PUSH-CHUNK myfile.part-002-of-005 4096 20480 5 1 7000")
            .unwrap()
        {
            Command::FilePushChunk {
                name,
                chunk_size,
                file_size,
                parts,
                index,
                start_port,
            } => {
                assert_eq!(name, "myfile.part-002-of-005");
                assert_eq!(chunk_size, 4096);
                assert_eq!(file_size, 20480);
                assert_eq!(parts, 5);
                assert_eq!(index, 1);
                assert_eq!(start_port, 7000);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn file_push_chunk_negative() {
        // Bad numeric fields
        assert!(parse_line("FILE PUSH-CHUNK n abc 1 1 0 7000").is_err());
        // Missing fields
        assert!(parse_line("FILE PUSH-CHUNK n 1").is_err());
        // RELAY-STREAM was removed when the fan-out refactor landed; it must not parse.
        assert!(parse_line("FILE RELAY-STREAM tok 7000 1024 3 0 0 myfile").is_err());
    }

    #[test]
    fn negatives() {
        assert!(parse_line("").is_err());
        assert!(parse_line("BLAH").is_err());
        assert!(parse_line("NODE NEXT").is_err()); // missing addr
        assert!(parse_line("FILE PUSH abc xyz").is_err()); // non-numeric size
        assert!(parse_line("FILE PUSH").is_err()); // no args
        assert!(parse_line("FILE").is_err()); // missing verb
    }

    #[test]
    fn crlf_tolerance() {
        assert_eq!(parse_line("NODE STATUS\r\n").unwrap(), Command::NodeStatus);
    }

    #[test]
    fn lowercase_noun_normalized() {
        assert_eq!(parse_line("node status").unwrap(), Command::NodeStatus);
    }
}
