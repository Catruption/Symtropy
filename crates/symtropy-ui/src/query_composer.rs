//! Prompt-style query composer.
//!
//! This is a frontend over [`crate::gallery::Query`]. Users type intent the way
//! they would describe an image to generate ("orange cat on a crate,
//! possessive mood, by kitty"); the composer lowers that into the same AST the
//! exact query language uses. Chips in the UI expose the generated terms so the
//! result is transparent and each phrase can be removed.
//!
//! The exact DSL remains available unchanged for power users. Nothing here
//! writes tags or relations: it only builds a search expression.

use crate::gallery::query::{ConfidenceRange, Query, QueryEntity, RelationQuery};
use crate::gallery::TagNode;
use std::ops::Range;
use symtropy_tags::RelationPredicate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptJoin {
    First,
    And,
    Or,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PromptChip {
    /// Cursor span of the source phrase, used to remove the chip from the prompt.
    pub span: Range<usize>,
    /// Human-readable phrase, e.g. `possessive mood`.
    pub label: String,
    /// Generated DSL term, e.g. `theme:ownership`.
    pub exact: String,
    pub query: Query,
    pub join: PromptJoin,
    pub negated: bool,
}

#[derive(Clone, Debug)]
pub struct PromptParse {
    pub chips: Vec<PromptChip>,
    pub query: Query,
    /// `maybe`/`weak` and `confirmed` adjust the searchable confidence bucket.
    pub confidence_hint: Option<ConfidenceRange>,
}

impl PromptParse {
    /// Number of distinct terms the composer recognized, for UI feedback.
    pub fn term_count(&self) -> usize {
        self.chips.len()
    }
}

impl Default for PromptParse {
    fn default() -> Self {
        Self {
            chips: Vec::new(),
            query: Query::AlwaysTrue,
            confidence_hint: None,
        }
    }
}

#[derive(Clone, Debug)]
struct Word {
    text: String,
    lower: String,
    start: usize,
    end: usize,
}

impl Word {
    fn is(&self, value: &str) -> bool {
        self.lower == value
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | ':' | '-' | '.' | '/' | '@' | '%' | '\'')
}

fn lex(input: &str) -> Vec<Word> {
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if matches!(c, '+' | ',' | ';' | '!') {
            out.push(Word { text: c.to_string(), lower: c.to_string(), start, end: start + c.len_utf8() });
            i += 1;
            continue;
        }
        if c == '-' {
            let prev_word = i > 0 && is_word_char(chars[i - 1].1);
            let next_word = i + 1 < chars.len() && is_word_char(chars[i + 1].1);
            if !prev_word && !next_word {
                out.push(Word { text: "-".into(), lower: "-".into(), start, end: start + 1 });
                i += 1;
                continue;
            }
        }
        let mut text = String::new();
        let mut end = start;
        while i < chars.len() && is_word_char(chars[i].1) {
            end = chars[i].0 + chars[i].1.len_utf8();
            text.push(chars[i].1);
            i += 1;
        }
        if text.is_empty() {
            end = start + c.len_utf8();
            text.push(c);
            i += 1;
        }
        let lower = text.to_ascii_lowercase();
        out.push(Word { text, lower, start, end });
    }
    out
}

fn phrase_key(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(
        |c: char| c.is_whitespace() || c == '-',
        "_",
    )
}

/// Minimal intent vocabulary. The live tag catalog is checked first, so any
/// real tag name always wins over these conveniences.
const ALIASES: &[(&str, &str)] = &[
    ("orange", "feature:color:dominant_orange"),
    ("red", "feature:color:dominant_red"),
    ("blue", "feature:color:dominant_blue"),
    ("green", "feature:color:dominant_green"),
    ("yellow", "feature:color:dominant_yellow"),
    ("purple", "feature:color:dominant_purple"),
    ("pink", "feature:color:dominant_pink"),
    ("black", "feature:color:dominant_black"),
    ("white", "feature:color:dominant_white"),
    ("grey", "feature:color:dominant_grey"),
    ("gray", "feature:color:dominant_grey"),
    ("brown", "feature:color:dominant_brown"),
    ("dark", "feature:tone:dark"),
    ("bright", "feature:tone:bright"),
    ("moody", "feature:tone:moody"),
    ("wide", "feature:aspect:landscape"),
    ("landscape", "feature:aspect:landscape"),
    ("tall", "feature:aspect:portrait"),
    ("portrait", "feature:aspect:portrait"),
    ("square", "feature:aspect:square"),
    ("cat", "feline"),
    ("cats", "feline"),
    ("kitty", "feline"),
    ("kitties", "feline"),
    ("fox", "fox"),
    ("foxes", "fox"),
    ("crate", "object:crate"),
    ("box", "object:box"),
    ("possessive", "theme:ownership"),
    ("possessive_mood", "theme:ownership"),
    ("possessive_feeling", "theme:ownership"),
    ("ownership", "theme:ownership"),
    ("possession", "act:possession"),
    ("possessed", "state:possessed"),
];

/// Tag ids the composer itself understands, so a local query planner is allowed
/// to use them even when they are not present in the current tag tree.
pub fn builtin_tag_ids() -> Vec<String> {
    ALIASES.iter().map(|(_, tag)| (*tag).to_string()).collect()
}

fn is_article(word: &str) -> bool {
    matches!(word, "a" | "an" | "the" | "some")
}

fn is_connector(word: &str) -> bool {
    matches!(word, "," | ";" | "+" | "|" | "and" | "or")
}

fn is_preposition(word: &str) -> bool {
    matches!(
        word,
        "by" | "on" | "near" | "beside" | "with" | "in" | "at" | "inside" | "part" | "like"
            | "showing" | "depicts" | "depicting" | "evidence" | "evidences"
    )
}

fn is_noise(word: &str) -> bool {
    matches!(
        word,
        "please" | "show" | "find" | "me" | "image" | "images" | "picture" | "pictures"
    )
}

struct Composer<'a> {
    words: Vec<Word>,
    catalog: &'a [String],
    pos: usize,
    join: PromptJoin,
    negate: bool,
    chips: Vec<PromptChip>,
    query: Query,
    confidence_hint: Option<ConfidenceRange>,
}

impl<'a> Composer<'a> {
    fn new(input: &str, catalog: &'a [String]) -> Self {
        Self {
            words: lex(input),
            catalog,
            pos: 0,
            join: PromptJoin::First,
            negate: false,
            chips: Vec::new(),
            query: Query::AlwaysTrue,
            confidence_hint: None,
        }
    }

    fn resolve(&self, phrase: &str) -> String {
        let key = phrase_key(phrase);
        if let Some(tag) = self.catalog.iter().find(|tag| {
            let candidate = crate::gallery::hierarchy::tag_key(tag);
            candidate.eq_ignore_ascii_case(&key)
        }) {
            return crate::gallery::hierarchy::tag_key(tag);
        }
        if let Some((_, tag)) = ALIASES.iter().find(|(alias, _)| *alias == key) {
            return (*tag).to_string();
        }
        key
    }

    fn artist_tag(&self, phrase: &str) -> String {
        let key = phrase_key(phrase);
        if key.contains(':') {
            return key;
        }
        format!("artist:{key}")
    }

    fn push(&mut self, query: Query, label: String, span: Range<usize>, join: PromptJoin, negated: bool) {
        let query = if negated { Query::Not(Box::new(query)) } else { query };
        let exact = query.to_string();
        let chip = PromptChip { span, label, exact, query: query.clone(), join, negated };
        self.chips.push(chip);

        let current = std::mem::replace(&mut self.query, Query::AlwaysTrue);
        self.query = match join {
            PromptJoin::First => query,
            PromptJoin::And => match current {
                Query::And(mut parts) => {
                    parts.push(query);
                    Query::And(parts)
                }
                other => Query::And(vec![other, query]),
            },
            PromptJoin::Or => match current {
                Query::Or(mut parts) => {
                    parts.push(query);
                    Query::Or(parts)
                }
                other => Query::Or(vec![other, query]),
            },
        };
        self.join = PromptJoin::And;
        self.negate = false;
    }

    fn collect_phrase(&mut self, mut pos: usize) -> (String, Range<usize>, usize) {
        let mut start = None;
        let mut end = 0;
        let mut parts = Vec::new();
        while pos < self.words.len() {
            let word = &self.words[pos];
            if is_connector(&word.lower) || is_preposition(&word.lower) {
                break;
            }
            if is_article(&word.lower) {
                pos += 1;
                continue;
            }
            if start.is_none() {
                start = Some(word.start);
            }
            end = word.end;
            parts.push(word.text.clone());
            pos += 1;
        }
        let span = start.map_or(0..0, |start| start..end);
        (parts.join(" "), span, pos)
    }

    fn relation(&mut self, predicate: RelationPredicate, preposition: &Word, mut pos: usize) -> usize {
        if matches!(predicate, RelationPredicate::PartOf) && pos < self.words.len() && self.words[pos].is("of") {
            pos += 1;
        }
        if matches!(predicate, RelationPredicate::Evidences) && pos < self.words.len() && self.words[pos].is("of") {
            pos += 1;
        }
        let (phrase, phrase_span, next) = self.collect_phrase(pos);
        if phrase.is_empty() {
            return pos.max(next);
        }
        let tag = if predicate == RelationPredicate::CreatedBy {
            self.artist_tag(&phrase)
        } else {
            self.resolve(&phrase)
        };
        let span = preposition.start..phrase_span.end;
        let query = Query::Relation(RelationQuery {
            subject: QueryEntity::CurrentItem,
            predicate,
            object: QueryEntity::Tag(tag),
        });
        let label = format!("{} {}", preposition.text, phrase);
        let join = self.join;
        let negate = self.negate;
        self.push(query, label, span, join, negate);
        next
    }

    fn descriptor(&mut self) {
        let word = self.words[self.pos].clone();
        let mut span = word.start..word.end;
        let mut phrase = word.text.clone();
        let mut words_used = 1;

        // Prefer the longest catalog/alias phrase: "rainbow dash" and
        // "possessive mood" are one chip, while "orange cat" stays two.
        let limit = (self.words.len() - self.pos).min(8);
        for count in 1..=limit {
            let slice = &self.words[self.pos..self.pos + count];
            if slice.iter().any(|w| is_connector(&w.lower) || is_preposition(&w.lower) || is_noise(&w.lower)) {
                break;
            }
            let candidate = slice.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            let key = phrase_key(&candidate);
            let known = self.catalog.iter().any(|tag| {
                crate::gallery::hierarchy::tag_key(tag).eq_ignore_ascii_case(&key)
            }) || ALIASES.iter().any(|(alias, _)| *alias == key);
            if known && count >= words_used {
                phrase = candidate;
                span = slice[0].start..slice[count - 1].end;
                words_used = count;
            }
        }

        let tag = self.resolve(&phrase);
        let query = Query::Tag(tag);
        let join = self.join;
        let negate = self.negate;
        self.push(query, phrase, span, join, negate);
        self.pos += words_used;
    }

    fn rating(&mut self, exact: bool, number: &Word, star: &Word) {
        let Ok(min) = number.text.trim_end_matches('+').parse::<u8>() else {
            self.descriptor();
            return;
        };
        if min > 5 {
            self.descriptor();
            return;
        }
        let query = Query::Rating { min, exact };
        let label = if exact { format!("exactly {min} stars") } else { format!("{min}+ stars") };
        let join = self.join;
        let negate = self.negate;
        self.push(query, label, number.start..star.end, join, negate);
        self.pos += 2;
    }

    fn run(mut self) -> PromptParse {
        while self.pos < self.words.len() {
            let word = self.words[self.pos].clone();
            match word.lower.as_str() {
                "," | ";" | "+" | "|" | "and" => {
                    self.join = PromptJoin::And;
                    self.pos += 1;
                }
                "or" => {
                    self.join = PromptJoin::Or;
                    self.pos += 1;
                }
                "!" | "not" | "no" | "without" | "-" => {
                    self.negate = true;
                    self.pos += 1;
                }
                "maybe" | "possibly" | "guess" | "weak" => {
                    self.confidence_hint = Some(ConfidenceRange::new(50, 99));
                    self.pos += 1;
                }
                "confirmed" | "accepted" | "definitely" => {
                    self.confidence_hint = Some(ConfidenceRange::accepted_only());
                    self.pos += 1;
                }
                "exactly" if self.pos + 2 < self.words.len() => {
                    let number = self.words[self.pos + 1].clone();
                    let star = self.words[self.pos + 2].clone();
                    if number.text.chars().all(|c| c.is_ascii_digit()) && matches!(star.lower.as_str(), "star" | "stars") {
                        self.pos += 1;
                        self.rating(true, &number, &star);
                    } else {
                        self.pos += 1;
                    }
                }
                "by" => {
                    let start = self.pos;
                    self.pos += 1;
                    let next = self.relation(RelationPredicate::CreatedBy, &word, self.pos);
                    if next == self.pos {
                        self.pos = start + 1;
                    } else {
                        self.pos = next;
                    }
                }
                "on" | "near" | "beside" | "with" => {
                    let start = self.pos;
                    self.pos += 1;
                    let next = self.relation(RelationPredicate::AdjacentTo, &word, self.pos);
                    if next == self.pos {
                        self.pos = start + 1;
                    } else {
                        self.pos = next;
                    }
                }
                "in" | "at" | "inside" => {
                    let start = self.pos;
                    self.pos += 1;
                    let next = self.relation(RelationPredicate::LocatedAt, &word, self.pos);
                    if next == self.pos {
                        self.pos = start + 1;
                    } else {
                        self.pos = next;
                    }
                }
                "showing" | "depicts" | "depicting" => {
                    let start = self.pos;
                    self.pos += 1;
                    let next = self.relation(RelationPredicate::Depicts, &word, self.pos);
                    if next == self.pos {
                        self.pos = start + 1;
                    } else {
                        self.pos = next;
                    }
                }
                "evidence" | "evidences" => {
                    let start = self.pos;
                    self.pos += 1;
                    let next = self.relation(RelationPredicate::Evidences, &word, self.pos);
                    if next == self.pos {
                        self.pos = start + 1;
                    } else {
                        self.pos = next;
                    }
                }
                "part" => {
                    let start = self.pos;
                    self.pos += 1;
                    let next = self.relation(RelationPredicate::PartOf, &word, self.pos);
                    if next == self.pos {
                        self.pos = start + 1;
                    } else {
                        self.pos = next;
                    }
                }
                "like" => {
                    let start = self.pos;
                    self.pos += 1;
                    let next = self.relation(RelationPredicate::MapsTo, &word, self.pos);
                    if next == self.pos {
                        self.pos = start + 1;
                    } else {
                        self.pos = next;
                    }
                }
                _ if is_article(&word.lower) || is_noise(&word.lower) => self.pos += 1,
                _ => {
                    let next = self.words.get(self.pos + 1).cloned();
                    let after_next = self.words.get(self.pos + 2).cloned();
                    if let Some(star) = next
                        .as_ref()
                        .filter(|w| matches!(w.lower.as_str(), "star" | "stars"))
                    {
                        if word.text.chars().all(|c| c.is_ascii_digit()) || word.text.ends_with('+') {
                            self.rating(false, &word, star);
                            continue;
                        }
                    }
                    if next.as_ref().is_some_and(|w| w.is("+"))
                        && after_next.as_ref().is_some_and(|w| matches!(w.lower.as_str(), "star" | "stars"))
                        && word.text.chars().all(|c| c.is_ascii_digit())
                    {
                        let star = after_next.unwrap();
                        self.rating(false, &word, &star);
                        self.pos += 1;
                        continue;
                    }
                    self.descriptor();
                }
            }
        }
        PromptParse { chips: self.chips, query: self.query, confidence_hint: self.confidence_hint }
    }
}

/// Compile a natural-language prompt into the gallery query AST.
pub fn compile(input: &str, catalog: &[String]) -> PromptParse {
    Composer::new(input, catalog).run()
}

/// Compile a prompt against a tag tree's catalog.
pub fn compile_tree(input: &str, tree: &TagNode) -> PromptParse {
    let catalog = crate::sidebar::tag_catalog(tree);
    compile(input, &catalog)
}

/// Remove a chip's source phrase and tidy the remaining connectors.
#[cfg(test)]
pub fn remove_span(input: &str, span: &Range<usize>) -> String {
    if span.start > span.end || span.end > input.len() {
        return input.to_string();
    }
    let mut out = format!("{}{}", &input[..span.start], &input[span.end..]);
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    out = out.replace(" ,", ",").replace(" ;", ";");
    let trimmed = out.trim().trim_matches(|c: char| matches!(c, '+' | '|' | ',' | ';')).trim();
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<String> {
        vec![
            "feline".into(), "house-cat".into(), "fox".into(), "rainbow dash".into(),
            "soft".into(), "artist".into(),
        ]
    }

    #[test]
    fn lowers_image_prompt_into_tags_and_relations() {
        let parsed = compile(
            "orange cat on a crate, possessive mood, by kitty",
            &catalog(),
        );
        let exact: Vec<&str> = parsed.chips.iter().map(|c| c.exact.as_str()).collect();
        assert_eq!(
            exact,
            vec![
                "feature:color:dominant_orange",
                "feline",
                "rel:item:this@adjacent_to@tag:object:crate",
                "theme:ownership",
                "rel:item:this@created_by@tag:artist:kitty",
            ]
        );
        assert!(matches!(parsed.query, Query::And(_)));
    }

    #[test]
    fn or_and_negation_are_query_operators() {
        let parsed = compile("not fluffy or red fox", &catalog());
        assert_eq!(parsed.chips[0].exact, "!fluffy");
        assert_eq!(parsed.chips[0].join, PromptJoin::First);
        assert_eq!(parsed.chips[1].join, PromptJoin::Or);
        assert!(matches!(parsed.query, Query::And(_)));
        assert!(matches!(&parsed.query, Query::And(parts) if matches!(parts.first(), Some(Query::Or(_)))));
    }

    #[test]
    fn maybe_sets_suggestion_confidence_hint() {
        let parsed = compile("maybe fox", &catalog());
        assert_eq!(parsed.confidence_hint, Some(ConfidenceRange::new(50, 99)));
        assert_eq!(parsed.chips[0].exact, "fox");
    }

    #[test]
    fn catalog_names_keep_spaces_as_query_tokens() {
        let parsed = compile("rainbow dash", &catalog());
        assert_eq!(parsed.chips[0].exact, "rainbow_dash");
    }

    #[test]
    fn chip_removal_drops_only_its_phrase() {
        let prompt = "orange cat on a crate, by kitty";
        let parsed = compile(prompt, &catalog());
        let crate_chip = parsed.chips.iter().find(|c| c.exact.contains("object:crate")).unwrap();
        let removed = remove_span(prompt, &crate_chip.span);
        assert_eq!(removed, "orange cat, by kitty");
    }

    #[test]
    fn natural_language_rating_becomes_a_rating_leaf() {
        let parsed = compile("exactly 4 stars fox", &catalog());
        assert_eq!(parsed.chips[0].exact, "rating:4");
        assert_eq!(parsed.chips[1].exact, "fox");
    }
}
