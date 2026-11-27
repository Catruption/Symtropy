//! Headless store with sandboxed proposal application.

use crate::model::*;
use crate::registry::TagRegistry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, Default)]
pub struct TagStore {
    revision: u64,
    registry: TagRegistry,
    items: BTreeMap<ItemId, ItemTags>,
    global_relations: Vec<TagRelation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TagStoreFile {
    #[serde(default = "default_version")] version: u32,
    #[serde(default)] revision: u64,
    #[serde(default)] definitions: Vec<TagDefinition>,
    #[serde(default)] global_relations: Vec<TagRelation>,
    #[serde(default)] items: Vec<ItemTags>,
}

impl Default for TagStoreFile {
    fn default() -> Self {
        Self {
            version: default_version(),
            revision: 0,
            definitions: Vec::new(),
            global_relations: Vec::new(),
            items: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct PlannedOp {
    op: usize,
    operation: Operation,
    change: Change,
}

impl TagStore {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn registry(&self) -> &TagRegistry {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut TagRegistry {
        &mut self.registry
    }

    pub fn item(&self, item_id: &str) -> Option<&ItemTags> {
        self.items.get(item_id)
    }

    pub fn item_mut(&mut self, item_id: impl Into<ItemId>) -> &mut ItemTags {
        let id = item_id.into();
        self.items.entry(id.clone()).or_insert_with(|| ItemTags::new(id))
    }

    pub fn items(&self) -> impl Iterator<Item = &ItemTags> {
        self.items.values()
    }

    /// Human-authored lock changes are deliberate edits, so they bump the
    /// revision and make any older AI proposal stale.
    pub fn set_assertion_locked(&mut self, item: &str, tag: &str, locked: bool) -> bool {
        let Some(assertion) = self.items.get_mut(item).and_then(|item| item.assertion_mut(tag)) else {
            return false;
        };
        if assertion.locked == locked {
            return false;
        }
        assertion.locked = locked;
        self.revision += 1;
        true
    }

    pub fn set_relation_locked(&mut self, scope: &RelationScope, id: &str, locked: bool) -> bool {
        let Some(relation) = self
            .relations_mut(scope)
            .iter_mut()
            .find(|relation| relation.id == id)
        else {
            return false;
        };
        if relation.locked == locked {
            return false;
        }
        relation.locked = locked;
        self.revision += 1;
        true
    }

    pub fn global_relations(&self) -> &[TagRelation] {
        &self.global_relations
    }

    /// Look up a relation in either global or item scope.
    pub fn relation(&self, scope: &RelationScope, id: &str) -> Option<&TagRelation> {
        self.relations(scope)
            .and_then(|relations| relations.iter().find(|relation| relation.id == id))
    }

    fn relations(&self, scope: &RelationScope) -> Option<&Vec<TagRelation>> {
        match scope {
            RelationScope::Global => Some(&self.global_relations),
            RelationScope::Item(item) => self.items.get(item).map(|item| &item.relations),
        }
    }

    fn relations_mut(&mut self, scope: &RelationScope) -> &mut Vec<TagRelation> {
        match scope {
            RelationScope::Global => &mut self.global_relations,
            RelationScope::Item(item) => &mut self.item_mut(item.clone()).relations,
        }
    }

    fn find_assertion(&self, item: &str, tag: &str) -> Option<&TagAssertion> {
        self.items.get(item).and_then(|item| item.assertion(tag))
    }

    fn find_relation(&self, scope: &RelationScope, id: &str) -> Option<&TagRelation> {
        self.relations(scope).and_then(|rels| rels.iter().find(|relation| relation.id == id))
    }

    pub fn validate(&self) -> Result<(), String> {
        self.registry.validate()?;
        let mut global_ids = BTreeSet::new();
        for relation in &self.global_relations {
            if relation.id.trim().is_empty() {
                return Err("relation id cannot be empty".to_string());
            }
            if !global_ids.insert(relation.id.clone()) {
                return Err(format!("duplicate global relation id '{}'", relation.id));
            }
        }
        for item in self.items.values() {
            let mut ids = BTreeSet::new();
            for relation in &item.relations {
                if relation.id.trim().is_empty() {
                    return Err(format!("item '{}' has an empty relation id", item.item_id));
                }
                if !ids.insert(relation.id.clone()) {
                    return Err(format!(
                        "item '{}' has duplicate relation id '{}'",
                        item.item_id, relation.id
                    ));
                }
            }
        }
        Ok(())
    }

    /// Return relations reachable from `start`, following `predicate` edges in
    /// either direction. This is a graph helper for UI/AI review, not a query.
    pub fn related_from(
        &self,
        start: &EntityRef,
        predicate: Option<RelationPredicate>,
        max_depth: usize,
        include_suggested: bool,
    ) -> Vec<TagRelation> {
        let all: Vec<TagRelation> = self
            .global_relations
            .iter()
            .chain(self.items.values().flat_map(|item| item.relations.iter()))
            .filter(|relation| {
                relation.state == AssertionState::Accepted
                    || (include_suggested && relation.state == AssertionState::Suggested)
            })
            .cloned()
            .collect();

        let mut queue = VecDeque::new();
        queue.push_back((start.clone(), 0usize));
        let mut visited = BTreeSet::new();
        visited.insert(start.key());
        let mut out = Vec::new();

        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for relation in &all {
                if let Some(wanted) = predicate {
                    if relation.predicate != wanted {
                        continue;
                    }
                }
                let next = if relation.subject == node {
                    Some(relation.object.clone())
                } else if relation.object == node {
                    Some(relation.subject.clone())
                } else {
                    None
                };
                let Some(next) = next else { continue };
                if visited.insert(next.key()) {
                    out.push(relation.clone());
                    queue.push_back((next, depth + 1));
                }
            }
        }
        out
    }

    pub fn preview(&self, proposal: &Proposal, policy: &ApplyPolicy) -> ProposalPreview {
        let mut preview = ProposalPreview {
            proposal_id: proposal.id.clone(),
            revision: self.revision,
            planned: Vec::new(),
            rejected: Vec::new(),
        };
        if policy.strict_revision && proposal.based_on_revision != self.revision {
            preview.rejected.push(RejectedChange {
                op: 0,
                kind: "revision".to_string(),
                target: proposal.based_on_revision.to_string(),
                reason: format!("proposal is stale: based on {}, store is {}", proposal.based_on_revision, self.revision),
            });
            return preview;
        }
        let (planned, rejected) = self.plan(proposal, policy);
        preview.planned = planned.into_iter().map(|entry| entry.change).collect();
        preview.rejected = rejected;
        preview
    }

    pub fn apply_proposal(
        &mut self,
        proposal: &Proposal,
        policy: &ApplyPolicy,
    ) -> Result<ApplyReport, String> {
        if policy.strict_revision && proposal.based_on_revision != self.revision {
            return Err(format!(
                "proposal '{}' is stale: based on {}, store is {}",
                proposal.id, proposal.based_on_revision, self.revision
            ));
        }

        let old_revision = self.revision;
        let (planned, mut rejected) = self.plan(proposal, policy);
        let mut next = self.clone();
        let mut applied = Vec::new();

        for entry in planned {
            match next.apply_one(entry.operation) {
                Ok(mut change) => {
                    change.op = entry.op;
                    applied.push(change);
                }
                Err(reason) => rejected.push(RejectedChange {
                    op: entry.op,
                    kind: entry.change.kind,
                    target: entry.change.target,
                    reason,
                }),
            }
        }

        next.validate()?;
        if !applied.is_empty() {
            next.revision = old_revision + 1;
            *self = next;
        }

        Ok(ApplyReport {
            proposal_id: proposal.id.clone(),
            old_revision,
            new_revision: self.revision,
            applied,
            rejected,
        })
    }

    fn plan(&self, proposal: &Proposal, policy: &ApplyPolicy) -> (Vec<PlannedOp>, Vec<RejectedChange>) {
        let mut planned = Vec::new();
        let mut rejected = Vec::new();

        for (index, operation) in proposal.operations.iter().enumerate() {
            let kind = operation_kind_by_op(operation).to_string();
            let target = operation_target(operation);
            let machine = proposal.provenance.is_machine() || operation_provenance(operation).is_machine();
            let confidence = operation_confidence(operation);

            if confidence < policy.min_confidence {
                rejected.push(RejectedChange {
                    op: index,
                    kind,
                    target,
                    reason: format!("confidence {} below minimum {}", confidence, policy.min_confidence),
                });
                continue;
            }

            match &operation {
                Operation::DefineTag { definition } => {
                    if definition.id.trim().is_empty() {
                        rejected.push(RejectedChange {
                            op: index,
                            kind,
                            target,
                            reason: "tag definition has an empty id".to_string(),
                        });
                        continue;
                    }
                    if let Some(existing) = self.registry.get(&definition.id) {
                        if machine
                            && !existing.provenance.is_machine()
                            && !policy.allow_machine_overwrite_accepted
                        {
                            rejected.push(RejectedChange {
                                op: index,
                                kind,
                                target,
                                reason: format!("tag definition '{}' is human-curated", definition.id),
                            });
                            continue;
                        }
                    }
                    planned.push(PlannedOp {
                        op: index,
                        operation: operation.clone(),
                        change: Change {
                            op: index,
                            kind,
                            target,
                            detail: format!("define {} tag '{}'", definition.kind.as_str(), definition.id),
                        },
                    });
                }
                Operation::AddAssertion { item, assertion } => {
                    if machine
                        && assertion.state == AssertionState::Accepted
                        && !policy.allow_machine_overwrite_accepted
                    {
                        rejected.push(RejectedChange {
                            op: index,
                            kind,
                            target,
                            reason: "machine output cannot create accepted tags".to_string(),
                        });
                        continue;
                    }
                    if let Some(existing) = self.find_assertion(item, &assertion.tag) {
                        let block = if existing.locked && machine && !policy.allow_machine_overwrite_locked {
                            Some(format!("tag '{}' is locked", assertion.tag))
                        } else if existing.state == AssertionState::Accepted
                            && machine
                            && !policy.allow_machine_overwrite_accepted
                        {
                            Some(format!("tag '{}' is already accepted", assertion.tag))
                        } else {
                            None
                        };
                        if let Some(reason) = block {
                            rejected.push(RejectedChange { op: index, kind, target, reason });
                            continue;
                        }
                    }
                    planned.push(PlannedOp {
                        op: index,
                        operation: operation.clone(),
                        change: Change {
                            op: index,
                            kind,
                            target,
                            detail: format!("{} tag '{}' as {:?}", "add", assertion.tag, assertion.state),
                        },
                    });
                }
                Operation::SetAssertionState { item, tag, state, .. } => {
                    let Some(existing) = self.find_assertion(item, tag) else {
                        rejected.push(RejectedChange {
                            op: index,
                            kind,
                            target,
                            reason: format!("item '{}' has no assertion '{}'", item, tag),
                        });
                        continue;
                    };
                    let block = if machine
                        && *state == AssertionState::Accepted
                        && !policy.allow_machine_overwrite_accepted
                    {
                        Some("machine output cannot mark a tag accepted".to_string())
                    } else if existing.locked && machine && !policy.allow_machine_overwrite_locked {
                        Some(format!("tag '{}' is locked", tag))
                    } else if existing.state == AssertionState::Accepted
                        && machine
                        && !policy.allow_machine_overwrite_accepted
                    {
                        Some(format!("tag '{}' is already accepted", tag))
                    } else {
                        None
                    };
                    if let Some(reason) = block {
                        rejected.push(RejectedChange { op: index, kind, target, reason });
                        continue;
                    }
                    planned.push(PlannedOp {
                        op: index,
                        operation: operation.clone(),
                        change: Change {
                            op: index,
                            kind,
                            target,
                            detail: format!("set '{}' to {:?} (was {:?})", tag, state, existing.state),
                        },
                    });
                }
                Operation::AddRelation { scope, relation } => {
                    if machine
                        && relation.state == AssertionState::Accepted
                        && !policy.allow_machine_overwrite_accepted
                    {
                        rejected.push(RejectedChange {
                            op: index,
                            kind,
                            target,
                            reason: "machine output cannot create accepted relations".to_string(),
                        });
                        continue;
                    }
                    if relation.id.trim().is_empty() {
                        rejected.push(RejectedChange {
                            op: index,
                            kind,
                            target,
                            reason: "relation has an empty id".to_string(),
                        });
                        continue;
                    }
                    if self.find_relation(scope, &relation.id).is_some() {
                        rejected.push(RejectedChange {
                            op: index,
                            kind,
                            target,
                            reason: format!("relation '{}' already exists", relation.id),
                        });
                        continue;
                    }
                    planned.push(PlannedOp {
                        op: index,
                        operation: operation.clone(),
                        change: Change {
                            op: index,
                            kind,
                            target,
                            detail: format!(
                                "add relation '{}': {} {} {}",
                                relation.id,
                                relation.subject.key(),
                                relation.predicate.as_str(),
                                relation.object.key()
                            ),
                        },
                    });
                }
                Operation::SetRelationState { scope, relation_id, state, .. } => {
                    let Some(existing) = self.find_relation(scope, relation_id) else {
                        rejected.push(RejectedChange {
                            op: index,
                            kind,
                            target,
                            reason: format!("{} has no relation '{}'", scope.key(), relation_id),
                        });
                        continue;
                    };
                    let block = if machine
                        && *state == AssertionState::Accepted
                        && !policy.allow_machine_overwrite_accepted
                    {
                        Some("machine output cannot mark a relation accepted".to_string())
                    } else if existing.locked && machine && !policy.allow_machine_overwrite_locked {
                        Some(format!("relation '{}' is locked", relation_id))
                    } else if existing.state == AssertionState::Accepted
                        && machine
                        && !policy.allow_machine_overwrite_accepted
                    {
                        Some(format!("relation '{}' is already accepted", relation_id))
                    } else {
                        None
                    };
                    if let Some(reason) = block {
                        rejected.push(RejectedChange { op: index, kind, target, reason });
                        continue;
                    }
                    planned.push(PlannedOp {
                        op: index,
                        operation: operation.clone(),
                        change: Change {
                            op: index,
                            kind,
                            target,
                            detail: format!("set relation '{}' to {:?} (was {:?})", relation_id, state, existing.state),
                        },
                    });
                }
            }
        }
        (planned, rejected)
    }

    fn apply_one(&mut self, operation: Operation) -> Result<Change, String> {
        match operation {
            Operation::DefineTag { definition } => {
                let id = definition.id.clone();
                self.registry.insert(definition)?;
                Ok(Change {
                    op: 0,
                    kind: "define_tag".to_string(),
                    target: id.clone(),
                    detail: format!("defined '{}'", id),
                })
            }
            Operation::AddAssertion { item, assertion } => {
                let target = format!("{}::{}", item, assertion.tag);
                self.item_mut(item).upsert_assertion(assertion);
                Ok(Change {
                    op: 0,
                    kind: "add_assertion".to_string(),
                    target,
                    detail: "assertion stored".to_string(),
                })
            }
            Operation::SetAssertionState { item, tag, state, superseded_by, provenance, evidence } => {
                let target = format!("{}::{}", item, tag);
                let store = self.item_mut(item);
                let Some(assertion) = store.assertion_mut(&tag) else {
                    return Err(format!("assertion '{}' disappeared", tag));
                };
                assertion.state = state;
                assertion.superseded_by = superseded_by;
                assertion.provenance = provenance;
                assertion.evidence.extend(evidence);
                assertion.version += 1;
                Ok(Change {
                    op: 0,
                    kind: "set_assertion_state".to_string(),
                    target,
                    detail: format!("state is now {:?}", state),
                })
            }
            Operation::AddRelation { scope, relation } => {
                let target = format!("{}::{}", scope.key(), relation.id);
                self.relations_mut(&scope).push(relation);
                self.relations_mut(&scope).sort_by(|a, b| a.id.cmp(&b.id));
                Ok(Change {
                    op: 0,
                    kind: "add_relation".to_string(),
                    target,
                    detail: "relation stored".to_string(),
                })
            }
            Operation::SetRelationState { scope, relation_id, state, superseded_by, provenance, evidence } => {
                let target = format!("{}::{}", scope.key(), relation_id);
                let relations = self.relations_mut(&scope);
                let Some(relation) = relations.iter_mut().find(|relation| relation.id == relation_id) else {
                    return Err(format!("relation '{}' disappeared", relation_id));
                };
                relation.state = state;
                relation.superseded_by = superseded_by;
                relation.provenance = provenance;
                relation.evidence.extend(evidence);
                relation.version += 1;
                Ok(Change {
                    op: 0,
                    kind: "set_relation_state".to_string(),
                    target,
                    detail: format!("state is now {:?}", state),
                })
            }
        }
    }

    pub fn to_json5_pretty(&self) -> Result<String, String> {
        let file = TagStoreFile {
            version: self.registry.version,
            revision: self.revision,
            definitions: self.registry.definitions().cloned().collect(),
            global_relations: self.global_relations.clone(),
            items: self.items.values().cloned().collect(),
        };
        serde_json::to_string_pretty(&file).map_err(|error| error.to_string())
    }

    pub fn from_json5(input: &str) -> Result<Self, String> {
        let file: TagStoreFile = json5::from_str(input).map_err(|error| error.to_string())?;
        let mut store = Self {
            revision: file.revision,
            registry: TagRegistry::with_version(file.version),
            items: BTreeMap::new(),
            global_relations: file.global_relations,
        };
        for definition in file.definitions {
            store.registry.insert(definition)?;
        }
        for item in file.items {
            let id = item.item_id.clone();
            store.items.insert(id, item);
        }
        store.validate()?;
        Ok(store)
    }
}

fn operation_kind_by_op(operation: &Operation) -> &'static str {
    match operation {
        Operation::DefineTag { .. } => "define_tag",
        Operation::AddAssertion { .. } => "add_assertion",
        Operation::SetAssertionState { .. } => "set_assertion_state",
        Operation::AddRelation { .. } => "add_relation",
        Operation::SetRelationState { .. } => "set_relation_state",
    }
}

fn operation_target(operation: &Operation) -> String {
    match operation {
        Operation::DefineTag { definition } => definition.id.clone(),
        Operation::AddAssertion { item, assertion } => format!("{}::{}", item, assertion.tag),
        Operation::SetAssertionState { item, tag, .. } => format!("{}::{}", item, tag),
        Operation::AddRelation { scope, relation } => format!("{}::{}", scope.key(), relation.id),
        Operation::SetRelationState { scope, relation_id, .. } => format!("{}::{}", scope.key(), relation_id),
    }
}

fn operation_provenance(operation: &Operation) -> &Provenance {
    match operation {
        Operation::DefineTag { definition } => &definition.provenance,
        Operation::AddAssertion { assertion, .. } => &assertion.provenance,
        Operation::SetAssertionState { provenance, .. } => provenance,
        Operation::AddRelation { relation, .. } => &relation.provenance,
        Operation::SetRelationState { provenance, .. } => provenance,
    }
}

fn operation_confidence(operation: &Operation) -> f32 {
    match operation {
        Operation::DefineTag { .. } => 1.0,
        Operation::AddAssertion { assertion, .. } => assertion.confidence,
        Operation::SetAssertionState { .. } => 1.0,
        Operation::AddRelation { relation, .. } => relation.confidence,
        Operation::SetRelationState { .. } => 1.0,
    }
}
