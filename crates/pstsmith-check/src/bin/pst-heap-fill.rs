//! Heap fill levels of a Unicode PST: one JSON line per heap-on-node stream (a node's data stream or a subnode stream)
//! with the fill-level bytes its HNHDR and HNBITMAPHDR pages store, in hex. Two files whose heaps were filled the same
//! way hold the same lines even when their block layout differs.
//!
//!     pst-heap-fill FILE.pst
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: pst-heap-fill FILE.pst");
    let heaps = match pstsmith::debug_heap_fill_levels(&path) {
        Ok(heaps) => heaps,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    for (nid, sub, pages, levels) in heaps {
        let hex: String = levels.iter().map(|byte| format!("{byte:02x}")).collect();
        println!(
            "{}",
            serde_json::json!({"nid": nid, "sub": sub, "pages": pages, "fill": hex})
        );
    }
}
