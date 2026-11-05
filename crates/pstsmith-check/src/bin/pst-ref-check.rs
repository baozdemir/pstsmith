//! Block reference and allocation map check of a Unicode PST.
//! - References: every block's stored reference count must be 1 + the references the file holds (NBT data and subnode
//!   roots, children of data-tree and subnode-tree blocks).
//! - Allocation map: every AMap page is compared with the bits the B-trees give (their pages, every block, the map
//!   pages). Space the trees use but the map marks free ("amap_unmarked_units") is a corruption: a later allocation
//!   could overwrite it. Space the map marks allocated that nothing references ("amap_leaked_units") is wasted space.
//!   The header's free size must equal the free bytes of the map pages on disk ("amap_free_header" and
//!   "amap_free_pages"; MS-PST cbAMapFree).
//!
//! Prints one JSON line. Exit 0 only when there is no reference mismatch, no missing block, no unmarked unit and the
//! free sizes agree;
//! unreferenced blocks and leaked units are reported, not failed (a template may hold some; the counts are compared
//! across writers instead). Exit 2 when the file cannot be read.
//!
//!     pst-ref-check FILE.pst
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: pst-ref-check FILE.pst");
    let fail = |error: std::io::Error| -> ! {
        eprintln!("{error}");
        std::process::exit(2);
    };
    let (blocks, mismatches, unreferenced, missing) =
        pstsmith::debug_block_references(&path).unwrap_or_else(|error| fail(error));
    let (amap_pages, amap_differing, leaked, unmarked, unmarked_sample, header_free, pages_free) =
        pstsmith::debug_allocation_map(&path).unwrap_or_else(|error| fail(error));
    let hex = |keys: &[u64]| {
        keys.iter()
            .take(20)
            .map(|key| format!("{key:#x}"))
            .collect::<Vec<_>>()
    };
    println!(
        "{}",
        serde_json::json!({
            "blocks": blocks,
            "mismatches": mismatches.len(),
            "mismatch_sample": mismatches.iter().take(20).map(|(key, stored, expected)| serde_json::json!([format!("{key:#x}"), stored, expected])).collect::<Vec<_>>(),
            "unreferenced": unreferenced.len(),
            "unreferenced_sample": hex(&unreferenced),
            "missing": missing.len(),
            "missing_sample": hex(&missing),
            "amap_pages": amap_pages,
            "amap_differing_pages": amap_differing,
            "amap_leaked_units": leaked,
            "amap_unmarked_units": unmarked,
            "amap_unmarked_sample": hex(&unmarked_sample),
            "amap_free_header": header_free,
            "amap_free_pages": pages_free,
        })
    );
    std::process::exit(
        if mismatches.is_empty() && missing.is_empty() && unmarked == 0 && header_free == pages_free
        {
            0
        } else {
            1
        },
    );
}
