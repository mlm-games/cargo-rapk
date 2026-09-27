//! Maven version ordering, following Apache Maven's `ComparableVersion`.
//!
//! Maven's ordering is not `semver`: qualifiers such as `alpha` sort *below* a
//! release, `1.9.0-alpha01 < 1.9.0`, and unknown qualifiers sort above every
//! known one. Resolution picks the highest version of each artifact, so getting
//! this wrong silently selects the wrong artifact rather than failing.
//!
//! `pubgrub` takes the version type as a generic parameter, so implementing
//! `Ord` here is all that is needed to hand Maven ordering to the solver.

use std::cmp::Ordering;
use std::fmt;

/// Qualifier ranks, lowest first. The empty string is a release, which is why
/// `1.0.0-alpha01` sorts below `1.0.0`. Anything unrecognised sorts above
/// `sp`, matching Maven's fallback of `"<len>-" + qualifier`.
const QUALIFIERS: [&str; 7] = ["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];

/// Spellings Maven treats as the release qualifier.
const RELEASE_ALIASES: [&str; 3] = ["ga", "final", "release"];

/// `cr` is an accepted spelling of `rc`.
const RC_ALIASES: [&str; 1] = ["cr"];

fn qualifier_rank(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    if RELEASE_ALIASES.contains(&lower.as_str()) {
        return Some(QUALIFIERS.iter().position(|q| q.is_empty()).unwrap());
    }
    for (i, q) in QUALIFIERS.iter().enumerate() {
        if *q == lower {
            return Some(i);
        }
    }
    for a in RC_ALIASES {
        if a == lower {
            return Some(QUALIFIERS.iter().position(|q| *q == "rc").unwrap());
        }
    }
    None
}

/// One parsed component of a version.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    /// A numeric component. `digits` keeps the written length so that a longer
    /// run of digits sorts higher, as Maven does for `1.0.0` vs `1.0.0.0`.
    Number { value: u64, digits: usize },
    /// An alphabetic qualifier, with the characters following the qualifier word
    /// (the `01` in `alpha01`).
    Text { qualifier: String, trailing: String },
}

/// A Maven artifact version, comparable with [`Ord`].
#[derive(Debug, Clone)]
pub struct MavenVersion {
    original: String,
    items: Vec<Item>,
}

impl MavenVersion {
    /// Parses a version, never failing: an unparseable component is kept as a
    /// text item so that resolution still has something to order.
    pub fn new(version: &str) -> Self {
        Self {
            original: version.to_owned(),
            items: parse(version),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.original
    }
}

impl PartialEq for MavenVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MavenVersion {}

impl PartialOrd for MavenVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MavenVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        compare(&self.items, &other.items)
    }
}

impl fmt::Display for MavenVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.original)
    }
}

impl std::str::FromStr for MavenVersion {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

/// Splits a version into numeric and qualifier components.
///
/// `.` ends a component; a digit/letter transition starts a new one. Leading
/// zeros are preserved in `digits` rather than dropped.
fn parse(version: &str) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let chars: Vec<char> = version.trim().chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '.' || c == '-' {
            // A separator after a qualifier means the qualifier had no trailing
            // number, which is the common `1.0.0-rc` shape.
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let value = text.parse::<u64>().unwrap_or(u64::MAX);
            items.push(Item::Number {
                value,
                digits: text.len(),
            });
            continue;
        }
        // A qualifier runs until the next digit, so `alpha01` yields the
        // qualifier `alpha` and the number `01`.
        let start = i;
        while i < chars.len() && !chars[i].is_ascii_digit() {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let lowered = word.to_ascii_lowercase();
        match QUALIFIERS
            .iter()
            .position(|q| !q.is_empty() && *q == lowered)
        {
            Some(_) => {
                items.push(Item::Text {
                    qualifier: lowered,
                    trailing: String::new(),
                });
            }
            None => items.push(Item::Text {
                qualifier: word.clone(),
                trailing: String::new(),
            }),
        }
    }
    items
}

/// Orders two qualifier texts. A known qualifier uses its rank; an unknown one
/// sorts above every known qualifier, then lexically, as Maven does.
fn compare_text(a: &str, b: &str) -> Ordering {
    let a_rank = qualifier_rank(a);
    let b_rank = qualifier_rank(b);
    match (a_rank, b_rank) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => {
            // Both unknown: compare on the synthetic "len(QUALIFIERS)-" prefix
            // Maven uses, so the effect is plain lexical order.
            a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase())
        }
    }
}

/// Orders a trailing component against a version that has run out.
///
/// Maven's rule, which is not simply "longer wins": a trailing number is
/// newer, a trailing pre-release qualifier is older, and a trailing *release*
/// qualifier is the same version, so `1.0.0-final` equals `1.0.0`.
fn compare_trailing(item: &Item) -> Ordering {
    match item {
        Item::Number { .. } => Ordering::Greater,
        Item::Text { qualifier, .. } => match qualifier_rank(qualifier) {
            Some(rank) => rank.cmp(&release_rank()),
            None => Ordering::Greater,
        },
    }
}

fn release_rank() -> usize {
    QUALIFIERS.iter().position(|q| q.is_empty()).unwrap()
}

fn compare(a: &[Item], b: &[Item]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let ord = match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) => compare_item(x, y),
            (Some(x), None) => compare_trailing(x),
            (None, Some(y)) => compare_trailing(y).reverse(),
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn compare_item(a: &Item, b: &Item) -> Ordering {
    match (a, b) {
        (
            Item::Number {
                value: av,
                digits: ad,
            },
            Item::Number {
                value: bv,
                digits: bd,
            },
        ) => av.cmp(bv).then_with(|| ad.cmp(bd)),
        (
            Item::Text {
                qualifier: aq,
                trailing: at,
            },
            Item::Text {
                qualifier: bq,
                trailing: bt,
            },
        ) => compare_text(aq, bq).then_with(|| at.cmp(bt)),
        // A number is newer than a qualifier at the same position, which is
        // what puts `1.0.0` above `1.0.0-alpha`.
        (Item::Number { .. }, Item::Text { .. }) => Ordering::Greater,
        (Item::Text { .. }, Item::Number { .. }) => Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    fn ord(a: &str, b: &str) -> Ordering {
        MavenVersion::new(a).cmp(&MavenVersion::new(b))
    }

    #[test]
    fn plain_numeric_versions_order_numerically() {
        assert_eq!(ord("1.13.0", "1.2.0"), Greater);
        assert_eq!(ord("1.0.0", "1.0.0"), Equal);
        assert_eq!(ord("2.6.1", "2.6.0"), Greater);
        assert_eq!(ord("1.8.22", "1.8.9"), Greater);
    }

    #[test]
    fn a_prerelease_sorts_below_its_release() {
        assert_eq!(ord("1.9.0-alpha01", "1.9.0"), Less);
        assert_eq!(ord("1.0.0-rc01", "1.0.0"), Less);
        assert_eq!(ord("1.0.0-SNAPSHOT", "1.0.0"), Less);
    }

    #[test]
    fn qualifiers_order_among_themselves() {
        assert_eq!(ord("1.0.0-alpha01", "1.0.0-beta01"), Less);
        assert_eq!(ord("1.0.0-beta01", "1.0.0-rc01"), Less);
        assert_eq!(ord("1.0.0-rc01", "1.0.0"), Less);
        assert_eq!(ord("1.0.0", "1.0.0-sp1"), Less);
    }

    #[test]
    fn a_trailing_prerelease_does_not_beat_the_release() {
        // The whole point: the resolver must not select `1.0.0-alpha01` over
        // `1.0.0` for the same artifact.
        assert!(MavenVersion::new("1.0.0") > MavenVersion::new("1.0.0-alpha01"));
    }

    #[test]
    fn numeric_trailing_parts_break_ties() {
        assert_eq!(ord("1.0.0-alpha01", "1.0.0-alpha02"), Less);
        assert_eq!(ord("1.0.0-rc1", "1.0.0-rc2"), Less);
        assert_eq!(ord("1.0.0-alpha9", "1.0.0-alpha10"), Less);
    }

    #[test]
    fn release_aliases_compare_equal_to_a_bare_release() {
        assert_eq!(ord("1.0.0", "1.0.0-final"), Equal);
        assert_eq!(ord("1.0.0", "1.0.0-ga"), Equal);
        assert_eq!(ord("1.0.0-rc1", "1.0.0-cr1"), Equal);
    }

    #[test]
    fn ordering_is_antisymmetric_and_reflexive() {
        let versions = [
            "1.0.0",
            "1.0.0-alpha01",
            "1.0.0-beta",
            "1.0.0-sp1",
            "1.13.0",
            "2.6.1",
            "1.8.22",
        ];
        for a in versions {
            for b in versions {
                assert_eq!(ord(a, b), ord(b, a).reverse(), "{a} vs {b}");
            }
            assert_eq!(ord(a, a), Equal);
        }
    }

    #[test]
    fn a_longer_version_sorts_higher() {
        assert_eq!(ord("1.0.0.1", "1.0.0"), Greater);
    }
}
