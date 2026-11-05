//! Writer session.
//!
//! A [`UnicodePstWriter`] reads the file's state (both B-trees and the allocation map) once when it opens the file and
//! then keeps the committed state in memory: each commit records its changes as a delta, rewrites only the B-tree pages
//! and map pages the delta touches and moves the in-memory state forward to exactly what it wrote. A commit does not
//! re-read both B-trees and the allocation map of the whole file, rebuild the allocation map from every block, re-plan
//! both B-trees by comparing every entry or open the file several times: those costs would grow with the file.
//!
//! Commit semantics (see `commit`):
//! - one append is one transaction: new blocks and pages go to space the committed allocation map marks free, then a
//!   header with the new roots is written and synced; that sync is the commit point. The allocation map, the header's
//!   map fields and the density list are written after it (the last header write is not synced; a crash there leaves a
//!   committed file whose allocation map is marked invalid, which MS-PST crash recovery rebuilds);
//! - nothing is buffered between commits: when an append returns `Ok`, the commit is complete; `close` only drops
//!   the handles;
//! - any error inside an append poisons the session. An error before the commit point leaves the file at the last
//!   committed state; an error after it leaves the new state committed with its allocation map marked invalid.
//!   Either way the in-memory state can no longer be trusted, so every later call fails until the file is opened
//!   again (`UnicodePstWriter::open` reads the committed state from disk).

use super::*;
use crate::measurement::count_op;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Bound;
use std::path::PathBuf;

/// Lookup of a block's B-tree entry by its search key, from the committed state plus a commit's pending changes.
pub(crate) trait BlockLookup {
    fn lookup_block(&self, key: u64) -> io::Result<Option<UnicodeBlockBTreeEntry>>;
    /// How the file's data blocks are encoded: what a read through this lookup decodes.
    fn crypt(&self) -> NdbCryptMethod {
        NdbCryptMethod::None
    }
}

impl BlockLookup for [UnicodeBlockBTreeEntry] {
    fn lookup_block(&self, key: u64) -> io::Result<Option<UnicodeBlockBTreeEntry>> {
        Ok(self
            .binary_search_by_key(&key, BTreeEntry::key)
            .ok()
            .map(|index| self[index]))
    }
}

/// Pages of one B-tree kept in memory between lookups, at most this many.
const TREE_CACHE_CAP: usize = 1024;
/// Appended block entries held before they move into the right edge (about 100 KB): a commit of ordinary size keeps
/// all of them until its end, so their reference-count updates are applied before any page is written; a large
/// streamed value moves them out every 4,096 blocks (32 MB of data).
const APPEND_BUFFER: usize = 4096;
/// Intermediate B-tree pages hold at most this many entries (as `plan_btree_level` wrote them).
const INNER_MAX: usize = 20;

/// A B-tree page as decoded from disk.
pub(crate) enum TreePage<E> {
    Leaf(Vec<E>),
    Inner(u8, Vec<(u64, UnicodePageRef)>),
}

/// The two Unicode B-trees' entry types: how a page of each is read, and how a leaf of each is written.
pub(crate) trait DiskEntry: Copy + BTreeEntry<Key = u64> + BTreeEntryReadWrite {
    const LEAF_MAX: usize;
    /// cbEnt of a leaf page (MS-PST 2.2.2.7.7.1): 24 for BBTENTRY, 32 for NBTENTRY.
    const LEAF_ENTRY_SIZE: u8;
    const PAGE_TYPE: PageType;
    fn read_page(file: &mut File, page_ref: UnicodePageRef) -> io::Result<TreePage<Self>>;
}

impl DiskEntry for UnicodeBlockBTreeEntry {
    const LEAF_MAX: usize = 20;
    const LEAF_ENTRY_SIZE: u8 = 24;
    const PAGE_TYPE: PageType = PageType::BlockBTree;
    fn read_page(file: &mut File, page_ref: UnicodePageRef) -> io::Result<TreePage<Self>> {
        Ok(match UnicodeBlockBTree::read(file, page_ref)? {
            RootBTreePage::Leaf(page) => TreePage::Leaf(page.entries().to_vec()),
            RootBTreePage::Intermediate(page, ..) => TreePage::Inner(
                page.level(),
                page.entries()
                    .iter()
                    .map(|entry| (entry.key(), entry.block()))
                    .collect(),
            ),
        })
    }
}

impl DiskEntry for UnicodeNodeBTreeEntry {
    const LEAF_MAX: usize = 15;
    const LEAF_ENTRY_SIZE: u8 = 32;
    const PAGE_TYPE: PageType = PageType::NodeBTree;
    fn read_page(file: &mut File, page_ref: UnicodePageRef) -> io::Result<TreePage<Self>> {
        Ok(match UnicodeNodeBTree::read(file, page_ref)? {
            RootBTreePage::Leaf(page) => TreePage::Leaf(page.entries().to_vec()),
            RootBTreePage::Intermediate(page, ..) => TreePage::Inner(
                page.level(),
                page.entries()
                    .iter()
                    .map(|entry| (entry.key(), entry.block()))
                    .collect(),
            ),
        })
    }
}

/// A B-tree page's 512 bytes (MS-PST v20250218 2.2.2.7.7.1 BTPAGE, 2.2.2.7.1 PAGETRAILER), encoded on the stack:
/// `count` entries of `entry_size` bytes, each written by `write_entry`, then cEnt, cEntMax, cbEnt, cLevel and
/// dwPadding, the CRC of those 496 bytes, and the trailer. These are the steps of the upstream page writer
/// (`UnicodeBTreePageReadWrite::write`), without its copy of the entries and with one write instead of five; a test
/// compares the two byte for byte.
pub(crate) fn encode_btree_page(
    count: usize,
    max_entries: u8,
    entry_size: u8,
    level: u8,
    page_type: PageType,
    page_ref: UnicodePageRef,
    mut write_entry: impl FnMut(usize, &mut &mut [u8]) -> io::Result<()>,
) -> io::Result<[u8; PAGE_SIZE]> {
    const ENTRIES: usize = 488;
    if count > usize::from(max_entries)
        || usize::from(max_entries) * usize::from(entry_size) > ENTRIES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a B-tree page cannot hold these entries",
        ));
    }
    let mut page = [0_u8; PAGE_SIZE];
    for index in 0..count {
        let start = index * usize::from(entry_size);
        let mut slot = &mut page[start..start + usize::from(entry_size)];
        write_entry(index, &mut slot)?;
    }
    page[ENTRIES..ENTRIES + 4].copy_from_slice(&[count as u8, max_entries, entry_size, level]);
    let crc = crate::crc::compute_crc(0, &page[..496]);
    let signature = page_type.signature(page_ref.index().index(), page_ref.block().into_u64());
    UnicodePageTrailer::new(page_type, signature, page_ref.block(), crc)
        .write(&mut &mut page[496..])?;
    Ok(page)
}

/// The child of an intermediate page whose key range holds `key`: child `i` takes the keys from its key up to the next
/// child's key; the first child also takes smaller keys.
fn child_for(children: &[(u64, UnicodePageRef)], key: u64) -> usize {
    children
        .partition_point(|(first, _)| *first <= key)
        .saturating_sub(1)
}

/// A B-tree kept on disk. The session holds only the root and a bounded cache of decoded pages; a lookup reads the
/// pages on one root-to-leaf path, and a commit rewrites only the pages on the paths of the keys it changes (copy on
/// write), so its work grows with the changes and the tree depth, not with the tree. Pages are never rebalanced: a leaf
/// or intermediate page is split into full pages when it overflows, and removed when it empties; an intermediate
/// entry's key is its child's first key.
pub(crate) struct DiskTree<E> {
    pub(crate) root: UnicodePageRef,
    reader: RefCell<File>,
    #[allow(clippy::type_complexity)]
    cache: RefCell<(HashMap<u64, Rc<TreePage<E>>>, VecDeque<u64>)>,
}

/// A page to write: a leaf's entries, or an intermediate page's level and children.
#[derive(Clone, Copy)]
pub(crate) enum PageContent<'a, E> {
    Leaf(&'a [E]),
    Inner(u8, &'a [(u64, UnicodePageRef)]),
}

impl<E: DiskEntry> DiskTree<E> {
    fn open(path: &Path, root: UnicodePageRef) -> io::Result<Self> {
        Ok(Self {
            root,
            reader: RefCell::new(File::open(path)?),
            cache: RefCell::new((HashMap::new(), VecDeque::new())),
        })
    }

    /// A tree read through an already open handle (read-only uses).
    fn over(file: File, root: UnicodePageRef) -> Self {
        Self {
            root,
            reader: RefCell::new(file),
            cache: RefCell::new((HashMap::new(), VecDeque::new())),
        }
    }

    fn page(&self, page_ref: UnicodePageRef) -> io::Result<Rc<TreePage<E>>> {
        let id = page_ref.block().into_u64();
        if let Some(page) = self.cache.borrow().0.get(&id) {
            return Ok(Rc::clone(page));
        }
        count_op!(BTREE_PAGES_READ, 1);
        let page = Rc::new(E::read_page(&mut self.reader.borrow_mut(), page_ref)?);
        self.remember(id, Rc::clone(&page));
        Ok(page)
    }

    fn remember(&self, id: u64, page: Rc<TreePage<E>>) {
        let mut cache = self.cache.borrow_mut();
        let (pages, order) = &mut *cache;
        if pages.insert(id, page).is_none() {
            order.push_back(id);
            if order.len() > TREE_CACHE_CAP {
                if let Some(old) = order.pop_front() {
                    pages.remove(&old);
                }
            }
        }
    }

    pub(crate) fn get(&self, key: u64) -> io::Result<Option<E>> {
        self.get_from(self.root, key)
    }

    /// Look `key` up in the subtree under `page_ref`.
    fn get_from(&self, page_ref: UnicodePageRef, key: u64) -> io::Result<Option<E>> {
        count_op!(BTREE_LOOKUPS, 1);
        let mut page_ref = page_ref;
        for _ in 0..=8 {
            match &*self.page(page_ref)? {
                TreePage::Leaf(items) => {
                    return Ok(items
                        .binary_search_by_key(&key, BTreeEntry::key)
                        .ok()
                        .map(|index| items[index]))
                }
                TreePage::Inner(_, children) => page_ref = children[child_for(children, key)].1,
            }
        }
        Err(PstError::InvalidBTreePage(page_ref.index().index()).into())
    }

    /// The last entry (largest key).
    fn last(&self) -> io::Result<Option<E>> {
        let mut page_ref = self.root;
        for _ in 0..=8 {
            match &*self.page(page_ref)? {
                TreePage::Leaf(items) => return Ok(items.last().copied()),
                TreePage::Inner(_, children) => {
                    page_ref = children
                        .last()
                        .ok_or(PstError::InvalidBTreePage(page_ref.index().index()))?
                        .1
                }
            }
        }
        Err(PstError::InvalidBTreePage(page_ref.index().index()).into())
    }

    /// Every entry in key order (O(entries): session open, the folderless append, fallbacks and tests). Pages read for
    /// it are not cached.
    pub(crate) fn for_each(&self, visit: impl FnMut(&E)) -> io::Result<()> {
        self.for_each_from(self.root, visit)
    }

    /// Every entry of the subtree under `page_ref`, in key order.
    fn for_each_from(&self, page_ref: UnicodePageRef, mut visit: impl FnMut(&E)) -> io::Result<()> {
        let mut stack = vec![(page_ref, 0_u8)];
        while let Some((page_ref, depth)) = stack.pop() {
            if depth > 8 {
                return Err(PstError::InvalidBTreePage(page_ref.index().index()).into());
            }
            let id = page_ref.block().into_u64();
            let cached = self.cache.borrow().0.get(&id).cloned();
            let page = match cached {
                Some(page) => page,
                None => Rc::new(E::read_page(&mut self.reader.borrow_mut(), page_ref)?),
            };
            match &*page {
                TreePage::Leaf(items) => items.iter().for_each(&mut visit),
                TreePage::Inner(_, children) => {
                    stack.extend(children.iter().rev().map(|(_, child)| (*child, depth + 1)))
                }
            }
        }
        Ok(())
    }

    pub(crate) fn entries(&self) -> io::Result<Vec<E>> {
        let mut all = Vec::new();
        self.for_each(|entry| all.push(*entry))?;
        Ok(all)
    }

    /// Write one new page (copy on write): a new page ID and 512 bytes of free space; the page joins the cache.
    fn write_page(
        &self,
        file: &mut File,
        page: TreePage<E>,
        next_page: &mut UnicodePageId,
        space: &mut FileSpace,
    ) -> io::Result<(u64, UnicodePageRef)> {
        let content = match &page {
            TreePage::Leaf(items) => PageContent::Leaf(items),
            TreePage::Inner(level, children) => PageContent::Inner(*level, children),
        };
        let (first, page_ref) = self.write_content(file, content, next_page, space)?;
        self.remember(page_ref.block().into_u64(), Rc::new(page));
        Ok((first, page_ref))
    }

    /// Encode and write one new page (copy on write): a new page ID and 512 bytes of free space. The page does not join
    /// the cache: the right edge writes its pages this way. A streamed value's pages are not read again in the session
    /// (a lookup of one reads it from disk), and caching each would fill the cache with them, one allocation per page,
    /// until it held TREE_CACHE_CAP of them.
    fn write_content(
        &self,
        file: &mut File,
        content: PageContent<'_, E>,
        next_page: &mut UnicodePageId,
        space: &mut FileSpace,
    ) -> io::Result<(u64, UnicodePageRef)> {
        count_op!(BTREE_PAGES_PLANNED, 1);
        let id = *next_page;
        *next_page = next_page.next()?;
        let offset = reserve_file_range(space, PAGE_SIZE as u64, PAGE_SIZE as u64);
        let page_ref = UnicodePageRef::new(id, UnicodeByteIndex::new(offset));
        let (first, bytes) = match content {
            PageContent::Leaf(items) => (
                items
                    .first()
                    .ok_or_else(|| io::Error::other("an empty B-tree page"))?
                    .key(),
                encode_btree_page(
                    items.len(),
                    E::LEAF_MAX as u8,
                    E::LEAF_ENTRY_SIZE,
                    0,
                    E::PAGE_TYPE,
                    page_ref,
                    |index, slot| <E as BTreeEntryReadWrite>::write(&items[index], slot),
                )?,
            ),
            PageContent::Inner(level, children) => (
                children
                    .first()
                    .ok_or_else(|| io::Error::other("an empty B-tree page"))?
                    .0,
                encode_btree_page(
                    children.len(),
                    INNER_MAX as u8,
                    24,
                    level,
                    E::PAGE_TYPE,
                    page_ref,
                    |index, slot| {
                        <UnicodeBTreePageEntry as BTreeEntryReadWrite>::write(
                            &UnicodeBTreePageEntry::new(children[index].0, children[index].1),
                            slot,
                        )
                    },
                )?,
            ),
        };
        write_at(file, offset, &bytes)?;
        Ok((first, page_ref))
    }

    /// Apply `changes` (key -> entry, or `None` to remove). Only the leaves whose key range received a change and their
    /// ancestors are read and rewritten; every replaced page's offset goes to `freed`. An empty change set does
    /// nothing. A change that would empty the whole tree is refused.
    pub(crate) fn apply(
        &mut self,
        file: &mut File,
        changes: &BTreeMap<u64, Option<E>>,
        next_page: &mut UnicodePageId,
        space: &mut FileSpace,
        freed: &mut Vec<u64>,
    ) -> io::Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        count_op!(BTREE_PLAN_CALLS, 1);
        let root_level = match &*self.page(self.root)? {
            TreePage::Leaf(_) => 0,
            TreePage::Inner(level, _) => *level,
        };
        let Some(mut level_pages) = self.rewrite(
            file,
            self.root,
            changes,
            (Bound::Unbounded, Bound::Unbounded),
            0,
            next_page,
            space,
            freed,
        )?
        else {
            return Ok(());
        };
        if level_pages.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a writer session cannot empty a B-tree",
            ));
        }
        let mut level = root_level;
        while level_pages.len() > 1 {
            level = level
                .checked_add(1)
                .filter(|level| *level <= 8)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Unicode PST B-tree exceeds eight levels",
                    )
                })?;
            let mut upper = Vec::new();
            for chunk in level_pages.chunks(INNER_MAX) {
                upper.push(self.write_page(
                    file,
                    TreePage::Inner(level, chunk.to_vec()),
                    next_page,
                    space,
                )?);
            }
            level_pages = upper;
        }
        self.root = level_pages[0].1;
        Ok(())
    }

    /// The pages that replace `page_ref` after `changes` (all within its key range), as (first key, page) in key order;
    /// `None` when nothing in it changes.
    #[allow(clippy::too_many_arguments)]
    fn rewrite(
        &self,
        file: &mut File,
        page_ref: UnicodePageRef,
        all: &BTreeMap<u64, Option<E>>,
        range: (Bound<u64>, Bound<u64>),
        depth: u8,
        next_page: &mut UnicodePageId,
        space: &mut FileSpace,
        freed: &mut Vec<u64>,
    ) -> io::Result<Option<Vec<(u64, UnicodePageRef)>>> {
        if depth > 8 {
            return Err(PstError::InvalidBTreePage(page_ref.index().index()).into());
        }
        let page = self.page(page_ref)?;
        match &*page {
            TreePage::Leaf(old) => {
                count_op!(BTREE_LEAF_ENTRIES_MERGED, old.len());
                let mut merged = Vec::with_capacity(old.len() + 1);
                let mut pending = all.range(range).peekable();
                for item in old {
                    while let Some((_, change)) = pending.next_if(|(key, _)| **key < item.key()) {
                        merged.extend(*change);
                    }
                    match pending.next_if(|(key, _)| **key == item.key()) {
                        Some((_, change)) => merged.extend(*change),
                        None => merged.push(*item),
                    }
                }
                merged.extend(pending.filter_map(|(_, change)| *change));
                if merged.len() == old.len()
                    && merged.iter().zip(old).all(|(a, b)| same_btree_entry(a, b))
                {
                    return Ok(None);
                }
                freed.push(page_ref.index().index());
                let mut out = Vec::new();
                for chunk in merged.chunks(E::LEAF_MAX) {
                    out.push(self.write_page(
                        file,
                        TreePage::Leaf(chunk.to_vec()),
                        next_page,
                        space,
                    )?);
                }
                Ok(Some(out))
            }
            TreePage::Inner(level, children) => {
                count_op!(BTREE_INNER_ENTRIES_VISITED, children.len());
                let mut replaced: Vec<Option<Vec<(u64, UnicodePageRef)>>> = Vec::new();
                let mut any = false;
                for index in 0..children.len() {
                    // Child `index` takes the keys of this page's range from its key up to the next child's key; the
                    // first child also the smaller ones, the last child the rest.
                    let low = if index == 0 {
                        range.0
                    } else {
                        Bound::Included(children[index].0)
                    };
                    let high = match children.get(index + 1) {
                        Some((key, _)) => Bound::Excluded(*key),
                        None => range.1,
                    };
                    let child_range = (low, high);
                    let empty = match (low, high) {
                        (Bound::Included(a), Bound::Excluded(b)) if a >= b => true,
                        _ => all.range(child_range).next().is_none(),
                    };
                    if empty {
                        replaced.push(None);
                        continue;
                    }
                    let result = self.rewrite(
                        file,
                        children[index].1,
                        all,
                        child_range,
                        depth + 1,
                        next_page,
                        space,
                        freed,
                    )?;
                    any |= result.is_some();
                    replaced.push(result);
                }
                if !any {
                    return Ok(None);
                }
                let mut entries = Vec::with_capacity(children.len() + 1);
                for (child, replacement) in children.iter().zip(replaced) {
                    match replacement {
                        Some(pages) => entries.extend(pages),
                        None => entries.push(*child),
                    }
                }
                freed.push(page_ref.index().index());
                let mut out = Vec::new();
                for chunk in entries.chunks(INNER_MAX) {
                    out.push(self.write_page(
                        file,
                        TreePage::Inner(*level, chunk.to_vec()),
                        next_page,
                        space,
                    )?);
                }
                Ok(Some(out))
            }
        }
    }

    /// Test only: the tree is well formed (keys ascending, every intermediate key is its child's first key, every leaf
    /// at the same depth, pages within their entry limits, levels descending by one) and the cache equals the disk.
    #[cfg(test)]
    fn assert_well_formed(&self) {
        fn same<E: DiskEntry>(a: &TreePage<E>, b: &TreePage<E>) -> bool {
            match (a, b) {
                (TreePage::Leaf(x), TreePage::Leaf(y)) => {
                    x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same_btree_entry(a, b))
                }
                (TreePage::Inner(l, x), TreePage::Inner(m, y)) => {
                    l == m
                        && x.len() == y.len()
                        && x.iter()
                            .zip(y)
                            .all(|(a, b)| a.0 == b.0 && same_page_ref(&a.1, &b.1))
                }
                _ => false,
            }
        }
        fn walk<E: DiskEntry>(
            tree: &DiskTree<E>,
            page_ref: UnicodePageRef,
            depth: usize,
            leaf_depth: &mut Option<usize>,
            keys: &mut Vec<u64>,
        ) -> u64 {
            let page = E::read_page(&mut tree.reader.borrow_mut(), page_ref).unwrap();
            if let Some(cached) = tree.cache.borrow().0.get(&page_ref.block().into_u64()) {
                assert!(
                    same(cached, &page),
                    "cached page {:#x} equals the disk",
                    page_ref.index().index()
                );
            }
            match &page {
                TreePage::Leaf(items) => {
                    assert!(!items.is_empty() && items.len() <= E::LEAF_MAX, "leaf size");
                    assert_eq!(
                        *leaf_depth.get_or_insert(depth),
                        depth,
                        "every leaf at the same depth"
                    );
                    keys.extend(items.iter().map(BTreeEntry::key));
                    items[0].key()
                }
                TreePage::Inner(level, children) => {
                    assert!(
                        !children.is_empty() && children.len() <= INNER_MAX,
                        "intermediate size"
                    );
                    for (key, child) in children {
                        if let TreePage::Inner(child_level, _) =
                            E::read_page(&mut tree.reader.borrow_mut(), *child).unwrap()
                        {
                            assert_eq!(child_level + 1, *level, "levels descend by one");
                        }
                        assert_eq!(
                            walk(tree, *child, depth + 1, leaf_depth, keys),
                            *key,
                            "intermediate key = child's first key"
                        );
                    }
                    children[0].0
                }
            }
        }
        let mut keys = Vec::new();
        walk(self, self.root, 0, &mut None, &mut keys);
        assert!(
            keys.windows(2).all(|pair| pair[0] < pair[1]),
            "keys ascending and unique"
        );
        assert!(
            self.cache.borrow().0.len() <= TREE_CACHE_CAP,
            "page cache bounded"
        );
    }
}

/// The block B-tree's right edge during one commit. New blocks get IDs above every committed key, so their entries
/// arrive in key order at the tree's right end. They go into the edge's open pages (one per level, copied from the
/// committed right path when the first one arrives), and every page that fills is written at once: a commit holds
/// O(depth) pages of new entries, however many blocks it writes. The edge becomes the tree's root path when the commit
/// finishes it; the committed right-path pages it replaces are released after the commit point.
pub(crate) struct RightEdge {
    leaf: Vec<UnicodeBlockBTreeEntry>,
    /// Open intermediate pages, level 1 upwards: the closed children (first key, page) in key order.
    levels: Vec<Vec<(u64, UnicodePageRef)>>,
    /// The committed right-path pages the edge replaces.
    replaced: Vec<u64>,
}

impl RightEdge {
    fn open(tree: &DiskTree<UnicodeBlockBTreeEntry>) -> io::Result<Self> {
        let mut path = Vec::new();
        let mut page_ref = tree.root;
        loop {
            if path.len() > 8 {
                return Err(PstError::InvalidBTreePage(page_ref.index().index()).into());
            }
            let page = tree.page(page_ref)?;
            let next = match &*page {
                TreePage::Leaf(_) => None,
                TreePage::Inner(_, children) => Some(
                    children
                        .last()
                        .ok_or(PstError::InvalidBTreePage(page_ref.index().index()))?
                        .1,
                ),
            };
            path.push((page_ref, page));
            match next {
                Some(child) => page_ref = child,
                None => break,
            }
        }
        let leaf = match &*path.last().expect("a path").1 {
            TreePage::Leaf(items) => items.clone(),
            TreePage::Inner(..) => unreachable!("the path ends in a leaf"),
        };
        let levels = path
            .iter()
            .rev()
            .skip(1)
            .map(|(_, page)| match &**page {
                TreePage::Inner(_, children) => children[..children.len() - 1].to_vec(),
                TreePage::Leaf(_) => Vec::new(),
            })
            .collect();
        Ok(Self {
            leaf,
            levels,
            replaced: path
                .iter()
                .map(|(page_ref, _)| page_ref.index().index())
                .collect(),
        })
    }

    fn push(
        &mut self,
        tree: &DiskTree<UnicodeBlockBTreeEntry>,
        file: &mut File,
        entry: UnicodeBlockBTreeEntry,
        next_page: &mut UnicodePageId,
        space: &mut FileSpace,
    ) -> io::Result<()> {
        self.leaf.push(entry);
        if self.leaf.len() >= 2 * UnicodeBlockBTreeEntry::LEAF_MAX {
            // Written from the open leaf; the entries then leave it in place (no vector per page).
            let written = tree.write_content(
                file,
                PageContent::Leaf(&self.leaf[..UnicodeBlockBTreeEntry::LEAF_MAX]),
                next_page,
                space,
            )?;
            self.leaf.drain(..UnicodeBlockBTreeEntry::LEAF_MAX);
            self.add(tree, file, 0, written, next_page, space)?;
        }
        Ok(())
    }

    fn add(
        &mut self,
        tree: &DiskTree<UnicodeBlockBTreeEntry>,
        file: &mut File,
        index: usize,
        child: (u64, UnicodePageRef),
        next_page: &mut UnicodePageId,
        space: &mut FileSpace,
    ) -> io::Result<()> {
        if self.levels.len() == index {
            self.levels.push(Vec::new());
        }
        self.levels[index].push(child);
        if self.levels[index].len() >= 2 * INNER_MAX {
            let level = u8::try_from(index + 1)
                .ok()
                .filter(|level| *level <= 8)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Unicode PST B-tree exceeds eight levels",
                    )
                })?;
            let written = tree.write_content(
                file,
                PageContent::Inner(level, &self.levels[index][..INNER_MAX]),
                next_page,
                space,
            )?;
            self.levels[index].drain(..INNER_MAX);
            self.add(tree, file, index + 1, written, next_page, space)?;
        }
        Ok(())
    }

    fn get(
        &self,
        tree: &DiskTree<UnicodeBlockBTreeEntry>,
        key: u64,
    ) -> io::Result<Option<UnicodeBlockBTreeEntry>> {
        if self.leaf.first().is_some_and(|first| key >= first.key()) {
            return Ok(self
                .leaf
                .binary_search_by_key(&key, BTreeEntry::key)
                .ok()
                .map(|index| self.leaf[index]));
        }
        for open in &self.levels {
            if open.first().is_some_and(|(first, _)| key >= *first) {
                return tree.get_from(open[child_for(open, key)].1, key);
            }
        }
        Ok(None)
    }

    fn last_key(&self) -> Option<u64> {
        self.leaf.last().map(BTreeEntry::key)
    }

    /// Every entry the edge holds (its written pages and its open leaf), in key order.
    fn for_each(
        &self,
        tree: &DiskTree<UnicodeBlockBTreeEntry>,
        mut visit: impl FnMut(&UnicodeBlockBTreeEntry),
    ) -> io::Result<()> {
        for open in self.levels.iter().rev() {
            for (_, child) in open {
                tree.for_each_from(*child, &mut visit)?;
            }
        }
        self.leaf.iter().for_each(visit);
        Ok(())
    }

    /// Write the open pages bottom-up; returns the new root.
    fn finish(
        self,
        tree: &DiskTree<UnicodeBlockBTreeEntry>,
        file: &mut File,
        next_page: &mut UnicodePageId,
        space: &mut FileSpace,
    ) -> io::Result<UnicodePageRef> {
        let mut lower = Vec::new();
        for chunk in self.leaf.chunks(UnicodeBlockBTreeEntry::LEAF_MAX) {
            lower.push(tree.write_page(file, TreePage::Leaf(chunk.to_vec()), next_page, space)?);
        }
        let mut level = 0_u8;
        for mut open in self.levels {
            level += 1;
            open.extend(lower);
            lower = Vec::new();
            for chunk in open.chunks(INNER_MAX) {
                lower.push(tree.write_page(
                    file,
                    TreePage::Inner(level, chunk.to_vec()),
                    next_page,
                    space,
                )?);
            }
        }
        while lower.len() > 1 {
            level = level
                .checked_add(1)
                .filter(|level| *level <= 8)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Unicode PST B-tree exceeds eight levels",
                    )
                })?;
            let mut upper = Vec::new();
            for chunk in lower.chunks(INNER_MAX) {
                upper.push(tree.write_page(
                    file,
                    TreePage::Inner(level, chunk.to_vec()),
                    next_page,
                    space,
                )?);
            }
            lower = upper;
        }
        lower
            .pop()
            .map(|(_, root)| root)
            .ok_or_else(|| io::Error::other("the right edge holds no page"))
    }
}

fn node_key(node: NodeId) -> u64 {
    u64::from(u32::from(node))
}

/// What a commit changes: key -> new entry, or `None` for a removed entry.
#[derive(Default)]
pub(crate) struct Changes {
    pub(crate) bbt: BTreeMap<u64, Option<UnicodeBlockBTreeEntry>>,
    pub(crate) nbt: BTreeMap<u64, Option<UnicodeNodeBTreeEntry>>,
    /// New internal blocks whose children's references their creator states in `explicit`: the reference counting does
    /// not walk their children.
    pub(crate) precounted: HashSet<u64>,
    /// Internal blocks this commit releases whose children's reference changes are stated in `explicit`; their
    /// children are not walked either. Each must actually be released, or the counts fall back to a full recount.
    pub(crate) released_handled: HashSet<u64>,
    /// Reference count changes stated by their creator.
    pub(crate) explicit: Vec<(UnicodeBlockId, i32)>,
    /// The committed block B-tree's largest key; a new entry above it is an append.
    floor: Option<u64>,
    /// Appends not yet in the right edge, in key order (moved there at every block flush).
    appends: Vec<UnicodeBlockBTreeEntry>,
    /// Appended internal blocks whose children the reference counting walks (not precounted): their number grows with
    /// the items of a commit, not with the bytes of a value (a streamed value's index blocks are precounted).
    appended_internal: Vec<UnicodeBlockBTreeEntry>,
    /// The block B-tree's right edge, once the commit has appended.
    edge: Option<RightEdge>,
    /// The next B-tree page ID: pages written during planning (the right edge) and at the commit share it.
    next_page: Option<UnicodePageId>,
    /// Tests only: every append, for the commit's entry check.
    #[cfg(test)]
    appended_log: Vec<UnicodeBlockBTreeEntry>,
}

#[cfg(test)]
thread_local! {
    /// Tests only: this thread's allocation calls, in all, over 15 KiB and over 127 KiB, when its last commit started,
    /// after its plan.
    static COMMIT_START: std::cell::Cell<(u64, u64, u64)> = const { std::cell::Cell::new((0, 0, 0)) };
}

thread_local! {
    /// The append buffer of the last commit's changes, kept for the next commit, like the block writer's buffer.
    static SPARE_APPENDS: RefCell<Vec<UnicodeBlockBTreeEntry>> = const { RefCell::new(Vec::new()) };
}

impl Drop for Changes {
    fn drop(&mut self) {
        let mut appends = std::mem::take(&mut self.appends);
        if appends.capacity() <= 2 * APPEND_BUFFER {
            appends.clear();
            SPARE_APPENDS.with(|spare| *spare.borrow_mut() = appends);
        }
    }
}

impl Changes {
    /// A new or changed block entry. One above every key so far (a new block) is an append for the right edge; any
    /// other goes to the change set, applied at the commit.
    pub(crate) fn put_block(&mut self, entry: UnicodeBlockBTreeEntry) {
        let key = entry.key();
        let above = |bound: Option<u64>| bound.is_none_or(|bound| key > bound);
        let last = self
            .appends
            .last()
            .map(BTreeEntry::key)
            .or_else(|| self.edge.as_ref().and_then(RightEdge::last_key));
        if self.floor.is_some() && above(self.floor) && above(last) && !self.bbt.contains_key(&key)
        {
            #[cfg(test)]
            crate::measurement::heap::uncounted(|| self.appended_log.push(entry));
            if entry.block().block().is_internal() && !self.precounted.contains(&key) {
                self.appended_internal.push(entry);
            }
            self.appends.push(entry);
        } else {
            self.bbt.insert(key, Some(entry));
        }
    }

    /// A block entry as this commit sees it: its change, its append, the right edge, or the committed tree.
    pub(crate) fn block(
        &self,
        state: &CommittedState,
        key: u64,
    ) -> io::Result<Option<UnicodeBlockBTreeEntry>> {
        if let Some(entry) = self.bbt.get(&key) {
            return Ok(*entry);
        }
        if self.floor.is_some_and(|floor| key > floor) {
            if let Ok(index) = self.appends.binary_search_by_key(&key, BTreeEntry::key) {
                return Ok(Some(self.appends[index]));
            }
            if let Some(edge) = &self.edge {
                return edge.get(&state.bbt, key);
            }
            return Ok(None);
        }
        state.bbt.get(key)
    }

    /// Every block entry as this commit sees it, in key order (O(blocks): the reference-count fallback and tests).
    pub(crate) fn all_block_entries(
        &self,
        state: &CommittedState,
    ) -> io::Result<Vec<UnicodeBlockBTreeEntry>> {
        let mut merged: BTreeMap<u64, UnicodeBlockBTreeEntry> = BTreeMap::new();
        state.bbt.for_each(|entry| {
            merged.insert(entry.key(), *entry);
        })?;
        if let Some(edge) = &self.edge {
            edge.for_each(&state.bbt, |entry| {
                merged.insert(entry.key(), *entry);
            })?;
        }
        merged.extend(self.appends.iter().map(|entry| (entry.key(), *entry)));
        for (key, change) in &self.bbt {
            match change {
                Some(entry) => merged.insert(*key, *entry),
                None => merged.remove(key),
            };
        }
        Ok(merged.into_values().collect())
    }

    fn drain_appends(
        &mut self,
        state: &CommittedState,
        file: &mut File,
        space: &mut FileSpace,
    ) -> io::Result<()> {
        if self.appends.is_empty() {
            return Ok(());
        }
        if self.edge.is_none() {
            self.edge = Some(RightEdge::open(&state.bbt)?);
        }
        let mut next_page = self.next_page.take().unwrap_or(state.header.next_page());
        let edge = self.edge.as_mut().expect("opened");
        // Drained in place, so the buffer keeps its capacity. Taking it would leave an empty one that grows again by
        // doubling up to APPEND_BUFFER entries (96 KiB) every 4,096 blocks.
        let mut result = Ok(());
        for entry in self.appends.drain(..) {
            if result.is_ok() {
                result = edge.push(&state.bbt, file, entry, &mut next_page, space);
            }
        }
        self.next_page = Some(next_page);
        result
    }
}

/// The committed state seen through one commit's pending changes.
pub(crate) struct Tx<'a> {
    state: &'a CommittedState,
    pub(crate) changes: Changes,
}

impl<'a> Tx<'a> {
    pub(crate) fn new(state: &'a CommittedState) -> Self {
        let mut changes = Changes::default();
        changes.floor = state.max_bbt_key;
        changes.next_page = Some(state.header.next_page());
        changes.appends = SPARE_APPENDS.with(|spare| std::mem::take(&mut *spare.borrow_mut()));
        Self { state, changes }
    }

    pub(crate) fn block(&self, key: u64) -> io::Result<Option<UnicodeBlockBTreeEntry>> {
        self.changes.block(self.state, key)
    }

    /// Move the buffered appends into the right edge once the buffer is full, writing every page that fills.
    pub(crate) fn drain_appends(
        &mut self,
        file: &mut File,
        space: &mut FileSpace,
    ) -> io::Result<()> {
        if self.changes.appends.len() < APPEND_BUFFER {
            return Ok(());
        }
        self.changes.drain_appends(self.state, file, space)
    }

    pub(crate) fn node(&self, node: NodeId) -> io::Result<Option<UnicodeNodeBTreeEntry>> {
        match self.changes.nbt.get(&node_key(node)) {
            Some(entry) => Ok(*entry),
            None => self.state.nbt.get(node_key(node)),
        }
    }

    pub(crate) fn put_block(&mut self, entry: UnicodeBlockBTreeEntry) {
        self.changes.put_block(entry);
    }

    pub(crate) fn put_node(&mut self, entry: UnicodeNodeBTreeEntry) {
        self.changes.nbt.insert(entry.key(), Some(entry));
    }

    /// The largest index of a committed node of this type.
    pub(crate) fn max_node_index(&self, id_type: NodeIdType) -> Option<u32> {
        self.state.max_node_index[id_type as usize]
    }

    /// The largest block index in the committed block B-tree.
    pub(crate) fn max_block_index(&self) -> Option<u64> {
        self.state.max_block_index
    }

    /// Every committed node of one type, in key order. O(nodes): used only by the folderless append's receive-folder
    /// lookup, which an append to a folder path never takes.
    pub(crate) fn nodes_of_type(
        &self,
        id_type: NodeIdType,
    ) -> io::Result<Vec<UnicodeNodeBTreeEntry>> {
        let mut nodes = Vec::new();
        self.state.nbt.for_each(|entry| {
            if matches!(entry.node().id_type(), Ok(t) if t == id_type) {
                nodes.push(*entry);
            }
        })?;
        Ok(nodes)
    }

    pub(crate) fn file_space(&self) -> FileSpace {
        self.state.amap.file_space()
    }
}

impl BlockLookup for Tx<'_> {
    fn lookup_block(&self, key: u64) -> io::Result<Option<UnicodeBlockBTreeEntry>> {
        self.block(key)
    }

    fn crypt(&self) -> NdbCryptMethod {
        self.state.header.crypt_method()
    }
}

/// Everything a planned change hands to `commit`.
pub(crate) struct CommitPlan {
    pub(crate) header: UnicodeHeader,
    pub(crate) changes: Changes,
    pub(crate) blocks: Vec<NewBlock>,
    pub(crate) next_bid: u64,
    pub(crate) next_offset: FileSpace,
    pub(crate) reserved_nodes: Vec<NodeId>,
}

/// At most this many free runs are kept as allocation candidates. Space beyond them stays correctly marked free in the
/// allocation map; it is only not reused by this session.
const FREE_RUN_CAP: usize = 1024;
/// At most this many AMap pages' bits are cached between commits.
const AMAP_CACHE_CAP: usize = 64;
/// AMap pages checked against the B-trees per pass at session open (496 bytes each).
const AMAP_CHECK_WINDOW: usize = 2048;

/// Every allocation the B-trees account for, as `rebuild_allocation_map` marks them: each page of both B-trees, and
/// each block of the block B-tree, read from disk page by page (nothing is retained).
fn btree_allocations(
    file: &mut File,
    nbt: UnicodePageRef,
    bbt: UnicodePageRef,
    mark: &mut dyn FnMut(u64, u64),
) -> io::Result<()> {
    let mut stack = vec![(nbt, false, 0_u8), (bbt, true, 0_u8)];
    while let Some((page_ref, blocks, depth)) = stack.pop() {
        if depth > 8 {
            return Err(PstError::InvalidBTreePage(page_ref.index().index()).into());
        }
        count_op!(BTREE_PAGES_READ, 1);
        mark(page_ref.index().index(), PAGE_SIZE as u64);
        let children: Vec<UnicodePageRef> = if blocks {
            match UnicodeBlockBTree::read(file, page_ref)? {
                RootBTreePage::Leaf(page) => {
                    for entry in page.entries() {
                        mark(
                            entry.block().index().index(),
                            block_marked_size(entry.size()),
                        );
                    }
                    Vec::new()
                }
                RootBTreePage::Intermediate(page, ..) => {
                    page.entries().iter().map(|entry| entry.block()).collect()
                }
            }
        } else {
            match UnicodeNodeBTree::read(file, page_ref)? {
                RootBTreePage::Leaf(_) => Vec::new(),
                RootBTreePage::Intermediate(page, ..) => {
                    page.entries().iter().map(|entry| entry.block()).collect()
                }
            }
        };
        stack.extend(children.into_iter().map(|child| (child, blocks, depth + 1)));
    }
    Ok(())
}

fn amap_page_base(index: usize) -> u64 {
    AMAP_FIRST_OFFSET + index as u64 * AMAP_DATA_SIZE
}

fn amap_page_count(eof: u64) -> u64 {
    eof.saturating_sub(AMAP_FIRST_OFFSET)
        .div_ceil(AMAP_DATA_SIZE)
}

fn amap_index_of(offset: u64) -> usize {
    ((offset - AMAP_FIRST_OFFSET) / AMAP_DATA_SIZE) as usize
}

fn set_bits(bits: &mut MapBits, start: usize, count: usize) {
    for bit in start..start + count {
        bits[bit / 8] |= 0x80 >> (bit % 8);
    }
}

fn clear_bits(bits: &mut MapBits, start: usize, count: usize) {
    for bit in start..start + count {
        bits[bit / 8] &= !(0x80 >> (bit % 8));
    }
}

/// Set (or clear) the bits of `[start, end)` that fall into AMap page `index`.
fn mark_range(bits: &mut MapBits, index: usize, start: u64, end: u64, allocated: bool) {
    let base = amap_page_base(index);
    let (start, end) = (start.max(base), end.min(base + AMAP_DATA_SIZE));
    if start >= end {
        return;
    }
    let (first, count) = (
        ((start - base) / 64) as usize,
        (end - start).div_ceil(64) as usize,
    );
    if allocated {
        set_bits(bits, first, count);
    } else {
        clear_bits(bits, first, count);
    }
}

/// The bits of a page that holds nothing but its own map pages.
fn reserved_bits(index: usize) -> MapBits {
    let mut bits = [0; size_of::<MapBits>()];
    bits[..map_page_count(index as u64) as usize].fill(0xFF);
    bits
}

fn free_bytes_of(bits: &MapBits) -> u64 {
    bits.iter()
        .map(|byte| u64::from(byte.count_zeros()))
        .sum::<u64>()
        * 64
}

fn block_marked_size(size: u16) -> u64 {
    u64::from(ndb::block::block_size(size + UnicodeBlockTrailer::SIZE))
}

fn map_page_trailer(page_type: PageType, offset: u64) -> UnicodePageTrailer {
    UnicodePageTrailer::new(page_type, 0, UnicodePageId::from(offset), 0)
}

/// A map page's 512 bytes, encoded into an array, not a new vector per page.
fn encode_map_page<const P: u8>(
    bits: MapBits,
    page_type: PageType,
    offset: u64,
) -> io::Result<[u8; PAGE_SIZE]> {
    let page = <UnicodeMapPage<P> as MapPageReadWrite<UnicodePstFile, P>>::new(
        bits,
        map_page_trailer(page_type, offset),
    )?;
    let mut bytes = [0_u8; PAGE_SIZE];
    let mut out = Cursor::new(&mut bytes[..]);
    <UnicodeMapPage<P> as MapPageReadWrite<UnicodePstFile, P>>::write(&page, &mut out)?;
    if out.position() != PAGE_SIZE as u64 {
        return Err(io::Error::other("a map page did not encode to one page"));
    }
    Ok(bytes)
}

fn write_at(file: &mut File, offset: u64, bytes: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)
}

/// An AMap page's bits as they are on disk.
fn read_amap_bits(file: &mut File, index: usize) -> io::Result<MapBits> {
    count_op!(AMAP_PAGES_READ, 1);
    let mut page = [0; PAGE_SIZE];
    file.seek(SeekFrom::Start(amap_page_base(index)))?;
    file.read_exact(&mut page)?;
    if page[size_of::<MapBits>()] != PageType::AllocationMap as u8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("AMap page {index} has the wrong page type"),
        ));
    }
    let mut bits = [0; size_of::<MapBits>()];
    bits.copy_from_slice(&page[..size_of::<MapBits>()]);
    Ok(bits)
}

/// Adds the free 64-byte slot at `at` to `runs` (offset order, adjacent slots joined). Once `cap` runs are kept, the
/// newest run is held in `tail` instead; `scan` then puts it in place of the last kept run, as
/// `AllocationState::commit` keeps the last run. The free tail of the last AMap range lies below EOF, so a session
/// opened on a file with more than `cap` runs would otherwise lose it and grow the file by a range.
fn add_free_slot(runs: &mut Vec<(u64, u64)>, tail: &mut Option<(u64, u64)>, at: u64, cap: usize) {
    if let Some((_, end)) = tail.as_mut().filter(|(_, end)| *end == at) {
        *end += 64;
    } else if tail.is_some() {
        *tail = Some((at, at + 64));
    } else if let Some((_, end)) = runs.last_mut().filter(|(_, end)| *end == at) {
        *end += 64;
    } else if runs.len() < cap {
        runs.push((at, at + 64));
    } else {
        *tail = Some((at, at + 64));
    }
}

/// The constant map pages a new AMap range carries besides its AMap page: PMap, FMap and FPMap pages, all 0xFF. FMap
/// pages are 0xFF as Outlook writes them, not each range's largest free run. Outlook never updates an FMap
/// (MS-PST 2.6.1: "SHOULD NOT modify FMaps after they are created"), so exact values become too small once it frees or
/// adds space: after classic Outlook added a message to a 10,000-item file, SCANPST reported "AMap page <@...> has
/// csFree of 0, but should have N".
fn constant_map_pages(index: usize) -> io::Result<impl Iterator<Item = (u64, [u8; PAGE_SIZE])>> {
    let index = index as u64;
    let mut pages = [None, None, None];
    if index.is_multiple_of(PMAP_PAGE_COUNT) {
        let offset = (index / PMAP_PAGE_COUNT) * PMAP_DATA_SIZE + PMAP_FIRST_OFFSET;
        pages[0] = Some((
            offset,
            encode_map_page::<{ PageType::AllocationPageMap as u8 }>(
                [0xFF; size_of::<MapBits>()],
                PageType::AllocationPageMap,
                offset,
            )?,
        ));
    }
    if index >= FMAP_FIRST_SIZE && (index - FMAP_FIRST_SIZE).is_multiple_of(FMAP_PAGE_COUNT) {
        let offset =
            ((index - FMAP_FIRST_SIZE) / FMAP_PAGE_COUNT) * FMAP_DATA_SIZE + FMAP_FIRST_OFFSET;
        pages[1] = Some((
            offset,
            encode_map_page::<{ PageType::FreeMap as u8 }>(
                [0xFF; size_of::<MapBits>()],
                PageType::FreeMap,
                offset,
            )?,
        ));
    }
    if index >= FPMAP_FIRST_SIZE && (index - FPMAP_FIRST_SIZE).is_multiple_of(FPMAP_PAGE_COUNT) {
        let offset =
            ((index - FPMAP_FIRST_SIZE) / FPMAP_PAGE_COUNT) * FPMAP_DATA_SIZE + FPMAP_FIRST_OFFSET;
        pages[2] = Some((
            offset,
            encode_map_page::<{ PageType::FreePageMap as u8 }>(
                [0xFF; size_of::<MapBits>()],
                PageType::FreePageMap,
                offset,
            )?,
        ));
    }
    Ok(pages.into_iter().flatten())
}

/// The bump allocator's progress through one AMap range: allocated from `first` to `end`, except the gaps.
struct RangeFill {
    index: usize,
    first: u64,
    end: u64,
    gaps: Vec<(u64, u64)>,
}

impl RangeFill {
    fn bits_onto(&self, mut bits: MapBits) -> MapBits {
        mark_range(&mut bits, self.index, self.first, self.end, true);
        for (start, end) in &self.gaps {
            mark_range(&mut bits, self.index, *start, *end, false);
        }
        bits
    }
}

/// What one commit allocates, recorded by `reserve_file_range` instead of a list of every block: allocations from free
/// runs below the committed EOF, and the progress of the EOF (bump) allocator range by range. A range beyond the
/// committed EOF that the bump allocator has left is complete; its map pages are queued and written by the next block
/// flush, before the commit point, which is safe because the committed state ends before it. The range that holds the
/// committed EOF and the range still being filled are applied after the commit point, like every change to committed
/// map pages.
pub(crate) struct AllocRecord {
    committed_eof: u64,
    /// AMap pages that exist in the committed file.
    committed_pages: usize,
    below: Vec<(u64, u64)>,
    boundary: Option<RangeFill>,
    current: Option<RangeFill>,
    /// Complete new ranges waiting to be written: AMap page index and its bits.
    complete: Vec<(usize, MapBits)>,
    /// The last new range written so far (all new ranges from `committed_pages` up to it are on disk).
    written_through: Option<usize>,
    /// Free bytes of the new ranges written so far.
    written_free: u64,
    /// Gaps of the most recent ranges, offered as free runs after the commit.
    recent_gaps: VecDeque<(u64, u64)>,
    /// The gap list of the last finished range, empty, for the next range.
    spare_gaps: Vec<(u64, u64)>,
}

impl AllocRecord {
    fn new(committed_eof: u64) -> Self {
        Self {
            committed_eof,
            committed_pages: amap_page_count(committed_eof) as usize,
            below: Vec::new(),
            boundary: None,
            current: None,
            complete: Vec::new(),
            written_through: None,
            written_free: 0,
            // Allocated once at its cap (16 KiB), so a large commit does not grow it on the way.
            recent_gaps: VecDeque::with_capacity(FREE_RUN_CAP),
            spare_gaps: Vec::new(),
        }
    }

    /// The record emptied for a commit at `committed_eof`, its collections keeping their capacity.
    fn reset(&mut self, committed_eof: u64) {
        self.committed_eof = committed_eof;
        self.committed_pages = amap_page_count(committed_eof) as usize;
        self.below.clear();
        self.boundary = None;
        self.current = None;
        self.complete.clear();
        self.written_through = None;
        self.written_free = 0;
        self.recent_gaps.clear();
        self.spare_gaps.clear();
    }

    /// An allocation from a free run below the committed EOF. One that continues the last one in the same
    /// allocation-map range extends it, so filling a gap with consecutive pages is one record, not one per page (a
    /// commit after a 1.5 GiB one recorded about 10,000 here). The map update marks the same bytes either way.
    pub(crate) fn below(&mut self, offset: u64, size: u64) {
        if let Some((start, length)) = self.below.last_mut() {
            if *start + *length == offset
                && amap_index_of(*start) == amap_index_of(offset + size - 1)
            {
                *length += size;
                return;
            }
        }
        self.below.push((offset, size));
    }

    /// An EOF allocation: the bump allocator moved from `start` to `offset` (skipping map pages, alignment and range
    /// tails) and took `size` bytes.
    pub(crate) fn bump(&mut self, start: u64, offset: u64, size: u64) {
        if offset < AMAP_FIRST_OFFSET {
            return;
        }
        let index = amap_index_of(offset);
        // Skipped space before `offset`: gaps in the ranges it crosses (map pages are reserved, not gaps).
        let mut at = start.max(AMAP_FIRST_OFFSET);
        while at < offset {
            let range = amap_index_of(at);
            let range_end = amap_page_base(range + 1);
            let data_start =
                amap_page_base(range) + map_page_count(range as u64) * PAGE_SIZE as u64;
            let (gap_start, gap_end) = (at.max(data_start), offset.min(range_end));
            if gap_start < gap_end {
                self.fill(range).gaps.push((gap_start, gap_end));
                if self.recent_gaps.len() == FREE_RUN_CAP {
                    self.recent_gaps.pop_front();
                }
                self.recent_gaps.push_back((gap_start, gap_end));
            }
            at = range_end;
        }
        let fill = self.fill(index);
        fill.end = offset + size;
    }

    /// The fill of range `index`, finishing the range before it when the bump allocator moved on.
    fn fill(&mut self, index: usize) -> &mut RangeFill {
        if self
            .current
            .as_ref()
            .is_some_and(|fill| fill.index != index)
        {
            let done = self.current.take().expect("checked");
            if done.index < self.committed_pages {
                self.boundary = Some(done);
            } else {
                let bits = done.bits_onto(reserved_bits(done.index));
                self.complete.push((done.index, bits));
                // The finished range's gap list serves the next range (no new allocation per range).
                let mut gaps = done.gaps;
                gaps.clear();
                self.spare_gaps = gaps;
            }
        }
        if self.current.is_none() {
            let data_start =
                amap_page_base(index) + map_page_count(index as u64) * PAGE_SIZE as u64;
            let first = data_start.max(self.committed_eof);
            self.current = Some(RangeFill {
                index,
                first,
                end: first,
                gaps: std::mem::take(&mut self.spare_gaps),
            });
        }
        self.current.as_mut().expect("set above")
    }

    /// Write the complete new ranges (before the commit point): their AMap pages and the constant map pages they carry.
    pub(crate) fn write_complete(&mut self, file: &mut File) -> io::Result<()> {
        // Drained and put back, so the vector keeps its capacity for the next block flush.
        let mut complete = std::mem::take(&mut self.complete);
        for (index, bits) in complete.drain(..) {
            count_op!(AMAP_PAGES_ENCODED, 1);
            let offset = amap_page_base(index);
            write_at(
                file,
                offset,
                &encode_map_page::<{ PageType::AllocationMap as u8 }>(
                    bits,
                    PageType::AllocationMap,
                    offset,
                )?,
            )?;
            for (offset, page) in constant_map_pages(index)? {
                write_at(file, offset, &page)?;
            }
            self.written_free += free_bytes_of(&bits);
            self.written_through = Some(index);
        }
        self.complete = complete;
        Ok(())
    }
}

/// The committed allocation map (MS-PST 2.2.2.7), without a per-block record: the map pages stay on disk; the session
/// keeps the EOF, the total of free bytes, a bounded list of free runs to allocate from, and a small cache of AMap
/// pages.
struct AllocationState {
    eof: u64,
    free_total: u64,
    /// Free runs below EOF in offset order, at most FREE_RUN_CAP (first-fit candidates).
    free_runs: Vec<(u64, u64)>,
    /// Recently used AMap pages' bits (as on disk), at most AMAP_CACHE_CAP, oldest first.
    cache: VecDeque<(usize, MapBits)>,
    /// The last commit's allocation record, emptied, for the next commit: its collections keep their capacity, so a
    /// commit does not hand the allocator its record's storage and back.
    spare_record: RefCell<Option<Box<AllocRecord>>>,
}

impl AllocationState {
    /// Session open: the allocation state, after checking the map on disk against the B-trees. The map pages are
    /// compared, window by window, with the bits the B-trees give (their pages and every block, as
    /// `rebuild_allocation_map` marks them), so memory stays bounded by the window. A map marked invalid, or one that
    /// differs (the Outlook template carries space no B-tree references; so may files from other writers), is rebuilt
    /// from the B-trees first. With `path` `None` (read only) nothing is repaired. Returns the state and, after a
    /// rebuild, the new header.
    fn open(
        file: &mut File,
        path: Option<&Path>,
        header: &UnicodeHeader,
    ) -> io::Result<(Self, Option<UnicodeHeader>)> {
        let mut current = header.clone();
        let mut rebuilt = None;
        for attempt in 0..2 {
            if current.root().amap_is_valid() != AmapStatus::Invalid {
                let (state, consistent) = Self::scan(file, &current)?;
                if consistent || path.is_none() {
                    return Ok((state, rebuilt));
                }
                if attempt == 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the allocation map still differs from the B-trees after a rebuild",
                    ));
                }
            }
            let Some(path) = path else {
                let (state, _) = Self::scan(file, &current)?;
                return Ok((state, rebuilt));
            };
            // Rebuild: mark the map invalid (durably, so an interrupted rebuild is redone), then rebuild it.
            current.root_mut().set_amap_status(AmapStatus::Invalid);
            current.update_unique();
            file.seek(SeekFrom::Start(0))?;
            current.write(file)?;
            file.sync_data()?;
            let mut pst = UnicodePstFile::open(path)?;
            pst.inner.rebuild_allocation_map()?;
            drop(pst);
            file.seek(SeekFrom::Start(0))?;
            current = UnicodeHeader::read(file)?;
            rebuilt = Some(current.clone());
        }
        Err(io::Error::other("allocation map rebuild did not converge"))
    }

    /// One pass over the map: free byte total, the first free runs, and whether every AMap page equals what the
    /// B-trees give (`AMAP_CHECK_WINDOW` pages at a time; both B-trees are walked once per window).
    fn scan(file: &mut File, header: &UnicodeHeader) -> io::Result<(Self, bool)> {
        let root = header.root();
        let eof = root.file_eof_index().index();
        let count = amap_page_count(eof) as usize;
        let mut state = Self {
            eof,
            free_total: 0,
            free_runs: Vec::new(),
            cache: VecDeque::new(),
            spare_record: RefCell::new(None),
        };
        let mut consistent = true;
        let mut tail = None;
        let mut window = 0;
        while window < count {
            let end = (window + AMAP_CHECK_WINDOW).min(count);
            let mut expected: Vec<MapBits> = (window..end).map(reserved_bits).collect();
            btree_allocations(
                file,
                *root.node_btree(),
                *root.block_btree(),
                &mut |offset, size| {
                    let (first, last) = (amap_index_of(offset), amap_index_of(offset + size - 1));
                    for index in first.max(window)..=last.min(end - 1) {
                        mark_range(
                            &mut expected[index - window],
                            index,
                            offset,
                            offset + size,
                            true,
                        );
                    }
                },
            )?;
            for index in window..end {
                let bits = read_amap_bits(file, index)?;
                consistent &= bits == expected[index - window];
                let bits = expected[index - window];
                state.free_total += free_bytes_of(&bits);
                let base = amap_page_base(index);
                for bit in 0..size_of::<MapBits>() * 8 {
                    let at = base + bit as u64 * 64;
                    if at >= eof {
                        break;
                    }
                    if bits[bit / 8] & (0x80 >> (bit % 8)) == 0 {
                        add_free_slot(&mut state.free_runs, &mut tail, at, FREE_RUN_CAP);
                    }
                }
            }
            window = end;
        }
        if let Some(run) = tail {
            *state.free_runs.last_mut().expect("the cap was reached") = run;
        }
        // An FMap page that is not all 0xFF (written by an earlier version of this writer, or by Outlook 2003) holds
        // values this writer never updates, so they would fall below the real free runs. Such a file is treated like a
        // differing map: `rebuild_allocation_map` writes its FMap pages as 0xFF.
        for k in 0..(count as u64)
            .max(FMAP_FIRST_SIZE)
            .saturating_sub(FMAP_FIRST_SIZE)
            .div_ceil(FMAP_PAGE_COUNT)
        {
            let mut bytes = [0; size_of::<MapBits>()];
            file.seek(SeekFrom::Start(k * FMAP_DATA_SIZE + FMAP_FIRST_OFFSET))?;
            file.read_exact(&mut bytes)?;
            consistent &= bytes.iter().all(|byte| *byte == 0xFF);
        }
        Ok((state, consistent))
    }

    fn file_space(&self) -> FileSpace {
        let mut space = FileSpace::from(self.eof);
        space.free = self.free_runs.clone();
        space.max_run = space
            .free
            .iter()
            .map(|(start, end)| end - start)
            .max()
            .unwrap_or(0);
        space.record = Some(match self.spare_record.borrow_mut().take() {
            Some(mut record) => {
                record.reset(self.eof);
                record
            }
            None => Box::new(AllocRecord::new(self.eof)),
        });
        space
    }

    fn cached_bits(&mut self, file: &mut File, index: usize) -> io::Result<MapBits> {
        if let Some(position) = self.cache.iter().position(|(cached, _)| *cached == index) {
            let entry = self.cache.remove(position).expect("found");
            let bits = entry.1;
            self.cache.push_back(entry);
            return Ok(bits);
        }
        let bits = read_amap_bits(file, index)?;
        self.remember(index, bits);
        Ok(bits)
    }

    fn remember(&mut self, index: usize, bits: MapBits) {
        self.cache.retain(|(cached, _)| *cached != index);
        if self.cache.len() == AMAP_CACHE_CAP {
            self.cache.pop_front();
        }
        self.cache.push_back((index, bits));
    }

    /// After the commit point: apply the commit's allocations below and at the committed EOF, finish the range being
    /// filled, release `freed` (offset, bytes), and write every changed map page. Returns the new free byte total and
    /// AMap page count. The pages are written, not synced; the caller syncs before marking the map valid.
    fn commit(
        &mut self,
        file: &mut File,
        space: FileSpace,
        freed: &[(u64, u64)],
        new_eof: u64,
    ) -> io::Result<(u64, u64)> {
        let FileSpace {
            free: remaining,
            record,
            eof: used_end,
            ..
        } = space;
        let mut record = record
            .ok_or_else(|| io::Error::other("a session commit needs its allocation record"))?;
        record.write_complete(file)?;
        let committed_pages = record.committed_pages;
        let written_through = record.written_through;
        // The pages this commit changes, in page order: each page's bits are read (or, for a new range, its reserved
        // bits taken), its changes applied and the page written, one page at a time, so a commit does not hold every
        // changed page's bits until all are known (up to about FREE_RUN_CAP pages, 0.5 MB and more, in a commit that
        // fills the previous commit's gaps). The changes to one page are disjoint ranges (allocations from free space,
        // space freed by this commit, the bump allocator's fills), so the order they are applied in does not change the
        // result.
        let mut fills: Vec<RangeFill> = [record.boundary.take(), record.current.take()]
            .into_iter()
            .flatten()
            .collect();
        fills.sort_unstable_by_key(|fill| fill.index);
        record.below.sort_unstable();
        let mut freed: Vec<(u64, u64)> = freed.to_vec();
        freed.sort_unstable();
        let (mut next_fill, mut next_below, mut next_freed) = (0, 0, 0);
        let mut free_total = self.free_total + record.written_free;
        loop {
            let index = [
                fills.get(next_fill).map(|fill| fill.index),
                record
                    .below
                    .get(next_below)
                    .map(|(offset, _)| amap_index_of(*offset)),
                freed
                    .get(next_freed)
                    .map(|(offset, _)| amap_index_of(*offset)),
            ]
            .into_iter()
            .flatten()
            .min();
            let Some(index) = index else { break };
            let on_disk =
                index < committed_pages || written_through.is_some_and(|last| index <= last);
            let (mut bits, counted) = if on_disk {
                let bits = self.cached_bits(file, index)?;
                (bits, free_bytes_of(&bits))
            } else {
                (reserved_bits(index), 0)
            };
            while let Some(fill) = fills.get(next_fill).filter(|fill| fill.index == index) {
                bits = fill.bits_onto(bits);
                if fill.index >= committed_pages {
                    for (offset, page) in constant_map_pages(fill.index)? {
                        write_at(file, offset, &page)?;
                    }
                }
                next_fill += 1;
            }
            while let Some((offset, size)) = record
                .below
                .get(next_below)
                .copied()
                .filter(|(offset, _)| amap_index_of(*offset) == index)
            {
                mark_range(&mut bits, index, offset, offset + size, true);
                next_below += 1;
            }
            while let Some((offset, size)) = freed
                .get(next_freed)
                .copied()
                .filter(|(offset, _)| amap_index_of(*offset) == index)
            {
                mark_range(&mut bits, index, offset, offset + size, false);
                next_freed += 1;
            }
            count_op!(AMAP_PAGES_ENCODED, 1);
            free_total = free_total + free_bytes_of(&bits) - counted;
            let offset = amap_page_base(index);
            write_at(
                file,
                offset,
                &encode_map_page::<{ PageType::AllocationMap as u8 }>(
                    bits,
                    PageType::AllocationMap,
                    offset,
                )?,
            )?;
            self.remember(index, bits);
        }
        // Allocation candidates for the next commit: what this commit left of the old runs, the space it freed and
        // the gaps it left, in offset order, adjacent runs joined, at most FREE_RUN_CAP.
        let mut runs: Vec<(u64, u64)> = remaining;
        runs.extend(freed.iter().map(|(offset, size)| (*offset, offset + size)));
        runs.extend(record.recent_gaps.iter().copied());
        // The free tail of the last AMap range, which the file now includes, so it is not left unused.
        if used_end < new_eof {
            runs.push((used_end, new_eof));
        }
        runs.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(runs.len().min(FREE_RUN_CAP));
        for (start, end) in runs {
            match merged.last_mut() {
                Some((_, last_end)) if *last_end >= start => *last_end = (*last_end).max(end),
                _ => merged.push((start, end)),
            }
        }
        // At most FREE_RUN_CAP, keeping the last run: it holds the tail, the largest place to allocate before EOF.
        if merged.len() > FREE_RUN_CAP {
            let last = merged.pop().expect("longer than the cap");
            merged.truncate(FREE_RUN_CAP - 1);
            merged.push(last);
        }
        self.free_runs = merged;
        self.free_total = free_total;
        self.eof = new_eof;
        *self.spare_record.borrow_mut() = Some(record);
        Ok((free_total, amap_page_count(new_eof)))
    }
}

/// Committed state of one Unicode PST, kept by a writer session between commits.
pub(crate) struct CommittedState {
    pub(crate) header: UnicodeHeader,
    density_list: Option<UnicodeDensityListPage>,
    pub(crate) bbt: DiskTree<UnicodeBlockBTreeEntry>,
    pub(crate) nbt: DiskTree<UnicodeNodeBTreeEntry>,
    amap: AllocationState,
    max_node_index: [Option<u32>; 32],
    max_block_index: Option<u64>,
    /// The block B-tree's largest key (entries above it are appends).
    max_bbt_key: Option<u64>,
}

impl CommittedState {
    /// The committed state of the file at `path`, opened as `file` (an invalid allocation map is rebuilt first).
    fn read(file: &mut File, path: &Path) -> io::Result<Self> {
        let state = Self::read_any(file, Some(path))?;
        // The writer keeps the file's encoding; Outlook writes permute (compressible) encoding by default.
        if !matches!(
            state.header.crypt_method(),
            NdbCryptMethod::None | NdbCryptMethod::Permute
        ) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "append supports unencrypted and permute-encoded Unicode PSTs, not cyclic encoding",
            ));
        }
        Ok(state)
    }

    /// `path`: the file may be repaired (an invalid allocation map rebuilt); `None`: read only.
    fn read_any(file: &mut File, path: Option<&Path>) -> io::Result<Self> {
        file.seek(SeekFrom::Start(0))?;
        let mut header = UnicodeHeader::read(file)?;
        let (amap, rebuilt) = AllocationState::open(file, path, &header)?;
        if let Some(rebuilt) = rebuilt {
            header = rebuilt;
        }
        let root = header.root().clone();
        // The B-trees stay on disk; the session holds their roots and page caches.
        let (bbt, nbt): (
            DiskTree<UnicodeBlockBTreeEntry>,
            DiskTree<UnicodeNodeBTreeEntry>,
        ) = match path {
            Some(path) => (
                DiskTree::open(path, *root.block_btree())?,
                DiskTree::open(path, *root.node_btree())?,
            ),
            None => (
                DiskTree::over(file.try_clone()?, *root.block_btree()),
                DiskTree::over(file.try_clone()?, *root.node_btree()),
            ),
        };
        let density_list =
            <UnicodeDensityListPage as DensityListPageReadWrite<UnicodePstFile>>::read(file).ok();
        // One pass over the node B-tree (session open only) for the largest index of each node type.
        let mut max_node_index = [None; 32];
        nbt.for_each(|entry| {
            let node = entry.node();
            let slot = &mut max_node_index[(u32::from(node) & 0x1F) as usize];
            *slot = Some(slot.map_or(node.index(), |max: u32| max.max(node.index())));
        })?;
        let last_block = bbt.last()?;
        let max_block_index = last_block.map(|entry| entry.block().block().index());
        let max_bbt_key = last_block.map(|entry| entry.key());
        Ok(Self {
            header,
            density_list,
            bbt,
            nbt,
            amap,
            max_node_index,
            max_block_index,
            max_bbt_key,
        })
    }
}

/// The writer core: the file, a read handle for the store reader and the committed state (`None` once poisoned).
pub(crate) struct WriterCore {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) path: PathBuf,
    pub(crate) file: File,
    reader: File,
    state: Option<CommittedState>,
    poisoned: Option<String>,
    /// Per folder (NID): the contents table's append state as the last commit to that folder left it.
    pub(crate) contents: HashMap<u32, ContentsState>,
    /// Folder paths resolved or created in this session.
    pub(crate) folders: HashMap<Vec<String>, NodeId>,
    /// Per parent folder (NID): its hierarchy table's append state as the last commit left it.
    pub(crate) hierarchy: HashMap<u32, ContentsState>,
    /// Per parent folder (NID) whose children were looked up: the children by exact display name.
    pub(crate) children: HashMap<u32, HashMap<String, NodeId>>,
    /// The store's PidNameKeywords property ID once resolved (the writer never changes the name-to-id map).
    pub(crate) keywords: Option<u16>,
    /// The store's name-to-ID map as the last successful commit left it, once read.
    pub(crate) names: Option<crate::NameMap>,
    /// The largest file this session may commit; `None` is no limit.
    pub(crate) max_file_size: Option<u64>,
    /// Tests only: every entry of both B-trees, kept in memory to check each commit's copy-on-write result.
    #[cfg(test)]
    shadow: Option<(
        BTreeMap<u64, UnicodeBlockBTreeEntry>,
        BTreeMap<u64, UnicodeNodeBTreeEntry>,
    )>,
}

impl WriterCore {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let state = CommittedState::read(&mut file, path)?;
        #[cfg(test)]
        let shadow = Some((
            state
                .bbt
                .entries()?
                .into_iter()
                .map(|entry| (entry.key(), entry))
                .collect(),
            state
                .nbt
                .entries()?
                .into_iter()
                .map(|entry| (entry.key(), entry))
                .collect(),
        ));
        Ok(Self {
            #[cfg(test)]
            shadow,
            path: path.to_path_buf(),
            reader: File::open(path)?,
            file,
            state: Some(state),
            poisoned: None,
            contents: HashMap::new(),
            folders: HashMap::new(),
            hierarchy: HashMap::new(),
            children: HashMap::new(),
            keywords: None,
            names: None,
            max_file_size: None,
        })
    }

    /// A commit patched `parent`'s hierarchy table (a row's cells): keep its append state's roots current, or drop the
    /// state when the rows are no longer in a subnode.
    pub(crate) fn hierarchy_changed(
        &mut self,
        parent: NodeId,
        heap: UnicodeBlockId,
        rows: Option<UnicodeBlockId>,
    ) {
        let key = u32::from(parent);
        match (self.hierarchy.get_mut(&key), rows) {
            (Some(state), Some(rows)) => {
                state.heap_root = heap;
                state.rows_root = rows;
            }
            (Some(_), None) => {
                self.hierarchy.remove(&key);
            }
            (None, _) => {}
        }
    }

    pub(crate) fn state(&self) -> io::Result<&CommittedState> {
        self.state.as_ref().ok_or_else(|| {
            io::Error::other(format!(
                "the PST writer session is unusable after a failed commit ({}); open the file again",
                self.poisoned.as_deref().unwrap_or("unknown error")
            ))
        })
    }

    /// A store reader over the committed state (a fresh reader per commit: the reader's caches would be stale after
    /// the next commit).
    pub(crate) fn store(&self) -> io::Result<Rc<UnicodeStore>> {
        // The clone shares the handle's file position; the header is read from wherever it points.
        let mut reader = self.reader.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        UnicodeStore::read(Rc::new(UnicodePstFile::read_from(Box::new(reader))?))
    }

    /// Run a planned change: `plan` reads what it needs and writes new blocks; then the change is committed. Any
    /// error poisons the session.
    pub(crate) fn run<T>(
        &mut self,
        plan: impl FnOnce(&mut File, &CommittedState, Rc<UnicodeStore>) -> io::Result<(CommitPlan, T)>,
    ) -> io::Result<T> {
        // The committed end of the file, taken before planning (which may already write blocks past it).
        let committed_eof = self.state()?.header.root().file_eof_index().index();
        crate::measurement::commit_started();
        let planned: io::Result<(CommitPlan, T)> = match self.store() {
            Ok(store) => match self.state.as_ref() {
                Some(state) => plan(&mut self.file, state, store),
                None => Err(io::Error::other("the PST writer session is poisoned")),
            },
            Err(error) => Err(error),
        };
        let result = planned.and_then(|(planned, value)| self.commit(planned).map(|()| value));
        if result.is_ok() {
            crate::measurement::commit_finished(|| self.structure_report());
        }
        if let Err(error) = &result {
            // A commit that would end the file past the limit wrote nothing before its commit point but blocks and
            // pages in space the committed state marks free or beyond its end, and no header; cutting the file back to
            // its committed end leaves the last commit's store, complete. The session stays poisoned (its state was
            // changed by the abandoned commit); `UnicodePstWriter::open` reads the file again.
            if error
                .get_ref()
                .is_some_and(|inner| inner.is::<crate::UnicodePstFileLimitReached>())
            {
                if let Err(cut) = self
                    .file
                    .set_len(committed_eof)
                    .and_then(|()| self.file.sync_data())
                {
                    self.poisoned = Some(format!(
                        "{error}; cutting the file back to its last commit failed: {cut}"
                    ));
                    self.state = None;
                    self.contents.clear();
                    self.folders.clear();
                    self.hierarchy.clear();
                    self.children.clear();
                    return Err(io::Error::other(format!(
                        "{error}; cutting the file back to its last commit failed: {cut}"
                    )));
                }
            }
            self.poisoned = Some(error.to_string());
            self.state = None;
            self.contents.clear();
            self.folders.clear();
            self.hierarchy.clear();
            self.children.clear();
        }
        result
    }

    fn commit(&mut self, plan: CommitPlan) -> io::Result<()> {
        #[cfg(test)]
        COMMIT_START.with(|calls| {
            use crate::measurement::heap::{
                calls_on_this_thread, huge_calls_on_this_thread, large_calls_on_this_thread,
            };
            calls.set((
                calls_on_this_thread(),
                large_calls_on_this_thread(),
                huge_calls_on_this_thread(),
            ))
        });
        let mut state = self
            .state
            .take()
            .ok_or_else(|| io::Error::other("the PST writer session is poisoned"))?;
        let file = &mut self.file;
        let CommitPlan {
            mut header,
            mut changes,
            blocks,
            next_bid,
            mut next_offset,
            reserved_nodes,
        } = plan;
        for block in &blocks {
            if block.precounted {
                changes.precounted.insert(block.id.search_key());
            }
            changes.put_block(new_block_entry(block));
        }

        // New blocks and pages only go to space the committed allocation map marks free, so the committed state stays
        // intact until the header switches roots: one barrier before that switch and one after it suffice.
        header.root_mut().set_amap_status(AmapStatus::Invalid);
        header.update_unique();
        let mut writer = BlockWriter::new(header.crypt_method());
        for block in blocks {
            writer.push(file, block)?;
        }
        writer.flush(file)?;

        update_block_ref_counts_delta(file, &state, &mut changes)?;

        // Space this commit frees, released after the commit point: the blocks it removes (their sizes from the committed
        // entries) and, below, the B-tree pages it replaces. Its allocations are recorded by the file-space allocator.
        let mut freed: Vec<(u64, u64)> = Vec::new();
        for (key, change) in &changes.bbt {
            if change.is_none() {
                // A committed block, or one this commit wrote and then dropped (its space was allocated here).
                let old = match changes.floor.is_some_and(|floor| *key > floor) {
                    true => match changes.appends.binary_search_by_key(key, BTreeEntry::key) {
                        Ok(index) => Some(changes.appends[index]),
                        Err(_) => changes
                            .edge
                            .as_ref()
                            .map(|edge| edge.get(&state.bbt, *key))
                            .transpose()?
                            .flatten(),
                    },
                    false => state.bbt.get(*key)?,
                };
                if let Some(old) = old {
                    freed.push((old.block().index().index(), block_marked_size(old.size())));
                }
            }
        }
        #[cfg(test)]
        let shadow_changes = {
            let mut bbt: BTreeMap<u64, Option<UnicodeBlockBTreeEntry>> = changes
                .appended_log
                .iter()
                .map(|entry| (entry.key(), Some(*entry)))
                .collect();
            bbt.extend(changes.bbt.iter().map(|(key, change)| (*key, *change)));
            (bbt, changes.nbt.clone())
        };
        // Changes to entries still in the append buffer (reference counts of new blocks) are made there, so those
        // entries are written once, with their final counts; then the buffer moves into the right edge.
        let buffered: Vec<u64> = changes
            .bbt
            .range(changes.appends.first().map_or(u64::MAX, BTreeEntry::key)..)
            .map(|(key, _)| *key)
            .collect();
        for key in buffered {
            if let Ok(index) = changes.appends.binary_search_by_key(&key, BTreeEntry::key) {
                match changes.bbt.remove(&key).flatten() {
                    Some(entry) => changes.appends[index] = entry,
                    None => {
                        changes.appends.remove(index);
                    }
                }
            }
        }
        changes.drain_appends(&state, file, &mut next_offset)?;
        // The largest block key and index after this commit (appends are the largest).
        let appended_last = changes
            .edge
            .as_ref()
            .and_then(|edge| edge.leaf.last().copied());

        // Each tree rewrites only the paths of the keys this commit changes (copy on write); a tree with no change is
        // left as it is. The new pages are written here, before the commit point.
        let mut next_page = changes.next_page.take().unwrap_or(header.next_page());
        let mut freed_pages = Vec::new();
        // The right edge first (its open pages become the tree's right path), then the other changes on that tree.
        if let Some(edge) = changes.edge.take() {
            freed_pages.extend(edge.replaced.iter().copied());
            state.bbt.root = edge.finish(&state.bbt, file, &mut next_page, &mut next_offset)?;
        }
        state.bbt.apply(
            file,
            &changes.bbt,
            &mut next_page,
            &mut next_offset,
            &mut freed_pages,
        )?;
        state.nbt.apply(
            file,
            &changes.nbt,
            &mut next_page,
            &mut next_offset,
            &mut freed_pages,
        )?;
        let (bbt_ref, nbt_ref) = (state.bbt.root, state.nbt.root);
        // MS-PST 2.6.1.1.2, "The PST file MUST grow at integer multiples of the number of bytes mapped by an AMap". The
        // file ends where its last AMap range ends, as Outlook's files do. cbAMapFree counts that range's free tail; a
        // file that stopped short would claim free space past its end, and Outlook, growing it, would count that space
        // a second time (SCANPST finds this after classic Outlook adds a message to such a file). The tail is a free
        // run for the next commit (`AllocationState::commit`).
        let new_eof = AMAP_FIRST_OFFSET
            + next_offset
                .eof
                .saturating_sub(AMAP_FIRST_OFFSET)
                .div_ceil(AMAP_DATA_SIZE)
                * AMAP_DATA_SIZE;
        // No header is written for a file that ends past the session's limit. The commit is abandoned here, before its
        // map pages, its length and its header; `run` cuts the file back to the committed end.
        if let Some(limit) = self.max_file_size {
            if new_eof > limit {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    crate::UnicodePstFileLimitReached {
                        size: new_eof,
                        limit,
                    },
                ));
            }
        }
        let amap_last = AMAP_FIRST_OFFSET
            + ((new_eof - AMAP_FIRST_OFFSET - 1) / AMAP_DATA_SIZE) * AMAP_DATA_SIZE;
        freed.extend(freed_pages.iter().map(|offset| (*offset, PAGE_SIZE as u64)));
        // Map pages of complete new ranges (all beyond the committed EOF) go before the commit point.
        if let Some(record) = next_offset.record.as_mut() {
            record.write_complete(file)?;
        }
        file.set_len(new_eof)?;
        file.sync_data()?;

        header.set_next_block(UnicodeBlockId::new(false, next_bid)?);
        header.set_next_page(next_page);
        for node in &reserved_nodes {
            header.reserve_node(*node)?;
        }
        header.root_mut().set_btrees(nbt_ref, bbt_ref);
        header
            .root_mut()
            .set_file_eof_index(UnicodeByteIndex::new(new_eof));
        header
            .root_mut()
            .set_amap_last_index(UnicodeByteIndex::new(amap_last));
        header.update_unique();
        file.seek(SeekFrom::Start(0))?;
        header.write(file)?;
        file.sync_data()?;
        // Commit point: the new roots are durable.

        let (free_bytes, amap_count) = state.amap.commit(file, next_offset, &freed, new_eof)?;
        file.flush()?;
        // The maps must be durable before the header marks them valid.
        file.sync_data()?;
        // MS-PST deprecates the header's rgbFM and rgbFP: they MUST be 0xFF, which is what Outlook writes and SCANPST restores.
        <UnicodeHeader as HeaderReadWrite<UnicodePstFile>>::first_free_map(&mut header).fill(0xFF);
        <UnicodeHeader as HeaderReadWrite<UnicodePstFile>>::first_free_page_map(&mut header)
            .fill(0xFF);
        header.update_unique();
        header.root_mut().set_amap_last_index(UnicodeByteIndex::new(
            AMAP_FIRST_OFFSET + amap_count.saturating_sub(1) * AMAP_DATA_SIZE,
        ));
        header
            .root_mut()
            .reset_free_size(UnicodeByteIndex::new(free_bytes))?;
        header.root_mut().set_amap_status(AmapStatus::Valid2);
        file.seek(SeekFrom::Start(0))?;
        header.write(file)?;
        file.flush()?;

        // Density list: what ensure_density_list and finish_write did with the file reopened.
        let keep = state
            .density_list
            .as_ref()
            .is_some_and(|list| list.trailer().block_id() == header.next_page());
        if !keep {
            let current_page = u32::try_from(
                (header.root().amap_last_index().index() - AMAP_FIRST_OFFSET) / AMAP_DATA_SIZE,
            )
            .map_err(|_| PstError::IntegerConversion)?;
            let block_id = header.next_page();
            let signature = PageType::DensityList
                .signature(ndb::page::DENSITY_LIST_FILE_OFFSET, block_id.into_u64());
            let trailer = UnicodePageTrailer::new(PageType::DensityList, signature, block_id, 0);
            state.density_list = Some(<UnicodeDensityListPage as DensityListPageReadWrite<
                UnicodePstFile,
            >>::new(false, current_page, &[], trailer)?);
        }
        header.update_unique();
        header.root_mut().set_amap_status(AmapStatus::Valid2);
        let density_list = state.density_list.as_ref().and_then(|list| {
            <UnicodeDensityListPage as DensityListPageReadWrite<UnicodePstFile>>::new(
                list.backfill_complete(),
                list.current_page(),
                list.entries(),
                *list.trailer(),
            )
            .ok()
        });
        file.seek(SeekFrom::Start(0))?;
        header.write(file)?;
        file.flush()?;
        if let Some(list) = &density_list {
            list.write(file)?;
            file.flush()?;
        }
        state.density_list = density_list;

        for (key, change) in &changes.nbt {
            if let Some(entry) = change {
                let node = entry.node();
                let slot = &mut state.max_node_index[(u32::from(node) & 0x1F) as usize];
                *slot = Some(slot.map_or(node.index(), |max: u32| max.max(node.index())));
            }
            let _ = key;
        }
        for change in changes.bbt.values().flatten().chain(appended_last.iter()) {
            let index = change.block().block().index();
            state.max_block_index = Some(state.max_block_index.map_or(index, |max| max.max(index)));
            state.max_bbt_key = Some(
                state
                    .max_bbt_key
                    .map_or(change.key(), |max| max.max(change.key())),
            );
        }
        state.header = header;
        self.state = Some(state);
        #[cfg(test)]
        {
            let (bbt, nbt) = shadow_changes;
            self.assert_matches_disk(bbt, nbt)?;
        }
        Ok(())
    }

    /// Test-only proof that the session's state equals a fresh read of the file, and that its allocation map equals a
    /// full rebuild from the B-trees on disk (on a copy of the file).
    #[cfg(test)]
    fn assert_matches_disk(
        &mut self,
        bbt_changes: BTreeMap<u64, Option<UnicodeBlockBTreeEntry>>,
        nbt_changes: BTreeMap<u64, Option<UnicodeNodeBTreeEntry>>,
    ) -> io::Result<()> {
        // The B-trees: both are well formed and their cached pages equal the disk, and their entries equal the entries
        // before this commit with its changes applied (a full-entry shadow kept by the tests).
        fn apply_shadow<E: DiskEntry>(
            shadow: &mut BTreeMap<u64, E>,
            changes: BTreeMap<u64, Option<E>>,
        ) {
            for (key, change) in changes {
                match change {
                    Some(entry) => shadow.insert(key, entry),
                    None => shadow.remove(&key),
                };
            }
        }
        fn encoded<E: DiskEntry>(entries: impl Iterator<Item = E>) -> Vec<Vec<u8>> {
            entries
                .map(|entry| {
                    let mut bytes = Vec::new();
                    entry.write(&mut bytes).unwrap();
                    bytes
                })
                .collect()
        }
        let (shadow_bbt, shadow_nbt) = self
            .shadow
            .as_mut()
            .expect("the test shadow is set at open");
        apply_shadow(shadow_bbt, bbt_changes);
        apply_shadow(shadow_nbt, nbt_changes);
        let state = self.state.as_ref().unwrap();
        state.bbt.assert_well_formed();
        state.nbt.assert_well_formed();
        let fresh = CommittedState::read_any(&mut self.file, None)?;
        assert_eq!(
            encoded(fresh.bbt.entries()?.into_iter()),
            encoded(shadow_bbt.values().copied()),
            "BBT entries = previous entries + this commit's changes"
        );
        assert_eq!(
            encoded(fresh.nbt.entries()?.into_iter()),
            encoded(shadow_nbt.values().copied()),
            "NBT entries = previous entries + this commit's changes"
        );
        // The session's allocation state: its EOF and free total are the file's, its cached AMap pages are as on disk,
        // and every run it may allocate from is free on disk and below EOF.
        let space = FileSpace::read(&mut self.file, state.header.root())?;
        assert_eq!(state.amap.eof, space.eof, "session EOF");
        assert_eq!(
            state.amap.free_total,
            state.header.root().amap_free_size().index(),
            "session free total"
        );
        // MS-PST 2.6.1.1.2, "The PST file MUST grow at integer multiples of the number of bytes mapped by an AMap", so
        // the file ends where an AMap range ends, and cbAMapFree is free space inside the file.
        assert_eq!(
            (space.eof - AMAP_FIRST_OFFSET) % AMAP_DATA_SIZE,
            0,
            "EOF {:#x} at the end of an AMap range",
            space.eof
        );
        assert_eq!(self.file.metadata()?.len(), space.eof, "file length = EOF");
        assert_eq!(
            space
                .free
                .iter()
                .map(|(start, end)| end - start)
                .sum::<u64>(),
            state.amap.free_total,
            "cbAMapFree = free space below EOF"
        );
        assert!(
            state.amap.free_runs.len() <= FREE_RUN_CAP,
            "session free runs bounded"
        );
        for (index, bits) in &state.amap.cache {
            assert_eq!(
                *bits,
                read_amap_bits(&mut self.file, *index)?,
                "cached AMap page {index}"
            );
        }
        for (start, end) in &state.amap.free_runs {
            assert!(*end <= space.eof, "free run {start:#x}..{end:#x} below EOF");
            assert!(
                space
                    .free
                    .iter()
                    .any(|(free_start, free_end)| free_start <= start && end <= free_end),
                "session free run {start:#x}..{end:#x} is free on disk"
            );
        }
        assert_eq!(
            fresh.max_node_index, state.max_node_index,
            "session max node indexes"
        );
        assert_eq!(
            fresh.max_block_index, state.max_block_index,
            "session max block index"
        );
        let mut header_bytes = Vec::new();
        state.header.write(&mut header_bytes)?;
        let mut disk_header = vec![0; header_bytes.len()];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut disk_header)?;
        assert_eq!(disk_header, header_bytes, "session header");

        // Full rebuild of the allocation map on a copy, from the B-trees on disk.
        let copy = self
            .path
            .with_extension(format!("amap-check-{}.pst", std::process::id()));
        std::fs::copy(&self.path, &copy)?;
        {
            let mut handle = OpenOptions::new().read(true).write(true).open(&copy)?;
            let mut header = UnicodeHeader::read(&mut handle)?;
            header.root_mut().set_amap_status(AmapStatus::Invalid);
            handle.seek(SeekFrom::Start(0))?;
            header.write(&mut handle)?;
            drop(handle);
            let mut pst = UnicodePstFile::open(&copy)?;
            pst.inner.rebuild_allocation_map()?;
        }
        let rebuilt = std::fs::read(&copy)?;
        let _ = std::fs::remove_file(&copy);
        let original = std::fs::read(&self.path)?;
        let count = amap_page_count(state.header.root().file_eof_index().index());
        for index in 0..count {
            let offset = amap_page_base(index as usize) as usize;
            assert_eq!(
                &original[offset..offset + PAGE_SIZE],
                &rebuilt[offset..offset + PAGE_SIZE],
                "AMap page {index} equals a full rebuild"
            );
        }
        for index in 0..count.div_ceil(PMAP_PAGE_COUNT) {
            let offset = (index * PMAP_DATA_SIZE + PMAP_FIRST_OFFSET) as usize;
            assert_eq!(
                &original[offset..offset + PAGE_SIZE],
                &rebuilt[offset..offset + PAGE_SIZE],
                "PMap page {index}"
            );
        }
        for index in 0..(count.max(FMAP_FIRST_SIZE) - FMAP_FIRST_SIZE).div_ceil(FMAP_PAGE_COUNT) {
            let offset = (index * FMAP_DATA_SIZE + FMAP_FIRST_OFFSET) as usize;
            assert_eq!(
                &original[offset..offset + PAGE_SIZE],
                &rebuilt[offset..offset + PAGE_SIZE],
                "FMap page {index}"
            );
        }
        let rebuilt_header = UnicodeHeader::read(&mut Cursor::new(&rebuilt))?;
        assert_eq!(
            rebuilt_header.root().amap_free_size().index(),
            state.header.root().amap_free_size().index(),
            "free size equals a full rebuild"
        );
        Ok(())
    }
}

#[cfg(test)]
impl WriterCore {
    /// Test-only proof that the cached append state of a folder's contents table equals a fresh load from disk.
    pub(crate) fn assert_contents_matches_disk(&mut self, folder: NodeId) -> io::Result<()> {
        let Some(cached) = self.contents.get(&u32::from(folder)) else {
            return Ok(());
        };
        let store = self.store()?;
        let state = self.state.as_ref().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(folder)?)?
            .contents_table()
            .cloned()
            .unwrap();
        let contents = state
            .nbt
            .get(node_key(NodeId::new(
                NodeIdType::ContentsTable,
                folder.index(),
            )?))?
            .unwrap();
        let subnodes = collect_subnode_entries(
            &mut self.file,
            *state.header.root().block_btree(),
            contents.sub_node(),
        )?;
        let rows_node = table_rows_node(table.context(), &subnodes)?;
        let rows = subnodes
            .iter()
            .find(|entry| entry.node() == rows_node)
            .map(|entry| entry.block());
        let fresh = load_contents_state(
            table.as_ref(),
            contents.data(),
            rows,
            unread_rows(table.as_ref())?,
        )?
        .expect("a cached table state must be loadable from disk");
        assert_eq!(fresh.row_count, cached.row_count, "cached row count");
        assert_eq!(fresh.last_id, cached.last_id, "cached last row id");
        assert_eq!(
            fresh.unread, cached.unread,
            "cached unread count equals a recount"
        );
        assert_eq!(
            u64::from(fresh.heap_root),
            u64::from(cached.heap_root),
            "cached heap root"
        );
        assert_eq!(
            u64::from(fresh.rows_root),
            u64::from(cached.rows_root),
            "cached rows root"
        );
        // The next append reads its pages from disk; the committed table must have the shape its view takes, and the
        // view must see the table the reader sees.
        let tx = Tx::new(state);
        let view = open_contents_append(&mut self.file, &tx, cached)?
            .expect("the append view opens on the committed table");
        assert_eq!(
            format!("{:?}", view.context),
            format!("{:?}", table.context()),
            "append view context"
        );
        assert_eq!(
            view.heap.count,
            table.writer_heap_pages()?.len(),
            "append view heap page count"
        );
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    /// Tests only: how often this thread's commits fell back to the full reference recount.
    pub(crate) static REFCOUNT_FALLBACKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Tests only: how often a hierarchy-table row patch or append that the path edits should take fell back to reading
    /// and rebuilding the table.
    pub(crate) static HIERARCHY_FALLBACKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Tests only: skip the hierarchy-table path edits, so the full-rebuild fallbacks run.
    pub(crate) static FORCE_TABLE_REBUILD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A hierarchy-table row patch or append fell back to the full read and rebuild.
pub(crate) fn count_hierarchy_fallback() {
    count_op!(HIERARCHY_FALLBACKS, 1);
    #[cfg(test)]
    HIERARCHY_FALLBACKS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
impl WriterCore {
    /// Test-only proof that the cached append state of `parent`'s hierarchy table equals a fresh load from disk.
    pub(crate) fn assert_hierarchy_matches_disk(&mut self, parent: NodeId) -> io::Result<()> {
        let Some(cached) = self.hierarchy.get(&u32::from(parent)) else {
            return Ok(());
        };
        let store = self.store()?;
        let state = self.state.as_ref().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(parent)?)?
            .hierarchy_table()
            .cloned()
            .unwrap();
        let entry = state
            .nbt
            .get(node_key(NodeId::new(
                NodeIdType::HierarchyTable,
                parent.index(),
            )?))?
            .unwrap();
        let subnodes = collect_subnode_entries(
            &mut self.file,
            *state.header.root().block_btree(),
            entry.sub_node(),
        )?;
        let rows_node = table_rows_node(table.context(), &subnodes)?;
        let rows = subnodes
            .iter()
            .find(|entry| entry.node() == rows_node)
            .map(|entry| entry.block());
        let fresh = load_contents_state(table.as_ref(), entry.data(), rows, 0)?
            .expect("a cached hierarchy state must be loadable from disk");
        assert_eq!(
            fresh.row_count, cached.row_count,
            "cached hierarchy row count"
        );
        assert_eq!(
            fresh.last_id, cached.last_id,
            "cached hierarchy last row id"
        );
        assert_eq!(
            u64::from(fresh.heap_root),
            u64::from(cached.heap_root),
            "cached hierarchy heap root"
        );
        assert_eq!(
            u64::from(fresh.rows_root),
            u64::from(cached.rows_root),
            "cached hierarchy rows root"
        );
        Ok(())
    }
}

/// Block reference counts from what this commit changed (the delta form of `update_block_ref_counts`): node entries
/// replaced or added, new internal blocks, and internal blocks that lose their last reference. When the stored counts
/// do not add up, falls back to the full `compact_unicode_block_tree` over all entries (O(blocks), rare).
fn update_block_ref_counts_delta(
    file: &mut File,
    state: &CommittedState,
    changes: &mut Changes,
) -> io::Result<()> {
    let view = |changes: &Changes, key: u64| -> io::Result<Option<UnicodeBlockBTreeEntry>> {
        changes.block(state, key)
    };
    let mut delta = HashMap::<u64, i32>::new();
    let add = |delta: &mut HashMap<u64, i32>, block: Option<UnicodeBlockId>, change| {
        if let Some(key) = block
            .map(|block| block.search_key())
            .filter(|key| *key != 0)
        {
            *delta.entry(key).or_default() += change;
        }
    };
    for (key, new) in &changes.nbt {
        let old = state.nbt.get(*key)?;
        if let (Some(a), Some(b)) = (old, new) {
            if same_btree_entry(&a, b) {
                continue;
            }
        }
        for (node, change) in [(old, -1), (*new, 1)] {
            if let Some(node) = node {
                add(&mut delta, Some(node.data()), change);
                add(&mut delta, node.sub_node(), change);
            }
        }
    }
    // New internal blocks: those in the change set that the committed tree does not have, and the appended ones.
    let mut new_internal: Vec<UnicodeBlockBTreeEntry> = changes.appended_internal.clone();
    for (key, entry) in &changes.bbt {
        if let Some(entry) = entry.filter(|entry| entry.block().block().is_internal()) {
            // Above the committed tree's largest key a block is new; below it, new only when the tree lacks it.
            if changes.floor.is_some_and(|floor| *key > floor) || state.bbt.get(*key)?.is_none() {
                new_internal.push(entry);
            }
        }
    }
    // How many new internal blocks this filter keeps from being walked (measured).
    count_op!(
        PRECOUNTED_FILTERED,
        new_internal
            .iter()
            .filter(|entry| changes.precounted.contains(&entry.key()))
            .count()
    );
    for entry in new_internal
        .iter()
        .filter(|entry| !changes.precounted.contains(&entry.key()))
    {
        for child in internal_block_children(file, entry)? {
            add(&mut delta, Some(child), 1);
        }
    }
    for (block, change) in &changes.explicit {
        add(&mut delta, Some(*block), *change);
    }

    let mut pending = delta.keys().copied().collect::<Vec<_>>();
    let mut released = HashSet::new();
    let mut consistent = true;
    while let Some(key) = pending.pop() {
        let Some(entry) = view(changes, key)? else {
            consistent = false;
            break;
        };
        let count = i32::from(entry.ref_count()) + delta.get(&key).copied().unwrap_or(0);
        if count < 1 {
            consistent = false;
            break;
        }
        if count == 1
            && entry.block().block().is_internal()
            && released.insert(key)
            && !changes.released_handled.contains(&key)
        {
            for child in internal_block_children(file, &entry)? {
                add(&mut delta, Some(child), -1);
                pending.push(child.search_key());
            }
        }
    }
    // A release the creator accounted for that did not happen would leave its children's counts wrong.
    if consistent
        && !changes
            .released_handled
            .iter()
            .all(|key| released.contains(key))
    {
        consistent = false;
    }
    count_op!(REFCOUNT_DELTA_ENTRIES, delta.len());
    let mut updated = Vec::new();
    if consistent {
        for (key, change) in &delta {
            let entry = view(changes, *key)?.ok_or(PstError::IntegerConversion)?;
            // A block whose references did not change in total (for example the search queue's full leaves, moved from
            // the old to the new XBLOCK) is not touched, so its B-tree leaf is not rewritten.
            if *change == 0 && entry.ref_count() > 1 {
                continue;
            }
            let Ok(count) = u16::try_from(i32::from(entry.ref_count()) + change) else {
                consistent = false;
                break;
            };
            let mut entry = entry;
            entry.set_ref_count(count);
            updated.push((*key, entry));
        }
    }
    if !consistent {
        // Counted, so tests can require that a commit's stated reference changes add up (a path edit that misstates one
        // would otherwise be corrected silently by this O(blocks) recount).
        count_op!(REFCOUNT_FALLBACKS, 1);
        #[cfg(test)]
        REFCOUNT_FALLBACKS.with(|count| count.set(count.get() + 1));
        // Fallback: recount every block from the nodes.
        let mut entries: Vec<UnicodeBlockBTreeEntry> = changes.all_block_entries(state)?;
        let nodes: Vec<UnicodeNodeBTreeEntry> = merged_entries(&state.nbt, &changes.nbt)?;
        compact_unicode_block_tree(file, &mut entries, &nodes)?;
        let after: HashMap<u64, UnicodeBlockBTreeEntry> =
            entries.iter().map(|entry| (entry.key(), *entry)).collect();
        for entry in changes.all_block_entries(state)? {
            match after.get(&entry.key()) {
                Some(kept) if !same_btree_entry(kept, &entry) => {
                    changes.bbt.insert(entry.key(), Some(*kept));
                }
                Some(_) => {}
                None => {
                    changes.bbt.insert(entry.key(), None);
                }
            }
        }
        return Ok(());
    }
    #[cfg(test)]
    let before: Vec<UnicodeBlockBTreeEntry> = changes.all_block_entries(state)?;
    for (key, entry) in updated {
        // Same retain rule as update_block_ref_counts: a touched entry whose count dropped to one is removed.
        if entry.ref_count() > 1 || !delta.contains_key(&key) {
            changes.bbt.insert(key, Some(entry));
        } else {
            changes.bbt.insert(key, None);
        }
    }
    #[cfg(test)]
    {
        let mut full = before;
        let nodes: Vec<UnicodeNodeBTreeEntry> = merged_entries(&state.nbt, &changes.nbt)?;
        compact_unicode_block_tree(file, &mut full, &nodes)?;
        let counts = |entries: &[UnicodeBlockBTreeEntry]| {
            entries
                .iter()
                .map(|entry| (entry.key(), entry.ref_count()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            counts(&changes.all_block_entries(state)?),
            counts(&full),
            "incremental block ref counts"
        );
    }
    Ok(())
}

/// All entries of a committed tree with a commit's changes applied, in key order (O(entries): fallback and tests).
fn merged_entries<E: DiskEntry>(
    tree: &DiskTree<E>,
    changes: &BTreeMap<u64, Option<E>>,
) -> io::Result<Vec<E>> {
    let mut merged: BTreeMap<u64, E> = tree
        .entries()?
        .into_iter()
        .map(|entry| (entry.key(), entry))
        .collect();
    for (key, change) in changes {
        match change {
            Some(entry) => {
                merged.insert(*key, *entry);
            }
            None => {
                merged.remove(key);
            }
        }
    }
    Ok(merged.into_values().collect())
}

/// A writer session over one unencrypted or permute-encoded Unicode PST. It reads the file's committed state once and
/// then appends without re-reading it; see the module documentation for commit, error and close semantics.
pub struct UnicodePstWriter {
    pub(crate) core: WriterCore,
}

impl UnicodePstWriter {
    /// Create a new Unicode PST at `path` (the bundled template with its Inbox, as `UnicodePstFile::create` makes it)
    /// and keep the session for appending. A conversion that creates its file this way reads the file's state once;
    /// opening it again after the first item would read every B-tree and allocation-map page of what that item wrote
    /// (41,585 B-tree and 7,300 map pages after a 1.5 GiB attachment). The file must not exist; a failure removes it.
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            core: create_unicode_session(path.as_ref())?,
        })
    }

    /// Open an existing unencrypted or permute-encoded Unicode PST for appending.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            core: WriterCore::open(path.as_ref())?,
        })
    }

    /// Append messages and their attachments to `folder_path` (relative to the IPM subtree) in one commit, creating
    /// missing folders (each in its own commit, before the messages).
    pub fn append_many_in_folder_with_attachments(
        &mut self,
        folder_path: &[&str],
        messages: &[UnicodePstBatchMessage<'_>],
    ) -> io::Result<()> {
        append_unicode_pst(&mut self.core, messages, Some(folder_path))
    }

    /// Make sure `folder_path` (relative to the IPM subtree, names compared exactly) exists: every missing folder on it
    /// is created, each in its own commit, as `append_many_in_folder_with_attachments` creates them; an existing path
    /// writes nothing. A folder that holds no message is created this way.
    pub fn ensure_folder_path(&mut self, folder_path: &[&str]) -> io::Result<()> {
        ensure_unicode_folder_path(&mut self.core, folder_path).map(|_| ())
    }

    /// Append messages and their attachments to the store's default receive folder in one commit.
    pub fn append_many_with_attachments(
        &mut self,
        messages: &[UnicodePstBatchMessage<'_>],
    ) -> io::Result<()> {
        append_unicode_pst(&mut self.core, messages, None)
    }

    /// Set the store display name in one commit.
    pub fn set_store_display_name(&mut self, name: &str) -> io::Result<()> {
        set_unicode_store_display_name(&mut self.core, name)
    }

    /// The largest file, in bytes, that a commit of this session may leave; `None` (the default) is no limit. A commit
    /// that would end the file past it, a folder's creation included, fails with an `io::ErrorKind::FileTooLarge` error
    /// carrying [`crate::UnicodePstFileLimitReached`] before its header is written; the file is cut back to its last
    /// commit, complete, and the session is poisoned.
    pub fn set_max_file_size(&mut self, limit: Option<u64>) {
        self.core.max_file_size = limit;
    }

    /// `Some(reason)` once a failed commit made the session unusable.
    pub fn poisoned(&self) -> Option<&str> {
        self.core.poisoned.as_deref()
    }

    /// Close the session. Every successful append is already committed; nothing is written here.
    pub fn close(self) -> io::Result<()> {
        drop(self);
        Ok(())
    }
}

/// The logical content of every node of a Unicode PST: (NID, parent NID, data stream, subnode streams by NID path).
/// Diagnostics for equivalence checks between writer versions whose block layout differs; not part of the writer API.
#[doc(hidden)]
#[allow(clippy::type_complexity)]
pub fn debug_node_streams(
    path: impl AsRef<Path>,
) -> io::Result<Vec<(u32, u32, Vec<u8>, Vec<(Vec<u32>, Vec<u8>)>)>> {
    let mut file = File::open(path.as_ref())?;
    let state = CommittedState::read_any(&mut file, None)?;
    let tx = Tx::new(&state);
    let bbt_root = *state.header.root().block_btree();
    let stream = |file: &mut File, block: UnicodeBlockId| -> io::Result<Vec<u8>> {
        if block.search_key() == 0 {
            return Ok(Vec::new());
        }
        let mut data = Vec::new();
        for (leaf, _) in data_tree_leaves(file, &tx, block)? {
            data.extend(read_data_block(file, &tx, leaf)?);
        }
        Ok(data)
    };
    let mut nodes = Vec::new();
    for entry in state.nbt.entries()?.iter() {
        let data = stream(&mut file, entry.data())?;
        let mut subnodes = Vec::new();
        let mut pending = vec![(Vec::new(), entry.sub_node())];
        while let Some((prefix, sub_node)) = pending.pop() {
            for sub in collect_subnode_entries(&mut file, bbt_root, sub_node)? {
                let mut path = prefix.clone();
                path.push(u32::from(sub.node()));
                subnodes.push((path.clone(), stream(&mut file, sub.block())?));
                if sub.sub_node().is_some() {
                    pending.push((path, sub.sub_node()));
                }
            }
        }
        subnodes.sort();
        nodes.push((
            u32::from(entry.node()),
            entry.parent().map(u32::from).unwrap_or(0),
            data,
            subnodes,
        ));
    }
    Ok(nodes)
}

/// Heap fill levels: for every node data stream and every subnode stream that is a heap-on-node (its first page starts
/// with an HNHDR, signature 0xEC), the fill-level bytes it stores: page 0's `rgbFillLevel` (4 bytes), then the
/// `rgbFillLevel` of every HNBITMAPHDR page (pages 8, 136, ...; 64 bytes each). Returns `(node, subnode path, pages,
/// fill-level bytes)`, sorted. Leaves are decoded with the file's own encoding. Measurement and test use only.
#[allow(clippy::type_complexity)]
pub fn debug_heap_fill_levels(
    path: impl AsRef<Path>,
) -> io::Result<Vec<(u32, Vec<u32>, usize, Vec<u8>)>> {
    let mut file = File::open(path.as_ref())?;
    let state = CommittedState::read_any(&mut file, None)?;
    let tx = Tx::new(&state);
    let crypt = state.header.crypt_method();
    let bbt_root = *state.header.root().block_btree();
    let pages = |file: &mut File, block: UnicodeBlockId| -> io::Result<Vec<Vec<u8>>> {
        let mut pages = Vec::new();
        if block.search_key() == 0 {
            return Ok(pages);
        }
        for (leaf, _) in data_tree_leaves(file, &tx, block)? {
            let entry = bbt_entry(&tx, leaf)?;
            match UnicodeDataTree::read(file, crypt, &entry)? {
                DataTree::Leaf(leaf) => pages.push(leaf.data().to_vec()),
                DataTree::Intermediate(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "expected a leaf data block",
                    ))
                }
            }
        }
        Ok(pages)
    };
    let fill = |pages: &[Vec<u8>]| -> Option<Vec<u8>> {
        let first = pages.first()?;
        if first.len() < 12 || first[2] != 0xEC {
            return None;
        }
        let mut levels = first[8..12].to_vec();
        for (index, page) in pages.iter().enumerate() {
            if index % 128 == 8 {
                levels.extend_from_slice(page.get(2..66)?);
            }
        }
        Some(levels)
    };
    let mut heaps = Vec::new();
    for entry in state.nbt.entries()?.iter() {
        let node = u32::from(entry.node());
        let data = pages(&mut file, entry.data())?;
        if let Some(levels) = fill(&data) {
            heaps.push((node, Vec::new(), data.len(), levels));
        }
        let mut pending = vec![(Vec::new(), entry.sub_node())];
        while let Some((prefix, sub_node)) = pending.pop() {
            for sub in collect_subnode_entries(&mut file, bbt_root, sub_node)? {
                let mut path = prefix.clone();
                path.push(u32::from(sub.node()));
                let sub_pages = pages(&mut file, sub.block())?;
                if let Some(levels) = fill(&sub_pages) {
                    heaps.push((node, path.clone(), sub_pages.len(), levels));
                }
                if sub.sub_node().is_some() {
                    pending.push((path, sub.sub_node()));
                }
            }
        }
    }
    heaps.sort();
    Ok(heaps)
}

/// Reference check: every block's stored reference count against the references the file holds, by the rule
/// `compact_unicode_block_tree` applies (count = 1 + references; references are the NBT data and subnode roots and the
/// children of data-tree and subnode-tree blocks, an internal block's children counted once). Returns the number of
/// blocks, the mismatches `(key, stored, expected)`, the unreferenced block keys and the referenced keys that have no
/// block. Measurement and test use only.
#[doc(hidden)]
#[allow(clippy::type_complexity)]
pub fn debug_block_references(
    path: impl AsRef<Path>,
) -> io::Result<(usize, Vec<(u64, u16, u32)>, Vec<u64>, Vec<u64>)> {
    let mut file = File::open(path.as_ref())?;
    let state = CommittedState::read_any(&mut file, None)?;
    let mut references: BTreeMap<u64, u32> = BTreeMap::new();
    let mut pending: Vec<UnicodeBlockId> = Vec::new();
    let note = |block: UnicodeBlockId,
                references: &mut BTreeMap<u64, u32>,
                pending: &mut Vec<UnicodeBlockId>| {
        let key = block.search_key();
        if key == 0 {
            return;
        }
        let count = references.entry(key).or_default();
        if *count == 0 {
            pending.push(block);
        }
        *count += 1;
    };
    for entry in state.nbt.entries()?.iter() {
        note(entry.data(), &mut references, &mut pending);
        if let Some(sub) = entry.sub_node() {
            note(sub, &mut references, &mut pending);
        }
    }
    let mut missing = Vec::new();
    while let Some(block) = pending.pop() {
        let Some(entry) = state.bbt.get(block.search_key())? else {
            missing.push(block.search_key());
            continue;
        };
        if block.is_internal() {
            for child in internal_block_children(&mut file, &entry)? {
                note(child, &mut references, &mut pending);
            }
        }
    }
    let mut mismatches = Vec::new();
    let mut unreferenced = Vec::new();
    let mut blocks = 0;
    for entry in state.bbt.entries()?.iter() {
        blocks += 1;
        match references.get(&entry.key()) {
            Some(count) if u32::from(entry.ref_count()) == count + 1 => {}
            Some(count) => mismatches.push((entry.key(), entry.ref_count(), count + 1)),
            None => unreferenced.push(entry.key()),
        }
    }
    missing.sort_unstable();
    Ok((blocks, mismatches, unreferenced, missing))
}

/// Allocation map check of a Unicode PST (`pst-ref-check`): every AMap page compared, bit by bit, with the bits the
/// B-trees give (their pages, every block, and the map pages themselves), read only. Returns the number of AMap pages,
/// the pages that differ, the 64-byte units the map marks allocated that nothing references (leaked: wasted space, not
/// a corruption) and the units something references that the map marks free (a later allocation could overwrite them: a
/// corruption), with up to 20 file offsets of the latter; then the header's free size and the free bytes the map pages
/// on disk give.
#[allow(clippy::type_complexity)]
pub fn debug_allocation_map(
    path: impl AsRef<Path>,
) -> io::Result<(usize, usize, u64, u64, Vec<u64>, u64, u64)> {
    let mut file = File::open(path.as_ref())?;
    let header = UnicodeHeader::read(&mut file)?;
    let root = header.root();
    let count = amap_page_count(root.file_eof_index().index()) as usize;
    let (mut differing, mut leaked, mut unmarked, mut sample, mut free) =
        (0, 0_u64, 0_u64, Vec::new(), 0_u64);
    let mut window = 0;
    while window < count {
        let end = (window + AMAP_CHECK_WINDOW).min(count);
        let mut expected: Vec<MapBits> = (window..end).map(reserved_bits).collect();
        btree_allocations(
            &mut file,
            *root.node_btree(),
            *root.block_btree(),
            &mut |offset, size| {
                let (first, last) = (amap_index_of(offset), amap_index_of(offset + size - 1));
                for index in first.max(window)..=last.min(end - 1) {
                    mark_range(
                        &mut expected[index - window],
                        index,
                        offset,
                        offset + size,
                        true,
                    );
                }
            },
        )?;
        for index in window..end {
            let bits = read_amap_bits(&mut file, index)?;
            free += free_bytes_of(&bits);
            let want = expected[index - window];
            if bits != want {
                differing += 1;
            }
            for byte in 0..bits.len() {
                leaked += u64::from((bits[byte] & !want[byte]).count_ones());
                let missing = want[byte] & !bits[byte];
                unmarked += u64::from(missing.count_ones());
                for bit in 0..8 {
                    if missing & (0x80 >> bit) != 0 && sample.len() < 20 {
                        sample.push(amap_page_base(index) + (byte as u64 * 8 + bit) * 64);
                    }
                }
            }
        }
        window = end;
    }
    Ok((
        count,
        differing,
        leaked,
        unmarked,
        sample,
        root.amap_free_size().index(),
        free,
    ))
}

impl WriterCore {
    /// Measurement only: the session's retained structures as a JSON object, counts and bytes from sizes and
    /// capacities; `null` without the `mem-profile` feature.
    #[allow(dead_code)]
    fn structure_report(&self) -> String {
        #[cfg(not(feature = "mem-profile"))]
        return String::from("null");
        #[cfg(feature = "mem-profile")]
        {
            use std::mem::size_of;
            fn tree<E>(tree: &DiskTree<E>) -> String {
                // The bounded page cache is all a tree keeps: decoded pages and their entries.
                let cache = tree.cache.borrow();
                let entries: usize = cache
                    .0
                    .values()
                    .map(|page| match &**page {
                        TreePage::Leaf(items) => items.len(),
                        TreePage::Inner(_, children) => children.len(),
                    })
                    .sum();
                let bytes: usize = cache.0.values().map(|page| match &**page {
                    TreePage::Leaf(items) => items.capacity() * size_of::<E>(),
                    TreePage::Inner(_, children) => children.capacity() * size_of::<(u64, UnicodePageRef)>(),
                } + size_of::<TreePage<E>>() + 2 * size_of::<usize>() + 16).sum::<usize>() + cache.1.capacity() * size_of::<u64>();
                format!(
                    "{{\"cached_pages\":{},\"cached_entries\":{entries},\"cache_bytes\":{bytes}}}",
                    cache.0.len()
                )
            }
            let Some(state) = self.state.as_ref() else {
                return String::from("null");
            };
            let amap = &state.amap;
            let runs_bytes = amap.free_runs.capacity() * 16;
            let cache_bytes = amap.cache.capacity() * size_of::<(usize, MapBits)>();
            format!(
                "{{\"bbt\":{},\"nbt\":{},\"amap\":{{\"free_runs\":{},\"runs_bytes\":{runs_bytes},\"cached_pages\":{},\"cache_bytes\":{cache_bytes}}},\"folder_cache\":{}}}",
                tree(&state.bbt), tree(&state.nbt), amap.free_runs.len(), amap.cache.len(), self.folders.len()
            )
        }
    }

    /// Measurement only: the contents-table append states the session keeps (`measurement::contents_after_commit`).
    #[allow(dead_code)]
    pub(crate) fn contents_report(&self) -> String {
        #[cfg(not(feature = "mem-profile"))]
        return String::from("null");
        #[cfg(feature = "mem-profile")]
        {
            use std::mem::size_of;
            // A state is a few integers per folder appended to; pages are read per commit and dropped after it.
            let rows: usize = self.contents.values().map(|c| c.row_count).sum();
            let state_bytes =
                self.contents.capacity() * (size_of::<u32>() + size_of::<ContentsState>() + 1);
            format!(
                "{{\"folders\":{},\"rows\":{rows},\"state_bytes\":{state_bytes},\"folder_paths\":{},\"folder_path_bytes\":{}}}",
                self.contents.len(),
                self.folders.len(),
                self.folders.iter().map(|(path, _)| path.iter().map(String::capacity).sum::<usize>() + path.capacity() * size_of::<String>() + size_of::<Vec<String>>() + size_of::<NodeId>()).sum::<usize>()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const RECIPIENTS: &[UnicodePstRecipient<'static>] = &[UnicodePstRecipient {
        name: "to",
        email: "to@example.com",
        recipient_type: UnicodePstRecipientType::To,
        properties: &[],
    }];

    fn temp(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "outlook-pst-session-{name}-{}-{stamp}.pst",
            std::process::id()
        ))
    }

    fn message<'a>(subject: &'a str, id: &'a str, is_read: bool) -> UnicodePstMessage<'a> {
        UnicodePstMessage {
            subject,
            recipients: RECIPIENTS,
            body: "body",
            html_body: None,
            message_id: Some(id),
            delivery_time: Some(133_750_080_000_000_000),
            creation_time: 133_750_080_000_000_000,
            last_modification_time: 133_750_080_000_000_000,
            search_key: None,
            is_read,
            importance: UnicodePstImportance::Normal,
            sender: Some(UnicodePstSender {
                name: "sender",
                email: "sender@example.com",
                address_type: "SMTP",
            }),
            client_submit_time: None,
            categories: &[],
            transport_headers: None,
            message_class: "IPM.Note",
            unsent: false,
            named_properties: &[],
            properties: &[],
        }
    }

    fn queue(path: &Path) -> Vec<SearchUpdate> {
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(path).unwrap())).unwrap();
        store.search_update_queue().unwrap().updates().to_vec()
    }

    fn folder(path: &Path, folder_path: &[&str]) -> (i32, i32) {
        let mut core = WriterCore::open(path).unwrap();
        let node = ensure_unicode_folder_path(&mut core, folder_path).unwrap();
        let store = core.store().unwrap();
        let folder = store
            .open_folder(&store.properties().make_entry_id(node).unwrap())
            .unwrap();
        (
            folder.properties().content_count().unwrap(),
            folder.properties().unread_count().unwrap(),
        )
    }

    #[test]
    fn many_session_commits_keep_the_search_queue_and_folder_counters() {
        let path = temp("queue");
        drop(
            UnicodePstFile::create(&path, &message("first", "<first@example.com>", true)).unwrap(),
        );
        let initial = queue(&path).len();
        let subjects: Vec<String> = (0..900).map(|index| format!("message {index}")).collect();
        let ids: Vec<String> = (0..900)
            .map(|index| format!("<m{index}@example.com>"))
            .collect();
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        // 6 commits of 150 messages over two folders: 902 queue records cross two 8176-byte leaves (408 records each).
        for batch in 0..6 {
            let folder_path: &[&str] = if batch % 2 == 0 {
                &["Inbox"]
            } else {
                &["Inbox", "Sub"]
            };
            let batch: Vec<UnicodePstBatchMessage<'_>> = (batch * 150..(batch + 1) * 150)
                .map(|index| UnicodePstBatchMessage {
                    message: message(&subjects[index], &ids[index], index % 3 == 0),
                    attachments: &[],
                })
                .collect();
            writer
                .append_many_in_folder_with_attachments(folder_path, &batch)
                .unwrap();
        }
        writer.close().unwrap();

        let updates = queue(&path);
        assert_eq!(
            updates.len(),
            initial + 900 + 2,
            "one record per message plus FolderAdded and FolderModified"
        );
        let added = updates[initial..]
            .iter()
            .filter(|update| matches!(update.data(), Some(SearchUpdateData::MessageAdded { .. })))
            .count();
        assert_eq!(added, 900);
        // The queue is a data tree whose leaves are full but the last.
        let mut core = WriterCore::open(&path).unwrap();
        let state = core.state.take().unwrap();
        let entry = state
            .nbt
            .get(node_key(NID_SEARCH_MANAGEMENT_QUEUE))
            .unwrap()
            .unwrap();
        let leaves = data_tree_leaves(&mut core.file, &Tx::new(&state), entry.data()).unwrap();
        assert!(leaves.len() > 2);
        assert!(leaves[..leaves.len() - 1]
            .iter()
            .all(|(_, size)| *size as usize == MAX_DATA_BLOCK_SIZE));

        let unread =
            |range: std::ops::Range<usize>| range.filter(|index| index % 3 != 0).count() as i32;
        let inbox: usize = [0, 2, 4]
            .iter()
            .map(|batch| batch * 150)
            .map(|start| unread(start..start + 150) as usize)
            .sum();
        let sub: usize = [1, 3, 5]
            .iter()
            .map(|batch| batch * 150)
            .map(|start| unread(start..start + 150) as usize)
            .sum();
        assert_eq!(folder(&path, &["Inbox"]), (1 + 450, inbox as i32));
        assert_eq!(folder(&path, &["Inbox", "Sub"]), (450, sub as i32));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn contents_tables_past_one_index_block_are_appended_on_their_changed_paths() {
        // A contents table whose heap and row matrix grow past 1,021 leaves (XXBLOCKs) is appended from a state of a
        // few integers, reading only the pages and the row block it changes. Two folders are written in turn and the
        // session is reopened half way (a fresh full load). After every commit the test build checks the state against
        // a fresh load from disk, the append view against the reader, and the reference counts against a full recount.
        let path = temp("contents-two-levels");
        drop(
            UnicodePstFile::create(&path, &message("first", "<first@example.com>", true)).unwrap(),
        );
        let fallbacks = REFCOUNT_FALLBACKS.with(std::cell::Cell::get);
        let total = 17 * 4096 + 3 * 300;
        let subjects: Vec<String> = (0..total)
            .map(|index| format!("contents message {index} {}", "x".repeat(index % 40)))
            .collect();
        let ids: Vec<String> = (0..total)
            .map(|index| format!("<c{index}@example.com>"))
            .collect();
        let mut next = 0;
        let mut expected = [(1_i32, 0_i32), (0, 0)];
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        for commit in 0..20 {
            let (folder_path, count, slot): (&[&str], usize, usize) = if commit % 6 == 5 {
                (&["Inbox"], 300, 0)
            } else {
                (&["Inbox", "Big"], 4096, 1)
            };
            let batch: Vec<UnicodePstBatchMessage<'_>> = (next..next + count)
                .map(|index| UnicodePstBatchMessage {
                    message: message(&subjects[index], &ids[index], index % 3 == 0),
                    attachments: &[],
                })
                .collect();
            writer
                .append_many_in_folder_with_attachments(folder_path, &batch)
                .unwrap();
            expected[slot].0 += count as i32;
            expected[slot].1 += (next..next + count).filter(|index| index % 3 != 0).count() as i32;
            next += count;
            if commit == 8 {
                writer.close().unwrap();
                writer = UnicodePstWriter::open(&path).unwrap();
            }
        }
        writer.close().unwrap();
        assert_eq!(folder(&path, &["Inbox"]), expected[0]);
        assert_eq!(folder(&path, &["Inbox", "Big"]), expected[1]);
        // Both data trees of the big table have two levels, and the reader sees every row.
        let mut core = WriterCore::open(&path).unwrap();
        let node = ensure_unicode_folder_path(&mut core, &["Inbox", "Big"]).unwrap();
        let store = core.store().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(node).unwrap())
            .unwrap()
            .contents_table()
            .cloned()
            .unwrap();
        assert_eq!(table.rows_matrix().count(), expected[1].0 as usize);
        let state = core.state.take().unwrap();
        let contents = state
            .nbt
            .get(node_key(
                NodeId::new(NodeIdType::ContentsTable, node.index()).unwrap(),
            ))
            .unwrap()
            .unwrap();
        let subnodes = collect_subnode_entries(
            &mut core.file,
            *state.header.root().block_btree(),
            contents.sub_node(),
        )
        .unwrap();
        let rows_node = table_rows_node(table.context(), &subnodes).unwrap();
        let rows = subnodes
            .iter()
            .find(|entry| entry.node() == rows_node)
            .unwrap()
            .block();
        let tx = Tx::new(&state);
        let heap_tree = DataTreePath::open(&mut core.file, &tx, contents.data())
            .unwrap()
            .unwrap();
        let rows_tree = DataTreePath::open(&mut core.file, &tx, rows)
            .unwrap()
            .unwrap();
        assert_eq!(
            (heap_tree.level, rows_tree.level),
            (2, 2),
            "heap {} leaves, rows {} leaves",
            heap_tree.leaves,
            rows_tree.leaves
        );
        // Past one XXBLOCK of heap pages, every page's fill level (HNHDR and many HNBITMAPHDRs) is right after the
        // appends changed pages along their paths.
        assert!(assert_fill_levels(&mut core.file, &tx, contents.data()) > 1021);
        drop(tx);
        // Every append's stated reference changes added up: none needed the full recount.
        assert_eq!(
            REFCOUNT_FALLBACKS.with(std::cell::Cell::get),
            fallbacks,
            "a commit fell back to the full reference recount"
        );
        std::fs::remove_file(path).unwrap();
    }

    /// Every block of the heap rooted at `root` has the fill level its free space gives, in HNHDR or its HNBITMAPHDR;
    /// slots for blocks the heap does not have are zero. Returns the number of blocks checked.
    fn assert_fill_levels(file: &mut File, bbt: &dyn BlockLookup, root: UnicodeBlockId) -> usize {
        let leaves = data_tree_leaves(file, bbt, root).unwrap();
        let mut stored = BTreeMap::new();
        for (page, (block, _)) in leaves.iter().enumerate() {
            if page == 0 || page % 128 == 8 {
                let data = read_data_block(file, bbt, *block).unwrap();
                stored.extend(read_fill_levels(page, &data).unwrap());
            }
        }
        for (page, level) in &stored {
            // A non-final page is a full block with free bytes after its page map.
            let expected = leaves.get(*page).map_or(0, |(block, _)| {
                fill_level(heap_page_free(&read_data_block(file, bbt, *block).unwrap())) as u8
            });
            assert_eq!(
                *level as u8,
                expected,
                "fill level of heap page {page} of {}",
                leaves.len()
            );
        }
        leaves.len()
    }

    /// Non-empty heap allocations of `table` that nothing in the table references: not the TCINFO, the row index
    /// (header and nodes), an in-heap row matrix or a row's cell. Zero-filled allocations that end a page other than
    /// the last are the writer's page padding and are not counted.
    fn unreferenced_heap_allocations(table: &dyn TableContext) -> usize {
        let pages = table.writer_heap_pages().unwrap();
        let mut used = BTreeSet::new();
        let slot = |hid: HeapId| {
            (
                usize::from(hid.block_index()),
                usize::from(hid.index().unwrap()) + 1,
            )
        };
        let get = |hid: HeapId| -> &[u8] {
            let (page, index) = slot(hid);
            &pages[page][index]
        };
        used.insert(slot(table.writer_heap_user_root().unwrap()));
        let context = table.context();
        if u32::from(context.row_index()) != 0 {
            used.insert(slot(context.row_index()));
            let index = HeapTreeHeader::read(&mut get(context.row_index())).unwrap();
            let mut nodes = if u32::from(index.root()) == 0 {
                vec![]
            } else {
                vec![index.root()]
            };
            for level in (0..=index.levels()).rev() {
                let mut children = Vec::new();
                for node in nodes {
                    used.insert(slot(node));
                    if level > 0 {
                        for record in get(node).chunks(8) {
                            children.push(HeapId::from(u32::from_le_bytes(
                                record[4..].try_into().unwrap(),
                            )));
                        }
                    }
                }
                nodes = children;
            }
        }
        if let Some(rows) = context
            .rows()
            .filter(|rows| matches!(rows.id_type(), Ok(NodeIdType::HeapNode)))
        {
            used.insert(slot(HeapId::from(u32::from(rows))));
        }
        for row in table.rows_matrix() {
            for value in row.columns(context).unwrap().into_iter().flatten() {
                if let TableRowColumnValue::Heap(id) = value {
                    if u32::from(id) != 0 {
                        used.insert(slot(id));
                    }
                }
            }
        }
        let mut unreferenced = 0;
        for (page, allocations) in pages.iter().enumerate() {
            let mut padding = page + 1 < pages.len();
            for (index, allocation) in allocations.iter().enumerate().skip(1).rev() {
                if used.contains(&(page, index)) || allocation.is_empty() {
                    padding = padding && allocation.is_empty() && !used.contains(&(page, index));
                    continue;
                }
                if padding && allocation.iter().all(|byte| *byte == 0) {
                    continue;
                }
                padding = false;
                unreferenced += 1;
            }
        }
        unreferenced
    }

    /// Every hierarchy table of the file's folder tree: (folder path, unreferenced allocations).
    fn hierarchy_garbage(path: &Path) -> Vec<(String, usize)> {
        let store = UnicodeStore::read(Rc::new(UnicodePstFile::open(path).unwrap())).unwrap();
        let mut out = Vec::new();
        let mut stack = vec![(
            String::from("/"),
            store
                .properties()
                .ipm_sub_tree_entry_id()
                .unwrap()
                .node_id(),
        )];
        while let Some((name, node)) = stack.pop() {
            let folder = store
                .open_folder(&store.properties().make_entry_id(node).unwrap())
                .unwrap();
            let Some(table) = folder.hierarchy_table() else {
                continue;
            };
            out.push((name.clone(), unreferenced_heap_allocations(table.as_ref())));
            for row in table.rows_matrix() {
                let child = NodeId::from(u32::from(row.id()));
                let child_name = store
                    .open_folder(&store.properties().make_entry_id(child).unwrap())
                    .unwrap()
                    .properties()
                    .display_name()
                    .unwrap()
                    .to_string();
                stack.push((format!("{name}{child_name}/"), child));
            }
        }
        out
    }

    #[test]
    fn hierarchy_tables_keep_no_unreferenced_heap_allocations() {
        // Folder creations, a first child under a folder with rows in the heap, and counter updates leave nothing
        // behind in any hierarchy heap, along the path edits and along the full-rebuild fallbacks (the test switch
        // turns the path edits off): a rebuild must not keep the old TCINFO, row index, in-heap row matrix or the
        // updated row's old values.
        for force in [false, true] {
            let path = temp(if force {
                "garbage-rebuild"
            } else {
                "garbage-paths"
            });
            drop(
                UnicodePstFile::create(&path, &message("first", "<first@example.com>", true))
                    .unwrap(),
            );
            FORCE_TABLE_REBUILD.with(|cell| cell.set(force));
            let ids: Vec<String> = (0..80)
                .map(|index| format!("<g{index}@example.com>"))
                .collect();
            let targets: Vec<Vec<String>> = (0..80)
                .map(|index| match index % 4 {
                    0 => vec!["Inbox".into(), format!("F{}", index % 24)],
                    1 => vec!["Deleted Items".into()],
                    2 => vec![
                        "Deleted Items".into(),
                        "Sub".into(),
                        format!("D{}", index % 3),
                    ],
                    _ => vec!["Inbox".into(), format!("F{}", index % 24), "Inner".into()],
                })
                .collect();
            let mut writer = UnicodePstWriter::open(&path).unwrap();
            for (index, target) in targets.iter().enumerate() {
                let target: Vec<&str> = target.iter().map(String::as_str).collect();
                let batch = [UnicodePstBatchMessage {
                    message: message("garbage", &ids[index], index % 2 == 0),
                    attachments: &[],
                }];
                writer
                    .append_many_in_folder_with_attachments(&target, &batch)
                    .unwrap();
            }
            writer.close().unwrap();
            FORCE_TABLE_REBUILD.with(|cell| cell.set(false));
            let garbage = hierarchy_garbage(&path);
            assert!(garbage.len() >= 20, "{garbage:?}");
            assert!(
                garbage.iter().all(|(_, count)| *count == 0),
                "force {force}: {garbage:?}"
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn an_empty_string_is_stored_as_hnid_zero_and_strings_carry_no_terminator() {
        // Strings are stored as Outlook stores them, without a terminating NUL, and a present empty string is HNID 0
        // (in a property context and, with its CEB bit set, in a table cell), never a zero-size allocation (which an
        // independent reader takes for a freed item, dropping the whole folder).
        let path = temp("empty-string");
        let mut first = message("", "<empty@example.com>", true);
        first.sender = first
            .sender
            .map(|sender| UnicodePstSender { name: "", ..sender });
        drop(UnicodePstFile::create(&path, &first).unwrap());
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        let batch = [UnicodePstBatchMessage {
            message: message("named", "<named@example.com>", true),
            attachments: &[],
        }];
        writer
            .append_many_in_folder_with_attachments(&["Inbox"], &batch)
            .unwrap();
        writer.close().unwrap();
        let mut core = WriterCore::open(&path).unwrap();
        let inbox = ensure_unicode_folder_path(&mut core, &["Inbox"]).unwrap();
        let store = core.store().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(inbox).unwrap())
            .unwrap()
            .contents_table()
            .cloned()
            .unwrap();
        let context = table.context();
        let subject = context
            .columns()
            .iter()
            .position(|column| column.prop_id() == 0x0037)
            .unwrap();
        let cells: Vec<_> = table
            .rows_matrix()
            .map(|row| row.columns(context).unwrap()[subject].clone())
            .collect();
        assert_eq!(cells.len(), 2);
        match &cells[0] {
            Some(TableRowColumnValue::Heap(id)) => {
                assert_eq!(u32::from(*id), 0, "an empty subject cell is HNID 0")
            }
            other => panic!("empty subject cell {other:?}"),
        }
        match &cells[1] {
            Some(value @ TableRowColumnValue::Heap(id)) if u32::from(*id) != 0 => {
                let PropertyValue::Unicode(text) =
                    table.read_column(value, PropertyType::Unicode).unwrap()
                else {
                    panic!("subject type")
                };
                assert_eq!(
                    text.buffer(),
                    "named".encode_utf16().collect::<Vec<_>>().as_slice(),
                    "no terminator"
                );
            }
            other => panic!("subject cell {other:?}"),
        }
        // No referenced zero-size allocation anywhere in the contents heap.
        let pages = table.writer_heap_pages().unwrap();
        for row in table.rows_matrix() {
            for value in row.columns(context).unwrap().into_iter().flatten() {
                if let TableRowColumnValue::Heap(id) = value {
                    if u32::from(id) != 0 {
                        assert!(!pages[usize::from(id.block_index())]
                            [usize::from(id.index().unwrap()) + 1]
                            .is_empty());
                    }
                }
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn an_empty_value_stored_as_hnid_zero_is_read_back_as_an_empty_value_of_its_type() {
        // Outlook stores a present empty string as HNID 0 (dist-list.pst and passworded.pst: 134 of them), and so does
        // this writer. libpff and XstReader read it as an empty value of the declared type; the crate's reader must not
        // read it as PtypNull in a property context or fail on it in a table cell (InvalidHeapIndex(0)).
        use crate::messaging::message::{Message as _, UnicodeMessage};
        let path = temp("empty-read");
        drop(
            UnicodePstFile::create(&path, &message("", "<empty-read@example.com>", true)).unwrap(),
        );
        let mut core = WriterCore::open(&path).unwrap();
        let inbox = ensure_unicode_folder_path(&mut core, &["Inbox"]).unwrap();
        let store = core.store().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(inbox).unwrap())
            .unwrap()
            .contents_table()
            .cloned()
            .unwrap();
        let context = table.context();
        let subject = context
            .columns()
            .iter()
            .position(|column| column.prop_id() == 0x0037)
            .unwrap();
        let row = table.rows_matrix().next().unwrap();
        let cell = row.columns(context).unwrap()[subject].clone().unwrap();
        assert!(
            matches!(cell, TableRowColumnValue::Heap(id) if u32::from(id) == 0),
            "the empty subject cell is HNID 0"
        );
        let PropertyValue::Unicode(text) = table.read_column(&cell, PropertyType::Unicode).unwrap()
        else {
            panic!("cell type")
        };
        assert!(text.buffer().is_empty());
        let entry_id = store
            .properties()
            .make_entry_id(NodeId::from(u32::from(row.id())))
            .unwrap();
        let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
        let value = message
            .properties()
            .iter()
            .find(|(id, _)| **id == 0x0037)
            .map(|(_, value)| value.clone());
        let Some(PropertyValue::Unicode(text)) = value else {
            panic!("subject in the message: {value:?}")
        };
        assert!(text.buffer().is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_message_larger_than_its_32_bit_size_is_refused_by_name_before_anything_is_written() {
        // PidTagAttachSize and PidTagMessageSize are PtypInteger32 (MS-PST 2.4.5.1.1, 2.4.6.1.1). A message whose
        // attachment object exceeds that, or whose attachments each fit but together exceed PidTagMessageSize (found by
        // the check before the commit), is refused with a typed FileTooLarge error naming its position, before its
        // streams are opened and before any block of the commit is written; the file keeps its last commit.
        for (case, lens) in [
            ("attachment", &[i32::MAX as u64][..]),
            ("message", &[1_100_000_000, 1_100_000_000][..]),
        ] {
            let path = temp("too-large");
            drop(
                UnicodePstFile::create(&path, &message("first", "<first@example.com>", true))
                    .unwrap(),
            );
            let before = std::fs::read(&path).unwrap();
            let opened = std::cell::Cell::new(false);
            let open = || -> io::Result<Box<dyn Read>> {
                opened.set(true);
                Ok(Box::new(io::repeat(0)))
            };
            let huge = lens
                .iter()
                .map(|len| UnicodePstAttachment {
                    filename: "huge.bin",
                    mime_type: "application/octet-stream",
                    content_id: None,
                    data: &[],
                    stream: Some(UnicodePstAttachmentStream {
                        len: *len,
                        open: &open,
                    }),
                    hidden: false,
                    attached_message: None,
                })
                .collect::<Vec<_>>();
            let mut writer = UnicodePstWriter::open(&path).unwrap();
            let batch = [
                UnicodePstBatchMessage {
                    message: message("fits", "<fits@example.com>", true),
                    attachments: &[],
                },
                UnicodePstBatchMessage {
                    message: message("huge", "<huge@example.com>", true),
                    attachments: &huge,
                },
            ];
            let error = writer
                .append_many_in_folder_with_attachments(&["Inbox"], &batch)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::FileTooLarge, "{case}");
            let too_large = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<UnicodePstMessageTooLarge>())
                .expect("typed error");
            assert_eq!(
                too_large.position, 2,
                "{case}: the second message of the call"
            );
            assert!(too_large.size > i32::MAX as u64, "{case}");
            assert!(!opened.get(), "{case}: a stream was opened");
            drop(writer);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "{case}: the file changed"
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_streamed_attachment_allocates_nothing_per_block() {
        // Streaming an attachment allocates nothing per data block, per B-tree or map page, or per allocation-map
        // range, and a commit does not grow its large buffers again (the churn of freed allocations leaves the
        // allocator's regions dirty and raises the peak RSS with the attachment and with the commits). After a thread
        // has written one 40 MiB attachment (its kept buffers have their full size), the appends of an 8 MiB and a
        // 128 MiB streamed attachment, each into a session warmed by one earlier commit, differ by fewer than 100
        // allocation calls on this thread, the 128 MiB one makes no more allocations over 15 KiB than the 8 MiB one,
        // and neither makes one over 127 KiB. Copying every block and allocating per page and per range would make them
        // differ by about 15,000 calls (one 8 KiB copy per block) plus five per B-tree page and one per map range. The
        // test build's own record of every appended entry (`appended_log`) is not counted.
        use crate::measurement::heap::{
            calls_on_this_thread, huge_calls_on_this_thread, large_calls_on_this_thread,
        };
        let calls = |len: u64| {
            let path = temp("alloc");
            drop(
                UnicodePstFile::create(&path, &message("first", "<first@example.com>", true))
                    .unwrap(),
            );
            let open = || -> io::Result<Box<dyn Read>> { Ok(Box::new(io::repeat(7).take(len))) };
            let attachment = UnicodePstAttachment {
                filename: "stream.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: &[],
                stream: Some(UnicodePstAttachmentStream { len, open: &open }),
                hidden: false,
                attached_message: None,
            };
            let mut writer = UnicodePstWriter::open(&path).unwrap();
            let batch = [UnicodePstBatchMessage {
                message: message("big", "<big@example.com>", true),
                attachments: std::slice::from_ref(&attachment),
            }];
            writer
                .append_many_in_folder_with_attachments(
                    &["Inbox"],
                    &[UnicodePstBatchMessage {
                        message: message("warm", "<warm@example.com>", true),
                        attachments: &[],
                    }],
                )
                .unwrap();
            let before = (
                calls_on_this_thread(),
                large_calls_on_this_thread(),
                huge_calls_on_this_thread(),
            );
            writer
                .append_many_in_folder_with_attachments(&["Inbox"], &batch)
                .unwrap();
            // Up to the commit: the test build's checks after the commit read whole B-trees and are not counted.
            let at_commit = COMMIT_START.with(std::cell::Cell::get);
            writer.close().unwrap();
            std::fs::remove_file(path).unwrap();
            (
                at_commit.0 - before.0,
                at_commit.1 - before.1,
                at_commit.2 - before.2,
            )
        };
        let _ = calls(40 << 20);
        let (small, large) = (calls(8 << 20), calls(128 << 20));
        println!(
            "allocation calls (all, over 15 KiB, over 127 KiB): 8 MiB {small:?}, 128 MiB {large:?}"
        );
        assert!(
            large.0 < small.0 + 100,
            "128 MiB: {large:?}, 8 MiB: {small:?}"
        );
        assert!(
            large.1 <= small.1,
            "allocations over 15 KiB, 128 MiB: {large:?}, 8 MiB: {small:?}"
        );
        assert!(
            small.2 == 0 && large.2 == 0,
            "allocations over 127 KiB: 8 MiB {small:?}, 128 MiB {large:?}"
        );
    }

    #[test]
    fn the_stack_page_encoder_writes_what_the_page_writer_wrote() {
        // encode_btree_page against the page writer it replaces (write_btree_page, kept for this test), byte for byte:
        // block leaves of 0 to 20 entries (reference counts of 1 and 2), node leaves of 0 to 15 entries (with and
        // without subnode and parent), and intermediate pages of both trees at levels 1 and 3.
        let path = temp("page-encoder");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let mut n = 0_u64;
        let mut check =
            |file: &mut File,
             page_type: PageType,
             data: NewBTreePageData,
             encoded: &dyn Fn(UnicodePageRef) -> [u8; PAGE_SIZE]| {
                n += 1;
                let page_ref = UnicodePageRef::new(
                    UnicodePageId::from(0x2000 + 4 * n),
                    UnicodeByteIndex::new(512 * n),
                );
                write_btree_page(file, NewBTreePage { page_ref, data }).unwrap();
                let mut written = [0_u8; PAGE_SIZE];
                file.seek(SeekFrom::Start(512 * n)).unwrap();
                file.read_exact(&mut written).unwrap();
                assert_eq!(written, encoded(page_ref), "page {n} ({page_type:?})");
            };
        let blocks = (0..20_u64)
            .map(|index| {
                let mut entry = UnicodeBlockBTreeEntry::new(
                    UnicodeBlockRef::new(
                        UnicodeBlockId::new(index % 3 == 0, 0x100 + index).unwrap(),
                        UnicodeByteIndex::new(0x4400 + 8192 * index),
                    ),
                    (100 + 37 * index) as u16,
                );
                if index % 2 == 0 {
                    entry.set_ref_count(2);
                }
                entry
            })
            .collect::<Vec<_>>();
        for count in [0, 1, 7, 19, 20] {
            let items = &blocks[..count];
            check(
                &mut file,
                PageType::BlockBTree,
                NewBTreePageData::BlockLeaf(items.to_vec()),
                &|page_ref| {
                    encode_btree_page(
                        count,
                        20,
                        24,
                        0,
                        PageType::BlockBTree,
                        page_ref,
                        |index, slot| {
                            <UnicodeBlockBTreeEntry as BTreeEntryReadWrite>::write(
                                &items[index],
                                slot,
                            )
                        },
                    )
                    .unwrap()
                },
            );
        }
        let nodes = (0..15_u32)
            .map(|index| {
                let node = NodeId::new(NodeIdType::NormalMessage, 0x40 + index).unwrap();
                let data = UnicodeBlockId::new(false, 0x200 + u64::from(index)).unwrap();
                let sub = (index % 2 == 0)
                    .then(|| UnicodeBlockId::new(true, 0x300 + u64::from(index)).unwrap());
                let parent = (index % 3 != 0)
                    .then(|| NodeId::new(NodeIdType::NormalFolder, 0x400 + index).unwrap());
                UnicodeNodeBTreeEntry::new(node, data, sub, parent)
            })
            .collect::<Vec<_>>();
        for count in [0, 1, 14, 15] {
            let items = &nodes[..count];
            check(
                &mut file,
                PageType::NodeBTree,
                NewBTreePageData::NodeLeaf(items.to_vec()),
                &|page_ref| {
                    encode_btree_page(
                        count,
                        15,
                        32,
                        0,
                        PageType::NodeBTree,
                        page_ref,
                        |index, slot| {
                            <UnicodeNodeBTreeEntry as BTreeEntryReadWrite>::write(
                                &nodes[index],
                                slot,
                            )
                        },
                    )
                    .unwrap()
                },
            );
        }
        let children = (0..20_u64)
            .map(|index| {
                (
                    0x1000 + 64 * index,
                    UnicodePageRef::new(
                        UnicodePageId::from(0x8000 + index),
                        UnicodeByteIndex::new(0x10000 + 512 * index),
                    ),
                )
            })
            .collect::<Vec<_>>();
        for (page_type, level, count) in [
            (PageType::BlockBTree, 1, 1),
            (PageType::BlockBTree, 3, 20),
            (PageType::NodeBTree, 1, 20),
            (PageType::NodeBTree, 3, 2),
        ] {
            let items = &children[..count];
            let entries = items
                .iter()
                .map(|(key, child)| UnicodeBTreePageEntry::new(*key, *child))
                .collect();
            check(
                &mut file,
                page_type,
                NewBTreePageData::Intermediate(page_type, level, entries),
                &|page_ref| {
                    encode_btree_page(count, 20, 24, level, page_type, page_ref, |index, slot| {
                        <UnicodeBTreePageEntry as BTreeEntryReadWrite>::write(
                            &UnicodeBTreePageEntry::new(items[index].0, items[index].1),
                            slot,
                        )
                    })
                    .unwrap()
                },
            );
        }
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_message_size_bound_is_never_below_the_exact_size() {
        // A message whose bound stays under PidTagMessageSize's limit is not built before its commit writes, so the
        // refusal of a too-large message before anything is written rests on this bound. Lengths around the heap's
        // allocation limit and page size, one- to four-byte characters, empty values, many recipients and many
        // categories; the tightest margin seen is printed.
        let lengths = [
            0_usize, 1, 2, 3, 7, 100, 1_000, 1_789, 1_790, 1_791, 2_000, 2_725, 2_730, 3_580,
            4_000, 8_176, 20_000, 600_000,
        ];
        let chars = ['a', '\u{e9}', '\u{20ac}', '\u{1f600}'];
        let class_max = format!("IPM.{}", "x".repeat(251));
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let (mut tightest, mut tightest_large) = (f64::INFINITY, f64::INFINITY);
        for case in 0..600 {
            let text = |next: &mut dyn FnMut(usize) -> usize| -> String {
                let len = if case % 5 == 0 {
                    1_790
                } else {
                    lengths[next(lengths.len())]
                };
                std::iter::repeat_n(chars[next(chars.len())], len).collect()
            };
            let [subject, sender_name, sender_email, sender_type, body, id, html, headers] =
                std::array::from_fn(|_| text(&mut next));
            let short = |value: String| value.chars().take(32).collect::<String>();
            let names = (0..[0, 1, 3, 40, 400][next(5)])
                .map(|_| (short(text(&mut next)), short(text(&mut next))))
                .collect::<Vec<_>>();
            let recipients = names
                .iter()
                .enumerate()
                .map(|(index, (name, email))| UnicodePstRecipient {
                    name,
                    email,
                    recipient_type: [
                        UnicodePstRecipientType::To,
                        UnicodePstRecipientType::Cc,
                        UnicodePstRecipientType::Bcc,
                    ][index % 3],
                    properties: &[],
                })
                .collect::<Vec<_>>();
            let categories = (0..[0, 1, 5, 300, 2_000][next(5)])
                .map(|_| ["", "c", "category"][next(3)])
                .collect::<Vec<_>>();
            let search_key = vec![1_u8; lengths[next(lengths.len())]];
            let mut input = message(&subject, &id, case % 2 == 0);
            input.sender = Some(UnicodePstSender {
                name: &sender_name,
                email: &sender_email,
                address_type: &sender_type,
            });
            // An absent sender, Message-ID or delivery time, and a message class up to its 255-character limit.
            if case % 7 == 3 {
                input.sender = None;
            }
            if case % 4 == 0 && case % 8 == 4 {
                input.message_id = None;
            }
            if case % 6 == 5 {
                input.delivery_time = None;
            }
            input.message_class = [class_max.as_str(), "IPM.Note", "REPORT.IPM.Note.NDR"][case % 3];
            input.body = &body;
            input.html_body = (case % 3 != 0).then_some(html.as_str());
            input.search_key = (case % 4 == 0).then_some(search_key.as_slice());
            input.client_submit_time = (case % 2 == 1).then_some(133_750_080_000_000_000);
            input.recipients = &recipients;
            input.categories = &categories;
            input.transport_headers = (case % 5 != 1).then_some(headers.as_str());
            let (pages, external) =
                message_data(&input, &[], &crate::NameIds::with_keywords(0x8001)).unwrap();
            let exact = pages
                .iter()
                .chain(external.iter().map(|(_, data)| data))
                .map(Vec::len)
                .sum::<usize>() as u64;
            let bound = message_data_bound(&input);
            assert!(exact <= bound, "case {case}: exact {exact} > bound {bound}");
            tightest = tightest.min(bound as f64 / exact as f64);
            if exact > 1 << 20 {
                tightest_large = tightest_large.min((bound - (1 << 20)) as f64 / exact as f64);
            }
        }
        println!("tightest bound/exact: {tightest:.3}; without the 1 MiB, over messages above 1 MiB: {tightest_large:.3}");

        // One value far above the 1 MiB margin per term of the bound, so that a bound without any one term fails.
        let huge = "x".repeat(2 << 20);
        let names = (0..30_000)
            .map(|index| format!("{index:064}"))
            .collect::<Vec<_>>();
        let recipients = names
            .iter()
            .enumerate()
            .map(|(index, name)| UnicodePstRecipient {
                name,
                email: "",
                recipient_type: [
                    UnicodePstRecipientType::To,
                    UnicodePstRecipientType::Cc,
                    UnicodePstRecipientType::Bcc,
                ][index % 3],
                properties: &[],
            })
            .collect::<Vec<_>>();
        let categories = vec!["category"; 100_000];
        // Given properties: one far above the margin with about 32,000 small ones, and about 32,000 of 64 bytes (each
        // with its BTH record).
        let mut given = vec![UnicodePstProperty {
            id: 0x1001,
            value: UnicodePstValue::Unicode(&huge),
        }];
        let free = |id: &u16| *id != 0x1001 && !crate::MESSAGE_OWNED_IDS.contains(id);
        given.extend(
            (0x0100_u16..0x7F00)
                .filter(free)
                .map(|id| UnicodePstProperty {
                    id,
                    value: UnicodePstValue::Integer32(i32::from(id)),
                }),
        );
        let many_small = (0x0100_u16..0x7F00)
            .filter(free)
            .map(|id| UnicodePstProperty {
                id,
                value: UnicodePstValue::Binary(&[7; 64]),
            })
            .collect::<Vec<_>>();
        // Named properties, the same way: one far above the margin with about 30,000 small ones, and about 30,000 of 64
        // bytes.
        let set = crate::guid_bytes(
            0x1111_2222,
            0x3333,
            0x4444,
            [0x55, 0x55, 0x66, 0x66, 0x77, 0x77, 0x88, 0x88],
        );
        let named_one = |lid: u32, value| crate::UnicodePstNamedProperty {
            set,
            name: crate::UnicodePstPropertyName::Lid(lid),
            value,
        };
        let mut named = vec![named_one(0, UnicodePstValue::Unicode(&huge))];
        named.extend((1..30_000).map(|lid| named_one(lid, UnicodePstValue::Integer32(lid as i32))));
        let many_small_named = (0..30_000)
            .map(|lid| named_one(lid, UnicodePstValue::Binary(&[7; 64])))
            .collect::<Vec<_>>();
        let terms = [
            "subject",
            "sender name",
            "sender address type",
            "sender email",
            "body",
            "message id",
            "search key",
            "html",
            "transport headers",
            "recipients",
            "categories",
            "properties",
            "many small properties",
            "named properties",
            "many small named properties",
        ];
        for term in terms {
            let mut input = message("s", "<id@example.com>", true);
            input.recipients = &[];
            match term {
                "subject" => input.subject = &huge,
                "sender name" => {
                    input.sender = input.sender.map(|sender| UnicodePstSender {
                        name: &huge,
                        ..sender
                    })
                }
                "sender address type" => {
                    input.sender = input.sender.map(|sender| UnicodePstSender {
                        address_type: &huge,
                        ..sender
                    })
                }
                "sender email" => {
                    input.sender = input.sender.map(|sender| UnicodePstSender {
                        email: &huge,
                        ..sender
                    })
                }
                "body" => input.body = &huge,
                "message id" => input.message_id = Some(&huge),
                "search key" => input.search_key = Some(huge.as_bytes()),
                "html" => input.html_body = Some(&huge),
                "transport headers" => input.transport_headers = Some(&huge),
                "recipients" => input.recipients = &recipients,
                "properties" => input.properties = &given,
                "many small properties" => input.properties = &many_small,
                "named properties" => input.named_properties = &named,
                "many small named properties" => input.named_properties = &many_small_named,
                _ => input.categories = &categories,
            }
            let (pages, external) = message_data(
                &input,
                &[],
                &crate::NameIds::with_names(0x8001, input.named_properties),
            )
            .unwrap();
            let exact = pages
                .iter()
                .chain(external.iter().map(|(_, data)| data))
                .map(Vec::len)
                .sum::<usize>() as u64;
            let bound = message_data_bound(&input);
            // Without this term the bound is 1 MiB plus a few hundred bytes, below 1.5 MiB.
            assert!(exact > 3 << 19, "{term}: {exact} is not above the margin");
            assert!(exact <= bound, "{term}: exact {exact} > bound {bound}");
        }
    }

    #[test]
    fn the_message_size_is_set_in_place_as_if_it_had_been_built_with_it() {
        // PidTagMessageSize is set in the encoded pages once the size is known. Setting it gives the pages a build with
        // the value would give, through one and two BTH levels; a missing property or another type is refused.
        for count in [9_u16, 1_000] {
            let properties = |size: i32| {
                (0..count)
                    .map(|index| 0x0E00 + index)
                    .map(|id| {
                        (
                            id,
                            if id == 0x0E08 {
                                PropertyValue::Integer32(size)
                            } else {
                                PropertyValue::Integer32(i32::from(id))
                            },
                        )
                    })
                    .chain([(
                        0x0037,
                        PropertyValue::Unicode(UnicodeValue::new(
                            "subject".encode_utf16().collect(),
                        )),
                    )])
                    .collect::<Vec<_>>()
            };
            let (expected, _) = property_context_pages(properties(0x1234_5678), 0x80).unwrap();
            let (mut pages, _) = property_context_pages(properties(0), 0x80).unwrap();
            assert_ne!(pages, expected);
            set_inline_property(&mut pages, 0x0E08, 0x0003, 0x1234_5678).unwrap();
            assert_eq!(pages, expected, "{count} properties");
            assert!(
                set_inline_property(&mut pages, 0x0D00, 0x0003, 1).is_err(),
                "a missing property"
            );
            assert!(
                set_inline_property(&mut pages, 0x0037, 0x0003, 1).is_err(),
                "another type"
            );
        }
    }

    #[test]
    fn a_message_near_the_size_limit_is_built_before_the_commit_and_refused_only_when_too_large() {
        // A message whose bound passes PidTagMessageSize's limit is built before the commit writes anything; it is
        // refused only when its exact size does not fit. The content is never read here (the stream is not opened).
        let open = || -> io::Result<Box<dyn Read>> { panic!("the stream was opened") };
        let input = message("near", "<near@example.com>", true);
        let prepared = |len: u64| {
            let attachment = UnicodePstAttachment {
                filename: "near.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: &[],
                stream: Some(UnicodePstAttachmentStream { len, open: &open }),
                hidden: false,
                attached_message: None,
            };
            prepare_message(
                &input,
                std::slice::from_ref(&attachment),
                7,
                &crate::NameIds::default(),
            )
        };
        let fits = i32::MAX as u64 - 600_000;
        prepared(fits).unwrap();
        assert!(
            fits + message_data_bound(&input) > i32::MAX as u64 + 6,
            "the bound passes the limit"
        );
        let error = prepared(i32::MAX as u64 - 1_000).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert!(error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<UnicodePstMessageTooLarge>())
            .is_some());
    }

    #[test]
    fn every_ltp_subnode_has_the_ltp_nid_type() {
        // A table's row matrix, a property value too large for its heap and an attachment's data are subnodes of
        // NID_TYPE_LTP, as in Outlook-written stores. With NID_TYPE_INTERNAL the first row matrix would be 0x21,
        // NID_MESSAGE_STORE, which libpff misreads. The writer creates no INTERNAL subnode at any depth.
        let path = temp("ltp-subnodes");
        let body = "large body ".repeat(500);
        let mut input = message("ltp", "<ltp@example.com>", false);
        input.body = &body;
        let data = vec![7_u8; 20_000];
        let attachment = UnicodePstAttachment {
            filename: "data.bin",
            mime_type: "application/octet-stream",
            content_id: None,
            data: &data,
            stream: None,
            hidden: false,
            attached_message: None,
        };
        drop(
            UnicodePstFile::create_with_attachments(
                &path,
                &input,
                std::slice::from_ref(&attachment),
            )
            .unwrap(),
        );
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        let batch = [UnicodePstBatchMessage {
            message: message("sub", "<sub@example.com>", true),
            attachments: &[],
        }];
        writer
            .append_many_in_folder_with_attachments(&["Inbox", "Sub"], &batch)
            .unwrap();
        writer.close().unwrap();
        let mut core = WriterCore::open(&path).unwrap();
        let state = core.state.take().unwrap();
        let bbt = *state.header.root().block_btree();
        let mut seen = 0;
        let mut stack: Vec<(NodeId, Option<UnicodeBlockId>)> = state
            .nbt
            .entries()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.node(), entry.sub_node()))
            .collect();
        while let Some((owner, sub)) = stack.pop() {
            for entry in collect_subnode_entries(&mut core.file, bbt, sub).unwrap() {
                seen += 1;
                assert!(
                    !matches!(entry.node().id_type(), Ok(NodeIdType::Internal)),
                    "subnode {:#x} of {:#x} is NID_TYPE_INTERNAL",
                    u32::from(entry.node()),
                    u32::from(owner)
                );
                stack.push((entry.node(), entry.sub_node()));
            }
        }
        assert!(seen > 5, "only {seen} subnodes");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_template_row_in_a_heap_row_matrix_is_patched_in_place() {
        // Only the template's own folders are written to, so the IPM subtree's hierarchy table keeps its rows in the
        // heap to the end. Its row of Deleted Items is patched in place, through the row index (the heap branch of
        // `patch_table_row_by_index`): the counters after each append, PidTagSubfolders when a child is created. The
        // row must mirror the folder after every commit; nothing falls back to the rebuild.
        let path = temp("template-row");
        create_template_store(&path).unwrap();
        let hierarchy = HIERARCHY_FALLBACKS.with(std::cell::Cell::get);
        let ids: Vec<String> = (0..6)
            .map(|index| format!("<t{index}@example.com>"))
            .collect();
        let row_of_deleted = |path: &Path| {
            let mut core = WriterCore::open(path).unwrap();
            let deleted = ensure_unicode_folder_path(&mut core, &["Deleted Items"]).unwrap();
            let store = core.store().unwrap();
            let ipm = store
                .properties()
                .ipm_sub_tree_entry_id()
                .unwrap()
                .node_id();
            let table = store
                .open_folder(&store.properties().make_entry_id(ipm).unwrap())
                .unwrap()
                .hierarchy_table()
                .cloned()
                .unwrap();
            let context = table.context();
            assert!(
                context
                    .rows()
                    .is_some_and(|rows| matches!(rows.id_type(), Ok(NodeIdType::HeapNode))),
                "the rows left the heap"
            );
            let row = table
                .rows_matrix()
                .find(|row| u32::from(row.id()) == u32::from(deleted))
                .unwrap();
            let values = row.columns(context).unwrap();
            let cell = |id: u16| {
                let at = context
                    .columns()
                    .iter()
                    .position(|column| column.prop_id() == id)
                    .unwrap();
                match &values[at] {
                    Some(TableRowColumnValue::Small(PropertyValue::Integer32(value))) => {
                        i64::from(*value)
                    }
                    Some(TableRowColumnValue::Small(PropertyValue::Boolean(value))) => {
                        i64::from(*value)
                    }
                    other => panic!("cell {id:#06x}: {other:?}"),
                }
            };
            (cell(0x3602), cell(0x3603), cell(0x360A))
        };
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        let mut expected = (0, 0, 0);
        for (index, id) in ids.iter().enumerate() {
            let target: &[&str] = if index == 3 {
                &["Deleted Items", "Sub"]
            } else {
                &["Deleted Items"]
            };
            let batch = [UnicodePstBatchMessage {
                message: message("template row", id, index % 2 == 0),
                attachments: &[],
            }];
            writer
                .append_many_in_folder_with_attachments(target, &batch)
                .unwrap();
            if index == 3 {
                expected.2 = 1;
            } else {
                expected.0 += 1;
                expected.1 += i64::from(index % 2 != 0);
            }
            assert_eq!(row_of_deleted(&path), expected, "after commit {index}");
            let (count, unread) = folder(&path, &["Deleted Items"]);
            assert_eq!(
                (i64::from(count), i64::from(unread)),
                (expected.0, expected.1)
            );
        }
        writer.close().unwrap();
        assert_eq!(
            HIERARCHY_FALLBACKS.with(std::cell::Cell::get),
            hierarchy,
            "a hierarchy-table change fell back to the full read and rebuild"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn many_sibling_folders_are_created_and_counted_on_their_changed_paths() {
        // 460 sibling folders under Inbox, so the row index of Inbox's hierarchy table grows past one BTH leaf (447
        // records); every tenth folder and the template's own "Deleted Items" (whose row is in a heap row matrix) and a
        // child of it receive messages. The session is reopened half way. Every hierarchy-table change goes along
        // paths: no commit falls back to reading and rebuilding a table, and none to the full reference recount. After
        // every folder creation the test build checks the session's hierarchy state against a fresh load from disk.
        let path = temp("siblings");
        drop(
            UnicodePstFile::create(&path, &message("first", "<first@example.com>", true)).unwrap(),
        );
        let refcounts = REFCOUNT_FALLBACKS.with(std::cell::Cell::get);
        let hierarchy = HIERARCHY_FALLBACKS.with(std::cell::Cell::get);
        let names: Vec<String> = (0..460).map(|index| format!("S{index:04}")).collect();
        let ids: Vec<String> = (0..200)
            .map(|index| format!("<s{index}@example.com>"))
            .collect();
        let mut next = 0;
        let mut template = [(0_i32, 0_i32); 2];
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        for (index, name) in names.iter().enumerate() {
            if index % 10 != 0 {
                ensure_unicode_folder_path(&mut writer.core, &["Inbox", name]).unwrap();
                continue;
            }
            let batch = [UnicodePstBatchMessage {
                message: message("sibling", &ids[next], index % 20 == 0),
                attachments: &[],
            }];
            writer
                .append_many_in_folder_with_attachments(&["Inbox", name], &batch)
                .unwrap();
            next += 1;
            if index % 100 == 0 {
                for (slot, target) in [&["Deleted Items"][..], &["Deleted Items", "Sub"][..]]
                    .into_iter()
                    .enumerate()
                {
                    let batch = [UnicodePstBatchMessage {
                        message: message("template", &ids[next], false),
                        attachments: &[],
                    }];
                    writer
                        .append_many_in_folder_with_attachments(target, &batch)
                        .unwrap();
                    next += 1;
                    template[slot].0 += 1;
                    template[slot].1 += 1;
                }
            }
            if index == 230 {
                writer.close().unwrap();
                writer = UnicodePstWriter::open(&path).unwrap();
            }
        }
        writer.close().unwrap();
        assert_eq!(
            HIERARCHY_FALLBACKS.with(std::cell::Cell::get),
            hierarchy,
            "a hierarchy-table change fell back to the full read and rebuild"
        );
        assert_eq!(
            REFCOUNT_FALLBACKS.with(std::cell::Cell::get),
            refcounts,
            "a commit fell back to the full reference recount"
        );
        assert_eq!(folder(&path, &["Deleted Items"]), template[0]);
        assert_eq!(folder(&path, &["Deleted Items", "Sub"]), template[1]);
        // The IPM subtree's row of Deleted Items (a template row; its table's rows were in the heap when it was first
        // patched) mirrors the folder's counters and has gained PidTagSubfolders.
        {
            let mut core = WriterCore::open(&path).unwrap();
            let deleted = ensure_unicode_folder_path(&mut core, &["Deleted Items"]).unwrap();
            let store = core.store().unwrap();
            let ipm = store
                .properties()
                .ipm_sub_tree_entry_id()
                .unwrap()
                .node_id();
            let table = store
                .open_folder(&store.properties().make_entry_id(ipm).unwrap())
                .unwrap()
                .hierarchy_table()
                .cloned()
                .unwrap();
            let context = table.context();
            let row = table
                .rows_matrix()
                .find(|row| u32::from(row.id()) == u32::from(deleted))
                .unwrap();
            let values = row.columns(context).unwrap();
            let cell = |id: u16| {
                let at = context
                    .columns()
                    .iter()
                    .position(|column| column.prop_id() == id)
                    .unwrap();
                match &values[at] {
                    Some(TableRowColumnValue::Small(value)) => value.clone(),
                    other => panic!("cell {id:#06x}: {other:?}"),
                }
            };
            assert!(
                matches!(cell(0x3602), PropertyValue::Integer32(count) if count == template[0].0)
            );
            assert!(
                matches!(cell(0x3603), PropertyValue::Integer32(unread) if unread == template[0].1)
            );
            assert!(matches!(cell(0x360A), PropertyValue::Boolean(true)));
        }
        for index in [0, 10, 440, 450, 459] {
            let expected = if index % 10 == 0 {
                (1, i32::from(index % 20 != 0))
            } else {
                (0, 0)
            };
            assert_eq!(
                folder(&path, &["Inbox", &names[index]]),
                expected,
                "folder {index}"
            );
        }

        // Inbox's hierarchy table holds every folder once, each row with its folder's counters, and its row index has
        // an intermediate level through which every row is found at its position.
        let mut core = WriterCore::open(&path).unwrap();
        let inbox = ensure_unicode_folder_path(&mut core, &["Inbox"]).unwrap();
        let store = core.store().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(inbox).unwrap())
            .unwrap()
            .hierarchy_table()
            .cloned()
            .unwrap();
        let context = table.context();
        let column = |id: u16| {
            context
                .columns()
                .iter()
                .position(|column| column.prop_id() == id)
                .unwrap()
        };
        let (count, unread) = (column(0x3602), column(0x3603));
        let rows: Vec<(u32, i32, i32)> = table
            .rows_matrix()
            .map(|row| {
                let values = row.columns(context).unwrap();
                let small = |at: usize| match &values[at] {
                    Some(TableRowColumnValue::Small(PropertyValue::Integer32(value))) => *value,
                    other => panic!("counter cell {other:?}"),
                };
                (u32::from(row.id()), small(count), small(unread))
            })
            .collect();
        assert_eq!(rows.len(), 460);
        assert!(rows.iter().enumerate().all(|(index, (_, count, unread))| {
            (*count, *unread)
                == if index % 10 == 0 {
                    (1, i32::from(index % 20 != 0))
                } else {
                    (0, 0)
                }
        }));
        let state = core.state.take().unwrap();
        let entry = state
            .nbt
            .get(node_key(
                NodeId::new(NodeIdType::HierarchyTable, inbox.index()).unwrap(),
            ))
            .unwrap()
            .unwrap();
        let subnodes = collect_subnode_entries(
            &mut core.file,
            *state.header.root().block_btree(),
            entry.sub_node(),
        )
        .unwrap();
        let tx = Tx::new(&state);
        let mut view = open_table_view(&mut core.file, &tx, entry.data(), |rows| {
            subnodes
                .iter()
                .find(|entry| entry.node() == rows)
                .map(|entry| entry.block())
        })
        .unwrap()
        .unwrap();
        let index_hid = view.context.row_index();
        ensure_heap_page(
            &mut core.file,
            &tx,
            &mut view.heap_tree,
            &mut view.heap,
            usize::from(index_hid.block_index()),
        )
        .unwrap();
        let index = HeapTreeHeader::read(&mut view.heap.get(index_hid).unwrap()).unwrap();
        assert!(
            index.levels() >= 1,
            "the row index has {} levels",
            index.levels()
        );
        assert!(
            assert_fill_levels(&mut core.file, &tx, entry.data()) > 1,
            "A10: the hierarchy heap's fill levels"
        );
        for (position, (id, _, _)) in rows.iter().enumerate() {
            assert_eq!(
                find_table_row(&mut core.file, &tx, &mut view, *id).unwrap(),
                Some(position)
            );
        }
        assert_eq!(
            find_table_row(&mut core.file, &tx, &mut view, u32::MAX).unwrap(),
            None
        );
        drop(tx);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_data_tree_with_a_short_inner_leaf_is_not_taken_as_a_path() {
        // `DataTreePath::open` takes only the shape this writer writes. Three leaves of 8,176, 8,000 and 8,176 bytes
        // have the leaf count of that shape (ceil(24,352 / 8,176) = 3) but not its sizes; only the last leaf's size
        // gives it away. The same leaves with the short one last are taken.
        let path = temp("data-tree-shape");
        drop(
            UnicodePstFile::create(&path, &message("first", "<first@example.com>", true)).unwrap(),
        );
        let mut core = WriterCore::open(&path).unwrap();
        let roots = core
            .run(|_file, state, _store| {
                let header = state.header.clone();
                let mut tx = Tx::new(state);
                let mut next_bid = header
                    .next_block()
                    .index()
                    .max(tx.max_block_index().unwrap_or(0).saturating_add(1));
                let mut next_offset = tx.file_space();
                let mut blocks = Vec::new();
                let mut roots = Vec::new();
                for (slot, sizes) in [
                    [MAX_DATA_BLOCK_SIZE, 8000, MAX_DATA_BLOCK_SIZE],
                    [MAX_DATA_BLOCK_SIZE, MAX_DATA_BLOCK_SIZE, 8000],
                ]
                .into_iter()
                .enumerate()
                {
                    let leaves = sizes
                        .iter()
                        .map(|size| {
                            Ok((
                                push_block(
                                    &mut blocks,
                                    &mut next_bid,
                                    &mut next_offset,
                                    false,
                                    NewBlockData::Data(vec![0x5A; *size]),
                                )?,
                                *size as u32,
                            ))
                        })
                        .collect::<io::Result<Vec<_>>>()?;
                    let root = push_data_tree_leaves(
                        &mut blocks,
                        &mut next_bid,
                        &mut next_offset,
                        leaves,
                    )?;
                    let node =
                        NodeId::new(NodeIdType::ListsTablesProperties, 0x7_F000 + slot as u32)?;
                    tx.put_node(UnicodeNodeBTreeEntry::new(node, root, None, None));
                    roots.push(root);
                }
                Ok((
                    CommitPlan {
                        header,
                        changes: tx.changes,
                        blocks,
                        next_bid,
                        next_offset,
                        reserved_nodes: Vec::new(),
                    },
                    roots,
                ))
            })
            .unwrap();
        let state = core.state.take().unwrap();
        let tx = Tx::new(&state);
        assert!(
            DataTreePath::open(&mut core.file, &tx, roots[0])
                .unwrap()
                .is_none(),
            "a short inner leaf"
        );
        let canonical = DataTreePath::open(&mut core.file, &tx, roots[1])
            .unwrap()
            .expect("full leaves, a short last one");
        assert_eq!((canonical.level, canonical.leaves), (1, 3));
        drop(tx);
        core.state = Some(state);
        drop(core);
        std::fs::remove_file(path).unwrap();
    }

    /// The children of the search queue's root index block.
    fn queue_index(core: &mut WriterCore) -> Vec<u64> {
        let state = core.state.take().unwrap();
        let entry = state
            .nbt
            .get(node_key(NID_SEARCH_MANAGEMENT_QUEUE))
            .unwrap()
            .unwrap();
        let tx = Tx::new(&state);
        let root = bbt_entry(&tx, entry.data()).unwrap();
        let children = match UnicodeDataTree::read(
            &mut core.file,
            state.header.crypt_method(),
            &root,
        )
        .unwrap()
        {
            DataTree::Leaf(_) => Vec::new(),
            DataTree::Intermediate(tree) => tree
                .entries()
                .iter()
                .map(|entry| entry.block().search_key())
                .collect(),
        };
        drop(tx);
        core.state = Some(state);
        children
    }

    /// The search queue's data stream, read through its data tree.
    fn queue_stream(core: &mut WriterCore) -> (u8, Vec<u8>) {
        let state = core.state.take().unwrap();
        let entry = state
            .nbt
            .get(node_key(NID_SEARCH_MANAGEMENT_QUEUE))
            .unwrap()
            .unwrap();
        let tx = Tx::new(&state);
        let root = bbt_entry(&tx, entry.data()).unwrap();
        let level = match UnicodeDataTree::read(&mut core.file, state.header.crypt_method(), &root)
            .unwrap()
        {
            DataTree::Leaf(_) => 0,
            DataTree::Intermediate(tree) => tree.header().level(),
        };
        let mut data = Vec::new();
        for (leaf, _) in data_tree_leaves(&mut core.file, &tx, entry.data()).unwrap() {
            data.extend(read_data_block(&mut core.file, &tx, leaf).unwrap());
        }
        drop(tx);
        core.state = Some(state);
        (level, data)
    }

    #[test]
    fn a_search_queue_past_one_index_block_is_appended_on_its_last_path() {
        // A queue of more than 1,021 leaves (an XXBLOCK over XBLOCKs) is appended by rewriting only its last leaf, the
        // last XBLOCK and the XXBLOCK; the stream is the old stream plus the new records, and the reference counts
        // (checked against a full recount after every commit) stay right.
        let path = temp("queue-two-levels");
        drop(
            UnicodePstFile::create(&path, &message("first", "<first@example.com>", true)).unwrap(),
        );
        let fallbacks = REFCOUNT_FALLBACKS.with(std::cell::Cell::get);
        let mut core = WriterCore::open(&path).unwrap();
        let (_, before) = queue_stream(&mut core);
        assert_eq!(before.len() % 20, 0);
        // Grow the queue to 1,022 full leaves plus a partial one by repeating its existing records.
        let records: Vec<u8> = before
            .chunks(20)
            .cycle()
            .take((1022 * MAX_DATA_BLOCK_SIZE + 4000) / 20)
            .flatten()
            .copied()
            .collect();
        core.run(|file, state, _store| {
            let header = state.header.clone();
            let mut tx = Tx::new(state);
            let queue = tx.node(NID_SEARCH_MANAGEMENT_QUEUE)?.unwrap();
            let mut next_bid = header
                .next_block()
                .index()
                .max(tx.max_block_index().unwrap_or(0).saturating_add(1));
            let mut next_offset = tx.file_space();
            let mut blocks = Vec::new();
            let data = push_data_tree(&mut blocks, &mut next_bid, &mut next_offset, &records)?;
            tx.put_node(UnicodeNodeBTreeEntry::new(
                NID_SEARCH_MANAGEMENT_QUEUE,
                data,
                queue.sub_node(),
                queue.parent(),
            ));
            let _ = file;
            Ok((
                CommitPlan {
                    header,
                    changes: tx.changes,
                    blocks,
                    next_bid,
                    next_offset,
                    reserved_nodes: Vec::new(),
                },
                (),
            ))
        })
        .unwrap();
        let (level, grown) = queue_stream(&mut core);
        assert_eq!(level, 2, "an XXBLOCK over XBLOCKs");
        assert_eq!(grown, records);
        let index_before = queue_index(&mut core);
        assert_eq!(index_before.len(), 2);
        let subjects: Vec<String> = (0..30).map(|index| format!("late {index}")).collect();
        let ids: Vec<String> = (0..30)
            .map(|index| format!("<late{index}@example.com>"))
            .collect();
        let mut writer = UnicodePstWriter { core };
        for round in 0..3 {
            let batch: Vec<UnicodePstBatchMessage<'_>> = (round * 10..(round + 1) * 10)
                .map(|index| UnicodePstBatchMessage {
                    message: message(&subjects[index], &ids[index], false),
                    attachments: &[],
                })
                .collect();
            writer
                .append_many_in_folder_with_attachments(&["Inbox"], &batch)
                .unwrap();
        }
        let mut core = writer.core;
        let index_after = queue_index(&mut core);
        assert_eq!(
            index_after[0], index_before[0],
            "the full XBLOCK was kept, not rewritten (in-place path)"
        );
        assert_ne!(
            index_after.last(),
            index_before.last(),
            "the last XBLOCK was replaced"
        );
        let (level, after) = queue_stream(&mut core);
        assert_eq!(level, 2);
        assert_eq!(
            &after[..records.len()],
            &records[..],
            "the old records are kept as they were"
        );
        // 30 MessageAdded records, plus the folder records of the first append.
        assert!(
            (after.len() - records.len()) / 20 >= 30,
            "the new records were appended"
        );
        // Every commit's stated reference changes added up.
        assert_eq!(
            REFCOUNT_FALLBACKS.with(std::cell::Cell::get),
            fallbacks,
            "a commit fell back to the full reference recount"
        );
        drop(core);
        std::fs::remove_file(path).unwrap();
    }

    /// A reader that hands out `data` in small pieces and can fail after `fail_after` bytes.
    struct Trickle {
        data: Vec<u8>,
        at: usize,
        piece: usize,
        fail_after: Option<usize>,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.fail_after.is_some_and(|limit| self.at >= limit) {
                return Err(io::Error::other("source read failed"));
            }
            let n = self.piece.min(buf.len()).min(self.data.len() - self.at);
            buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    fn snapshot(path: &Path) -> (Vec<u8>, Vec<u8>) {
        let mut file = File::open(path).unwrap();
        let mut header = vec![0; 564];
        file.read_exact(&mut header).unwrap();
        let state = CommittedState::read_any(
            &mut OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap(),
            None,
        )
        .unwrap();
        let mut entries = Vec::new();
        for entry in state.bbt.entries().unwrap().iter() {
            entry.write(&mut entries).unwrap();
        }
        for entry in state.nbt.entries().unwrap().iter() {
            entry.write(&mut entries).unwrap();
        }
        (header, entries)
    }

    fn limit_reached(error: &io::Error) -> Option<crate::UnicodePstFileLimitReached> {
        error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<crate::UnicodePstFileLimitReached>())
            .copied()
    }

    #[test]
    fn message_size_is_every_byte_its_node_and_subnodes_hold() {
        // Classic Outlook stores PidTagMessageSize as exactly the bytes a message's node and its subnodes hold, at
        // every depth (the leaves of their data trees; 20 messages it wrote, with and without attachments, attached
        // e-mails and values in several blocks), and SCANPST's repair sets it so. Measured here from the file.
        let path = temp("m33-message-size");
        let body = "line\n".repeat(3000);
        let name = "N".repeat(2000);
        let recipients = [UnicodePstRecipient {
            name: &name,
            email: "long@example.invalid",
            recipient_type: UnicodePstRecipientType::To,
            properties: &[],
        }];
        let inner = UnicodePstMessage {
            body: &body,
            ..message("inner", "<inner@example.com>", true)
        };
        let attachments = [
            UnicodePstAttachment {
                filename: "a.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: &[7; 20_000],
                stream: None,
                hidden: false,
                attached_message: None,
            },
            UnicodePstAttachment {
                filename: "Inner",
                mime_type: "",
                content_id: None,
                data: &[],
                stream: None,
                hidden: false,
                attached_message: Some(Box::new(UnicodePstAttachedMessage {
                    message: inner,
                    attachments: vec![],
                })),
            },
        ];
        let mut writer = UnicodePstWriter::create(&path).unwrap();
        let plain = [UnicodePstBatchMessage {
            message: message("plain", "<plain@example.com>", true),
            attachments: &[],
        }];
        writer
            .append_many_in_folder_with_attachments(&["Inbox"], &plain)
            .unwrap();
        let large = UnicodePstMessage {
            body: &body,
            recipients: &recipients,
            ..message("large", "<large@example.com>", false)
        };
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: large,
                    attachments: &attachments,
                }],
            )
            .unwrap();
        writer.close().unwrap();

        let mut core = WriterCore::open(&path).unwrap();
        let folder = ensure_unicode_folder_path(&mut core, &["Inbox"]).unwrap();
        let store = core.store().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(folder).unwrap())
            .unwrap()
            .contents_table()
            .cloned()
            .unwrap();
        let state = core.state.take().unwrap();
        let bbt = *state.header.root().block_btree();
        let tx = Tx::new(&state);
        fn held(
            file: &mut File,
            tx: &Tx<'_>,
            bbt: UnicodePageRef,
            data: UnicodeBlockId,
            sub: Option<UnicodeBlockId>,
        ) -> u64 {
            let mut total = data_tree_leaves(file, tx, data)
                .unwrap()
                .iter()
                .map(|(_, size)| u64::from(*size))
                .sum::<u64>();
            for entry in collect_subnode_entries(file, bbt, sub).unwrap() {
                total += held(file, tx, bbt, entry.block(), entry.sub_node());
            }
            total
        }
        let mut seen = 0;
        for row in table.rows_matrix() {
            let node = NodeId::from(u32::from(row.id()));
            let message = UnicodeMessage::read(
                store.clone(),
                &store.properties().make_entry_id(node).unwrap(),
                None,
            )
            .unwrap();
            let Some(PropertyValue::Integer32(size)) = message.properties().get(0x0E08) else {
                panic!("no PidTagMessageSize")
            };
            let entry = state.nbt.get(node_key(node)).unwrap().unwrap();
            let stored = held(&mut core.file, &tx, bbt, entry.data(), entry.sub_node());
            assert_eq!(
                *size as u64, stored,
                "{node:?}: PidTagMessageSize against the bytes its node and subnodes hold"
            );
            seen += 1;
        }
        assert_eq!(seen, 2);
        drop(tx);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn message_size_counts_the_padding_of_a_row_matrix_in_several_blocks() {
        // A recipient or attachment table whose row matrix needs more than one data block is written with every block
        // but the last padded to 8,176 bytes (MS-PST 2.3.4.4), and those bytes are held by the message's subnodes. 200
        // recipients with two given Integer32 properties (65-byte rows: 125 per block, 51 bytes of padding each) and
        // 400 one-byte attachments (25-byte rows: 327 per block, 1 byte each).
        let path = temp("row-matrix-padding");
        let props = [
            UnicodePstProperty {
                id: 0x5FFD,
                value: UnicodePstValue::Integer32(1),
            },
            UnicodePstProperty {
                id: 0x5FFF,
                value: UnicodePstValue::Integer32(0),
            },
        ];
        let names: Vec<String> = (0..200).map(|i| format!("Attendee {i}")).collect();
        let emails: Vec<String> = (0..200).map(|i| format!("a{i}@example.invalid")).collect();
        let recipients: Vec<UnicodePstRecipient<'_>> = (0..200)
            .map(|i| UnicodePstRecipient {
                name: &names[i],
                email: &emails[i],
                recipient_type: UnicodePstRecipientType::To,
                properties: &props,
            })
            .collect();
        let attachments: Vec<UnicodePstAttachment<'_>> = (0..400)
            .map(|_| UnicodePstAttachment {
                filename: "a.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: b"x",
                stream: None,
                hidden: false,
                attached_message: None,
            })
            .collect();
        let mut writer = UnicodePstWriter::create(&path).unwrap();
        let rows = UnicodePstMessage {
            recipients: &recipients,
            ..message("rows", "<rows@example.com>", true)
        };
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[
                    UnicodePstBatchMessage {
                        message: rows,
                        attachments: &[],
                    },
                    UnicodePstBatchMessage {
                        message: message("attachments", "<attachments@example.com>", true),
                        attachments: &attachments,
                    },
                ],
            )
            .unwrap();
        writer.close().unwrap();

        let mut core = WriterCore::open(&path).unwrap();
        let folder = ensure_unicode_folder_path(&mut core, &["Inbox"]).unwrap();
        let store = core.store().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(folder).unwrap())
            .unwrap()
            .contents_table()
            .cloned()
            .unwrap();
        let state = core.state.take().unwrap();
        let bbt = *state.header.root().block_btree();
        let tx = Tx::new(&state);
        fn held(
            file: &mut File,
            tx: &Tx<'_>,
            bbt: UnicodePageRef,
            data: UnicodeBlockId,
            sub: Option<UnicodeBlockId>,
        ) -> u64 {
            let mut total = data_tree_leaves(file, tx, data)
                .unwrap()
                .iter()
                .map(|(_, size)| u64::from(*size))
                .sum::<u64>();
            for entry in collect_subnode_entries(file, bbt, sub).unwrap() {
                total += held(file, tx, bbt, entry.block(), entry.sub_node());
            }
            total
        }
        let mut seen = 0;
        for row in table.rows_matrix() {
            let node = NodeId::from(u32::from(row.id()));
            let message = UnicodeMessage::read(
                store.clone(),
                &store.properties().make_entry_id(node).unwrap(),
                None,
            )
            .unwrap();
            let Some(PropertyValue::Integer32(size)) = message.properties().get(0x0E08) else {
                panic!("no PidTagMessageSize")
            };
            let entry = state.nbt.get(node_key(node)).unwrap().unwrap();
            let stored = held(&mut core.file, &tx, bbt, entry.data(), entry.sub_node());
            assert_eq!(
                *size as u64, stored,
                "{node:?}: PidTagMessageSize against the bytes its node and subnodes hold"
            );
            seen += 1;
        }
        assert_eq!(seen, 2);
        drop(tx);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn table_cells_in_subnodes_share_the_block_of_their_objects_value() {
        // Outlook writes a table cell too large for the heap as a subnode whose block is the block of the same value in
        // the row's object, the block's reference count one higher (its own 120-recipient message: the contents cell
        // PidTagDisplayTo and the message's value are block 0x176, cRef 3). A copy is what SCANPST reports as "row
        // doesn't match sub-object". Here: a 2,000-character recipient name (PidTagDisplayTo, 4,000 bytes) and a given
        // 5,000-byte PidTagConversationIndex in the contents row, a 2,005-character subject and the conversation topic
        // made from it (two properties, two blocks with the same bytes: each cell shares its own property's), a
        // 1,900-character file name in the attachment table; each cell's subnode is the object's block, which has two
        // references (cRef 3). The test build's reference-count check compares every commit's counts with a full
        // recount.
        let path = temp("m32-shared-cells");
        let name = "N".repeat(2000);
        let blob = vec![0x5A_u8; 5000];
        let file_name = format!("{}.bin", "n".repeat(1896));
        let recipients = [UnicodePstRecipient {
            name: &name,
            email: "long@example.invalid",
            recipient_type: UnicodePstRecipientType::To,
            properties: &[],
        }];
        let properties = [UnicodePstProperty {
            id: 0x0071,
            value: UnicodePstValue::Binary(&blob),
        }];
        let attachments = [UnicodePstAttachment {
            filename: &file_name,
            mime_type: "application/octet-stream",
            content_id: None,
            data: b"data",
            stream: None,
            hidden: false,
            attached_message: None,
        }];
        let mut writer = UnicodePstWriter::create(&path).unwrap();
        let first = [UnicodePstBatchMessage {
            message: message("first", "<first@example.com>", true),
            attachments: &[],
        }];
        writer
            .append_many_in_folder_with_attachments(&["Inbox"], &first)
            .unwrap();
        let subject = format!("large{}", "S".repeat(2000));
        let large = UnicodePstMessage {
            recipients: &recipients,
            properties: &properties,
            ..message(&subject, "<large@example.com>", true)
        };
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: large,
                    attachments: &attachments,
                }],
            )
            .unwrap();
        writer.close().unwrap();

        let mut core = WriterCore::open(&path).unwrap();
        let folder = ensure_unicode_folder_path(&mut core, &["Inbox"]).unwrap();
        let store = core.store().unwrap();
        let table = store
            .open_folder(&store.properties().make_entry_id(folder).unwrap())
            .unwrap()
            .contents_table()
            .cloned()
            .unwrap();
        let state = core.state.take().unwrap();
        let bbt = *state.header.root().block_btree();
        let tx = Tx::new(&state);
        let subnodes = |file: &mut File, entry: Option<UnicodeBlockId>| {
            collect_subnode_entries(file, bbt, entry).unwrap()
        };
        let ref_count =
            |block: UnicodeBlockId| tx.block(block.search_key()).unwrap().unwrap().ref_count();
        let large_row = table.rows_matrix().find(|row| {
            let entry_id = store.properties().make_entry_id(NodeId::from(u32::from(row.id()))).unwrap();
            let message = UnicodeMessage::read(store.clone(), &entry_id, None).unwrap();
            matches!(message.properties().get(0x0037), Some(PropertyValue::Unicode(subject)) if subject.to_string().starts_with("large"))
        }).unwrap();
        let message_node = NodeId::from(u32::from(large_row.id()));
        let message_entry = state.nbt.get(node_key(message_node)).unwrap().unwrap();
        let message_subnodes = subnodes(&mut core.file, message_entry.sub_node());
        let message_store = UnicodeMessage::read(
            store.clone(),
            &store.properties().make_entry_id(message_node).unwrap(),
            None,
        )
        .unwrap();
        let contents = state
            .nbt
            .get(node_key(
                NodeId::new(NodeIdType::ContentsTable, folder.index()).unwrap(),
            ))
            .unwrap()
            .unwrap();
        let contents_subnodes = subnodes(&mut core.file, contents.sub_node());
        let values = large_row.columns(table.context()).unwrap();
        let mut shared = Vec::new();
        for (id, length) in [
            (0x0E04_u16, 4000_usize),
            (0x0071, 5000),
            (0x0037, 4010),
            (0x0070, 4010),
        ] {
            let (_, value) = table
                .context()
                .columns()
                .iter()
                .zip(&values)
                .find(|(column, _)| column.prop_id() == id)
                .unwrap();
            let Some(TableRowColumnValue::Node(cell)) = value else {
                panic!("{id:#06x}: not a subnode: {value:?}")
            };
            let cell_block = contents_subnodes
                .iter()
                .find(|entry| entry.node() == *cell)
                .unwrap()
                .block();
            // The message has the property, and the cell's block is one of the message's own value subnodes.
            assert!(message_store.properties().get(id).is_some(), "{id:#06x}");
            let object_block = message_subnodes
                .iter()
                .filter(|entry| {
                    entry.node().id_type().ok() == Some(NodeIdType::ListsTablesProperties)
                })
                .map(|entry| entry.block())
                .find(|block| *block == cell_block);
            assert!(
                object_block.is_some(),
                "{id:#06x}: the contents cell's block {cell_block:?} is no block of the message"
            );
            assert_eq!(
                data_tree_leaves(&mut core.file, &tx, cell_block)
                    .unwrap()
                    .iter()
                    .map(|(_, size)| *size as usize)
                    .sum::<usize>(),
                length
            );
            assert_eq!(ref_count(cell_block), 3, "{id:#06x}: two references");
            shared.push(cell_block);
        }
        // The subject's and the topic's cells: the same bytes, each its own property's block.
        assert_ne!(
            shared[2], shared[3],
            "subject and topic cells share one block"
        );
        // The attachment table's file-name cell and the attachment's PidTagAttachFilename.
        let attachment_table = message_subnodes
            .iter()
            .find(|entry| entry.node() == NID_ATTACHMENT_TABLE)
            .unwrap();
        let table_cells = subnodes(&mut core.file, attachment_table.sub_node());
        let attachment = message_subnodes
            .iter()
            .find(|entry| entry.node().id_type().ok() == Some(NodeIdType::Attachment))
            .unwrap();
        let attachment_values = subnodes(&mut core.file, attachment.sub_node());
        let cells_in_subnodes = table_cells
            .iter()
            .filter(|entry| entry.node().index() > 0x80)
            .collect::<Vec<_>>();
        assert_eq!(
            cells_in_subnodes.len(),
            1,
            "one file-name cell in a subnode"
        );
        let cell_block = cells_in_subnodes[0].block();
        assert!(
            attachment_values
                .iter()
                .any(|entry| entry.block() == cell_block),
            "the file-name cell is a block of the attachment"
        );
        assert_eq!(ref_count(cell_block), 3);
        assert_eq!(shared.len(), 4);
        drop(tx);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_commit_past_the_size_limit_is_abandoned_and_the_file_is_cut_back_to_its_last_commit() {
        // A commit that would end the file past the session's limit fails with the typed error before its header; the
        // file is cut back to the end of its last commit, with that commit's header and B-tree entries, so it is the
        // last commit's complete store; a new session appends to it. The streamed 1 MiB attachment is written past the
        // committed end while the commit is planned, so the cut is what removes it.
        let path = temp("limit");
        let mut writer = UnicodePstWriter::create(&path).unwrap();
        let first = [UnicodePstBatchMessage {
            message: message("first", "<first@example.com>", true),
            attachments: &[],
        }];
        writer
            .append_many_in_folder_with_attachments(&["Inbox"], &first)
            .unwrap();
        let committed = std::fs::metadata(&path).unwrap().len();
        let before = snapshot(&path);
        let len = 1 << 20;
        let open = || -> io::Result<Box<dyn Read>> { Ok(Box::new(io::repeat(7).take(len))) };
        let attachments = [UnicodePstAttachment {
            filename: "stream.bin",
            mime_type: "application/octet-stream",
            content_id: None,
            data: &[],
            stream: Some(UnicodePstAttachmentStream { len, open: &open }),
            hidden: false,
            attached_message: None,
        }];
        let large = [UnicodePstBatchMessage {
            message: message("large", "<large@example.com>", false),
            attachments: &attachments,
        }];
        writer.set_max_file_size(Some(committed + 64 * 1024));
        let error = writer
            .append_many_in_folder_with_attachments(&["Inbox"], &large)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge, "{error}");
        let reached =
            limit_reached(&error).unwrap_or_else(|| panic!("not the typed error: {error}"));
        assert_eq!(reached.limit, committed + 64 * 1024);
        // The file grows by less than the attachment: the commit reuses space the committed state marks free.
        assert!(reached.size > reached.limit, "{reached:?}");
        assert!(writer.poisoned().is_some());
        drop(writer);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            committed,
            "the file is cut back to its last commit"
        );
        assert!(
            snapshot(&path) == before,
            "the header or the B-trees changed"
        );
        assert_eq!(folder(&path, &["Inbox"]), (1, 0));
        // The cut file is a store a new session appends to, with and without the attachment.
        let mut again = UnicodePstWriter::open(&path).unwrap();
        again
            .append_many_in_folder_with_attachments(&["Inbox"], &large)
            .unwrap();
        again.close().unwrap();
        assert_eq!(folder(&path, &["Inbox"]), (2, 1));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_folder_created_past_the_size_limit_is_abandoned_too_and_a_limit_not_reached_changes_nothing(
    ) {
        // The limit holds for every commit of the session, a folder's creation included (a limit below the file's size
        // refuses any commit; a folder alone fits in the template's free space, so it would not grow the file). A
        // session whose limit is never reached writes the same bytes as one without a limit.
        let path = temp("limit-folder");
        let mut writer = UnicodePstWriter::create(&path).unwrap();
        writer.set_max_file_size(Some(std::fs::metadata(&path).unwrap().len() - 1));
        let before = snapshot(&path);
        let length = std::fs::metadata(&path).unwrap().len();
        let error = writer.ensure_folder_path(&["New", "Child"]).unwrap_err();
        assert!(limit_reached(&error).is_some(), "{error}");
        drop(writer);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), length);
        assert!(snapshot(&path) == before);
        let store =
            Rc::new(UnicodeStore::read(Rc::new(UnicodePstFile::open(&path).unwrap())).unwrap());
        let ipm = store
            .properties()
            .ipm_sub_tree_entry_id()
            .unwrap()
            .node_id();
        assert!(
            crate::find_child_folder(&store, ipm, "New")
                .unwrap()
                .is_none(),
            "the abandoned folder is in the file"
        );
        std::fs::remove_file(&path).unwrap();

        let bytes_with = |limit: Option<u64>| {
            let path = temp("limit-same");
            let mut writer = UnicodePstWriter::create(&path).unwrap();
            writer.set_max_file_size(limit);
            let batch = [UnicodePstBatchMessage {
                message: message("same", "<same@example.com>", true),
                attachments: &[],
            }];
            writer
                .append_many_in_folder_with_attachments(&["Inbox", "Sub"], &batch)
                .unwrap();
            writer.ensure_folder_path(&["Empty"]).unwrap();
            writer.close().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            std::fs::remove_file(path).unwrap();
            bytes
        };
        assert!(
            bytes_with(None) == bytes_with(Some(1 << 40)),
            "a limit that is not reached changed the file"
        );
    }

    #[test]
    fn free_runs_past_the_cap_keep_the_last_run() {
        // With the cap reached, the run nearest EOF (the free tail of the last range) replaces the last kept run, as
        // `AllocationState::commit` keeps it.
        let (mut runs, mut tail) = (Vec::new(), None);
        for at in [0, 128, 256, 384, 512, 576] {
            add_free_slot(&mut runs, &mut tail, at, 3);
        }
        assert_eq!(runs, [(0, 64), (128, 192), (256, 320)]);
        assert_eq!(
            tail,
            Some((512, 640)),
            "the newest run, adjacent slots joined"
        );
        let (mut runs, mut tail) = (Vec::new(), None);
        for at in [0, 64, 192] {
            add_free_slot(&mut runs, &mut tail, at, 3);
        }
        assert_eq!(
            (runs, tail),
            (vec![(0, 128), (192, 256)], None),
            "under the cap nothing is held apart"
        );
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index * 31 % 251) as u8).collect()
    }

    #[test]
    fn commits_past_the_first_free_map_page_keep_every_map_page_equal_to_a_full_rebuild() {
        // The FMap starts after 128 AMap pages (about 32 MB). The test-only checks after every commit compare every
        // AMap, PMap and FMap page and the header's free size with a full rebuild, so these commits cover the cached
        // per-page free space and FMap values of pages that a commit did not touch.
        let path = temp("fmap");
        drop(
            UnicodePstFile::create(&path, &message("first", "<first@example.com>", true)).unwrap(),
        );
        let bytes = payload(3 * 1024 * 1024);
        let subjects: Vec<String> = (0..14).map(|index| format!("large {index}")).collect();
        let ids: Vec<String> = (0..14)
            .map(|index| format!("<large{index}@example.com>"))
            .collect();
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        for index in 0..14 {
            let attachment = UnicodePstAttachment {
                filename: "large.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: &bytes,
                stream: None,
                hidden: false,
                attached_message: None,
            };
            let folder_path: &[&str] = if index % 2 == 0 {
                &["Inbox"]
            } else {
                &["Inbox", "Sub"]
            };
            writer
                .append_many_in_folder_with_attachments(
                    folder_path,
                    &[UnicodePstBatchMessage {
                        message: message(&subjects[index], &ids[index], false),
                        attachments: &[attachment],
                    }],
                )
                .unwrap();
        }
        writer.close().unwrap();
        let eof = std::fs::metadata(&path).unwrap().len();
        assert!(
            amap_page_count(eof) > FMAP_FIRST_SIZE + 1,
            "the file reaches the first FMap page ({eof} bytes)"
        );
        // The FMap page is all 0xFF, as classic Outlook writes it, never a range's largest free run.
        let file = std::fs::read(&path).unwrap();
        let fmap = &file[FMAP_FIRST_OFFSET as usize..][..PAGE_SIZE];
        assert!(
            fmap[..size_of::<MapBits>()]
                .iter()
                .all(|byte| *byte == 0xFF),
            "FMap bytes are 0xFF"
        );
        assert_eq!(
            fmap[size_of::<MapBits>()],
            PageType::FreeMap as u8,
            "FMap page type"
        );
        // A file whose FMap page holds exact values (an earlier version of this writer wrote them) gets 0xFF again when
        // it is opened, before anything frees space under the stale values.
        let stale = encode_map_page::<{ PageType::FreeMap as u8 }>(
            [0; size_of::<MapBits>()],
            PageType::FreeMap,
            FMAP_FIRST_OFFSET,
        )
        .unwrap();
        write_at(
            &mut OpenOptions::new().write(true).open(&path).unwrap(),
            FMAP_FIRST_OFFSET,
            &stale,
        )
        .unwrap();
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: message("after", "<after@example.com>", false),
                    attachments: &[],
                }],
            )
            .unwrap();
        writer.close().unwrap();
        let file = std::fs::read(&path).unwrap();
        assert!(
            file[FMAP_FIRST_OFFSET as usize..][..size_of::<MapBits>()]
                .iter()
                .all(|byte| *byte == 0xFF),
            "a stale FMap page is 0xFF again"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_commit_with_more_new_blocks_than_the_append_buffer_moves_them_into_the_right_edge() {
        // A 40 MB attachment is about 5,100 data blocks and 6 XBLOCKs: more than APPEND_BUFFER new block entries in one
        // commit, so the buffer moves into the right edge while the commit runs, edge pages are written as they fill,
        // and the small message after it changes reference counts of entries already in the edge. The test-only checks
        // after every commit (incremental reference counts against a full recount, every map page against a full
        // rebuild, both trees well formed and equal to the shadow of every entry) cover that path.
        let path = temp("edge");
        drop(
            UnicodePstFile::create(&path, &message("first", "<first@example.com>", true)).unwrap(),
        );
        let large = payload(40 * 1024 * 1024 + 5);
        let small = payload(20_000);
        let source = large.clone();
        let open = move || -> io::Result<Box<dyn Read>> {
            Ok(Box::new(Trickle {
                data: source.clone(),
                at: 0,
                piece: 65_536,
                fail_after: None,
            }))
        };
        let streamed = UnicodePstAttachment {
            filename: "large.bin",
            mime_type: "application/octet-stream",
            content_id: None,
            data: &[],
            stream: Some(UnicodePstAttachmentStream {
                len: large.len() as u64,
                open: &open,
            }),
            hidden: false,
            attached_message: None,
        };
        let attachment = || UnicodePstAttachment {
            filename: "small.bin",
            mime_type: "application/octet-stream",
            content_id: None,
            data: &small,
            stream: None,
            hidden: false,
            attached_message: None,
        };
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[
                    UnicodePstBatchMessage {
                        message: message("large", "<large@example.com>", false),
                        attachments: &[streamed],
                    },
                    UnicodePstBatchMessage {
                        message: message("small", "<small@example.com>", false),
                        attachments: &[attachment()],
                    },
                ],
            )
            .unwrap();
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: message("after", "<after@example.com>", true),
                    attachments: &[attachment()],
                }],
            )
            .unwrap();
        writer.close().unwrap();
        // "first" (from create, read), "large" and "small" (unread), "after" (read).
        assert_eq!(folder(&path, &["Inbox"]), (4, 2));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_streamed_attachment_writes_the_same_file_as_its_bytes() {
        let bytes = payload(3 * 1024 * 1024 + 17);
        let paths = [temp("bytes"), temp("stream")];
        for (index, path) in paths.iter().enumerate() {
            drop(
                UnicodePstFile::create(path, &message("first", "<first@example.com>", true))
                    .unwrap(),
            );
            let source = bytes.clone();
            let open = move || -> io::Result<Box<dyn Read>> {
                Ok(Box::new(Trickle {
                    data: source.clone(),
                    at: 0,
                    piece: 7,
                    fail_after: None,
                }))
            };
            let attachment = UnicodePstAttachment {
                filename: "big.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: if index == 0 { &bytes } else { &[] },
                stream: (index == 1).then_some(UnicodePstAttachmentStream {
                    len: bytes.len() as u64,
                    open: &open,
                }),
                hidden: false,
                attached_message: None,
            };
            let mut writer = UnicodePstWriter::open(path).unwrap();
            writer
                .append_many_in_folder_with_attachments(
                    &["Inbox"],
                    &[UnicodePstBatchMessage {
                        message: message("second", "<second@example.com>", false),
                        attachments: &[attachment],
                    }],
                )
                .unwrap();
            writer.close().unwrap();
        }
        assert!(
            std::fs::read(&paths[0]).unwrap() == std::fs::read(&paths[1]).unwrap(),
            "same bytes, same file"
        );
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_stream_that_is_short_long_or_fails_is_refused_before_the_commit() {
        let bytes = payload(3 * 1024 * 1024);
        for (case, declared, fail_after, expected) in [
            (
                "short",
                bytes.len() as u64 + 1,
                None,
                io::ErrorKind::UnexpectedEof,
            ),
            (
                "long",
                bytes.len() as u64 - 1,
                None,
                io::ErrorKind::InvalidData,
            ),
            (
                "fails",
                bytes.len() as u64,
                Some(2 * 1024 * 1024 + 5),
                io::ErrorKind::Other,
            ),
        ] {
            let path = temp(case);
            drop(
                UnicodePstFile::create(&path, &message("first", "<first@example.com>", true))
                    .unwrap(),
            );
            let before = snapshot(&path);
            let source = bytes.clone();
            let open = move || -> io::Result<Box<dyn Read>> {
                Ok(Box::new(Trickle {
                    data: source.clone(),
                    at: 0,
                    piece: 4096,
                    fail_after,
                }))
            };
            let attachment = UnicodePstAttachment {
                filename: "big.bin",
                mime_type: "application/octet-stream",
                content_id: None,
                data: &[],
                stream: Some(UnicodePstAttachmentStream {
                    len: declared,
                    open: &open,
                }),
                hidden: false,
                attached_message: None,
            };
            let mut writer = UnicodePstWriter::open(&path).unwrap();
            let error = writer
                .append_many_in_folder_with_attachments(
                    &["Inbox"],
                    &[UnicodePstBatchMessage {
                        message: message("second", "<second@example.com>", false),
                        attachments: &[attachment],
                    }],
                )
                .unwrap_err();
            assert_eq!(error.kind(), expected, "{case}: {error}");
            assert!(writer.poisoned().is_some(), "{case}");
            drop(writer);
            // Blocks went to the file (the failing case reads past two flushes of 128 blocks), the commit did not.
            assert!(
                before == snapshot(&path),
                "{case}: the committed state did not change"
            );
            assert_eq!(folder(&path, &["Inbox"]), (1, 0), "{case}");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_failed_append_keeps_the_committed_state_and_poisons_the_session() {
        let path = temp("poison");
        drop(
            UnicodePstFile::create(&path, &message("first", "<first@example.com>", true)).unwrap(),
        );
        let mut writer = UnicodePstWriter::open(&path).unwrap();
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: message("second", "<second@example.com>", false),
                    attachments: &[],
                }],
            )
            .unwrap();
        let snapshot = |path: &Path| {
            let mut file = File::open(path).unwrap();
            let mut header = vec![0; 564];
            file.read_exact(&mut header).unwrap();
            let state = CommittedState::read_any(
                &mut OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap(),
                None,
            )
            .unwrap();
            let encode = |bytes: &mut Vec<u8>, write: &dyn Fn(&mut Vec<u8>) -> io::Result<()>| {
                write(bytes).unwrap()
            };
            let mut entries = Vec::new();
            for entry in state.bbt.entries().unwrap().iter() {
                encode(&mut entries, &|bytes| entry.write(bytes));
            }
            for entry in state.nbt.entries().unwrap().iter() {
                encode(&mut entries, &|bytes| entry.write(bytes));
            }
            (header, entries)
        };
        let before = snapshot(&path);
        let invalid = UnicodePstAttachment {
            filename: "invalid.png",
            mime_type: "image/png",
            content_id: Some("<>"),
            data: b"x",
            stream: None,
            hidden: true,
            attached_message: None,
        };
        let error = writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[
                    UnicodePstBatchMessage {
                        message: message("third", "<third@example.com>", false),
                        attachments: &[],
                    },
                    UnicodePstBatchMessage {
                        message: message("fourth", "<fourth@example.com>", false),
                        attachments: &[invalid],
                    },
                ],
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(writer.poisoned().is_some());
        let refused = writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: message("fifth", "<fifth@example.com>", false),
                    attachments: &[],
                }],
            )
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("unusable after a failed commit"),
            "{refused}"
        );
        drop(writer);
        assert!(
            before == snapshot(&path),
            "the committed state did not change"
        );

        let mut writer = UnicodePstWriter::open(&path).unwrap();
        writer
            .append_many_in_folder_with_attachments(
                &["Inbox"],
                &[UnicodePstBatchMessage {
                    message: message("sixth", "<sixth@example.com>", false),
                    attachments: &[],
                }],
            )
            .unwrap();
        writer.close().unwrap();
        assert_eq!(folder(&path, &["Inbox"]), (3, 2));
        std::fs::remove_file(path).unwrap();
    }
}
