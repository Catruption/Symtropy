//! Tag hierarchy.
//!
//! Parses a tab-indented tag tree (as exported from DigiKam or hand-written)
//! into a flat lookup structure. Provides the operations the query engine and
//! sidebar tree need:
//!
//! - `is_descendant_of(child, parent)` — is `child` at or below `parent`?
//! - `roots()` — top-level categories.
//!
//! The hierarchy is immutable after `parse`. All lookups are O(depth),
//! where depth is at most ~6 in any realistic taxonomy.

use std::collections::HashMap;

/// Query tokens cannot contain spaces or operator characters, but taxonomy tag
/// names can. Both sides use this key so `derpy_hooves` matches `derpy hooves`.
pub fn tag_key(tag: &str) -> String {
    tag.trim()
        .chars()
        .map(|c| if c.is_whitespace() || "()+!|".contains(c) { '_' } else { c })
        .collect()
}

/// A single tag in the tree.
#[derive(Clone, Debug)]
pub struct TagNode {
    pub children: Vec<String>,
}

/// The parsed hierarchy.
#[derive(Debug, Default)]
pub struct Hierarchy {
    nodes: HashMap<String, TagNode>,
    roots: Vec<String>,
    /// Comment lines preserved from the tag file when it is rewritten.
    comments: Vec<String>,
    /// Ancestors keyed by normalized tag names so `derpy_hooves` finds the
    /// taxonomy entry written as `derpy hooves`. Roots have an empty list.
    ancestors_by_key: HashMap<String, Vec<String>>,
}

impl Hierarchy {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn node(&self, tag: &str) -> Option<&TagNode> {
        self.nodes.get(tag)
    }

    /// Is `child` at or below `parent`? A tag counts as its own descendant,
    /// so `is_descendant_of("fox", "fox")` is true.
    pub fn is_descendant_of(&self, child: &str, parent: &str) -> bool {
        let child = tag_key(child);
        let parent = tag_key(parent);
        if child == parent {
            return true;
        }
        self.ancestors_by_key
            .get(&child)
            .map_or(false, |anc| anc.iter().any(|a| a == &parent))
    }

    /// Top-level categories, in source order.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// The raw parent name of a tag, if it has one.
    pub fn parent_of(&self, tag: &str) -> Option<String> {
        if self.roots.iter().any(|root| root == tag) {
            return None;
        }
        self.nodes.iter().find_map(|(parent, node)| {
            node.children.iter().any(|child| child == tag).then(|| parent.clone())
        })
    }

    /// The ancestor chain of a tag, root first. Does not include the tag.
    pub fn ancestors_of(&self, tag: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut current = self.parent_of(tag);
        while let Some(parent) = current {
            chain.push(parent.clone());
            current = self.parent_of(&parent);
        }
        chain.reverse();
        chain
    }

    /// Every tag at or below `tag`, including `tag` itself.
    pub fn descendants_of(&self, tag: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .nodes
            .keys()
            .filter(|candidate| self.is_descendant_of(candidate, tag))
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// Add a tag under `parent`, or at the top level when `parent` is `None`.
    pub fn insert(&mut self, name: &str, parent: Option<&str>) -> Result<(), String> {
        let name = valid_name(name)?;
        let parent = match parent {
            Some(p) if !p.trim().is_empty() => Some(valid_name(p)?),
            _ => None,
        };
        if self.nodes.contains_key(&name)
            || self.nodes.keys().any(|existing| tag_key(existing) == tag_key(&name))
        {
            return Err(format!("tag '{}' already exists", name));
        }
        if let Some(p) = &parent {
            if !self.nodes.contains_key(p) {
                return Err(format!("parent tag '{}' does not exist", p));
            }
        }

        self.nodes.insert(name.clone(), TagNode { children: Vec::new() });
        if let Some(p) = &parent {
            self.nodes.get_mut(p).unwrap().children.push(name);
        } else {
            self.roots.push(name);
        }
        self.reindex();
        Ok(())
    }

    /// Rename a tag everywhere it appears, preserving its position.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        let from = valid_name(from)?;
        let to = valid_name(to)?;
        if !self.nodes.contains_key(&from) {
            return Err(format!("tag '{}' does not exist", from));
        }
        if from == to {
            return Ok(());
        }
        if self.nodes.contains_key(&to)
            || self.nodes.keys().any(|existing| existing != &from && tag_key(existing) == tag_key(&to))
        {
            return Err(format!("tag '{}' already exists", to));
        }

        let old_nodes = std::mem::take(&mut self.nodes);
        let mut nodes = HashMap::with_capacity(old_nodes.len());
        for (name, node) in old_nodes {
            let new_name = if name == from { to.clone() } else { name };
            let children = node
                .children
                .into_iter()
                .map(|child| if child == from { to.clone() } else { child })
                .collect();
            nodes.insert(new_name, TagNode { children });
        }
        self.nodes = nodes;
        for name in &mut self.roots {
            if *name == from {
                *name = to.clone();
            }
        }
        for comment in &mut self.comments {
            if comment.contains(&from) {
                *comment = comment.replace(&from, &to);
            }
        }
        self.reindex();
        Ok(())
    }

    /// Reparent a tag. Moving under itself or one of its descendants is rejected.
    pub fn move_tag(&mut self, tag: &str, new_parent: Option<&str>) -> Result<(), String> {
        let tag = valid_name(tag)?;
        if !self.nodes.contains_key(&tag) {
            return Err(format!("tag '{}' does not exist", tag));
        }
        let parent = match new_parent {
            Some(p) if !p.trim().is_empty() => Some(valid_name(p)?),
            _ => None,
        };
        if let Some(p) = &parent {
            if !self.nodes.contains_key(p) {
                return Err(format!("parent tag '{}' does not exist", p));
            }
            if p == &tag || self.is_descendant_of(p, &tag) {
                return Err("cannot move a tag under itself or one of its descendants".into());
            }
        }

        let old_parent = self.parent_of(&tag);
        if old_parent == parent {
            return Ok(());
        }
        self.detach(&tag, old_parent.as_deref());
        if let Some(p) = &parent {
            self.nodes.get_mut(p).unwrap().children.push(tag);
        } else {
            self.roots.push(tag);
        }
        self.reindex();
        Ok(())
    }

    /// Replace `source` with `target`, moving its children to the target.
    pub fn merge(&mut self, source: &str, target: &str) -> Result<(), String> {
        let source = valid_name(source)?;
        let target = valid_name(target)?;
        if source == target {
            return Err("a tag cannot be merged into itself".into());
        }
        if !self.nodes.contains_key(&source) || !self.nodes.contains_key(&target) {
            return Err("both tags must exist before merging".into());
        }
        if self.is_descendant_of(&target, &source) {
            return Err("cannot merge a tag into one of its own descendants".into());
        }

        let parent = self.parent_of(&source);
        self.detach(&source, parent.as_deref());
        let children = self.nodes.remove(&source).unwrap().children;
        let target_node = self.nodes.get_mut(&target).unwrap();
        for child in children {
            if child != target && !target_node.children.contains(&child) {
                target_node.children.push(child);
            }
        }
        self.reindex();
        Ok(())
    }

    /// Delete a tag and promote its children to its own level.
    pub fn delete(&mut self, tag: &str) -> Result<(), String> {
        let tag = valid_name(tag)?;
        if !self.nodes.contains_key(&tag) {
            return Err(format!("tag '{}' does not exist", tag));
        }
        let parent = self.parent_of(&tag).filter(|p| p != &tag);
        self.detach(&tag, parent.as_deref());
        let children = self.nodes.remove(&tag).unwrap().children;
        for child in children {
            if child == tag {
                continue;
            }
            if let Some(p) = &parent {
                let node = self.nodes.get_mut(p).unwrap();
                if !node.children.contains(&child) {
                    node.children.push(child);
                }
            } else if !self.roots.contains(&child) {
                self.roots.push(child);
            }
        }
        self.reindex();
        Ok(())
    }

    /// Serialize the current taxonomy back to the simple indented format.
    pub fn to_text(&self) -> String {
        fn walk(nodes: &HashMap<String, TagNode>, name: &str, depth: usize, out: &mut String) {
            out.push_str(&"  ".repeat(depth));
            out.push_str(name);
            out.push('\n');
            if let Some(node) = nodes.get(name) {
                for child in &node.children {
                    walk(nodes, child, depth + 1, out);
                }
            }
        }

        let mut out = String::new();
        for comment in &self.comments {
            out.push_str(comment);
            out.push('\n');
        }
        if !self.comments.is_empty() {
            out.push('\n');
        }
        for root in &self.roots {
            walk(&self.nodes, root, 0, &mut out);
        }
        out
    }

    fn detach(&mut self, tag: &str, parent: Option<&str>) {
        if let Some(p) = parent {
            if let Some(node) = self.nodes.get_mut(p) {
                node.children.retain(|child| child != tag);
            }
        } else {
            self.roots.retain(|root| root != tag);
        }
    }

    fn reindex(&mut self) {
        fn walk(
            nodes: &HashMap<String, TagNode>,
            name: &str,
            ancestors: &[String],
            by_key: &mut HashMap<String, Vec<String>>,
        ) {
            by_key.insert(tag_key(name), ancestors.to_vec());
            if let Some(node) = nodes.get(name) {
                let mut next = ancestors.to_vec();
                next.push(tag_key(name));
                for child in &node.children {
                    walk(nodes, child, &next, by_key);
                }
            }
        }

        let nodes = self.nodes.clone();
        let roots = self.roots.clone();
        let mut by_key = HashMap::with_capacity(nodes.len());
        for root in &roots {
            walk(&nodes, root, &[], &mut by_key);
        }
        self.ancestors_by_key = by_key;
    }

    /// Parse an indented tag tree.
    ///
    /// One tab or one indentation unit (detected from the file) is one depth
    /// level. Empty
    /// lines and lines starting with `#` are skipped, so the format is
    /// commentable. Duplicate tag names: the first wins, the second is
    /// logged and skipped — the tree is a tree, not a graph.
    pub fn parse(input: &str) -> Self {
        let mut nodes: HashMap<String, TagNode> = HashMap::new();
        let mut ancestors_by_key: HashMap<String, Vec<String>> = HashMap::new();
        let mut roots: Vec<String> = Vec::new();
        let mut comments: Vec<String> = Vec::new();
        let mut stack: Vec<(usize, String)> = Vec::new();

        let indent = indentation_width(input);
        for raw in input.lines() {
            let depth = leading_depth(raw, indent);
            let name = raw.trim();
            if name.starts_with('#') {
                comments.push(name.to_string());
                continue;
            }
            if name.is_empty() {
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

            let key = tag_key(name);
            let keyed_ancestors = ancestors.iter().map(|a| tag_key(a)).collect();
            nodes.insert(name.to_string(), TagNode { children: Vec::new() });
            ancestors_by_key.insert(key, keyed_ancestors);

            if let Some(p) = &parent {
                if let Some(p_node) = nodes.get_mut(p) {
                    p_node.children.push(name.to_string());
                }
            } else {
                roots.push(name.to_string());
            }

            stack.push((depth, name.to_string()));
        }

        Self { nodes, roots, comments, ancestors_by_key }
    }
}

fn valid_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("tag name cannot be empty".into());
    }
    if name.chars().any(|c| matches!(c, '\t' | '\n' | '\r' | '|')) {
        return Err("tag names cannot contain tabs, newlines, or '|'".into());
    }
    Ok(name.to_string())
}

/// The tag file can indent with tabs, two spaces, or four spaces. Use the
/// smallest positive indentation as one level so real taxonomies parse.
fn indentation_width(input: &str) -> usize {
    input
        .lines()
        .filter_map(|line| {
            let spaces = line.chars().take_while(|c| *c == ' ').count();
            (spaces > 0).then_some(spaces)
        })
        .min()
        .unwrap_or(4)
        .max(1)
}

fn leading_depth(line: &str, indent: usize) -> usize {
    let mut depth = 0;
    let mut spaces = 0;
    for c in line.chars() {
        match c {
            '\t' => { depth += 1; spaces = 0; }
            ' ' => { spaces += 1; if spaces == indent { depth += 1; spaces = 0; } }
            _ => break,
        }
    }
    depth + spaces / indent
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
            assert!(h.node(t).is_some(), "missing {}", t);
        }
    }

    #[test]
    fn roots_are_top_level() {
        let h = Hierarchy::parse(SAMPLE);
        assert_eq!(h.roots(), &["animal".to_string(), "artist".to_string()]);
    }

    #[test]
    fn children_follow_indent() {
        let h = Hierarchy::parse(SAMPLE);
        assert!(h.node("animal").unwrap().children.contains(&"fox".to_string()));
        assert!(h.node("fox").unwrap().children.contains(&"fennec".to_string()));
        assert!(!h.node("animal").unwrap().children.contains(&"fennec".to_string()));
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
        assert!(h.node("bat").is_some());
        assert!(h.node("fox").is_some());
    }

    #[test]
    fn duplicate_keeps_first() {
        let h = Hierarchy::parse("animal\n\tbat\nbird\n\tbat\n");
        assert_eq!(h.len(), 3);
        assert!(h.node("bat").is_some());
    }

    #[test]
    fn space_indented_works() {
        let h = Hierarchy::parse("animal\n    bat\n    fox\n        fennec\n");
        assert_eq!(h.len(), 4);
        assert!(h.is_descendant_of("fennec", "fox"));
        assert!(h.is_descendant_of("fennec", "animal"));
    }

    #[test]
    fn tag_key_normalizes_operators_and_spaces() {
        assert_eq!(tag_key("derpy hooves"), "derpy_hooves");
        assert_eq!(tag_key("allegiance (clans)"), "allegiance__clans_");
    }

    #[test]
    fn mutations_round_trip_through_text() {
        let mut h = Hierarchy::parse("animal\n  fox\n    fennec\nbird\n  bat\n");
        h.rename("fox", "red fox").unwrap();
        h.move_tag("red fox", Some("bird")).unwrap();
        h.merge("red fox", "bat").unwrap();
        h.delete("bat").unwrap();
        assert!(h.insert("dragon", Some("animal")).is_ok());

        let text = h.to_text();
        let reparsed = Hierarchy::parse(&text);
        assert_eq!(reparsed.len(), h.len());
        assert!(reparsed.is_descendant_of("fennec", "bird"));
        assert!(reparsed.is_descendant_of("dragon", "animal"));
        assert!(reparsed.node("bat").is_none());
        assert!(reparsed.node("red fox").is_none());
    }

    #[test]
    fn move_rejects_cycles_and_duplicate_names_are_rejected() {
        let mut h = Hierarchy::parse("animal\n  fox\n    fennec\n");
        assert!(h.move_tag("animal", Some("fennec")).is_err());
        assert!(h.move_tag("fox", None).is_ok());
        assert!(h.insert("fennec", None).is_err());
        assert!(h.rename("fox", "animal").is_err());
    }

    #[test]
    fn ancestors_and_descendants_use_raw_names() {
        let h = Hierarchy::parse("animal\n  fox\n    fennec\nbird\n  bat\n");
        assert_eq!(h.ancestors_of("fennec"), vec!["animal".to_string(), "fox".to_string()]);
        assert_eq!(h.parent_of("fennec").as_deref(), Some("fox"));
        assert!(h.descendants_of("animal").contains(&"fennec".to_string()));
        assert!(!h.descendants_of("animal").contains(&"bat".to_string()));
    }

    #[test]
    fn two_space_indented_works() {
        let h = Hierarchy::parse("MLP\n  applejack\n  derpy hooves\nanimal\n  fox\n    fennec\n");
        assert_eq!(h.len(), 6);
        assert!(h.is_descendant_of("applejack", "MLP"));
        assert!(h.is_descendant_of("derpy hooves", "MLP"));
        assert!(h.is_descendant_of("fennec", "fox"));
        assert!(h.is_descendant_of("fennec", "animal"));
    }
}