//! Query language.
//!
//! A small expression language over tags, ratings, and the tag hierarchy.
//! Operators are symbols (no English keywords) so tag names can be anything
//! that isn't a symbol:
//!
//!     +        AND
//!     |        OR
//!     -        EXCEPT (binary: "a but not b")
//!     !        NOT (unary prefix)
//!     ( )      grouping
//!     rating:N exactly N stars
//!     rating:N+ N stars or more
//!     rel:S@predicate@O typed relation (first-class edge, not a tag)
//!
//! Relation endpoints are `item:this`, `item:path`, `tag:id`, or `ext:url`.
//! A global relation matches an image when one of its tag endpoints is on the
//! image; an item relation matches when the current item is an endpoint.
//! Relation queries are directional: `rel:item:this@created_by@tag:artist:x`
//! is not the same as its reverse.
//!
//! Tags are single tokens matching `[A-Za-z0-9_:.\-]+`. `-` is an operator
//! only when surrounded by whitespace, so `house-cat` is one tag and
//! `fox - plush` is a difference.
//!
//! Precedence, tight to loose: `()` > `!` > `+` `-` > `|`.
//!
//! Whitespace-separated atoms are implicitly AND-ed, so `fox plush` is the
//! same as `fox + plush`.
//!
//! Tags may contain spaces in the source taxonomy (like `warrior cats`),
//! but queries cannot. Normalize names with underscores (`warrior_cats`)
//! before using them in queries.

use super::hierarchy::tag_key;
use super::Hierarchy;
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use symtropy_tags::{AssertionState, EntityRef, ProvenanceKind, RelationPredicate, TagRelation};
use std::fmt;

/// Which authority a claim came from. This is deliberately separate from
/// confidence: a machine guess at 99% is still not a human fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimSource {
    Human,
    Ai,
    Import,
}

impl ClaimSource {
    pub fn from_provenance(kind: ProvenanceKind) -> Self {
        match kind {
            // Legacy sidecars have no explicit provenance. Their accepted tags
            // are the curated baseline, so Unknown must stay searchable there.
            ProvenanceKind::Human | ProvenanceKind::Unknown => Self::Human,
            ProvenanceKind::Ai | ProvenanceKind::Computed => Self::Ai,
            ProvenanceKind::Source | ProvenanceKind::Import => Self::Import,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Ai => "ai",
            Self::Import => "import",
        }
    }
}

impl fmt::Display for ClaimSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which claim authorities an active filter may see. Curated is the default,
/// so ordinary tag searches never silently include machine suggestions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClaimScope {
    #[default]
    Curated,
    Machine,
    /// Used when an explicit AI query is allowed to match human tags too.
    All,
}

impl ClaimScope {
    pub fn accepts(self, source: ClaimSource) -> bool {
        match self {
            Self::Curated => source == ClaimSource::Human,
            Self::Machine => matches!(source, ClaimSource::Ai | ClaimSource::Import),
            Self::All => true,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Curated => "curated",
            Self::Machine => "machine",
            Self::All => "all",
        }
    }
}

impl fmt::Display for ClaimScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryEntity {
    CurrentItem,
    Item(String),
    Tag(String),
    External(String),
}

impl QueryEntity {
    fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("relation endpoint cannot be empty".to_string());
        }
        if value == "this" || value == "item:this" {
            return Ok(Self::CurrentItem);
        }
        if let Some(rest) = value.strip_prefix("item:") {
            return rest
                .is_empty()
                .then(|| Err("item endpoint cannot be empty".to_string()))
                .unwrap_or_else(|| Ok(Self::Item(rest.to_string())));
        }
        if let Some(rest) = value.strip_prefix("tag:") {
            return rest
                .is_empty()
                .then(|| Err("tag endpoint cannot be empty".to_string()))
                .unwrap_or_else(|| Ok(Self::Tag(rest.to_string())));
        }
        if let Some(rest) = value.strip_prefix("ext:") {
            return rest
                .is_empty()
                .then(|| Err("external endpoint cannot be empty".to_string()))
                .unwrap_or_else(|| Ok(Self::External(rest.to_string())));
        }
        Err(format!(
            "relation endpoint '{}' needs item:, tag:, ext:, or this",
            value
        ))
    }

    fn display(&self) -> String {
        match self {
            Self::CurrentItem => "item:this".to_string(),
            Self::Item(value) => format!("item:{value}"),
            Self::Tag(value) => format!("tag:{value}"),
            Self::External(value) => format!("ext:{value}"),
        }
    }

    fn map_tags(&self, map: &impl Fn(&str) -> Option<String>) -> Option<Self> {
        match self {
            Self::Tag(tag) => map(tag).map(Self::Tag),
            other => Some(other.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RelationQuery {
    pub subject: QueryEntity,
    pub predicate: RelationPredicate,
    pub object: QueryEntity,
}

impl RelationQuery {
    fn parse(input: &str) -> Result<Self, String> {
        let parts: Vec<&str> = input.split('@').collect();
        if parts.len() != 3 {
            return Err("relation query needs subject@predicate@object".to_string());
        }
        let predicate_text = parts[1].trim();
        let predicate = RelationPredicate::parse(predicate_text);
        const PREDICATES: &[&str] = &[
            "related_to", "subclass_of", "part_of", "member_of", "maps_to", "aliases", "realizes",
            "evidences", "adjacent_to", "contrasts_with", "requires_context", "implies_cue",
            "created_by", "depicts", "located_at", "inspired_by", "derived_from",
        ];
        if !PREDICATES.iter().any(|name| *name == predicate_text.to_ascii_lowercase()) {
            return Err(format!("unknown relation predicate '{}'", predicate_text));
        }
        Ok(Self {
            subject: QueryEntity::parse(parts[0])?,
            predicate,
            object: QueryEntity::parse(parts[2])?,
        })
    }

    fn display(&self) -> String {
        format!(
            "{}@{}@{}",
            self.subject.display(),
            self.predicate.as_str(),
            self.object.display()
        )
    }

    fn map_tags(&self, map: &impl Fn(&str) -> Option<String>) -> Option<Self> {
        Some(Self {
            subject: self.subject.map_tags(map)?,
            predicate: self.predicate,
            object: self.object.map_tags(map)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    Tag(String),
    Relation(RelationQuery),
    Rating { min: u8, exact: bool },
    Not(Box<Query>),
    And(Vec<Query>),
    Or(Vec<Query>),
    Except(Vec<Query>),
    AlwaysTrue,
}

/// Inclusive confidence bucket used by the tag filter.
///
/// Accepted human claims are represented as `100`; machine/source suggestions
/// are clamped to `0..=99`. That keeps "accepted" distinct from a model that
/// happens to be extremely confident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfidenceRange {
    pub min: u8,
    pub max: u8,
}

impl ConfidenceRange {
    pub fn new(min: u8, max: u8) -> Self {
        if min <= max {
            Self { min, max }
        } else {
            Self { min: max, max: min }
        }
    }

    pub fn accepted_only() -> Self {
        Self { min: 100, max: 100 }
    }

    pub fn all() -> Self {
        Self { min: 0, max: 100 }
    }

    pub fn contains(self, confidence: u8) -> bool {
        (self.min..=self.max).contains(&confidence)
    }
}

impl Default for ConfidenceRange {
    fn default() -> Self {
        Self::accepted_only()
    }
}

impl fmt::Display for ConfidenceRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.min == self.max {
            write!(f, "{}%", self.min)
        } else {
            write!(f, "{}–{}%", self.min, self.max)
        }
    }
}

/// One image's effective tag confidences for filtering.
///
/// A tag appears once, keyed by its normalized name. Accepted claims have
/// confidence `100`; suggestions keep their strongest score below `100`.
#[derive(Clone, Debug)]
struct TagClaim {
    tag: String,
    confidence: u8,
    source: ClaimSource,
}

#[derive(Clone, Debug)]
struct EvidenceRelation {
    relation: TagRelation,
    global: bool,
    source: ClaimSource,
}

#[derive(Clone, Debug, Default)]
pub struct TagEvidence {
    item_id: String,
    entries: HashMap<String, TagClaim>,
    relations: Vec<EvidenceRelation>,
}

impl TagEvidence {
    /// Sidecar tags are the curated baseline. Namespaced `src:` tags are
    /// imported evidence and stay in the machine tier until explicitly
    /// promoted into a normal tag.
    pub fn from_tags<'a>(tags: impl IntoIterator<Item = &'a str>) -> Self {
        let mut evidence = Self::default();
        for tag in tags {
            let source = if tag_key(tag).starts_with("src:") {
                ClaimSource::Import
            } else {
                ClaimSource::Human
            };
            evidence.insert_with_source(tag, 100, source);
        }
        evidence
    }

    pub fn with_item_id(mut self, item_id: &str) -> Self {
        self.item_id = item_id.to_string();
        self
    }

    /// Add a relation visible to this image. Global relations only match when
    /// one of their tag endpoints is present on the image; item relations must
    /// involve the current item. The claim source comes from its provenance.
    pub fn insert_relation(&mut self, relation: TagRelation, global: bool) {
        let source = ClaimSource::from_provenance(relation.provenance.kind);
        self.relations.push(EvidenceRelation { relation, global, source });
    }

    /// Insert an accepted human claim. Accepted wins over lower guesses.
    #[cfg(test)]
    pub fn insert_accepted(&mut self, tag: &str) {
        self.insert_with_source(tag, 100, ClaimSource::Human);
    }

    /// Insert an accepted claim from an explicit authority.
    pub fn insert_accepted_with_source(&mut self, tag: &str, source: ClaimSource) {
        self.insert_with_source(tag, 100, source);
    }

    /// Insert an AI suggestion. Scores are clamped to `0..=99` so a suggestion
    /// can never masquerade as a human-confirmed tag.
    #[cfg(test)]
    pub fn insert_suggested(&mut self, tag: &str, confidence: f32) {
        self.insert_suggested_with_source(tag, confidence, ClaimSource::Ai);
    }

    pub fn insert_suggested_with_source(
        &mut self,
        tag: &str,
        confidence: f32,
        source: ClaimSource,
    ) {
        let percent = (confidence.clamp(0.0, 1.0) * 99.0).floor() as u8;
        self.insert_with_source(tag, percent.min(99), source);
    }

    fn insert_with_source(&mut self, tag: &str, confidence: u8, source: ClaimSource) {
        let key = tag_key(tag);
        match self.entries.get_mut(&key) {
            Some(existing) => {
                // Curated facts win ties; otherwise prefer the strongest score.
                if confidence > existing.confidence
                    || (confidence == existing.confidence && source == ClaimSource::Human)
                {
                    existing.confidence = confidence;
                    existing.source = source;
                }
            }
            None => {
                self.entries.insert(key, TagClaim {
                    tag: tag.to_string(),
                    confidence,
                    source,
                });
            }
        }
    }

    /// True when this image has a claim for `wanted` (or a descendant of it)
    /// inside both the confidence range and the active claim scope.
    pub fn contains_tag_scoped(
        &self,
        wanted: &str,
        hierarchy: &Hierarchy,
        range: ConfidenceRange,
        scope: ClaimScope,
    ) -> bool {
        let wanted = tag_key(wanted);
        self.entries.values().any(|claim| {
            scope.accepts(claim.source)
                && range.contains(claim.confidence)
                && (tag_key(&claim.tag) == wanted
                    || hierarchy.is_descendant_of(&claim.tag, &wanted))
        })
    }

    /// Convenience for tests that intentionally search every authority.
    #[cfg(test)]
    pub fn contains_tag(
        &self,
        wanted: &str,
        hierarchy: &Hierarchy,
        range: ConfidenceRange,
    ) -> bool {
        self.contains_tag_scoped(wanted, hierarchy, range, ClaimScope::All)
    }

    pub fn contains_relation_scoped(
        &self,
        wanted: &RelationQuery,
        hierarchy: &Hierarchy,
        range: ConfidenceRange,
        scope: ClaimScope,
    ) -> bool {
        self.relations.iter().any(|entry| {
            if !scope.accepts(entry.source) {
                return false;
            }
            if relation_confidence(&entry.relation, range).is_none() {
                return false;
            }
            if entry.relation.predicate != wanted.predicate {
                return false;
            }
            let subject = endpoint_matches(&wanted.subject, &entry.relation.subject, self);
            let object = endpoint_matches(&wanted.object, &entry.relation.object, self);
            if !subject || !object {
                return false;
            }
            if entry.global {
                tag_endpoint_applies(&entry.relation.subject, self, hierarchy, range, scope)
                    || tag_endpoint_applies(&entry.relation.object, self, hierarchy, range, scope)
            } else {
                matches!(&entry.relation.subject, EntityRef::Item(id) if id == &self.item_id)
                    || matches!(&entry.relation.object, EntityRef::Item(id) if id == &self.item_id)
            }
        })
    }

}

fn relation_confidence(relation: &TagRelation, range: ConfidenceRange) -> Option<u8> {
    match relation.state {
        AssertionState::Accepted => Some(100),
        AssertionState::Suggested => {
            let percent = (relation.confidence.clamp(0.0, 1.0) * 99.0).floor() as u8;
            Some(percent.min(99))
        }
        AssertionState::Rejected | AssertionState::Superseded => None,
    }
    .filter(|confidence| range.contains(*confidence))
}

fn endpoint_matches(wanted: &QueryEntity, endpoint: &EntityRef, evidence: &TagEvidence) -> bool {
    match (wanted, endpoint) {
        (QueryEntity::CurrentItem, EntityRef::Item(id)) => id == &evidence.item_id,
        (QueryEntity::Item(id), EntityRef::Item(value)) => id == value,
        (QueryEntity::Tag(id), EntityRef::Tag(value)) => tag_key(id) == tag_key(value),
        (QueryEntity::External(value), EntityRef::External(endpoint)) => value == endpoint,
        _ => false,
    }
}

fn tag_endpoint_applies(
    endpoint: &EntityRef,
    evidence: &TagEvidence,
    hierarchy: &Hierarchy,
    range: ConfidenceRange,
    scope: ClaimScope,
) -> bool {
    match endpoint {
        EntityRef::Tag(tag) => evidence.contains_tag_scoped(tag, hierarchy, range, scope),
        _ => false,
    }
}

impl Query {
    /// Parse `input`. Empty input and the persisted wildcard form are `AlwaysTrue`.
    pub fn parse(input: &str) -> Result<Self, String> {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed == "*" {
            return Ok(Query::AlwaysTrue);
        }
        let tokens = tokenize(input)?;
        if tokens.is_empty() {
            return Ok(Query::AlwaysTrue);
        }
        let mut p = Parser { tokens, pos: 0 };
        let q = p.parse_or()?;
        if p.pos < p.tokens.len() {
            return Err(format!("unexpected token at position {}", p.pos));
        }
        Ok(q)
    }

    /// Evaluate against an image's tags and rating.
    ///
    /// `applied` is the effective tag set (explicit + all ancestors). A query
    /// for `animal` matches an image tagged `fox` because `fox` is a
    /// descendant of `animal` in the hierarchy.
    /// Accepted-only evaluation for plain tag sets. Kept for query tests; the
    /// gallery always uses [`Self::matches_evidence`].
    #[cfg(test)]
    pub fn matches(&self, applied: &HashSet<String>, rating: u8, hierarchy: &Hierarchy) -> bool {
        let evidence = TagEvidence::from_tags(applied.iter().map(String::as_str));
        self.matches_evidence(
            &evidence,
            rating,
            hierarchy,
            ConfidenceRange::accepted_only(),
            ClaimScope::All,
        )
    }

    /// Evaluate against effective tag claims. The caller chooses both the
    /// confidence bucket and which authorities are searchable. Those are
    /// independent: a machine claim at 99% is still not a human fact.
    pub fn matches_evidence(
        &self,
        evidence: &TagEvidence,
        rating: u8,
        hierarchy: &Hierarchy,
        range: ConfidenceRange,
        scope: ClaimScope,
    ) -> bool {
        match self {
            Query::Tag(t) => evidence.contains_tag_scoped(t, hierarchy, range, scope),
            Query::Relation(relation) => {
                evidence.contains_relation_scoped(relation, hierarchy, range, scope)
            }
            Query::Rating { min, exact } => {
                if *exact {
                    rating == *min
                } else {
                    rating >= *min
                }
            }
            Query::Not(q) => !q.matches_evidence(evidence, rating, hierarchy, range, scope),
            Query::And(qs) => qs.iter().all(|q| {
                q.matches_evidence(evidence, rating, hierarchy, range, scope)
            }),
            Query::Or(qs) => qs.iter().any(|q| {
                q.matches_evidence(evidence, rating, hierarchy, range, scope)
            }),
            Query::Except(qs) => {
                let Some(first) = qs.first() else { return true };
                first.matches_evidence(evidence, rating, hierarchy, range, scope)
                    && qs[1..].iter().all(|q| {
                        !q.matches_evidence(evidence, rating, hierarchy, range, scope)
                    })
            }
            Query::AlwaysTrue => true,
        }
    }

    /// Rewrite tag leaves, dropping any tag the mapper rejects. Used when the
    /// taxonomy changes so an active filter does not silently keep a stale name.
    pub fn map_tags(&self, map: &impl Fn(&str) -> Option<String>) -> Self {
        fn walk(q: &Query, map: &impl Fn(&str) -> Option<String>) -> Option<Query> {
            match q {
                Query::Tag(tag) => map(tag).map(Query::Tag),
                Query::Relation(relation) => relation.map_tags(map).map(Query::Relation),
                Query::Rating { .. } | Query::AlwaysTrue => Some(q.clone()),
                Query::Not(inner) => walk(inner, map).map(|inner| Query::Not(Box::new(inner))),
                Query::And(qs) => {
                    let parts: Vec<Query> = qs.iter().filter_map(|q| walk(q, map)).collect();
                    (!parts.is_empty()).then(|| Query::And(parts))
                }
                Query::Or(qs) => {
                    let parts: Vec<Query> = qs.iter().filter_map(|q| walk(q, map)).collect();
                    (!parts.is_empty()).then(|| Query::Or(parts))
                }
                Query::Except(qs) => {
                    let parts: Vec<Query> = qs.iter().filter_map(|q| walk(q, map)).collect();
                    match parts.len() {
                        0 => None,
                        1 => Some(parts.into_iter().next().unwrap()),
                        _ => Some(Query::Except(parts)),
                    }
                }
            }
        }

        walk(self, map).unwrap_or(Query::AlwaysTrue)
    }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Query::Tag(t) => write!(f, "{}", t),
            Query::Relation(relation) => write!(f, "rel:{}", relation.display()),
            Query::Rating { min, exact } => {
                if *exact {
                    write!(f, "rating:{}", min)
                } else {
                    write!(f, "rating:{}+", min)
                }
            }
            Query::Not(q) => write!(f, "!{}", wrap(q, 3)),
            Query::And(qs) => write_joined(f, qs, " + ", 2),
            Query::Or(qs) => write_joined(f, qs, " | ", 1),
            Query::Except(qs) => write_joined(f, qs, " - ", 2),
            Query::AlwaysTrue => write!(f, "*"),
        }
    }
}

fn write_joined(f: &mut fmt::Formatter<'_>, qs: &[Query], sep: &str, prec: u8) -> fmt::Result {
    let mut first = true;
    for q in qs {
        if !first {
            write!(f, "{}", sep)?;
        }
        first = false;
        write!(f, "{}", wrap(q, prec))?;
    }
    Ok(())
}

/// Render `q`, wrapping in parens if its precedence is lower than `prec`.
fn wrap(q: &Query, prec: u8) -> String {
    let own = match q {
        Query::AlwaysTrue | Query::Tag(_) | Query::Relation(_) | Query::Rating { .. } => 4,
        Query::Not(_) => 3,
        Query::And(_) | Query::Except(_) => 2,
        Query::Or(_) => 1,
    };
    if own < prec {
        format!("({})", q)
    } else {
        q.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Token {
    LParen,
    RParen,
    Plus,
    Pipe,
    Minus,
    Bang,
    Word(String),
    Rating { min: u8, exact: bool },
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => { out.push(Token::LParen); i += 1; }
            ')' => { out.push(Token::RParen); i += 1; }
            '+' => { out.push(Token::Plus); i += 1; }
            '|' => { out.push(Token::Pipe); i += 1; }
            '!' => { out.push(Token::Bang); i += 1; }
            '-' => {
                // `-` is the EXCEPT operator only when surrounded by whitespace.
                let prev_ws = i == 0 || chars[i - 1].is_whitespace();
                let next_ws = i + 1 >= chars.len() || chars[i + 1].is_whitespace();
                if prev_ws && next_ws {
                    out.push(Token::Minus);
                    i += 1;
                } else {
                    return Err(format!(
                        "unexpected '-' at position {} (a bare '-' must be surrounded by spaces to mean EXCEPT)",
                        i
                    ));
                }
            }
            _ => {
                let start = i;
                let mut word = String::new();
                while i < chars.len() {
                    let c = chars[i];
                    if c.is_whitespace() || "()|".contains(c) {
                        break;
                    }
                    if c == '!' && !word.starts_with("rel:") {
                        break;
                    }
                    if c == '+' {
                        // `+` is an AND operator between words, but part of
                        // `rating:N+` and of URLs inside `rel:` endpoints.
                        if word.starts_with("rating:") || word.starts_with("rel:") {
                            word.push(c);
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    if c == '-' {
                        // Inside a word: allowed only if not surrounded by whitespace.
                        let prev_ws = i > 0 && chars[i - 1].is_whitespace();
                        let next_ws = i + 1 >= chars.len() || chars[i + 1].is_whitespace();
                        if prev_ws && next_ws {
                            break;
                        }
                    }
                    word.push(c);
                    i += 1;
                }
                if word.is_empty() {
                    return Err(format!("empty token at position {}", start));
                }
                if let Some(rest) = word.strip_prefix("rating:") {
                    let exact = !rest.ends_with('+');
                    let num = if exact { rest } else { &rest[..rest.len() - 1] };
                    let min: u8 = num
                        .parse()
                        .map_err(|_| format!("invalid rating: '{}'", word))?;
                    if min > 5 {
                        return Err(format!("rating must be 0-5, got {}", min));
                    }
                    out.push(Token::Rating { min, exact });
                } else {
                    out.push(Token::Word(word));
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }
    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, t: Token) -> bool {
        if self.peek() == Some(&t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `|` level — lowest precedence, left-assoc.
    fn parse_or(&mut self) -> Result<Query, String> {
        let mut parts = vec![self.parse_and()?];
        while self.eat(Token::Pipe) {
            parts.push(self.parse_and()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Query::Or(parts) })
    }

    /// `+` and `-` level. `-` is EXCEPT and is treated at the same
    /// precedence as `+`, left-assoc, so `a + b - c` is `(a + b) - c`.
    /// Whitespace-separated atoms are implicitly AND-ed.
    fn parse_and(&mut self) -> Result<Query, String> {
        let mut parts = vec![self.parse_unary()?];
        loop {
            if self.eat(Token::Plus) {
                parts.push(self.parse_unary()?);
            } else if self.eat(Token::Minus) {
                // EXCEPT absorbs everything after it at this precedence
                let mut exclusions = vec![parts.pop().unwrap_or(Query::AlwaysTrue)];
                exclusions.push(self.parse_unary()?);
                while self.eat(Token::Minus) {
                    exclusions.push(self.parse_unary()?);
                }
                parts.push(Query::Except(exclusions));
            } else if self.atom_starts_here() {
                // Implicit AND: `fox plush` == `fox + plush`.
                parts.push(self.parse_unary()?);
            } else {
                break;
            }
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Query::And(parts) })
    }

    fn atom_starts_here(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::LParen) | Some(Token::Word(_)) | Some(Token::Rating { .. }) | Some(Token::Bang)
        )
    }

    fn parse_unary(&mut self) -> Result<Query, String> {
        if self.eat(Token::Bang) {
            return Ok(Query::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Query, String> {
        match self.advance() {
            Some(Token::LParen) => {
                let q = self.parse_or()?;
                if !self.eat(Token::RParen) {
                    return Err("expected ')'".into());
                }
                Ok(q)
            }
            Some(Token::Word(w)) => {
                if let Some(relation) = w.strip_prefix("rel:") {
                    Ok(Query::Relation(RelationQuery::parse(relation)?))
                } else {
                    Ok(Query::Tag(w))
                }
            }
            Some(Token::Rating { min, exact }) => Ok(Query::Rating { min, exact }),
            Some(t) => Err(format!("unexpected token: {:?}", t)),
            None => Err("unexpected end of query".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> Hierarchy {
        Hierarchy::parse("animal\n\tfox\n\t\tfennec\n\tfeline\n\t\tcat\n")
    }

    fn tag(s: &str) -> HashSet<String> {
        s.split_whitespace().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parse_tag() {
        assert_eq!(Query::parse("fox").unwrap(), Query::Tag("fox".into()));
    }

    #[test]
    fn underscored_query_matches_spaced_tag() {
        let mut tags = HashSet::new();
        tags.insert("derpy hooves".to_string());
        let hierarchy = Hierarchy::parse("MLP\n  derpy hooves\n");

        assert!(Query::parse("derpy_hooves").unwrap().matches(&tags, 0, &hierarchy));
        assert!(Query::parse("MLP").unwrap().matches(&tags, 0, &hierarchy));
    }

    #[test]
    fn wildcard_round_trip_is_always_true() {
        let saved = Query::AlwaysTrue.to_string();
        assert_eq!(saved, "*");
        assert_eq!(Query::parse(&saved).unwrap(), Query::AlwaysTrue);
        assert_eq!(Query::parse("  *  ").unwrap(), Query::AlwaysTrue);
        assert!(Query::AlwaysTrue.matches(&tag(""), 0, &h()));
    }

    #[test]
    fn parse_and() {
        assert_eq!(
            Query::parse("fox + plush").unwrap(),
            Query::And(vec![Query::Tag("fox".into()), Query::Tag("plush".into())])
        );
    }

    #[test]
    fn parse_implicit_and() {
        let a = Query::parse("fox plush").unwrap();
        let b = Query::parse("fox + plush").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn parse_or() {
        assert_eq!(
            Query::parse("fox | wolf").unwrap(),
            Query::Or(vec![Query::Tag("fox".into()), Query::Tag("wolf".into())])
        );
    }

    #[test]
    fn parse_except() {
        assert_eq!(
            Query::parse("fox - plush").unwrap(),
            Query::Except(vec![Query::Tag("fox".into()), Query::Tag("plush".into())])
        );
    }

    #[test]
    fn parse_not() {
        assert_eq!(
            Query::parse("!plush").unwrap(),
            Query::Not(Box::new(Query::Tag("plush".into())))
        );
    }

    #[test]
    fn parse_grouping() {
        assert_eq!(
            Query::parse("(fox | wolf) - plush").unwrap(),
            Query::Except(vec![
                Query::Or(vec![Query::Tag("fox".into()), Query::Tag("wolf".into())]),
                Query::Tag("plush".into()),
            ])
        );
    }

    #[test]
    fn precedence_and_tighter_than_or() {
        // "fox + plush | wolf" == "(fox + plush) | wolf"
        let parsed = Query::parse("fox + plush | wolf").unwrap();
        let expected = Query::Or(vec![
            Query::And(vec![Query::Tag("fox".into()), Query::Tag("plush".into())]),
            Query::Tag("wolf".into()),
        ]);
        assert_eq!(parsed, expected);
    }

    #[test]
    fn hyphen_in_tag_is_allowed() {
        assert_eq!(Query::parse("house-cat").unwrap(), Query::Tag("house-cat".into()));
    }

    #[test]
    fn leading_minus_is_error() {
        // A hyphen inside a word is part of the tag; only a standalone `-` is
        // the EXCEPT operator, and it needs a left operand.
        assert_eq!(Query::parse("fox-plush").unwrap(), Query::Tag("fox-plush".into()));
        assert!(Query::parse("-plush").is_err());
    }

    #[test]
    fn rating_exact() {
        assert_eq!(
            Query::parse("rating:3").unwrap(),
            Query::Rating { min: 3, exact: true }
        );
    }

    #[test]
    fn rating_min() {
        assert_eq!(
            Query::parse("rating:3+").unwrap(),
            Query::Rating { min: 3, exact: false }
        );
    }

    #[test]
    fn evaluate_tag() {
        let hh = h();
        let q = Query::parse("fox").unwrap();
        assert!(q.matches(&tag("fox"), 0, &hh));
        assert!(q.matches(&tag("fox animal"), 0, &hh));
        assert!(!q.matches(&tag("wolf"), 0, &hh));
    }

    #[test]
    fn evaluate_tag_via_descendant() {
        let hh = h();
        let q = Query::parse("animal").unwrap();
        // Tagged fennec → matches animal because fennec is descendant of animal.
        assert!(q.matches(&tag("fennec"), 0, &hh));
        assert!(q.matches(&tag("fennec fox animal"), 0, &hh));
        assert!(!q.matches(&tag("plush"), 0, &hh));
    }

    #[test]
    fn evaluate_except() {
        let hh = h();
        let q = Query::parse("animal - feline").unwrap();
        assert!(q.matches(&tag("fox"), 0, &hh));
        assert!(!q.matches(&tag("cat"), 0, &hh)); // cat is feline
        assert!(!q.matches(&tag("plush"), 0, &hh)); // no animal
    }

    #[test]
    fn evaluate_rating() {
        let hh = h();
        let exact = Query::parse("rating:3").unwrap();
        let min = Query::parse("rating:3+").unwrap();
        assert!(exact.matches(&tag(""), 3, &hh));
        assert!(!exact.matches(&tag(""), 4, &hh));
        assert!(min.matches(&tag(""), 3, &hh));
        assert!(min.matches(&tag(""), 5, &hh));
        assert!(!min.matches(&tag(""), 2, &hh));
    }

    #[test]
    fn confidence_range_gates_suggestions_without_hiding_accepted_tags() {
        let hh = h();
        let q = Query::parse("fox").unwrap();
        let mut evidence = TagEvidence::default();
        evidence.insert_suggested("fox", 0.92);

        assert!(!q.matches_evidence(&evidence, 0, &hh, ConfidenceRange::accepted_only(), ClaimScope::All));
        assert!(q.matches_evidence(&evidence, 0, &hh, ConfidenceRange::new(90, 99), ClaimScope::All));
        assert!(!q.matches_evidence(&evidence, 0, &hh, ConfidenceRange::new(0, 50), ClaimScope::All));

        evidence.insert_accepted("fox");
        assert!(q.matches_evidence(&evidence, 0, &hh, ConfidenceRange::accepted_only(), ClaimScope::All));
        assert!(!q.matches_evidence(&evidence, 0, &hh, ConfidenceRange::new(0, 99), ClaimScope::All));
    }

    #[test]
    fn confidence_range_applies_before_hierarchy_expansion() {
        let hh = h();
        let q = Query::parse("animal").unwrap();
        let mut evidence = TagEvidence::default();
        evidence.insert_suggested("fennec", 0.4);

        assert!(q.matches_evidence(&evidence, 0, &hh, ConfidenceRange::new(0, 49), ClaimScope::All));
        assert!(!q.matches_evidence(&evidence, 0, &hh, ConfidenceRange::new(80, 99), ClaimScope::All));
    }

    #[test]
    fn relation_query_matches_global_relation_on_a_tagged_image() {
        let hh = h();
        let relation = TagRelation::new(
            "global-realizes",
            EntityRef::Tag("theme:ownership".to_string()),
            RelationPredicate::Realizes,
            EntityRef::Tag("act:possession".to_string()),
            AssertionState::Accepted,
            symtropy_tags::Provenance::human(),
            1.0,
        );
        let mut evidence =
            TagEvidence::from_tags(["theme:ownership"]).with_item_id("a.webp");
        evidence.insert_relation(relation.clone(), true);

        let forward = Query::parse("rel:tag:theme:ownership@realizes@tag:act:possession").unwrap();
        let reverse = Query::parse("rel:tag:act:possession@realizes@tag:theme:ownership").unwrap();
        assert!(forward.matches_evidence(&evidence, 0, &hh, ConfidenceRange::accepted_only(), ClaimScope::All));
        assert!(!reverse.matches_evidence(&evidence, 0, &hh, ConfidenceRange::accepted_only(), ClaimScope::All));

        let mut untagged = TagEvidence::default().with_item_id("b.webp");
        untagged.insert_relation(relation, true);
        assert!(!forward.matches_evidence(&untagged, 0, &hh, ConfidenceRange::accepted_only(), ClaimScope::All));
    }

    #[test]
    fn relation_query_matches_item_relations_and_respects_confidence() {
        let hh = h();
        let relation = TagRelation::new(
            "item-created-by",
            EntityRef::Item("a.webp".to_string()),
            RelationPredicate::CreatedBy,
            EntityRef::Tag("artist:kitty".to_string()),
            AssertionState::Suggested,
            symtropy_tags::Provenance::ai("test", "v1"),
            0.4,
        );
        let mut evidence = TagEvidence::default().with_item_id("a.webp");
        evidence.insert_relation(relation, false);

        let query = Query::parse("rel:item:this@created_by@tag:artist:kitty").unwrap();
        assert!(query.matches_evidence(&evidence, 0, &hh, ConfidenceRange::new(0, 49), ClaimScope::All));
        assert!(!query.matches_evidence(&evidence, 0, &hh, ConfidenceRange::accepted_only(), ClaimScope::All));
    }

    #[test]
    fn relation_query_accepts_external_url_endpoints() {
        let query = Query::parse(
            "rel:ext:https://example.test/posts?id=7+8&q=a!b@evidences@item:this",
        )
        .unwrap();
        let Query::Relation(relation) = query else { panic!("expected relation query") };
        assert_eq!(relation.predicate, RelationPredicate::Evidences);
        assert!(matches!(relation.subject, QueryEntity::External(url) if url == "https://example.test/posts?id=7+8&q=a!b"));
        assert!(matches!(relation.object, QueryEntity::CurrentItem));
    }

    #[test]
    fn rejected_relations_are_ignored() {
        let hh = h();
        let relation = TagRelation::new(
            "rejected",
            EntityRef::Item("a.webp".to_string()),
            RelationPredicate::CreatedBy,
            EntityRef::Tag("artist:kitty".to_string()),
            AssertionState::Rejected,
            symtropy_tags::Provenance::human(),
            1.0,
        );
        let mut evidence = TagEvidence::default().with_item_id("a.webp");
        evidence.insert_relation(relation, false);

        let query = Query::parse("rel:item:this@created_by@tag:artist:kitty").unwrap();
        assert!(!query.matches_evidence(&evidence, 0, &hh, ConfidenceRange::all(), ClaimScope::All));
    }

    #[test]
    fn suggestions_never_reach_the_accepted_bucket() {
        let hh = h();
        let mut evidence = TagEvidence::default();
        evidence.insert_suggested("fox", 1.0);

        assert!(evidence.contains_tag("fox", &hh, ConfidenceRange::new(99, 99)));
        assert!(!evidence.contains_tag("fox", &hh, ConfidenceRange::accepted_only()));
    }

    #[test]
    fn claim_scope_separates_curated_facts_from_machine_guesses() {
        let hh = h();
        let mut curated = TagEvidence::from_tags(["fox"]);
        curated.insert_suggested("feline", 0.95);

        assert!(!Query::parse("feline").unwrap().matches_evidence(
            &curated, 0, &hh,
            ConfidenceRange::new(90, 99), ClaimScope::Curated,
        ));
        assert!(Query::parse("feline").unwrap().matches_evidence(
            &curated, 0, &hh,
            ConfidenceRange::new(90, 99), ClaimScope::Machine,
        ));
        assert!(Query::parse("fox").unwrap().matches_evidence(
            &curated, 0, &hh,
            ConfidenceRange::all(), ClaimScope::Curated,
        ));
        assert!(!Query::parse("fox").unwrap().matches_evidence(
            &curated, 0, &hh,
            ConfidenceRange::all(), ClaimScope::Machine,
        ));
    }

    #[test]
    fn namespaced_source_tags_stay_in_the_machine_tier() {
        let hh = h();
        let evidence = TagEvidence::from_tags(["src:e621:possession"]);
        let query = Query::parse("src:e621:possession").unwrap();

        assert!(!query.matches_evidence(
            &evidence, 0, &hh, ConfidenceRange::all(), ClaimScope::Curated,
        ));
        assert!(query.matches_evidence(
            &evidence, 0, &hh, ConfidenceRange::all(), ClaimScope::Machine,
        ));
    }

    #[test]
    fn relations_use_provenance_for_claim_scope() {
        let hh = h();
        let relation = TagRelation::new(
            "machine-created-by",
            EntityRef::Item("a.webp".to_string()),
            RelationPredicate::CreatedBy,
            EntityRef::Tag("artist:kitty".to_string()),
            AssertionState::Accepted,
            symtropy_tags::Provenance::ai("test", "v1"),
            1.0,
        );
        let mut evidence = TagEvidence::default().with_item_id("a.webp");
        evidence.insert_relation(relation, false);
        let query = Query::parse("rel:item:this@created_by@tag:artist:kitty").unwrap();

        assert!(!query.matches_evidence(
            &evidence, 0, &hh, ConfidenceRange::all(), ClaimScope::Curated,
        ));
        assert!(query.matches_evidence(
            &evidence, 0, &hh, ConfidenceRange::all(), ClaimScope::Machine,
        ));
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "fox",
            "fox + plush",
            "fox | wolf",
            "fox - plush",
            "rel:item:this@created_by@tag:artist:kitty",
            "rel:ext:https://example.test/posts?id=7+8&q=a!b@evidences@item:this",
            "!plush",
            "(fox | wolf) - plush",
            "rating:3",
            "rating:3+",
        ] {
            let q = Query::parse(s).unwrap();
            let round = q.to_string();
            let requery = Query::parse(&round).unwrap_or_else(|e| panic!("reparse '{}' failed: {}", round, e));
            assert_eq!(q, requery, "failed round-trip for '{}'", s);
        }
    }
}
