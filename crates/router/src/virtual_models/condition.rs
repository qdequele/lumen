//! `switch` conditions (ADR 014): compiled once per load, evaluated per
//! request against a [`FactSource`] without allocating.

use lumen_core::Capability;
use regex::Regex;
use serde_json::{Map, Value};

/// What a condition can look at. Implemented by the server per request;
/// every method is cheap, and `input_tokens` is only called when a condition
/// references it.
pub trait FactSource {
    /// The authenticated key's budget group (ADR 009).
    fn group(&self) -> Option<&str>;
    /// A top-level field of the request metadata header (ADR 002).
    fn metadata(&self, key: &str) -> Option<&Value>;
    /// Whether the request carries an image part (chat, embed).
    fn has_images(&self) -> bool;
    /// Whether the chat request declares tools.
    fn has_tools(&self) -> bool;
    /// Whether the chat request streams.
    fn stream(&self) -> bool;
    /// The ADR 003 input-token estimate.
    fn input_tokens(&self) -> u64;
    /// The rerank document count.
    fn documents(&self) -> Option<u64>;
}

/// A compiled `when` table.
#[derive(Debug, Clone)]
pub enum Condition {
    /// Every condition holds (the keys of one table).
    All(Vec<Condition>),
    /// At least one holds (`any = [...]`).
    Any(Vec<Condition>),
    /// One attribute test.
    Test(Attr, Op),
}

/// A condition attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attr {
    /// `group`.
    Group,
    /// `metadata.<key>`.
    Metadata(String),
    /// `has_images`.
    HasImages,
    /// `has_tools`.
    HasTools,
    /// `stream`.
    Stream,
    /// `input_tokens`.
    InputTokens,
    /// `documents`.
    Documents,
}

/// A comparison.
#[derive(Debug, Clone)]
pub enum Op {
    /// Bare value.
    Eq(Scalar),
    /// `{ ne = v }`.
    Ne(Scalar),
    /// `{ in = [...] }`.
    In(Vec<Scalar>),
    /// `{ gt = n }`.
    Gt(f64),
    /// `{ gte = n }`.
    Gte(f64),
    /// `{ lt = n }`.
    Lt(f64),
    /// `{ lte = n }`.
    Lte(f64),
    /// `{ regex = "..." }`, compiled at load.
    Regex(Regex),
}

/// A literal in a condition.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    /// A string.
    Str(String),
    /// A number.
    Num(f64),
    /// A boolean.
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Bool,
    Num,
    Any,
}

impl Attr {
    fn parse(key: &str) -> Result<Self, String> {
        Ok(match key {
            "group" => Self::Group,
            "has_images" => Self::HasImages,
            "has_tools" => Self::HasTools,
            "stream" => Self::Stream,
            "input_tokens" => Self::InputTokens,
            "documents" => Self::Documents,
            other => match other.strip_prefix("metadata.") {
                Some(k) if !k.is_empty() => Self::Metadata(k.to_owned()),
                _ => {
                    return Err(format!(
                        "unknown condition `{key}` (expected group, metadata.<key>, has_images, \
                         has_tools, stream, input_tokens, documents or any)"
                    ))
                }
            },
        })
    }

    const fn kind(&self) -> Kind {
        match self {
            Self::Group => Kind::Str,
            Self::Metadata(_) => Kind::Any,
            Self::HasImages | Self::HasTools | Self::Stream => Kind::Bool,
            Self::InputTokens | Self::Documents => Kind::Num,
        }
    }

    fn applies_to(&self, capability: Capability) -> bool {
        match self {
            Self::Group | Self::Metadata(_) => true,
            Self::HasImages => matches!(capability, Capability::Chat | Capability::Embed),
            Self::HasTools | Self::Stream => capability == Capability::Chat,
            Self::InputTokens => matches!(
                capability,
                Capability::Chat | Capability::Embed | Capability::Rerank
            ),
            Self::Documents => capability == Capability::Rerank,
        }
    }
}

impl Condition {
    /// Compile a `when` table for a virtual model serving `capability`.
    ///
    /// # Errors
    /// A human-readable reason naming the offending key.
    pub fn compile(when: &Map<String, Value>, capability: Capability) -> Result<Self, String> {
        if when.is_empty() {
            return Err("`when` must not be empty".to_owned());
        }
        let mut all = Vec::with_capacity(when.len());
        for (key, value) in when {
            if key == "any" {
                let Value::Array(items) = value else {
                    return Err("`any` must be an array of condition tables".to_owned());
                };
                if items.is_empty() {
                    return Err("`any` must not be empty".to_owned());
                }
                let mut any = Vec::with_capacity(items.len());
                for item in items {
                    let Value::Object(table) = item else {
                        return Err("`any` must be an array of condition tables".to_owned());
                    };
                    any.push(Self::compile(table, capability)?);
                }
                all.push(Self::Any(any));
                continue;
            }
            let attr = Attr::parse(key)?;
            if !attr.applies_to(capability) {
                return Err(format!(
                    "condition `{key}` does not apply to {capability} requests"
                ));
            }
            let op = compile_op(attr.kind(), key, value)?;
            all.push(Self::Test(attr, op));
        }
        Ok(Self::All(all))
    }

    /// Whether the request described by `facts` satisfies the condition.
    #[must_use]
    pub fn matches(&self, facts: &dyn FactSource) -> bool {
        match self {
            Self::All(items) => items.iter().all(|c| c.matches(facts)),
            Self::Any(items) => items.iter().any(|c| c.matches(facts)),
            Self::Test(attr, op) => op.test(&fact(attr, facts)),
        }
    }
}

fn compile_op(kind: Kind, key: &str, value: &Value) -> Result<Op, String> {
    let Value::Object(table) = value else {
        return Ok(Op::Eq(scalar(kind, key, value)?));
    };
    let mut entries = table.iter();
    let (Some((op, operand)), None) = (entries.next(), entries.next()) else {
        return Err(format!(
            "`{key}`: an operator table takes exactly one of in, ne, gt, gte, lt, lte, regex"
        ));
    };
    match op.as_str() {
        "in" => {
            let Value::Array(items) = operand else {
                return Err(format!("`{key}`: `in` takes an array"));
            };
            if items.is_empty() {
                return Err(format!("`{key}`: `in` must not be empty"));
            }
            items
                .iter()
                .map(|item| scalar(kind, key, item))
                .collect::<Result<Vec<_>, _>>()
                .map(Op::In)
        }
        "ne" => Ok(Op::Ne(scalar(kind, key, operand)?)),
        "gt" | "gte" | "lt" | "lte" => {
            if !matches!(kind, Kind::Num | Kind::Any) {
                return Err(format!("`{key}`: `{op}` needs a numeric attribute"));
            }
            let n = operand
                .as_f64()
                .ok_or_else(|| format!("`{key}`: `{op}` takes a number"))?;
            Ok(match op.as_str() {
                "gt" => Op::Gt(n),
                "gte" => Op::Gte(n),
                "lt" => Op::Lt(n),
                _ => Op::Lte(n),
            })
        }
        "regex" => {
            if !matches!(kind, Kind::Str | Kind::Any) {
                return Err(format!("`{key}`: `regex` needs a string attribute"));
            }
            let pattern = operand
                .as_str()
                .ok_or_else(|| format!("`{key}`: `regex` takes a string"))?;
            Regex::new(pattern)
                .map(Op::Regex)
                .map_err(|e| format!("`{key}`: invalid regex: {e}"))
        }
        other => Err(format!("`{key}`: unknown operator `{other}`")),
    }
}

fn scalar(kind: Kind, key: &str, value: &Value) -> Result<Scalar, String> {
    let s = match value {
        Value::String(s) => Scalar::Str(s.clone()),
        Value::Bool(b) => Scalar::Bool(*b),
        Value::Number(n) => Scalar::Num(
            n.as_f64()
                .ok_or_else(|| format!("`{key}`: number out of range"))?,
        ),
        _ => return Err(format!("`{key}`: expected a string, number or boolean")),
    };
    let fits = matches!(
        (kind, &s),
        (Kind::Any, _)
            | (Kind::Str, Scalar::Str(_))
            | (Kind::Bool, Scalar::Bool(_))
            | (Kind::Num, Scalar::Num(_))
    );
    if fits {
        Ok(s)
    } else {
        Err(format!("`{key}`: value has the wrong type"))
    }
}

enum Fact<'a> {
    Str(&'a str),
    Num(f64),
    Bool(bool),
    Missing,
}

#[allow(clippy::cast_precision_loss)] // token and document counts far below 2^52
fn fact<'a>(attr: &Attr, facts: &'a dyn FactSource) -> Fact<'a> {
    match attr {
        Attr::Group => facts.group().map_or(Fact::Missing, Fact::Str),
        Attr::Metadata(key) => match facts.metadata(key) {
            Some(Value::String(s)) => Fact::Str(s),
            Some(Value::Number(n)) => n.as_f64().map_or(Fact::Missing, Fact::Num),
            Some(Value::Bool(b)) => Fact::Bool(*b),
            _ => Fact::Missing,
        },
        Attr::HasImages => Fact::Bool(facts.has_images()),
        Attr::HasTools => Fact::Bool(facts.has_tools()),
        Attr::Stream => Fact::Bool(facts.stream()),
        Attr::InputTokens => Fact::Num(facts.input_tokens() as f64),
        Attr::Documents => facts
            .documents()
            .map_or(Fact::Missing, |d| Fact::Num(d as f64)),
    }
}

impl Op {
    fn test(&self, fact: &Fact<'_>) -> bool {
        if matches!(fact, Fact::Missing) {
            return false;
        }
        let num = || match fact {
            Fact::Num(n) => Some(*n),
            _ => None,
        };
        match self {
            Self::Eq(s) => equals(fact, s),
            Self::Ne(s) => !equals(fact, s),
            Self::In(items) => items.iter().any(|s| equals(fact, s)),
            Self::Gt(n) => num().is_some_and(|x| x > *n),
            Self::Gte(n) => num().is_some_and(|x| x >= *n),
            Self::Lt(n) => num().is_some_and(|x| x < *n),
            Self::Lte(n) => num().is_some_and(|x| x <= *n),
            Self::Regex(re) => matches!(fact, Fact::Str(s) if re.is_match(s)),
        }
    }
}

fn equals(fact: &Fact<'_>, scalar: &Scalar) -> bool {
    match (fact, scalar) {
        (Fact::Str(a), Scalar::Str(b)) => *a == b,
        (Fact::Bool(a), Scalar::Bool(b)) => a == b,
        (Fact::Num(a), Scalar::Num(b)) => (a - b).abs() < f64::EPSILON,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Default)]
    struct Stub {
        group: Option<String>,
        meta: Map<String, Value>,
        images: bool,
        tools: bool,
        stream: bool,
        tokens: u64,
        docs: Option<u64>,
        token_reads: std::cell::Cell<u32>,
    }

    impl FactSource for Stub {
        fn group(&self) -> Option<&str> {
            self.group.as_deref()
        }
        fn metadata(&self, key: &str) -> Option<&Value> {
            self.meta.get(key)
        }
        fn has_images(&self) -> bool {
            self.images
        }
        fn has_tools(&self) -> bool {
            self.tools
        }
        fn stream(&self) -> bool {
            self.stream
        }
        fn input_tokens(&self) -> u64 {
            self.token_reads.set(self.token_reads.get() + 1);
            self.tokens
        }
        fn documents(&self) -> Option<u64> {
            self.docs
        }
    }

    fn when(v: Value) -> Map<String, Value> {
        let Value::Object(table) = v else {
            panic!("a `when` table is a JSON object");
        };
        table
    }

    #[test]
    fn equality_on_group_and_metadata() {
        let c = Condition::compile(
            &when(json!({ "group": "eu", "metadata.plan": "pro" })),
            Capability::Chat,
        )
        .unwrap();
        let mut s = Stub {
            group: Some("eu".into()),
            ..Stub::default()
        };
        s.meta.insert("plan".into(), json!("pro"));
        assert!(c.matches(&s));
        s.meta.insert("plan".into(), json!("free"));
        assert!(!c.matches(&s));
    }

    #[test]
    fn operators_in_ne_numeric_regex_any() {
        let c = Condition::compile(
            &when(json!({ "metadata.region": { "in": ["eu", "uk"] } })),
            Capability::Chat,
        )
        .unwrap();
        let mut s = Stub::default();
        s.meta.insert("region".into(), json!("uk"));
        assert!(c.matches(&s));

        let c = Condition::compile(
            &when(json!({ "input_tokens": { "gt": 32000 } })),
            Capability::Chat,
        )
        .unwrap();
        assert!(c.matches(&Stub {
            tokens: 40_000,
            ..Stub::default()
        }));
        assert!(!c.matches(&Stub {
            tokens: 100,
            ..Stub::default()
        }));

        let c = Condition::compile(
            &when(json!({ "group": { "regex": "^tenant-[0-9]+$" } })),
            Capability::Chat,
        )
        .unwrap();
        assert!(c.matches(&Stub {
            group: Some("tenant-42".into()),
            ..Stub::default()
        }));

        let c = Condition::compile(
            &when(json!({ "any": [ { "has_images": true }, { "has_tools": true } ] })),
            Capability::Chat,
        )
        .unwrap();
        assert!(c.matches(&Stub {
            tools: true,
            ..Stub::default()
        }));
        assert!(!c.matches(&Stub::default()));

        let c = Condition::compile(&when(json!({ "group": { "ne": "eu" } })), Capability::Chat)
            .unwrap();
        assert!(c.matches(&Stub {
            group: Some("us".into()),
            ..Stub::default()
        }));
    }

    #[test]
    fn a_missing_attribute_is_false_even_for_ne() {
        let c = Condition::compile(&when(json!({ "group": { "ne": "eu" } })), Capability::Chat)
            .unwrap();
        assert!(!c.matches(&Stub::default()));
        let c =
            Condition::compile(&when(json!({ "metadata.plan": "pro" })), Capability::Chat).unwrap();
        assert!(!c.matches(&Stub::default()));
    }

    #[test]
    fn input_tokens_is_only_read_when_referenced() {
        let s = Stub::default();
        let c = Condition::compile(&when(json!({ "stream": false })), Capability::Chat).unwrap();
        assert!(c.matches(&s));
        assert_eq!(s.token_reads.get(), 0);
    }

    #[test]
    fn compile_errors_name_the_problem() {
        let e = |v: Value, cap| Condition::compile(&when(v), cap).unwrap_err();
        assert!(e(json!({}), Capability::Chat).contains("must not be empty"));
        assert!(e(json!({ "colour": "red" }), Capability::Chat).contains("unknown condition"));
        assert!(e(json!({ "has_tools": true }), Capability::Rerank).contains("does not apply"));
        assert!(e(json!({ "documents": 3 }), Capability::Chat).contains("does not apply"));
        assert!(e(json!({ "group": 3 }), Capability::Chat).contains("wrong type"));
        assert!(e(json!({ "group": { "gt": 3 } }), Capability::Chat).contains("numeric"));
        assert!(e(json!({ "group": { "regex": "(" } }), Capability::Chat).contains("invalid regex"));
        assert!(e(json!({ "group": { "in": "eu" } }), Capability::Chat).contains("array"));
        assert!(e(
            json!({ "group": { "eq": "eu", "ne": "x" } }),
            Capability::Chat
        )
        .contains("exactly one"));
        assert!(e(json!({ "metadata.": "x" }), Capability::Chat).contains("unknown condition"));
    }
}
