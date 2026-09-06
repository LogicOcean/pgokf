// SPDX-License-Identifier: AGPL-3.0-only
//! A dependency-free radial layout of a concept's neighborhood as inline SVG.
//!
//! `concept_neighbors` returns each reachable concept with its hop distance
//! and the path taken; that is enough for a readable picture: the seed at the
//! center, one ring per hop, and an edge from each neighbor to the previous
//! step of its path. Pure functions over plain data, so the geometry is
//! unit-tested without a browser.

use std::collections::HashMap;
use std::f64::consts::TAU;
use std::fmt::Write as _;

/// One node to draw.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GraphNode {
    pub id: String,
    pub label: String,
    pub hops: i32,
    pub href: String,
}

/// A directed edge between two node ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphEdge {
    pub from: String,
    pub to: String,
}

/// A laid-out node: position in a fixed viewbox.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placed {
    pub node: GraphNode,
    pub x: f64,
    pub y: f64,
}

const SIZE: f64 = 640.0;
const RING: f64 = 110.0;
const MAX_LABEL: usize = 22;

/// Place the seed at the center and each neighbor on the ring of its hop
/// count, spread evenly around the ring in input order.
pub(crate) fn layout(seed_label: &str, seed_href: &str, neighbors: &[GraphNode]) -> Vec<Placed> {
    let center = SIZE / 2.0;
    let mut placed = vec![Placed {
        node: GraphNode {
            id: String::new(),
            label: seed_label.to_owned(),
            hops: 0,
            href: seed_href.to_owned(),
        },
        x: center,
        y: center,
    }];
    let mut per_ring: HashMap<i32, usize> = HashMap::new();
    for n in neighbors {
        *per_ring.entry(n.hops).or_default() += 1;
    }
    let mut seen: HashMap<i32, usize> = HashMap::new();
    for n in neighbors {
        let total = per_ring.get(&n.hops).copied().unwrap_or(1).max(1);
        let index = seen.entry(n.hops).or_default();
        #[allow(clippy::cast_precision_loss)]
        let angle = TAU * (*index as f64) / (total as f64) - TAU / 4.0;
        *index += 1;
        let radius = RING * f64::from(n.hops.max(1));
        placed.push(Placed {
            node: n.clone(),
            x: center + radius * angle.cos(),
            y: center + radius * angle.sin(),
        });
    }
    placed
}

/// Render placed nodes and edges as an SVG document fragment. Labels are
/// escaped; hrefs are attribute-escaped.
pub(crate) fn svg(placed: &[Placed], edges: &[GraphEdge]) -> String {
    let index: HashMap<&str, &Placed> = placed.iter().map(|p| (p.node.id.as_str(), p)).collect();
    let mut out = String::new();
    let _ = write!(
        out,
        "<svg class=\"graph\" viewBox=\"0 0 {SIZE} {SIZE}\" role=\"img\" aria-label=\"Link graph\">"
    );
    for e in edges {
        if let (Some(a), Some(b)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) {
            let _ = write!(
                out,
                "<line class=\"edge\" x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\"/>",
                a.x, a.y, b.x, b.y
            );
        }
    }
    for p in placed {
        let class = if p.node.hops == 0 {
            "node seed"
        } else {
            "node"
        };
        let label = truncate(&p.node.label, MAX_LABEL);
        let _ = write!(
            out,
            "<a href=\"{}\"><g class=\"{class}\" transform=\"translate({:.1},{:.1})\"><circle r=\"{}\"/><text dy=\"{}\">{}</text></g></a>",
            escape_attr(&p.node.href),
            p.x,
            p.y,
            if p.node.hops == 0 { 14 } else { 9 },
            if p.node.hops == 0 { 30 } else { 22 },
            escape_text(&label)
        );
    }
    out.push_str("</svg>");
    out
}

fn truncate(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        return label.to_owned();
    }
    let mut cut: String = label.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

fn escape_text(value: &str) -> String {
    crate::markdown::escape(value)
}

fn escape_attr(value: &str) -> String {
    crate::markdown::escape(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, hops: i32) -> GraphNode {
        GraphNode {
            id: id.to_owned(),
            label: id.to_owned(),
            hops,
            href: format!("/c/{id}"),
        }
    }

    #[test]
    fn layout_puts_the_seed_at_the_center_and_neighbors_on_hop_rings() {
        // Arrange
        let neighbors = vec![node("a", 1), node("b", 1), node("c", 2)];

        // Act
        let placed = layout("seed", "/seed", &neighbors);

        // Assert
        let center = SIZE / 2.0;
        assert!((placed[0].x - center).abs() < 1e-9 && (placed[0].y - center).abs() < 1e-9);
        let r1 = ((placed[1].x - center).powi(2) + (placed[1].y - center).powi(2)).sqrt();
        let r3 = ((placed[3].x - center).powi(2) + (placed[3].y - center).powi(2)).sqrt();
        assert!((r1 - RING).abs() < 1e-6);
        assert!((r3 - 2.0 * RING).abs() < 1e-6);
        // Two neighbors on the first ring sit on opposite sides.
        assert!((placed[1].x + placed[2].x - 2.0 * center).abs() < 1e-6);
    }

    #[test]
    fn svg_escapes_labels_and_hrefs() {
        // Arrange
        let placed = layout("<seed>", "/s?a=1&b=\"2\"", &[node("x", 1)]);

        // Act
        let out = svg(
            &placed,
            &[GraphEdge {
                from: String::new(),
                to: "x".into(),
            }],
        );

        // Assert
        assert!(out.contains("&lt;seed&gt;"));
        assert!(out.contains("href=\"/s?a=1&amp;b=&quot;2&quot;\""));
        assert!(out.contains("<line class=\"edge\""));
    }

    #[test]
    fn truncate_adds_an_ellipsis_past_the_limit() {
        // Arrange & Act & Assert
        assert_eq!(truncate("short", 22), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd\u{2026}");
    }
}
