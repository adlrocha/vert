//! Lays a [`Diagram`] out as a tree of arrows, which reads well in a terminal
//! at any width. A chain of single arrows runs straight down, so a flow reads
//! top to bottom; only a node with several arrows out branches. Each node is
//! drawn once; an arrow back to a node already drawn (a cycle, or a second
//! way in) ends in a repeat marker instead.

use super::Diagram;

/// One row of the laid-out diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Tree drawing before the node (`├─▶ `, `│   └─▶ `, …), or only the
    /// indent for a chained node.
    pub prefix: String,
    /// The node on this row.
    pub node: usize,
    /// The arrow that led here, if any.
    pub edge: Option<usize>,
    /// Whether the node was already drawn above.
    pub repeat: bool,
    /// Whether the arrow that led here was the only one out of the node
    /// above; it is then drawn as a `↓` line between the two, at `prefix`.
    pub chained: bool,
}

/// Lays `diagram` out from its roots (nodes that point somewhere and that
/// nothing points at), then from every node still left in the agent's order,
/// which covers cycles and nodes with no arrows at all.
///
/// ```
/// use vert::explain::{Change, Diagram, Edge, Node};
/// let node = |label: &str| Node { label: label.into(), change: Change::Same, refs: vec![] };
/// let edge = |from, to| Edge { from, to, label: String::new(), change: Change::Same };
/// let d = Diagram {
///     kind: "states".into(),
///     title: String::new(),
///     nodes: vec![node("idle"), node("running")],
///     edges: vec![edge(0, 1), edge(1, 0)],
/// };
/// let rows = vert::explain::tree::layout(&d);
/// let drawn: Vec<_> = rows.iter().map(|r| (r.node, r.repeat, r.chained)).collect();
/// // idle, ↓ running, ↓ idle again.
/// assert_eq!(drawn, [(0, false, false), (1, false, true), (0, true, true)]);
/// ```
#[must_use]
pub fn layout(diagram: &Diagram) -> Vec<Row> {
    let n = diagram.nodes.len();
    let mut pointed_at = vec![false; n];
    let mut points = vec![false; n];
    for e in diagram.edges.iter().filter(|e| e.from != e.to) {
        pointed_at[e.to] = true;
        points[e.from] = true;
    }
    let mut walk = Walk {
        diagram,
        seen: vec![false; n],
        rows: Vec::new(),
    };
    let roots = (0..n).filter(|&i| points[i] && !pointed_at[i]).chain(0..n);
    for root in roots {
        if !walk.seen[root] {
            walk.visit(root, None, String::new(), "", false);
        }
    }
    walk.rows
}

struct Walk<'a> {
    diagram: &'a Diagram,
    seen: Vec<bool>,
    rows: Vec<Row>,
}

impl Walk<'_> {
    /// Draws `node` after `prefix`; `indent` is the drawing that reaches the
    /// column where the node's own text starts.
    fn visit(
        &mut self,
        node: usize,
        edge: Option<usize>,
        prefix: String,
        indent: &str,
        chained: bool,
    ) {
        let repeat = self.seen[node];
        self.rows.push(Row {
            prefix,
            node,
            edge,
            repeat,
            chained,
        });
        if repeat {
            return;
        }
        self.seen[node] = true;
        let out: Vec<usize> = (0..self.diagram.edges.len())
            .filter(|&i| self.diagram.edges[i].from == node)
            .collect();
        if let [only] = out[..] {
            let to = self.diagram.edges[only].to;
            self.visit(to, Some(only), indent.to_owned(), indent, true);
            return;
        }
        for (k, &e) in out.iter().enumerate() {
            let last = k + 1 == out.len();
            let here = format!("{indent}{}", if last { "└─▶ " } else { "├─▶ " });
            let below = format!("{indent}{}", if last { "    " } else { "│   " });
            self.visit(self.diagram.edges[e].to, Some(e), here, &below, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explain::{Change, Edge, Node};

    fn diagram(nodes: usize, edges: &[(usize, usize)]) -> Diagram {
        Diagram {
            kind: "components".to_owned(),
            title: String::new(),
            nodes: (0..nodes)
                .map(|i| Node {
                    label: format!("n{i}"),
                    change: Change::Same,
                    refs: Vec::new(),
                })
                .collect(),
            edges: edges
                .iter()
                .map(|&(from, to)| Edge {
                    from,
                    to,
                    label: String::new(),
                    change: Change::Same,
                })
                .collect(),
        }
    }

    fn drawn(d: &Diagram) -> Vec<String> {
        layout(d)
            .iter()
            .flat_map(|r| {
                let link = r.chained.then(|| format!("{}↓", r.prefix));
                let repeat = if r.repeat { " ↺" } else { "" };
                link.into_iter()
                    .chain([format!("{}n{}{repeat}", r.prefix, r.node)])
            })
            .collect()
    }

    #[test]
    fn draws_branches_and_shared_children_once() {
        // n0 → n1 → n3, n0 → n2 → n3, plus a separate n4.
        let d = diagram(5, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        assert_eq!(
            drawn(&d),
            [
                "n0",
                "├─▶ n1",
                "│   ↓",
                "│   n3",
                "└─▶ n2",
                "    ↓",
                "    n3 ↺",
                "n4"
            ]
        );
        let rows = layout(&d);
        assert_eq!(rows[1].edge, Some(0));
        assert_eq!(rows[0].edge, None);
    }

    #[test]
    fn roots_first_then_the_agents_order() {
        // n0 stands alone; n1 and n2 form a cycle.
        let d = diagram(3, &[(1, 2), (2, 1)]);
        assert_eq!(drawn(&d), ["n0", "n1", "↓", "n2", "↓", "n1 ↺"]);
        let d = diagram(3, &[(1, 2), (2, 1), (0, 0)]);
        assert_eq!(drawn(&d)[0], "n0", "agent order otherwise");
        // A real root still goes first.
        let d = diagram(3, &[(1, 2), (2, 0)]);
        assert_eq!(drawn(&d), ["n1", "↓", "n2", "↓", "n0"]);
    }

    #[test]
    fn cycles_and_self_loops_terminate() {
        let ring = diagram(3, &[(0, 1), (1, 2), (2, 0), (1, 1)]);
        assert_eq!(
            drawn(&ring),
            ["n0", "↓", "n1", "├─▶ n2", "│   ↓", "│   n0 ↺", "└─▶ n1 ↺"]
        );
        assert_eq!(layout(&diagram(0, &[])), []);
    }
}
