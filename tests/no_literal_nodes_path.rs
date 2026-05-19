//! CI grep gate. The PR-T0 refactor threaded `node.storage_root` through
//! every chunk read/write site in `src/server.rs`. If a future change
//! reintroduces a literal `"nodes/"` path it'll silently land in the cwd,
//! trample concurrent test runs, and break in-process integration tests.
//! This test fails loudly when that happens.

use std::fs;

#[test]
fn no_literal_nodes_path_in_server_rs() {
    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/server.rs"))
        .expect("read server.rs");

    for (lineno, line) in src.lines().enumerate() {
        // Skip comments — comments may legitimately reference the on-disk
        // layout for documentation.
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        assert!(
            !line.contains("\"nodes/"),
            "src/server.rs:{} reintroduced a literal \"nodes/\" path: {}\n\
             Use `node.storage_root.join(...)` instead.",
            lineno + 1,
            line.trim()
        );
    }
}
