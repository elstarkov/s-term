//! Binary split-tree describing how panes tile the window.
//!
//! The tree is stored in an arena (`Vec<Entry>`) so nodes can reference each
//! other by index without fighting the borrow checker. A `Leaf` holds a pane,
//! a `Split` holds two children plus the axis they're arranged on and the ratio
//! of space given to the first child.
//!
//! Geometry (the on-screen rect of every pane and every divider) is produced by
//! a pure pass over the tree, decoupled from rendering so the app can read it
//! both for drawing and for keyboard navigation.

use egui::{pos2, vec2, Rect};

pub type PaneId = u64;

/// How a split arranges its two children.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Children sit side-by-side (left | right). The divider is vertical and
    /// drags along X. Produced by "split right" (Cmd+D).
    Horizontal,
    /// Children stack (top / bottom). The divider is horizontal and drags
    /// along Y. Produced by "split down" (Cmd+Shift+D).
    Vertical,
}

/// Cardinal direction for pane-to-pane navigation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

enum Node {
    Leaf { pane: PaneId },
    Split { axis: Axis, ratio: f32, a: usize, b: usize },
}

struct Entry {
    node: Node,
    parent: Option<usize>,
}

/// Thickness of the draggable divider gap between two panes, in points.
pub const DIVIDER: f32 = 8.0;

/// A divider handle produced by the geometry pass.
pub struct Divider {
    /// Arena index of the `Split` this divider belongs to.
    pub node: usize,
    pub axis: Axis,
    /// On-screen rect of the draggable handle.
    pub rect: Rect,
    /// Current split ratio of the owning node (first child's share).
    pub ratio: f32,
    /// Resizable extent along the split axis (total length minus divider).
    pub avail: f32,
}

pub struct Tree {
    nodes: Vec<Entry>,
    root: usize,
    free: Vec<usize>,
}

impl Tree {
    /// A fresh tree containing a single full-window pane.
    pub fn new(root_pane: PaneId) -> Self {
        Self {
            nodes: vec![Entry {
                node: Node::Leaf { pane: root_pane },
                parent: None,
            }],
            root: 0,
            free: Vec::new(),
        }
    }

    fn alloc(&mut self, node: Node, parent: Option<usize>) -> usize {
        if let Some(i) = self.free.pop() {
            self.nodes[i] = Entry { node, parent };
            i
        } else {
            self.nodes.push(Entry { node, parent });
            self.nodes.len() - 1
        }
    }

    fn find_leaf(&self, pane: PaneId) -> Option<usize> {
        self.find_leaf_from(self.root, pane)
    }

    /// Search only the nodes reachable from `idx`, so freed-but-not-yet-reused
    /// arena slots can never produce a false match.
    fn find_leaf_from(&self, idx: usize, pane: PaneId) -> Option<usize> {
        match self.nodes[idx].node {
            Node::Leaf { pane: p } => (p == pane).then_some(idx),
            Node::Split { a, b, .. } => self
                .find_leaf_from(a, pane)
                .or_else(|| self.find_leaf_from(b, pane)),
        }
    }

    /// Split the pane `target` into two, inserting `new_pane`. `new_after`
    /// places the new pane to the right/below (true) or left/above (false).
    pub fn split(&mut self, target: PaneId, new_pane: PaneId, axis: Axis, new_after: bool) {
        let Some(leaf) = self.find_leaf(target) else {
            return;
        };
        // The existing pane moves into a fresh leaf; the old slot becomes a Split.
        let kept = self.alloc(Node::Leaf { pane: target }, Some(leaf));
        let fresh = self.alloc(Node::Leaf { pane: new_pane }, Some(leaf));
        let (a, b) = if new_after { (kept, fresh) } else { (fresh, kept) };
        self.nodes[leaf].node = Node::Split { axis, ratio: 0.5, a, b };
    }

    /// Recursively copy the subtree rooted at `src_idx` in `src` into this
    /// arena, returning the new root index. Pane ids are globally unique so they
    /// carry over unchanged — only structural indices are remapped.
    fn import_from(&mut self, src: &Tree, src_idx: usize, parent: Option<usize>) -> usize {
        match src.nodes[src_idx].node {
            Node::Leaf { pane } => self.alloc(Node::Leaf { pane }, parent),
            Node::Split { axis, ratio, a, b } => {
                let new = self.alloc(Node::Split { axis, ratio, a: 0, b: 0 }, parent);
                let na = self.import_from(src, a, Some(new));
                let nb = self.import_from(src, b, Some(new));
                if let Node::Split { a, b, .. } = &mut self.nodes[new].node {
                    *a = na;
                    *b = nb;
                }
                new
            }
        }
    }

    /// Splice another tree's entire layout next to `target`, splitting along
    /// `axis`. `new_after` puts the imported panes to the right/below. Used to
    /// merge a dragged tab into this tab as new pane(s).
    pub fn attach_subtree(
        &mut self,
        target: PaneId,
        src: &Tree,
        axis: Axis,
        new_after: bool,
    ) -> bool {
        let Some(leaf) = self.find_leaf(target) else {
            return false;
        };
        let kept = self.alloc(Node::Leaf { pane: target }, Some(leaf));
        let imported = self.import_from(src, src.root, Some(leaf));
        let (a, b) = if new_after { (kept, imported) } else { (imported, kept) };
        self.nodes[leaf].node = Node::Split { axis, ratio: 0.5, a, b };
        true
    }

    /// Remove `pane`, collapsing its parent split into the surviving sibling.
    /// Returns `true` if a pane was removed; `false` if it was the last pane.
    pub fn close(&mut self, pane: PaneId) -> bool {
        let Some(leaf) = self.find_leaf(pane) else {
            return false;
        };
        let Some(parent) = self.nodes[leaf].parent else {
            // Closing the root leaf: nothing left to tile.
            return false;
        };
        let sibling = match self.nodes[parent].node {
            Node::Split { a, b, .. } => {
                if a == leaf {
                    b
                } else {
                    a
                }
            }
            Node::Leaf { .. } => unreachable!("parent is always a split"),
        };
        let grandparent = self.nodes[parent].parent;
        self.nodes[sibling].parent = grandparent;
        match grandparent {
            None => self.root = sibling,
            Some(gp) => {
                if let Node::Split { a, b, .. } = &mut self.nodes[gp].node {
                    if *a == parent {
                        *a = sibling;
                    } else if *b == parent {
                        *b = sibling;
                    }
                }
            }
        }
        self.free.push(leaf);
        self.free.push(parent);
        true
    }

    /// Update a split's ratio (clamped to keep both panes usable).
    pub fn set_ratio(&mut self, node: usize, ratio: f32) {
        if let Some(Node::Split { ratio: r, .. }) = self.nodes.get_mut(node).map(|e| &mut e.node) {
            *r = ratio.clamp(0.05, 0.95);
        }
    }

    /// Whether this tree (tab) currently holds the given pane.
    pub fn contains(&self, pane: PaneId) -> bool {
        self.find_leaf(pane).is_some()
    }

    /// The first pane in the tree — a safe fallback focus target.
    pub fn first_pane(&self) -> PaneId {
        self.first_leaf(self.root)
    }

    /// Panes in stable tree order (top/left child first) — the order used for
    /// indexed focus switching (Option+number).
    pub fn panes_in_order(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_panes(self.root, &mut out);
        out
    }

    fn collect_panes(&self, idx: usize, out: &mut Vec<PaneId>) {
        match self.nodes[idx].node {
            Node::Leaf { pane } => out.push(pane),
            Node::Split { a, b, .. } => {
                self.collect_panes(a, out);
                self.collect_panes(b, out);
            }
        }
    }

    /// First pane reachable from a node (depth-first, first child).
    fn first_leaf(&self, idx: usize) -> PaneId {
        match self.nodes[idx].node {
            Node::Leaf { pane } => pane,
            Node::Split { a, .. } => self.first_leaf(a),
        }
    }

    /// A sensible pane to focus after `pane` is closed: the sibling subtree's
    /// first leaf (mirrors how iTerm2 keeps focus local to the closed split).
    pub fn focus_after_close(&self, pane: PaneId) -> Option<PaneId> {
        let leaf = self.find_leaf(pane)?;
        let parent = self.nodes[leaf].parent?;
        let sibling = match self.nodes[parent].node {
            Node::Split { a, b, .. } => {
                if a == leaf {
                    b
                } else {
                    a
                }
            }
            Node::Leaf { .. } => return None,
        };
        Some(self.first_leaf(sibling))
    }

    /// Compute on-screen rects for every pane and divider within `area`.
    pub fn geometry(&self, area: Rect) -> (Vec<(PaneId, Rect)>, Vec<Divider>) {
        let mut leaves = Vec::new();
        let mut dividers = Vec::new();
        self.layout_into(self.root, area, &mut leaves, &mut dividers);
        (leaves, dividers)
    }

    fn layout_into(
        &self,
        idx: usize,
        rect: Rect,
        leaves: &mut Vec<(PaneId, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self.nodes[idx].node {
            Node::Leaf { pane } => leaves.push((pane, rect)),
            Node::Split { axis, ratio, a, b } => match axis {
                Axis::Horizontal => {
                    let avail = (rect.width() - DIVIDER).max(0.0);
                    let aw = avail * ratio;
                    let a_rect =
                        Rect::from_min_size(rect.min, vec2(aw, rect.height()));
                    let div_rect = Rect::from_min_size(
                        pos2(rect.min.x + aw, rect.min.y),
                        vec2(DIVIDER, rect.height()),
                    );
                    let b_rect = Rect::from_min_size(
                        pos2(rect.min.x + aw + DIVIDER, rect.min.y),
                        vec2(avail - aw, rect.height()),
                    );
                    dividers.push(Divider {
                        node: idx,
                        axis,
                        rect: div_rect,
                        ratio,
                        avail,
                    });
                    self.layout_into(a, a_rect, leaves, dividers);
                    self.layout_into(b, b_rect, leaves, dividers);
                }
                Axis::Vertical => {
                    let avail = (rect.height() - DIVIDER).max(0.0);
                    let ah = avail * ratio;
                    let a_rect =
                        Rect::from_min_size(rect.min, vec2(rect.width(), ah));
                    let div_rect = Rect::from_min_size(
                        pos2(rect.min.x, rect.min.y + ah),
                        vec2(rect.width(), DIVIDER),
                    );
                    let b_rect = Rect::from_min_size(
                        pos2(rect.min.x, rect.min.y + ah + DIVIDER),
                        vec2(rect.width(), avail - ah),
                    );
                    dividers.push(Divider {
                        node: idx,
                        axis,
                        rect: div_rect,
                        ratio,
                        avail,
                    });
                    self.layout_into(a, a_rect, leaves, dividers);
                    self.layout_into(b, b_rect, leaves, dividers);
                }
            },
        }
    }
}

/// Pick the best pane to move to from `current` in direction `dir`, given the
/// current geometry. Chooses the nearest pane that lies in `dir` and overlaps
/// on the perpendicular axis (so navigation feels spatial, like iTerm2/Ghostty).
pub fn neighbor(
    leaves: &[(PaneId, Rect)],
    current: PaneId,
    dir: Dir,
) -> Option<PaneId> {
    let cur = leaves.iter().find(|(p, _)| *p == current)?.1;
    let cc = cur.center();
    let mut best: Option<PaneId> = None;
    let mut best_dist = f32::MAX;
    for (pane, r) in leaves {
        if *pane == current {
            continue;
        }
        let c = r.center();
        let in_dir = match dir {
            Dir::Left => c.x < cc.x - 1.0,
            Dir::Right => c.x > cc.x + 1.0,
            Dir::Up => c.y < cc.y - 1.0,
            Dir::Down => c.y > cc.y + 1.0,
        };
        let overlaps = match dir {
            Dir::Left | Dir::Right => r.top() < cur.bottom() && r.bottom() > cur.top(),
            Dir::Up | Dir::Down => r.left() < cur.right() && r.right() > cur.left(),
        };
        if in_dir && overlaps {
            let d = (c - cc).length();
            if d < best_dist {
                best_dist = d;
                best = Some(*pane);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))
    }

    #[test]
    fn attach_merges_two_single_pane_tabs() {
        let mut a = Tree::new(1); // tab A: pane 1
        let b = Tree::new(2); // tab B: pane 2
        assert!(a.contains(1) && !a.contains(2));

        // Drop B onto pane 1, landing to the right.
        assert!(a.attach_subtree(1, &b, Axis::Horizontal, true));
        assert!(a.contains(1) && a.contains(2));

        let (leaves, dividers) = a.geometry(area());
        assert_eq!(leaves.len(), 2, "both panes present");
        assert_eq!(dividers.len(), 1, "one divider between them");
        let p1 = leaves.iter().find(|(p, _)| *p == 1).unwrap().1;
        let p2 = leaves.iter().find(|(p, _)| *p == 2).unwrap().1;
        assert!(p1.left() < p2.left(), "pane 1 is left of pane 2");
    }

    #[test]
    fn attach_imports_a_whole_multi_pane_subtree() {
        let mut a = Tree::new(1);
        let mut b = Tree::new(2);
        b.split(2, 3, Axis::Vertical, true); // tab B holds panes 2 (top) and 3 (bottom)

        assert!(a.attach_subtree(1, &b, Axis::Horizontal, false)); // land on the left
        assert!(a.contains(1) && a.contains(2) && a.contains(3));

        let (leaves, _) = a.geometry(area());
        assert_eq!(leaves.len(), 3, "all three panes tile the merged tab");
    }

    #[test]
    fn panes_in_order_is_left_to_right() {
        let mut a = Tree::new(1);
        a.split(1, 2, Axis::Horizontal, true); // [1 | 2]
        a.split(2, 3, Axis::Horizontal, true); // [1 | [2 | 3]]
        assert_eq!(a.panes_in_order(), vec![1, 2, 3]);
    }

    #[test]
    fn close_collapses_back_to_a_single_pane() {
        let mut a = Tree::new(1);
        let b = Tree::new(2);
        a.attach_subtree(1, &b, Axis::Horizontal, true);
        assert!(a.close(2));
        assert!(a.contains(1) && !a.contains(2));
        assert_eq!(a.geometry(area()).0.len(), 1);
    }
}
