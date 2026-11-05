//! Logical content digest of a Unicode PST: one JSON line per node with the SHA-256 of its data stream and of every
//! subnode stream. Two files with the same digests hold the same nodes and bytes even when their block layout differs.
//!
//!     pst-node-digest FILE.pst
use sha2::{Digest, Sha256};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: pst-node-digest FILE.pst");
    let nodes = match pstsmith::debug_node_streams(&path) {
        Ok(nodes) => nodes,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    for (nid, parent, data, subnodes) in nodes {
        let sub: Vec<serde_json::Value> = subnodes
            .iter()
            .map(|(path, data)| serde_json::json!([path, data.len(), hex(&Sha256::digest(data))]))
            .collect();
        println!(
            "{}",
            serde_json::json!({"nid": nid, "parent": parent, "len": data.len(), "sha256": hex(&Sha256::digest(&data)), "sub": sub})
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
