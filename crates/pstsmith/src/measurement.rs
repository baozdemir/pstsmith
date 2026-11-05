//! Measurement-only instrumentation. Compiled only with the cargo features `mem-profile` or `op-counters`; a build
//! without them contains none of this module, except that the writer's own test build has the counting allocator too
//! (a test counts the allocations of a streamed attachment).
//!
//! - `mem-profile`: a counting global allocator (registered by the binary), so heap bytes live now and the peak since
//!   the last reset can be read without any external tool.
//! - `op-counters`: named counters of per-commit operations, so their growth can be measured.
//! - After every writer commit, when `PST_INSTRUMENT_OUT` names a file, one JSON line is appended to it: the heap now
//!   (retained) and its peak during the commit, the counters of that commit, and a report of the session's structures.

#[cfg(any(test, feature = "mem-profile"))]
pub mod heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    static CURRENT: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);
    /// The highest heap since the process started; never reset (work between commits, such as resolving a folder path,
    /// is outside every commit's peak).
    static PEAK_EVER: AtomicUsize = AtomicUsize::new(0);
    /// Allocation calls (alloc, alloc_zeroed and growing realloc) by requested size, since the last commit started: at
    /// most 256 bytes, 1,008 bytes, 15 KiB, 127 KiB, 8 MiB, and larger. The bounds are near the size limits of macOS
    /// libmalloc's zones; which zone serves a call is not inferred from them. The counts show which sizes a commit
    /// allocates again and again.
    pub const CLASS_BOUNDS: [usize; 5] = [256, 1008, 15 << 10, 127 << 10, 8 << 20];
    static CALLS: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];
    /// Bytes requested by those calls, by the same classes (a growing realloc counts its new size).
    static BYTES: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];

    std::thread_local! {
        /// Allocation calls made on this thread, in all and above 15 KiB (const-initialised, so reading them allocates
        /// nothing).
        static CALLS_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        static LARGE_CALLS_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        static HUGE_CALLS_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        /// Set while `uncounted` runs: the test build's own bookkeeping is not counted.
        static PAUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn count_call(size: usize) {
        let class = CLASS_BOUNDS
            .iter()
            .position(|bound| size <= *bound)
            .unwrap_or(CLASS_BOUNDS.len());
        CALLS[class].fetch_add(1, Relaxed);
        BYTES[class].fetch_add(size, Relaxed);
        if PAUSED.try_with(std::cell::Cell::get).unwrap_or(false) {
            return;
        }
        let _ = CALLS_HERE.try_with(|calls| calls.set(calls.get() + 1));
        if class >= 3 {
            let _ = LARGE_CALLS_HERE.try_with(|calls| calls.set(calls.get() + 1));
        }
        if class >= 4 {
            let _ = HUGE_CALLS_HERE.try_with(|calls| calls.set(calls.get() + 1));
        }
    }

    /// Allocation calls made on the calling thread since it started.
    pub fn calls_on_this_thread() -> u64 {
        CALLS_HERE.try_with(std::cell::Cell::get).unwrap_or(0)
    }

    /// Allocation calls over 15 KiB made on the calling thread since it started.
    pub fn large_calls_on_this_thread() -> u64 {
        LARGE_CALLS_HERE.try_with(std::cell::Cell::get).unwrap_or(0)
    }

    /// Allocation calls over 127 KiB made on the calling thread since it started.
    pub fn huge_calls_on_this_thread() -> u64 {
        HUGE_CALLS_HERE.try_with(std::cell::Cell::get).unwrap_or(0)
    }

    /// Run `f` without counting its allocations on this thread's counters (the test build's own bookkeeping).
    pub fn uncounted<T>(f: impl FnOnce() -> T) -> T {
        let _ = PAUSED.try_with(|paused| paused.set(true));
        let value = f();
        let _ = PAUSED.try_with(|paused| paused.set(false));
        value
    }

    /// The bytes requested per class since the last call, as a JSON array; they are reset.
    pub fn take_bytes_json() -> String {
        let counts: Vec<String> = BYTES
            .iter()
            .map(|count| count.swap(0, Relaxed).to_string())
            .collect();
        format!("[{}]", counts.join(","))
    }

    /// The allocation calls per class since the last call, as a JSON array; they are reset.
    pub fn take_calls_json() -> String {
        let counts: Vec<String> = CALLS
            .iter()
            .map(|count| count.swap(0, Relaxed).to_string())
            .collect();
        format!("[{}]", counts.join(","))
    }

    /// The counting allocator. A binary built with `mem-profile` registers it with `#[global_allocator]`.
    pub struct Counting;

    fn grow(bytes: usize) {
        let now = CURRENT.fetch_add(bytes, Relaxed) + bytes;
        PEAK.fetch_max(now, Relaxed);
        PEAK_EVER.fetch_max(now, Relaxed);
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                grow(layout.size());
                count_call(layout.size());
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                grow(layout.size());
                count_call(layout.size());
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
            CURRENT.fetch_sub(layout.size(), Relaxed);
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new = unsafe { System.realloc(ptr, layout, new_size) };
            if !new.is_null() {
                if new_size >= layout.size() {
                    grow(new_size - layout.size());
                    count_call(new_size);
                } else {
                    CURRENT.fetch_sub(layout.size() - new_size, Relaxed);
                }
            }
            new
        }
    }

    pub fn current() -> usize {
        CURRENT.load(Relaxed)
    }

    pub fn peak() -> usize {
        PEAK.load(Relaxed)
    }

    pub fn peak_ever() -> usize {
        PEAK_EVER.load(Relaxed)
    }

    pub fn reset_peak() {
        PEAK.store(CURRENT.load(Relaxed), Relaxed);
    }
}

#[cfg(feature = "op-counters")]
pub mod ops {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// AMap pages visited to total the free bytes.
    pub const AMAP_PAGES_SUMMED: usize = 0;
    /// AMap pages encoded because they may have changed.
    pub const AMAP_PAGES_ENCODED: usize = 1;
    /// B-tree leaves visited by a pass over all leaves (validation, child lists).
    pub const BTREE_LEAVES_VISITED: usize = 2;
    /// Intermediate-level entries visited or rebuilt.
    pub const BTREE_INNER_ENTRIES_VISITED: usize = 3;
    /// Entries merged in leaves that received a change.
    pub const BTREE_LEAF_ENTRIES_MERGED: usize = 4;
    /// B-tree pages planned (written) by a commit.
    pub const BTREE_PAGES_PLANNED: usize = 5;
    /// Calls of the B-tree planner (one per tree per commit).
    pub const BTREE_PLAN_CALLS: usize = 6;
    /// Leaf-list elements shifted by inserting or removing leaves in the in-memory list.
    pub const BTREE_LEAF_LIST_SHIFTED: usize = 7;
    /// Free runs copied when the free-space list is built.
    pub const FREE_RUNS_COPIED: usize = 8;
    /// Free runs examined to place one allocation.
    pub const FREE_RUNS_SCANNED: usize = 9;
    /// B-tree lookups by key.
    pub const BTREE_LOOKUPS: usize = 10;
    /// B-tree pages read and decoded from disk.
    pub const BTREE_PAGES_READ: usize = 11;
    /// AMap pages read from disk.
    pub const AMAP_PAGES_READ: usize = 12;
    /// Ref-count delta entries computed.
    pub const REFCOUNT_DELTA_ENTRIES: usize = 13;
    /// Table heap pages read from disk by a path view (contents and hierarchy tables).
    pub const CONTENTS_PAGES_LOADED: usize = 14;
    /// Data-tree index blocks read by the path-only data tree view.
    pub const DATA_TREE_INDEX_BLOCKS_READ: usize = 15;
    /// New internal blocks the reference counting's precount filter kept from being walked.
    pub const PRECOUNTED_FILTERED: usize = 16;
    /// Commits whose stated reference changes did not add up, so the full recount ran.
    pub const REFCOUNT_FALLBACKS: usize = 17;
    /// Blocks written by a commit.
    pub const BLOCKS_WRITTEN: usize = 18;
    /// Bytes of block data written by a commit, trailers excluded.
    pub const BLOCK_BYTES_WRITTEN: usize = 19;
    /// Table rows decoded by full table-context reads.
    pub const TABLE_ROWS_READ: usize = 20;
    /// Folders opened through the reader.
    pub const FOLDERS_OPENED: usize = 21;
    /// Hierarchy-table row patches or appends that fell back to the full read and rebuild.
    pub const HIERARCHY_FALLBACKS: usize = 22;

    pub const NAMES: [&str; 23] = [
        "amap_pages_summed",
        "amap_pages_encoded",
        "btree_leaves_visited",
        "btree_inner_entries_visited",
        "btree_leaf_entries_merged",
        "btree_pages_planned",
        "btree_plan_calls",
        "btree_leaf_list_shifted",
        "free_runs_copied",
        "free_runs_scanned",
        "btree_lookups",
        "btree_pages_read",
        "amap_pages_read",
        "refcount_delta_entries",
        "contents_pages_loaded",
        "data_tree_index_blocks_read",
        "precounted_filtered",
        "refcount_fallbacks",
        "blocks_written",
        "block_bytes_written",
        "table_rows_read",
        "folders_opened",
        "hierarchy_fallbacks",
    ];

    static COUNTS: [AtomicU64; 23] = [const { AtomicU64::new(0) }; 23];

    pub fn add(counter: usize, n: u64) {
        COUNTS[counter].fetch_add(n, Relaxed);
    }

    /// The counters since the last call, as a JSON object; they are reset.
    pub fn take_json() -> String {
        let fields: Vec<String> = NAMES
            .iter()
            .enumerate()
            .map(|(index, name)| format!("\"{name}\":{}", COUNTS[index].swap(0, Relaxed)))
            .collect();
        format!("{{{}}}", fields.join(","))
    }
}

/// Count an operation (no-op without `op-counters`).
#[allow(unused_macros)]
macro_rules! count_op {
    ($counter:ident, $n:expr) => {{
        #[cfg(feature = "op-counters")]
        $crate::measurement::ops::add($crate::measurement::ops::$counter, ($n) as u64);
    }};
}
#[allow(unused_imports)]
pub(crate) use count_op;

#[cfg(any(feature = "mem-profile", feature = "op-counters"))]
thread_local! {
    static STARTED: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// Called at the start of every writer commit (`WriterCore::run`).
pub(crate) fn commit_started() {
    #[cfg(feature = "mem-profile")]
    {
        heap::reset_peak();
        let _ = heap::take_calls_json();
        let _ = heap::take_bytes_json();
    }
    #[cfg(any(feature = "mem-profile", feature = "op-counters"))]
    STARTED.with(|cell| cell.set(Some(std::time::Instant::now())));
}

/// Add the session's contents-table append states to the last record, as `"contents"`: a bulk append holds the target
/// folder's state outside the session while its commit runs and puts it back after `commit_finished`, so the
/// structure report alone would miss it. The heap figures of that record already include it.
#[allow(unused_variables)]
pub(crate) fn contents_after_commit(report: impl FnOnce() -> String) {
    #[cfg(feature = "mem-profile")]
    {
        use std::io::{Seek, SeekFrom, Write};
        let Some(path) = std::env::var_os("PST_INSTRUMENT_OUT") else {
            return;
        };
        let Ok(mut file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        else {
            return;
        };
        // The last record ends with "}\n": reopen it and add the field.
        let Ok(len) = file.seek(SeekFrom::End(0)) else {
            return;
        };
        if len < 2 || file.set_len(len - 2).is_err() {
            return;
        }
        let _ = file
            .seek(SeekFrom::End(0))
            .and_then(|_| file.write_all(format!(",\"contents\":{}}}\n", report()).as_bytes()));
    }
}

/// Called after every successful writer commit: appends one JSON line to `PST_INSTRUMENT_OUT` when it is set.
#[allow(unused_variables)]
pub(crate) fn commit_finished(structures: impl FnOnce() -> String) {
    #[cfg(any(feature = "mem-profile", feature = "op-counters"))]
    {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static COMMITS: AtomicU64 = AtomicU64::new(0);
        let Some(path) = std::env::var_os("PST_INSTRUMENT_OUT") else {
            return;
        };
        let commit = COMMITS.fetch_add(1, Relaxed);
        #[cfg(feature = "mem-profile")]
        let heap = format!(
            "\"heap_retained\":{},\"heap_peak\":{},\"heap_peak_ever\":{},\"alloc_calls\":{},\"alloc_bytes\":{}",
            heap::current(),
            heap::peak(),
            heap::peak_ever(),
            heap::take_calls_json(),
            heap::take_bytes_json()
        );
        #[cfg(not(feature = "mem-profile"))]
        let heap = String::from("\"heap_retained\":null,\"heap_peak\":null");
        #[cfg(feature = "op-counters")]
        let ops = ops::take_json();
        #[cfg(not(feature = "op-counters"))]
        let ops = String::from("null");
        let micros = STARTED
            .with(|cell| cell.get())
            .map_or(0, |started| started.elapsed().as_micros());
        let line = format!(
            "{{\"commit\":{commit},\"wall_us\":{micros},{heap},\"ops\":{ops},\"structures\":{}}}\n",
            structures()
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}
