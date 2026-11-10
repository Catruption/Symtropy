//! Tag hierarchy.
//!
//! Parses a tab-indented tag tree (as exported from DigiKam or hand-written)
//! into a flat lookup structure. Provides the operations the query engine,
//! sidecar rollup, and autocomplete need:
//!
//! - `contains(tag)` — does this tag exist?
//! - `is_descendant_of(child, parent)` — is `child` at or below `parent`?
//! - `ancestors(tag)` — root → immediate parent chain.
//! - `path(tag)` — full path including the tag itself.
//! - `all_tags()` — sorted flat list, for autocomplete.
//! - `roots()` — top-level categories.
//!
//! The hierarchy is immutable after `parse`. All lookups are O(depth),
//! where depth is at most ~6 in any realistic taxonomy.

use std::collections::HashMap;

/// A single tag in the tree.
#[derive(Debug)]
pub struct TagNode {
    pub name: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
}

/// The parsed hierarchy.
#[derive(Debug, Default)]
pub struct Hierarchy {
    nodes: HashMap<String, TagNode>,
    roots: Vec<String>,
    /// root → immediate parent, for each tag. Empty for roots.
    ancestors_of: HashMap<String, Vec<String>>,
}

impl Hierarchy {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn contains(&self, tag: &str) -> bool {
        self.nodes.contains_key(tag)
    }

    pub fn node(&self, tag: &str) -> Option<&TagNode> {
        self.nodes.get(tag)
    }

    /// Root → immediate parent. Empty slice for root tags.
    pub fn ancestors(&self, tag: &str) -> &[String] {
        self.ancestors_of.get(tag).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Is `child` at or below `parent`? A tag counts as its own descendant,
    /// so `is_descendant_of("fox", "fox")` is true.
    pub fn is_descendant_of(&self, child: &str, parent: &str) -> bool {
        if child == parent {
            return true;
        }
        self.ancestors_of
            .get(child)
            .map_or(false, |anc| anc.iter().any(|a| a == parent))
    }

    /// Full path including the tag itself. Empty if the tag is unknown.
    pub fn path(&self, tag: &str) -> Vec<&str> {
        let mut out: Vec<&str> = self.ancestors(tag).iter().map(|s| s.as_str()).collect();
        if let Some(n) = self.nodes.get(tag) {
            out.push(n.name.as_str());
        }
        out
    }

    /// Depth (0 for roots, 1 for direct children, etc.).
    pub fn depth(&self, tag: &str) -> usize {
        self.ancestors_of.get(tag).map_or(0, |v| v.len())
    }

    /// Top-level categories, in source order.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Every tag name, sorted lexicographically. Used for autocomplete.
    pub fn all_tags(&self) -> Vec<&str> {
        let mut tags: Vec<&str> = self.nodes.keys().map(|s| s.as_str()).collect();
        tags.sort();
        tags
    }

    /// Parse a tab-indented tag tree.
    ///
    /// Each leading tab (or each 4-space indent) is one depth level. Empty
    /// lines and lines starting with `#` are skipped, so the format is
    /// commentable. Duplicate tag names: the first wins, the second is
    /// logged and skipped — the tree is a tree, not a graph.
    pub fn parse(input: &str) -> Self {
        let mut nodes: HashMap<String, TagNode> = HashMap::new();
        let mut ancestors_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut roots: Vec<String> = Vec::new();
        let mut stack: Vec<(usize, String)> = Vec::new();

        for raw in input.lines() {
            let depth = leading_depth(raw);
            let name = raw.trim();
            if name.is_empty() || name.starts_with('#') {
                continue;
            }

            // Pop until we find a parent shallower than us.
            while let Some((last_depth, _)) = stack.last() {
                if *last_depth >= depth {
                    stack.pop();
                } else {
                    break;
                }
            }

            let parent = stack.last().map(|(_, n)| n.clone());

            if nodes.contains_key(name) {
                eprintln!(
                    "hierarchy: duplicate tag '{}' at depth {}, keeping first occurrence",
                    name, depth
                );
                continue;
            }

            let ancestors: Vec<String> = stack.iter().map(|(_, n)| n.clone()).collect();

            if parent.is_none() && depth > 0 {
                eprintln!(
                    "hierarchy: tag '{}' at depth {} has no parent, treating as root",
                    name, depth
                );
            }

            nodes.insert(
                name.to_string(),
                TagNode {
                    name: name.to_string(),
                    parent: parent.clone(),
                    children: Vec::new(),
                },
            );
            ancestors_of.insert(name.to_string(), ancestors);

            if let Some(p) = &parent {
                if let Some(p_node) = nodes.get_mut(p) {
                    p_node.children.push(name.to_string());
                }
            } else {
                roots.push(name.to_string());
            }

            stack.push((depth, name.to_string()));
        }

        Self { nodes, roots, ancestors_of }
    }
}

fn leading_depth(line: &str) -> usize {
    let mut depth = 0;
    let mut spaces = 0;
    for c in line.chars() {
        match c {
            '\t' => { depth += 1; spaces = 0; }
            ' ' => { spaces += 1; if spaces == 4 { depth += 1; spaces = 0; } }
            _ => break,
        }
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
animal
\tbat
\tfox
\t\tfennec
\t\tkitsune
\tfish
\t\taxolotl
artist
\taccelldraws
\t6unnnya
";

    #[test]
    fn parses_all() {
        let h = Hierarchy::parse(SAMPLE);
        assert_eq!(h.len(), 10);
        for t in ["animal", "bat", "fox", "fennec", "kitsune", "fish", "axolotl", "artist", "accelldraws", "6unnnya"] {
            assert!(h.contains(t), "missing {}", t);
        }
    }

    #[test]
    fn roots_are_top_level() {
        let h = Hierarchy::parse(SAMPLE);
        assert_eq!(h.roots(), &["animal".to_string(), "artist".to_string()]);
    }

    #[test]
    fn ancestors_are_ordered() {
        let h = Hierarchy::parse(SAMPLE);
        assert_eq!(h.ancestors("fennec"), &["animal".to_string(), "fox".to_string()]);
        assert_eq!(h.ancestors("animal").len(), 0);
        assert_eq!(h.depth("fennec"), 2);
        assert_eq!(h.depth("animal"), 0);
    }

    #[test]
    fn path_includes_self() {
        let h = Hierarchy::parse(SAMPLE);
        assert_eq!(h.path("fennec"), vec!["animal", "fox", "fennec"]);
        assert_eq!(h.path("animal"), vec!["animal"]);
        assert!(h.path("nonexistent").is_empty());
    }

    #[test]
    fn descendant_checks() {
        let h = Hierarchy::parse(SAMPLE);
        assert!(h.is_descendant_of("fennec", "fox"));
        assert!(h.is_descendant_of("fennec", "animal"));
        assert!(h.is_descendant_of("fox", "fox"));
        assert!(!h.is_descendant_of("fox", "fennec"));
        assert!(!h.is_descendant_of("bat", "fox"));
        assert!(!h.is_descendant_of("nope", "animal"));
        assert!(!h.is_descendant_of("fox", "nope"));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let h = Hierarchy::parse("# top\nanimal\n\n\tbat\n# comment\n\tfox\n");
        assert_eq!(h.len(), 3);
        assert!(h.contains("bat"));
        assert!(h.contains("fox"));
    }

    #[test]
    fn duplicate_keeps_first() {
        let h = Hierarchy::parse("animal\n\tbat\nbird\n\tbat\n");
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn space_indented_works() {
        let h = Hierarchy::parse("animal\n    bat\n    fox\n        fennec\n");
        assert_eq!(h.len(), 4);
        assert_eq!(h.ancestors("fennec"), &["animal".to_string(), "fox".to_string()]);
    }

    #[test]
    fn all_tags_is_sorted() {
        let h = Hierarchy::parse(SAMPLE);
        let tags = h.all_tags();
        let mut expected = tags.clone();
        expected.sort();
        assert_eq!(tags, expected);
    }
}