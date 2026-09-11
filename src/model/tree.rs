//! The in-RAM tree: a struct-of-arrays arena plus the rollup primitives.
//!
//! This is the single biggest architectural enabler in duTime. Holding the
//! whole dictionary resident turns subtree aggregation — the operation the
//! entire product is about, and the one a time-series database fundamentally
//! cannot do — into a pointer walk instead of a recursive SQL query.
//!
//! Struct-of-arrays rather than a `Node` struct with `Vec<Node>` children:
//! ~10 MB for this machine's 126k tracked entities, ~90 MB even if every one of
//! its 1.3M inodes were tracked. The rollup then walks contiguous `Vec<i64>`s
//! instead of chasing pointers through scattered allocations.

use super::{Kind, PathId};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

/// Sentinel for "no parent" (a configured root). Using `u32::MAX` rather than
/// `Option<u32>` keeps the parent array at 4 bytes per node.
pub const NO_PARENT: u32 = u32::MAX;

/// Index into the arena. Distinct from [`PathId`], which is the database
/// identity and is only known after the dictionary is reconciled.
pub type NodeIdx = u32;

/// Inclusive (subtree) totals produced by [`Tree::rollup`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rollup {
    pub bytes: Vec<i64>,
    pub blocks: Vec<i64>,
    /// Count of files in the subtree, including untracked small ones.
    pub files: Vec<i64>,
    pub dirs: Vec<i64>,
}

/// An arena-allocated filesystem tree carrying exclusive ("own") sizes.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    pub parent: Vec<NodeIdx>,
    pub depth: Vec<u32>,
    pub name: Vec<OsString>,
    pub kind: Vec<Kind>,
    pub own_bytes: Vec<i64>,
    pub own_blocks: Vec<i64>,
    pub own_files: Vec<i64>,
    /// Database identity, filled in once the dictionary is reconciled.
    pub path_id: Vec<PathId>,
    /// Child lookup by name, used only while building.
    children: Vec<HashMap<OsString, NodeIdx>>,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Create the root node. Must be called exactly once, before any
    /// [`Tree::get_or_insert`].
    pub fn add_root(&mut self, name: OsString, kind: Kind) -> NodeIdx {
        debug_assert!(self.is_empty(), "root must be node 0");
        self.push(NO_PARENT, 0, name, kind)
    }

    /// Find or create `name` as a child of `parent`.
    pub fn get_or_insert(&mut self, parent: NodeIdx, name: &OsStr, kind: Kind) -> NodeIdx {
        if let Some(&existing) = self.children[parent as usize].get(name) {
            return existing;
        }
        let depth = self.depth[parent as usize] + 1;
        let idx = self.push(parent, depth, name.to_os_string(), kind);
        self.children[parent as usize].insert(name.to_os_string(), idx);
        idx
    }

    pub fn child(&self, parent: NodeIdx, name: &OsStr) -> Option<NodeIdx> {
        self.children[parent as usize].get(name).copied()
    }

    pub fn children_of(&self, parent: NodeIdx) -> impl Iterator<Item = NodeIdx> + '_ {
        self.children[parent as usize].values().copied()
    }

    fn push(&mut self, parent: NodeIdx, depth: u32, name: OsString, kind: Kind) -> NodeIdx {
        let idx = self.parent.len() as NodeIdx;
        self.parent.push(parent);
        self.depth.push(depth);
        self.name.push(name);
        self.kind.push(kind);
        self.own_bytes.push(0);
        self.own_blocks.push(0);
        self.own_files.push(0);
        self.path_id.push(0);
        self.children.push(HashMap::new());
        idx
    }

    /// Resolve a slash-free component list to a node, if it exists.
    pub fn resolve(&self, rel: &[OsString]) -> Option<NodeIdx> {
        let mut cur: NodeIdx = 0;
        for c in rel {
            cur = self.child(cur, c)?;
        }
        Some(cur)
    }

    /// Reconstruct a node's path components, root-first (excluding the root).
    pub fn rel_path(&self, mut n: NodeIdx) -> Vec<OsString> {
        let mut out = Vec::new();
        while self.parent[n as usize] != NO_PARENT {
            out.push(self.name[n as usize].clone());
            n = self.parent[n as usize];
        }
        out.reverse();
        out
    }

    /// Walk from `n` up to the root, yielding `n` first.
    pub fn ancestors(&self, n: NodeIdx) -> impl Iterator<Item = NodeIdx> + '_ {
        let mut cur = Some(n);
        std::iter::from_fn(move || {
            let this = cur?;
            let p = self.parent[this as usize];
            cur = if p == NO_PARENT { None } else { Some(p) };
            Some(this)
        })
    }

    /// Accumulate exclusive sizes into inclusive subtree totals.
    ///
    /// Processes nodes in descending depth order so each node's children are
    /// complete before it is folded into its own parent. O(n) with no
    /// recursion, so a pathological 29-deep tree can't blow the stack.
    pub fn rollup(&self) -> Rollup {
        let n = self.len();
        let mut r = Rollup {
            bytes: self.own_bytes.clone(),
            blocks: self.own_blocks.clone(),
            files: self.own_files.clone(),
            dirs: vec![0; n],
        };
        for i in 0..n {
            if self.kind[i] == Kind::Dir {
                r.dirs[i] = 1;
            }
            // A promoted file entity is a node in its own right, so it is not
            // counted in its parent's `own_files`; count it here instead.
            if self.kind[i] == Kind::File {
                r.files[i] += 1;
            }
        }

        // Bucket node indices by depth, then fold deepest-first.
        let max_depth = self.depth.iter().copied().max().unwrap_or(0);
        let mut by_depth: Vec<Vec<NodeIdx>> = vec![Vec::new(); max_depth as usize + 1];
        for (i, &d) in self.depth.iter().enumerate() {
            by_depth[d as usize].push(i as NodeIdx);
        }
        for d in (1..=max_depth).rev() {
            for &i in &by_depth[d as usize] {
                let p = self.parent[i as usize] as usize;
                let (i, p) = (i as usize, p);
                r.bytes[p] += r.bytes[i];
                r.blocks[p] += r.blocks[i];
                r.files[p] += r.files[i];
                r.dirs[p] += r.dirs[i];
            }
        }
        r
    }
}
