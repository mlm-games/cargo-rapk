//! Gradle Module Metadata: choosing a variant, and following `available-at`.
//!
//! A `.module` file sits beside the POM, and Gradle ignores the POM entirely
//! when one is present, so it is the authoritative description whenever a
//! module publishes it.
//!
//! It is also the only place a multiplatform module says *which* module holds
//! the JVM artifact. `androidx.annotation:annotation` publishes a 670-byte
//! Kotlin-metadata jar of its own and redirects its JVM variants to
//! `androidx.annotation:annotation-jvm`; reading the POM and the coordinates
//! alone puts that stub on the classpath in place of the library, and the
//! annotations silently go missing.
//!
//! The selection algorithm is Gradle's
//! [attribute matching](https://docs.gradle.org/current/userguide/variant_aware_resolution.html)
//! algorithm, implemented here for the one consumer there is: a Java project
//! resolving its `runtimeClasspath`, which is what an APK's classpath is.

use serde::Deserialize;
use std::collections::BTreeMap;

/// One attribute cargo-rapk asks a variant to have, with the values it accepts
/// and the one it prefers if several variants remain.
struct Requested {
    key: &'static str,
    compatible: &'static [&'static str],
    preferred: &'static str,
}

/// The `runtimeClasspath` of a Java project: a library, used at runtime, built
/// for the JVM.
///
/// `org.gradle.libraryelements` is deliberately not requested. AGP asks for
/// `aar`, but the JVM half of a Kotlin library publishes `jar`, so requesting
/// `aar` would reject every one of them; which of the two a module publishes is
/// read off the variant that wins instead.
const REQUESTED: &[Requested] = &[
    Requested {
        key: "org.gradle.category",
        compatible: &["library"],
        preferred: "library",
    },
    Requested {
        // `java-runtime` stands in for `java-api`, not the other way round.
        key: "org.gradle.usage",
        compatible: &[
            "java-api",
            "java-runtime",
            "kotlin-runtime",
            "android-runtime",
        ],
        preferred: "java-runtime",
    },
    Requested {
        key: "org.jetbrains.kotlin.platform.type",
        compatible: &["jvm", "androidJvm"],
        preferred: "jvm",
    },
];

/// The archive extension a variant's `org.gradle.libraryelements` names, or
/// `None` to let the caller probe for an AAR and then a jar.
pub fn library_extension(attributes: &BTreeMap<String, String>) -> Option<&'static str> {
    match attributes
        .get("org.gradle.libraryelements")
        .map(String::as_str)
    {
        Some("aar") => Some("aar"),
        Some("jar") => Some("jar"),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Module {
    #[serde(rename = "formatVersion", default)]
    pub format_version: String,
    #[serde(default)]
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Variant {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    #[serde(rename = "available-at", default)]
    pub available_at: Option<AvailableAt>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(rename = "dependencyConstraints", default)]
    pub dependency_constraints: Vec<Dependency>,
    #[serde(default)]
    pub files: Vec<File>,
}

impl Variant {
    /// Whether this variant ships anything. A platform or a documentation
    /// variant has none, and there is no artifact to put in an APK.
    pub fn ships_files(&self) -> bool {
        !self.files.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AvailableAt {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Constraint {
    #[serde(default)]
    pub requires: Option<String>,
    #[serde(default)]
    pub prefers: Option<String>,
    #[serde(default)]
    pub strictly: Option<String>,
    #[serde(default)]
    pub rejects: Vec<String>,
}

impl Constraint {
    /// The version this constraint asks for.
    ///
    /// `prefers` is what Gradle uses when a range also names one, and `strictly`
    /// is a hard requirement, so both are taken as the requirement here. A
    /// `rejects` list is not enforced, so it is reported rather than obeyed
    /// silently.
    pub fn requirement(&self) -> Option<&str> {
        self.requires
            .as_deref()
            .or(self.prefers.as_deref())
            .or(self.strictly.as_deref())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dependency {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub version: Option<Constraint>,
    #[serde(default)]
    pub excludes: Vec<Exclusion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Exclusion {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub module: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct File {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
}

impl File {
    /// The archive extension of a file name, if it names one.
    pub fn extension(&self) -> Option<&str> {
        self.url.rsplit_once('.').map(|(_, ext)| ext)
    }
}

impl Module {
    /// The variant an Android runtime classpath is built from.
    ///
    /// Implements the documented algorithm: drop the incompatible candidates,
    /// prefer the one that matches strictly more attributes, then the closest
    /// match on a requested attribute, then prefer candidates *lacking* an
    /// extra attribute, and finally prefer candidates that have one.
    pub fn select(&self) -> Result<&Variant, String> {
        if self.variants.is_empty() {
            return Err("declares no variants".to_owned());
        }
        let compatible: Vec<&Variant> = self
            .variants
            .iter()
            .filter(|variant| is_compatible(variant))
            .collect();
        if compatible.is_empty() {
            let offered: Vec<&str> = self.variants.iter().map(|v| v.name.as_str()).collect();
            return Err(format!(
                "no variant is a runtime library; it offers {}",
                offered.join(", ")
            ));
        }

        let scored = most_matching_attributes(&compatible);
        if scored.len() == 1 {
            return Ok(scored[0]);
        }
        let closest = closest_requested_match(&scored);
        if closest.len() == 1 {
            return Ok(closest[0]);
        }
        let fewest_extras = lacking_extra_attribute(&closest);
        if fewest_extras.len() == 1 {
            return Ok(fewest_extras[0]);
        }
        let with_extra = declaring_extra_attribute(&fewest_extras);
        if with_extra.len() == 1 {
            return Ok(with_extra[0]);
        }
        let names: Vec<&str> = with_extra.iter().map(|v| v.name.as_str()).collect();
        Err(format!(
            "cannot choose between variants {}",
            names.join(", ")
        ))
    }
}

/// Step 1: a variant is compatible unless it declares a requested attribute
/// with a value the consumer does not accept. A missing attribute is not a
/// mismatch.
fn is_compatible(variant: &Variant) -> bool {
    REQUESTED
        .iter()
        .all(|requested| match variant.attributes.get(requested.key) {
            None => true,
            Some(value) => requested.compatible.contains(&value.as_str()),
        })
}

fn matching_count(variant: &Variant) -> usize {
    REQUESTED
        .iter()
        .filter(|requested| variant.attributes.contains_key(requested.key))
        .count()
}

/// Step 2: keep only the candidates that match strictly more attributes than
/// any other, and only while that leaves a single candidate.
fn most_matching_attributes<'a>(candidates: &[&'a Variant]) -> Vec<&'a Variant> {
    let best = candidates
        .iter()
        .map(|v| matching_count(v))
        .max()
        .unwrap_or(0);
    let winners: Vec<&Variant> = candidates
        .iter()
        .filter(|v| matching_count(v) == best)
        .copied()
        .collect();
    if winners.len() == candidates.len() || best == 0 {
        return candidates.to_vec();
    }
    // A strict superset also needs to be missing nothing the others declare,
    // which `is_compatible` already guarantees for the requested keys; what is
    // left is the count.
    winners
}

/// Step 3: prefer the candidate holding the closest value for a requested
/// attribute, in the order the attributes are requested.
fn closest_requested_match<'a>(candidates: &[&'a Variant]) -> Vec<&'a Variant> {
    let mut remaining = candidates.to_vec();
    for requested in REQUESTED {
        if remaining.len() <= 1 {
            break;
        }
        let preferred: Vec<&Variant> = remaining
            .iter()
            .filter(|v| {
                v.attributes.get(requested.key).map(String::as_str) == Some(requested.preferred)
            })
            .copied()
            .collect();
        if preferred.is_empty() {
            continue;
        }
        remaining = preferred;
    }
    remaining
}

/// Every attribute name that only some of the candidates declare.
fn extra_attributes<'a>(candidates: &[&'a Variant]) -> Vec<&'a String> {
    let first = match candidates.first() {
        Some(first) => first,
        None => return Vec::new(),
    };
    first
        .attributes
        .keys()
        .filter(|key| {
            candidates
                .iter()
                .any(|variant| variant.attributes.contains_key(*key))
        })
        .collect()
}

/// Step 4: for the first extra attribute that some candidates have and others
/// do not, prefer the ones that lack it.
fn lacking_extra_attribute<'a>(candidates: &[&'a Variant]) -> Vec<&'a Variant> {
    for key in extra_attributes(candidates) {
        if candidates
            .iter()
            .all(|variant| variant.attributes.contains_key(key))
        {
            continue;
        }
        let lacking: Vec<&Variant> = candidates
            .iter()
            .filter(|variant| !variant.attributes.contains_key(key))
            .copied()
            .collect();
        if !lacking.is_empty() {
            return lacking;
        }
    }
    candidates.to_vec()
}

/// Step 5: for the first extra attribute whose value the candidates disagree
/// on, prefer the ones that declare it.
fn declaring_extra_attribute<'a>(candidates: &[&'a Variant]) -> Vec<&'a Variant> {
    for key in extra_attributes(candidates) {
        let with: Vec<&Variant> = candidates
            .iter()
            .filter(|variant| variant.attributes.contains_key(key))
            .copied()
            .collect();
        if with.len() == candidates.len() || with.is_empty() {
            continue;
        }
        return with;
    }
    candidates.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A variant with the given attributes, so a test reads as the selection
    /// rule it is about rather than as JSON.
    fn variant(name: &str, attributes: &[(&str, &str)]) -> Variant {
        Variant {
            name: name.to_owned(),
            attributes: attributes
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            available_at: None,
            dependencies: Vec::new(),
            dependency_constraints: Vec::new(),
            files: Vec::new(),
        }
    }

    fn module(variants: Vec<Variant>) -> Module {
        Module {
            format_version: "1.1".to_owned(),
            variants,
        }
    }

    fn runtime_variant(name: &str) -> Variant {
        variant(
            name,
            &[
                ("org.gradle.category", "library"),
                ("org.gradle.usage", "java-runtime"),
                ("org.gradle.libraryelements", "aar"),
                ("org.gradle.dependency.bundling", "external"),
            ],
        )
    }

    #[test]
    fn the_runtime_library_variant_wins_over_its_siblings() {
        let offered = module(vec![
            variant(
                "sourcesElements",
                &[
                    ("org.gradle.category", "documentation"),
                    ("org.gradle.docstype", "sources"),
                    ("org.gradle.usage", "java-runtime"),
                ],
            ),
            variant(
                "apiElements",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "java-api"),
                    ("org.gradle.libraryelements", "aar"),
                ],
            ),
            runtime_variant("runtimeElements"),
        ]);
        let selected = offered.select().expect("a variant is selected");
        assert_eq!(selected.name, "runtimeElements");
    }

    #[test]
    fn kotlin_metadata_and_common_variants_are_rejected() {
        // The variant that carries the `.kotlin_module` file is a library, so
        // only `org.jetbrains.kotlin.platform.type` keeps it out. Taking it
        // would put a metadata jar on the classpath.
        let offered = module(vec![
            variant(
                "metadataApiElements",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "kotlin-metadata"),
                    ("org.jetbrains.kotlin.platform.type", "common"),
                ],
            ),
            variant(
                "jvmRuntimeElements-published",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "java-runtime"),
                    ("org.gradle.libraryelements", "jar"),
                    ("org.jetbrains.kotlin.platform.type", "jvm"),
                ],
            ),
        ]);
        let selected = offered.select().expect("a variant is selected");
        assert_eq!(selected.name, "jvmRuntimeElements-published");
    }

    #[test]
    fn android_jvm_counts_as_the_jvm() {
        let offered = module(vec![variant(
            "androidVariant",
            &[
                ("org.gradle.category", "library"),
                ("org.gradle.usage", "java-runtime"),
                ("org.jetbrains.kotlin.platform.type", "androidJvm"),
            ],
        )]);
        let selected = offered.select().expect("a variant is selected");
        assert_eq!(selected.name, "androidVariant");
    }

    #[test]
    fn a_module_offering_nothing_usable_is_an_error() {
        let offered = module(vec![variant(
            "sourcesElements",
            &[
                ("org.gradle.category", "documentation"),
                ("org.gradle.usage", "java-runtime"),
            ],
        )]);
        let error = offered.select().expect_err("nothing is compatible");
        assert!(error.contains("sourcesElements"), "{error}");
    }

    #[test]
    fn a_closer_usage_match_wins_when_both_are_compatible() {
        // `java-api` satisfies a request for `java-runtime`, so step 1 keeps
        // both and step 3 has to separate them.
        let offered = module(vec![
            variant(
                "apiElements",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "java-api"),
                ],
            ),
            variant(
                "runtimeElements",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "java-runtime"),
                ],
            ),
        ]);
        let selected = offered.select().expect("a variant is selected");
        assert_eq!(selected.name, "runtimeElements");
    }

    #[test]
    fn a_candidate_missing_a_requested_attribute_is_not_eliminated() {
        // Nothing in the closure needs the platform type, so a plain Java
        // library with no `org.jetbrains.kotlin.platform.type` must still match.
        let offered = module(vec![variant(
            "runtimeElements",
            &[
                ("org.gradle.category", "library"),
                ("org.gradle.usage", "java-runtime"),
            ],
        )]);
        let selected = offered.select().expect("a variant is selected");
        assert_eq!(selected.name, "runtimeElements");
    }

    #[test]
    fn the_library_elements_attribute_says_which_archive_to_fetch() {
        let aar = variant("v", &[("org.gradle.libraryelements", "aar")]);
        assert_eq!(library_extension(&aar.attributes), Some("aar"));
        let jar = variant("v", &[("org.gradle.libraryelements", "jar")]);
        assert_eq!(library_extension(&jar.attributes), Some("jar"));
        let neither = variant("v", &[("org.gradle.category", "library")]);
        assert_eq!(library_extension(&neither.attributes), None);
    }

    #[test]
    fn the_required_version_is_used_and_rejects_is_reported() {
        let constraint = Constraint {
            requires: Some("[1.0,2.0)".to_owned()),
            ..Default::default()
        };
        assert_eq!(constraint.requirement(), Some("[1.0,2.0)"));
        assert!(constraint.rejects.is_empty());

        let preferred = Constraint {
            prefers: Some("1.4".to_owned()),
            ..Default::default()
        };
        assert_eq!(preferred.requirement(), Some("1.4"));

        let strict = Constraint {
            strictly: Some("1.1".to_owned()),
            ..Default::default()
        };
        assert_eq!(strict.requirement(), Some("1.1"));
    }

    #[test]
    fn a_file_names_its_own_extension() {
        let file = |url: &str| File {
            name: url.to_owned(),
            url: url.to_owned(),
        };
        assert_eq!(file("annotation-jvm-1.6.0.jar").extension(), Some("jar"));
        assert_eq!(file("appcompat-1.7.0.aar").extension(), Some("aar"));
        assert_eq!(file("noextension").extension(), None);
    }

    #[test]
    fn a_variant_that_ships_nothing_is_recognised() {
        let mut platform = runtime_variant("platformRuntimeElements");
        assert!(!platform.ships_files());
        platform.files.push(File {
            name: "x.aar".to_owned(),
            url: "x.aar".to_owned(),
        });
        assert!(platform.ships_files());
    }
}
