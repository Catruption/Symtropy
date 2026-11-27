//! Global tag definitions and aliases.

use crate::model::{TagDefinition, TagId, TagStatus};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Shared normalization with Symtropy's query/taxonomy convention: whitespace
/// and query-operator characters become underscores. Lookup is lowercase so
/// pasted/imported aliases do not fragment on casing.
pub fn normalize_lookup(value: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = false;
    for raw in value.trim().chars() {
        let ch = if raw.is_whitespace() || "()+!|".contains(raw) {
            '_'
        } else {
            raw.to_ascii_lowercase()
        };
        if ch == '_' {
            if last_underscore {
                continue;
            }
            last_underscore = true;
        } else {
            last_underscore = false;
        }
        out.push(ch);
    }
    out.trim_matches('_').to_string()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagRegistry {
    #[serde(default = "crate::model::default_version")] pub version: u32,
    #[serde(default)] definitions: BTreeMap<TagId, TagDefinition>,
    #[serde(skip)] by_norm: BTreeMap<String, TagId>,
    #[serde(skip)] aliases: BTreeMap<String, TagId>,
}

impl TagRegistry {
    pub fn with_version(version: u32) -> Self {
        Self { version, ..Self::default() }
    }

    pub fn definitions(&self) -> impl Iterator<Item = &TagDefinition> {
        self.definitions.values()
    }

    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&TagDefinition> {
        self.definitions.get(id)
    }

    pub fn resolve(&self, value: &str) -> Option<&TagDefinition> {
        if let Some(def) = self.definitions.get(value) {
            return Some(def);
        }
        let key = normalize_lookup(value);
        if key.is_empty() {
            return None;
        }
        if let Some(id) = self.by_norm.get(&key) {
            return self.definitions.get(id);
        }
        if let Some(id) = self.aliases.get(&key) {
            return self.definitions.get(id);
        }
        None
    }

    pub fn insert(&mut self, definition: TagDefinition) -> Result<(), String> {
        let id = definition.id.trim().to_string();
        if id.is_empty() {
            return Err("tag id cannot be empty".to_string());
        }
        let key = normalize_lookup(&id);
        if key.is_empty() {
            return Err("tag id has no usable characters".to_string());
        }
        if let Some(other) = self.by_norm.get(&key) {
            if other != &id {
                return Err(format!("tag id '{}' conflicts with '{}'", id, other));
            }
        }
        self.definitions.insert(id, definition);
        self.rebuild_index();
        Ok(())
    }

    pub fn rebuild_index(&mut self) {
        self.by_norm.clear();
        self.aliases.clear();
        for (id, def) in &self.definitions {
            self.by_norm.insert(normalize_lookup(id), id.clone());
            for alias in &def.aliases {
                let key = normalize_lookup(alias);
                if !key.is_empty() {
                    self.aliases.entry(key).or_insert_with(|| id.clone());
                }
            }
        }
    }

    pub fn ancestors(&self, id: &str) -> Vec<TagId> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = self.definitions.get(id).and_then(|def| def.parent.clone());
        while let Some(parent) = current {
            if !seen.insert(parent.clone()) || !self.definitions.contains_key(&parent) {
                break;
            }
            out.push(parent.clone());
            current = self.definitions.get(&parent).and_then(|def| def.parent.clone());
        }
        out
    }

    pub fn is_descendant_of(&self, child: &str, parent: &str) -> bool {
        child == parent || self.ancestors(child).iter().any(|id| id == parent)
    }

    pub fn descendants(&self, parent: &str) -> Vec<TagId> {
        let mut out: Vec<TagId> = self
            .definitions
            .keys()
            .filter(|id| self.is_descendant_of(id, parent))
            .cloned()
            .collect();
        out.sort();
        out
    }

    pub fn active(&self) -> impl Iterator<Item = &TagDefinition> {
        self.definitions.values().filter(|def| def.status == TagStatus::Active)
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut keys: BTreeMap<String, &TagId> = BTreeMap::new();
        for id in self.definitions.keys() {
            let key = normalize_lookup(id);
            if key.is_empty() {
                return Err("tag id has no usable characters".to_string());
            }
            if let Some(other) = keys.insert(key, id) {
                return Err(format!("tag id '{}' conflicts with '{}'", id, other));
            }
        }

        for def in self.definitions.values() {
            if let Some(parent) = &def.parent {
                if !self.definitions.contains_key(parent) {
                    return Err(format!("tag '{}' has missing parent '{}'", def.id, parent));
                }
                let mut seen = BTreeSet::new();
                seen.insert(def.id.clone());
                let mut current = Some(parent.clone());
                while let Some(id) = current {
                    if !seen.insert(id.clone()) {
                        return Err(format!("tag hierarchy cycle at '{}'", id));
                    }
                    current = self.definitions.get(&id).and_then(|d| d.parent.clone());
                }
            }
        }
        Ok(())
    }
}
