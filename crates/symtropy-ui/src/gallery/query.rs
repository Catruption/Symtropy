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

use super::Hierarchy;
use std::collections::HashSet;
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    Tag(String),
    Rating { min: u8, exact: bool },
    Not(Box<Query>),
    And(Vec<Query>),
    Or(Vec<Query>),
    Except(Vec<Query>),
    AlwaysTrue,
}

impl Query {
    /// Parse `input`. Empty input is `AlwaysTrue`.
    pub fn parse(input: &str) -> Result<Self, String> {
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
    pub fn matches(&self, applied: &HashSet<String>, rating: u8, hierarchy: &Hierarchy) -> bool {
        match self {
            Query::Tag(t) => {
                // Directly tagged or tagged with any descendant of `t`.
                applied.contains(t)
                    || applied
                        .iter()
                        .any(|a| hierarchy.is_descendant_of(a, t))
            }
            Query::Rating { min, exact } => {
                if *exact {
                    rating == *min
                } else {
                    rating >= *min
                }
            }
            Query::Not(q) => !q.matches(applied, rating, hierarchy),
            Query::And(qs) => qs.iter().all(|q| q.matches(applied, rating, hierarchy)),
            Query::Or(qs) => qs.iter().any(|q| q.matches(applied, rating, hierarchy)),
            Query::Except(qs) => {
                let Some(first) = qs.first() else { return true };
                first.matches(applied, rating, hierarchy)
                    && qs[1..].iter().all(|q| !q.matches(applied, rating, hierarchy))
            }
            Query::AlwaysTrue => true,
        }
    }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Query::Tag(t) => write!(f, "{}", t),
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
        Query::AlwaysTrue | Query::Tag(_) | Query::Rating { .. } => 4,
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
                    if c.is_whitespace() || "()|+!".contains(c) {
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
            Some(Token::Word(w)) => Ok(Query::Tag(w)),
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
    fn bare_minus_is_error() {
        assert!(Query::parse("fox-plush").is_err());
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
    fn display_round_trips() {
        for s in [
            "fox",
            "fox + plush",
            "fox | wolf",
            "fox - plush",
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
