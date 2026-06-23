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

/// Thickness of the draggable divider between two panes, in points.
pub const DIVIDER: f32 = 6.0;

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
        self.nodes.iter().position(|e| {
            matches!(e.node, Node::Leaf { pane: p } if p == pane)
        })
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
