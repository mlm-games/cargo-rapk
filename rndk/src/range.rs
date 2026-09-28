//! Maven version requirements: exact versions, ranges, and the `LATEST` /
//! `RELEASE` aliases.
//!
//! A dependency does not have to name a version. `[1.0,2.0)` is a range, and
//! anything that is not a well-formed range is a *recommendation*: a version
//! Maven would use if nothing better is available. Choosing between a
//! recommendation and a range needs the repository's version list, so parsing
//! and resolving are separate steps.

use crate::version::MavenVersion;
use std::fmt;

/// One end of a range restriction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bound {
    Unbounded,
    Inclusive(MavenVersion),
    Exclusive(MavenVersion),
}

impl Bound {
    fn accepts_at_or_above(&self, version: &MavenVersion) -> bool {
        match self {
            Self::Unbounded => true,
            Self::Inclusive(min) => version >= min,
            Self::Exclusive(min) => version > min,
        }
    }

    fn accepts_at_or_below(&self, version: &MavenVersion) -> bool {
        match self {
            Self::Unbounded => true,
            Self::Inclusive(max) => version <= max,
            Self::Exclusive(max) => version < max,
        }
    }
}

/// A single interval of a range. `[1.0,2.0)` is one; a range may be a union
/// of several.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restriction {
    lower: Bound,
    upper: Bound,
}

impl Restriction {
    pub fn contains(&self, version: &MavenVersion) -> bool {
        self.lower.accepts_at_or_above(version) && self.upper.accepts_at_or_below(version)
    }
}

/// A union of restrictions, e.g. `[1.0,2.0),[3.0,)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRange {
    restrictions: Vec<Restriction>,
}

impl VersionRange {
    pub fn contains(&self, version: &MavenVersion) -> bool {
        self.restrictions.iter().any(|r| r.contains(version))
    }

    /// Parses a range specification, or `None` if the text is not one.
    ///
    /// A single part between the brackets is both bounds, which is how `[1.0]`
    /// pins exactly one version. Empty parts are unbounded, so `[1.5,)` and
    /// `(,2.0]` are open ended.
    fn parse(spec: &str) -> Option<Self> {
        let mut restrictions = Vec::new();
        let mut rest = spec;
        loop {
            let open = rest.chars().next()?;
            if open != '[' && open != '(' {
                return None;
            }
            let body = rest.get(1..)?;
            let end = body.find([']', ')'])?;
            let close = body.as_bytes()[end];
            let (low, high) = match body[..end].split_once(',') {
                Some((low, high)) => (low.trim(), high.trim()),
                None => (body[..end].trim(), body[..end].trim()),
            };
            restrictions.push(Restriction {
                lower: bound(low, open == '[')?,
                upper: bound(high, close == b']')?,
            });
            match body.as_bytes().get(end + 1) {
                None => break,
                Some(b',') => rest = &body[end + 2..],
                Some(_) => return None,
            }
        }
        (!restrictions.is_empty()).then_some(Self { restrictions })
    }
}

fn bound(text: &str, inclusive: bool) -> Option<Bound> {
    if text.is_empty() {
        return Some(Bound::Unbounded);
    }
    let version = MavenVersion::new(text);
    Some(if inclusive {
        Bound::Inclusive(version)
    } else {
        Bound::Exclusive(version)
    })
}

/// What a dependency asked for.
///
/// Only [`Requirement::Exact`] names a version outright. Everything else is
/// resolved against the repository's version list, which is why a range, a
/// `LATEST` alias and an unclassified specification all need the network before
/// they can be turned into a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    /// A version written in full, e.g. `1.7.0`.
    Exact(MavenVersion),
    /// A version Maven could not classify, e.g. one still holding an
    /// unsubstituted `${property}`. Resolved like a range.
    Soft(MavenVersion),
    /// `[1.0,2.0)`.
    Range(VersionRange),
    /// The most recent version, snapshot or not.
    Latest,
    /// The most recent non-snapshot version.
    Release,
    /// A requirement that also names versions it will not accept, which is GMM's
    /// `rejects`. Gradle enforces these: `DefaultResolvedVersionConstraint
    /// .accepts` filters a candidate through `rejectedVersionsSelector`, and a
    /// static version is rejected too, so a module published only at a rejected
    /// version fails the build rather than being used.
    Rejecting {
        base: Box<Requirement>,
        rejects: Vec<MavenVersion>,
    },
}

impl Requirement {
    /// A requirement carrying a set of rejected versions, as GMM's `rejects`
    /// does.
    pub fn rejecting(spec: &str, rejects: &[String]) -> Self {
        let base = Self::parse(spec);
        if rejects.is_empty() {
            return base;
        }
        Self::Rejecting {
            base: Box::new(base),
            rejects: rejects
                .iter()
                .map(|v| MavenVersion::new(v.trim()))
                .collect(),
        }
    }
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unbounded => Ok(()),
            Self::Inclusive(version) | Self::Exclusive(version) => write!(f, "{version}"),
        }
    }
}

impl fmt::Display for Restriction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let (Bound::Inclusive(low), Bound::Inclusive(high)) = (&self.lower, &self.upper)
            && low == high
        {
            // Maven writes a single pinned version as `[1.0]`, not `[1.0,1.0]`.
            return write!(f, "[{low}]");
        }
        let open = if matches!(self.lower, Bound::Exclusive(_)) {
            "("
        } else {
            "["
        };
        let close = if matches!(self.upper, Bound::Exclusive(_)) {
            ")"
        } else {
            "]"
        };
        match &self.upper {
            Bound::Unbounded => write!(f, "{open}{}", self.lower),
            upper => write!(f, "{open}{},{upper}{close}", self.lower),
        }
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, restriction) in self.restrictions.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            write!(f, "{restriction}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(version) | Self::Soft(version) => f.write_str(version.as_str()),
            Self::Range(range) => write!(f, "{range}"),
            Self::Latest => f.write_str("LATEST"),
            Self::Release => f.write_str("RELEASE"),
            Self::Rejecting { base, rejects } => {
                write!(f, "{base} rejecting ")?;
                let list: Vec<String> = rejects.iter().map(|v| v.to_string()).collect();
                f.write_str(&list.join(", "))
            }
        }
    }
}

impl Requirement {
    pub fn parse(spec: &str) -> Self {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("LATEST") {
            return Self::Latest;
        }
        if spec.eq_ignore_ascii_case("RELEASE") {
            return Self::Release;
        }
        if let Some(range) = VersionRange::parse(spec) {
            return Self::Range(range);
        }
        let version = MavenVersion::new(spec);
        // A specification that is neither a version nor a range is a
        // recommendation: it still holds a `${property}` the POM failed to
        // substitute, or it is malformed, and neither may be treated as a
        // fixed choice that beats a nearer declaration.
        if spec.contains(['$', '[', ']', '(', ')']) {
            Self::Soft(version)
        } else {
            Self::Exact(version)
        }
    }

    /// The version this requirement selects.
    ///
    /// `available` is the repository's version list, which only a soft
    /// requirement needs: an exact version is taken as written, and a missing
    /// artifact then fails at download with a message naming it.
    pub fn resolve(&self, available: &[MavenVersion]) -> Option<MavenVersion> {
        if let Self::Rejecting { base, rejects } = self {
            // An exact version that is rejected leaves no acceptable version at
            // all. Filtering the candidate list and then resolving the base is
            // what lets a range fall back to the next newest rather than to
            // nothing, which is the whole point of a published rejection.
            if let Self::Exact(version) = base.as_ref()
                && rejects.iter().any(|rejected| rejected == version)
            {
                return None;
            }
            let allowed: Vec<MavenVersion> = available
                .iter()
                .filter(|candidate| !rejects.iter().any(|rejected| rejected == *candidate))
                .cloned()
                .collect();
            return base.resolve(&allowed);
        }
        match self {
            Self::Exact(version) => Some(version.clone()),
            Self::Soft(version) => available
                .iter()
                .filter(|candidate| *candidate >= version)
                .min()
                .cloned()
                .or_else(|| Some(version.clone())),
            Self::Range(range) => available
                .iter()
                .filter(|candidate| range.contains(candidate))
                .max()
                .cloned(),
            Self::Latest => available.iter().max().cloned(),
            Self::Release => available
                .iter()
                .filter(|candidate| !is_snapshot(candidate.as_str()))
                .max()
                .cloned(),
            Self::Rejecting { .. } => None,
        }
    }
}

fn is_snapshot(version: &str) -> bool {
    version.to_ascii_uppercase().ends_with("-SNAPSHOT")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<MavenVersion> {
        list.iter().map(|v| MavenVersion::new(v)).collect()
    }

    fn resolved(spec: &str, available: &[&str]) -> Option<String> {
        Requirement::parse(spec)
            .resolve(&versions(available))
            .map(|v| v.as_str().to_owned())
    }

    #[test]
    fn a_bare_version_is_taken_as_written() {
        assert!(matches!(Requirement::parse("1.7.0"), Requirement::Exact(_)));
        assert_eq!(
            resolved("1.7.0", &["1.6.0", "1.8.0"]).as_deref(),
            Some("1.7.0")
        );
    }

    #[test]
    fn brackets_must_balance_or_the_spec_is_a_recommendation() {
        assert!(matches!(
            Requirement::parse("[1.0,2.0)"),
            Requirement::Range(_)
        ));
        assert!(matches!(Requirement::parse("[1.0"), Requirement::Soft(_)));
        assert!(matches!(Requirement::parse("1.0"), Requirement::Exact(_)));
    }

    #[test]
    fn half_open_bounds_exclude_the_upper_version() {
        assert_eq!(
            resolved("[1.0,2.0)", &["0.9", "1.0", "1.9", "2.0"]).as_deref(),
            Some("1.9")
        );
        assert_eq!(
            resolved("(1.0,2.0)", &["1.0", "1.5", "2.0"]).as_deref(),
            Some("1.5")
        );
        assert_eq!(
            resolved("[1.0,2.0]", &["1.0", "2.0", "2.1"]).as_deref(),
            Some("2.0")
        );
    }

    #[test]
    fn a_single_part_range_pins_one_version() {
        assert_eq!(
            resolved("[1.5]", &["1.4", "1.5", "1.6"]).as_deref(),
            Some("1.5")
        );
        assert_eq!(resolved("[1.5]", &["1.4", "1.6"]), None);
    }

    #[test]
    fn an_empty_bound_is_unbounded() {
        assert_eq!(resolved("[1.5,)", &["1.5", "9.0"]).as_deref(), Some("9.0"));
        assert_eq!(
            resolved("(,1.5]", &["0.1", "1.5", "1.6"]).as_deref(),
            Some("1.5")
        );
    }

    #[test]
    fn a_union_takes_the_highest_member() {
        assert_eq!(
            resolved("(,1.0],[1.5,)", &["0.9", "1.2", "1.5", "2.0"]).as_deref(),
            Some("2.0")
        );
        assert_eq!(
            resolved("(,1.0],[1.5,1.7)", &["0.9", "1.2", "1.6"]).as_deref(),
            Some("1.6")
        );
    }

    #[test]
    fn prereleases_never_satisfy_a_release_boundary() {
        // `1.0.0-rc1 < 1.0.0`, so the range ends up picking the release rather
        // than the candidate that a semver comparison would rank lower.
        assert_eq!(
            resolved("[1.0,2.0)", &["1.0.0-rc1", "1.0.0"]).as_deref(),
            Some("1.0.0")
        );
    }

    #[test]
    fn latest_and_release_differ_on_snapshots() {
        let available = ["1.0", "1.1", "1.2-SNAPSHOT"];
        assert_eq!(
            resolved("LATEST", &available).as_deref(),
            Some("1.2-SNAPSHOT")
        );
        assert_eq!(resolved("RELEASE", &available).as_deref(), Some("1.1"));
    }

    #[test]
    fn an_unsubstituted_property_is_a_recommendation_and_never_a_fixed_version() {
        // `${revision}` means interpolation failed, so it must not be treated as
        // a version that was named outright.
        assert!(matches!(
            Requirement::parse("${unresolved.property}"),
            Requirement::Soft(_)
        ));
    }

    #[test]
    fn a_recommendation_falls_back_to_the_nearest_newer_version() {
        let available = ["1.0", "1.1", "1.2"];
        assert_eq!(
            Requirement::Soft(MavenVersion::new("1.0.5"))
                .resolve(&versions(&available))
                .map(|v| v.as_str().to_owned()),
            Some("1.1".to_owned())
        );
        assert_eq!(
            Requirement::Soft(MavenVersion::new("1.0"))
                .resolve(&versions(&available))
                .map(|v| v.as_str().to_owned()),
            Some("1.0".to_owned())
        );
    }
}
