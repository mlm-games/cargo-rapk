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
use std::collections::{BTreeMap, BTreeSet};

/// One attribute cargo-rapk asks a variant to have, with the values it accepts
/// and the one it prefers if several variants remain.
struct Requested {
    key: &'static str,
    compatible: &'static [&'static str],
    preferred: &'static str,
    /// The attribute names a *minimum* JVM version, where any version up to the
    /// consumer's own is usable and the highest of those is the closest match.
    /// Every other attribute is a fixed set of acceptable values.
    jvm_version: bool,
}

/// The `runtimeClasspath` of a Java project: a library, used at runtime, built
/// for the JVM.
///
/// `org.gradle.libraryelements` is deliberately not requested. AGP asks for
/// `aar`, but the JVM half of a Kotlin library publishes `jar`, so requesting
/// `aar` would reject every one of them; which of the two a module publishes is
/// read off the variant that wins instead.
/// In Gradle's disambiguation precedence, which is the order a requested
/// attribute is settled in when several variants remain. `org.jetbrains.kotlin.
/// platform.type` is not in it, so it is settled after every one of these.
const REQUESTED: &[Requested] = &[
    Requested {
        key: "org.gradle.category",
        compatible: &["library"],
        preferred: "library",
        jvm_version: false,
    },
    Requested {
        // Gradle's `UsageCompatibilityRules` accepts a `java-runtime` producer
        // for a `java-api` request but not the reverse, so `java-api` is *not*
        // among the values a runtime classpath accepts. Reading it as a plain
        // set of acceptable values accepted `apiElements` on a runtime
        // classpath, which ships a compile-only artifact.
        key: "org.gradle.usage",
        compatible: &["java-runtime", "kotlin-runtime", "android-runtime"],
        preferred: "java-runtime",
        jvm_version: false,
    },
    Requested {
        // A library built for Java 11 runs on a Java 17 JDK, but not on a
        // Java 8 one, so a variant asking for more than this build has is
        // rejected rather than reached past.
        key: "org.gradle.jvm.version",
        compatible: &[],
        preferred: "",
        jvm_version: true,
    },
    Requested {
        key: "org.jetbrains.kotlin.platform.type",
        compatible: &["jvm", "androidJvm"],
        preferred: "jvm",
        jvm_version: false,
    },
];

/// The highest Java version a resolved variant may require. The JDK the build
/// already needs for `javac` and `d8` is the one whose classes have to load.
pub fn target_jvm_version() -> u32 {
    std::env::var("CARGO_RAPK_JVM_VERSION")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(crate::ndk::Ndk::java_version)
        .unwrap_or(8)
}

/// The `org.gradle.usage` a variant declares.
///
/// Gradle's `UsageCompatibilityHandler` rewrites a deprecated
/// `java-runtime-jars` into `java-runtime` plus `org.gradle.libraryelements=jar`
/// before anything is matched, so a module still published the old way is a
/// runtime variant rather than one that matches nothing.
fn usage_of(variant: &Variant) -> Option<String> {
    let usage = variant.attributes.get("org.gradle.usage")?;
    Some(
        ["-jars", "-classes", "-resources"]
            .into_iter()
            .find_map(|suffix| usage.as_str().strip_suffix(suffix))
            .unwrap_or(usage.as_str())
            .to_owned(),
    )
}

/// The `org.gradle.jvm.version` a variant declares, as a number.
///
/// A writer may spell Java 8 as `1.8`, which is the same version and parses as
/// nothing otherwise — leaving the variant looking like one that states no
/// requirement, so it is never rejected and never preferred.
fn declared_jvm_version(variant: &Variant) -> Option<u32> {
    let value = variant.attributes.get("org.gradle.jvm.version")?.as_str();
    if let Ok(major) = value.parse::<u32>() {
        return Some(major);
    }
    value
        .strip_prefix("1.")
        .and_then(|rest| rest.split(['.', '-']).next())
        .and_then(|major| major.parse().ok())
}

/// The archive extension a variant's `org.gradle.libraryelements` names, or
/// `None` to let the caller probe for an AAR and then a jar.
pub fn library_extension(attributes: &BTreeMap<String, Value>) -> Option<&'static str> {
    match attributes
        .get("org.gradle.libraryelements")
        .map(|v| v.as_str())
    {
        Some("aar") => Some("aar"),
        Some("jar") => Some("jar"),
        _ => None,
    }
}

/// An attribute or size value. The specification allows "a string, a boolean,
/// or an integer" for an attribute, and real modules use all three —
/// `androidx.annotation:annotation:1.3.0` writes `"org.gradle.jvm.version": 8`
/// unquoted — so a string-only field silently discards that module's whole
/// metadata and falls back to its POM.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Value(pub String);

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Scalar;
        impl serde::de::Visitor<'_> for Scalar {
            type Value = Value;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string, a boolean or a number")
            }
            fn visit_str<E>(self, v: &str) -> Result<Value, E> {
                Ok(Value(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<Value, E> {
                Ok(Value(v))
            }
            fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
                Ok(Value(v.to_string()))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
                Ok(Value(v.to_string()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
                Ok(Value(v.to_string()))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
                Ok(Value(v.to_string()))
            }
        }
        deserializer.deserialize_any(Scalar)
    }
}

impl Value {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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
    pub attributes: BTreeMap<String, Value>,
    #[serde(rename = "available-at", default)]
    pub available_at: Option<AvailableAt>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(rename = "dependencyConstraints", default)]
    pub dependency_constraints: Vec<Dependency>,
    #[serde(default)]
    pub files: Vec<File>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
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

/// A `(group, name, version)` triple a variant provides, or a dependency
/// requires of the variant it resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, PartialOrd, Ord)]
pub struct Capability {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.group, self.name, self.version)
    }
}

impl Capability {
    /// The capability a component provides implicitly, from its own coordinates.
    pub fn implicit(group: &str, name: &str, version: &str) -> Self {
        Self {
            group: group.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }
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
    /// Capabilities the resolved variant must provide for this dependency to be
    /// usable. A variant that provides none of them is not a candidate.
    #[serde(rename = "requestedCapabilities", default)]
    pub requested_capabilities: Vec<Capability>,
    #[serde(rename = "thirdPartyCompatibility", default)]
    pub third_party_compatibility: Option<ThirdPartyCompatibility>,
}

/// Applies to a dependency on a module that published no metadata of its own.
#[derive(Debug, Clone, Deserialize)]
pub struct ThirdPartyCompatibility {
    #[serde(rename = "artifactSelector", default)]
    pub artifact_selector: Option<ArtifactSelector>,
}

/// Which file to take from a module, for a module whose metadata does not say.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ArtifactSelector {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub classifier: Option<String>,
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
        self.select_for_jvm(target_jvm_version())
    }

    /// The variant that coordinates versions instead of shipping files.
    ///
    /// `select` asks for `org.gradle.category=library` and so deliberately
    /// rejects a platform, which would leave this module looking like nothing it
    /// could use. A platform is therefore found on its own terms, subject to the
    /// same JVM and usage requirements.
    pub fn platform_variant(&self, target: u32) -> Option<&Variant> {
        self.variants.iter().find(|variant| {
            variant
                .attributes
                .get("org.gradle.category")
                .is_some_and(|category| category.as_str() == "platform")
                && is_compatible(variant, target, false)
        })
    }

    /// As [`Module::select`], against a known Java version.
    ///
    /// A variant that needs a newer JVM than the one the build will use cannot
    /// have its classes loaded, so it is not a candidate however well its other
    /// attributes match.
    pub fn select_for_jvm(&self, target: u32) -> Result<&Variant, String> {
        if self.variants.is_empty() {
            return Err("declares no variants".to_owned());
        }
        let compatible: Vec<&Variant> = self
            .variants
            .iter()
            .filter(|variant| is_compatible(variant, target, true))
            .collect();
        if compatible.is_empty() {
            let offered: Vec<&str> = self.variants.iter().map(|v| v.name.as_str()).collect();
            return Err(format!(
                "no variant is a runtime library; it offers {}",
                offered.join(", ")
            ));
        }

        // Gradle's `disambiguateCompatibleCandidates` in order: settle the
        // requested attributes, then prefer a variant that declares an extra
        // attribute, then one that lacks it. There is deliberately no
        // "matches the most attributes" step, because Gradle has none: a
        // compile-only `apiElements` carries more requested attributes than the
        // `runtimeElements` beside it, and counting them picked the wrong one.
        let mut remaining = compatible;
        if remaining.len() > 1 {
            remaining = closest_requested_match(&remaining, target);
        }
        if remaining.len() > 1 {
            remaining = declaring_extra_attribute(&remaining);
        }
        if remaining.len() > 1 {
            remaining = lacking_extra_attribute(&remaining);
        }
        if remaining.len() == 1 {
            return Ok(remaining[0]);
        }
        let with_extra = remaining;
        let names: Vec<&str> = with_extra.iter().map(|v| v.name.as_str()).collect();
        Err(format!(
            "cannot choose between variants {}",
            names.join(", ")
        ))
    }
}

/// A variant is compatible unless it declares a requested attribute
/// with a value the consumer does not accept. A missing attribute is not a
/// mismatch.
fn is_compatible(variant: &Variant, target: u32, require_library: bool) -> bool {
    REQUESTED.iter().all(|requested| {
        if requested.jvm_version {
            return declared_jvm_version(variant).is_none_or(|needed| needed <= target);
        }
        if !require_library && requested.key == "org.gradle.category" {
            return true;
        }
        let value = if requested.key == "org.gradle.usage" {
            usage_of(variant)
        } else {
            variant
                .attributes
                .get(requested.key)
                .map(|v| v.as_str().to_owned())
        };
        match &value {
            None => true,
            Some(value) => requested.compatible.contains(&value.as_str()),
        }
    })
}

/// Settle each requested attribute in turn, in Gradle's disambiguation
/// precedence, keeping only the candidates holding the preferred value.
fn closest_requested_match<'a>(candidates: &[&'a Variant], _target: u32) -> Vec<&'a Variant> {
    let mut remaining = candidates.to_vec();
    for requested in REQUESTED {
        if remaining.len() <= 1 {
            break;
        }
        // A variant that targets the newest JVM this build can run is the
        // closest match, the same rule the other attributes follow.
        if requested.jvm_version {
            let best = remaining
                .iter()
                .filter_map(|v| declared_jvm_version(v))
                .max();
            if let Some(best) = best {
                let with: Vec<&Variant> = remaining
                    .iter()
                    .filter(|v| declared_jvm_version(v) == Some(best))
                    .copied()
                    .collect();
                if !with.is_empty() && with.len() < remaining.len() {
                    remaining = with;
                }
            }
            continue;
        }
        let preferred: Vec<&Variant> = remaining
            .iter()
            .filter(|v| {
                if requested.key == "org.gradle.usage" {
                    usage_of(v).as_deref() == Some(requested.preferred)
                } else {
                    v.attributes.get(requested.key).map(|a| a.as_str()) == Some(requested.preferred)
                }
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
///
/// Gradle's `collectExtraAttributes` unions over *all* candidates and drops any
/// name every candidate has, since a shared attribute says nothing about which
/// to prefer. Reading it off one candidate missed a name that only a later
/// candidate carried, which is how two variants of a shadow-jar publisher ended
/// up indistinguishable.
fn extra_attributes(candidates: &[&Variant]) -> BTreeSet<String> {
    let mut shared: BTreeSet<String> = candidates
        .first()
        .map(|first| first.attributes.keys().cloned().collect())
        .unwrap_or_default();
    for variant in &candidates[1..] {
        shared = shared
            .intersection(&variant.attributes.keys().cloned().collect())
            .cloned()
            .collect();
    }
    candidates
        .iter()
        .flat_map(|variant| variant.attributes.keys())
        .filter(|key| !shared.contains(*key))
        .cloned()
        .collect()
}

/// Step 3: for the first extra attribute only some candidates declare, prefer
/// the ones that declare it.
///
/// Gradle's `disambiguateWithExtraAttributes` runs the attribute's
/// disambiguation rule with no consumer value and keeps whatever the rule
/// matches, which discards the candidates with no value for it. So the variant
/// carrying the attribute is preferred over the one that omits it.
fn declaring_extra_attribute<'a>(candidates: &[&'a Variant]) -> Vec<&'a Variant> {
    for key in extra_attributes(candidates) {
        let with: Vec<&Variant> = candidates
            .iter()
            .filter(|variant| variant.attributes.contains_key(&key))
            .copied()
            .collect();
        if with.len() == candidates.len() || with.is_empty() {
            continue;
        }
        return with;
    }
    candidates.to_vec()
}

/// Step 4: for the first extra attribute still undecided, prefer the ones that
/// lack it, since an extra attribute is a weaker match than a requested one.
fn lacking_extra_attribute<'a>(candidates: &[&'a Variant]) -> Vec<&'a Variant> {
    for key in extra_attributes(candidates) {
        if candidates
            .iter()
            .all(|variant| variant.attributes.contains_key(&key))
        {
            continue;
        }
        let lacking: Vec<&Variant> = candidates
            .iter()
            .filter(|variant| !variant.attributes.contains_key(&key))
            .copied()
            .collect();
        if !lacking.is_empty() {
            return lacking;
        }
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
                .map(|(key, value)| ((*key).to_owned(), Value((*value).to_owned())))
                .collect(),
            available_at: None,
            dependencies: Vec::new(),
            dependency_constraints: Vec::new(),
            files: Vec::new(),
            capabilities: Vec::new(),
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
    fn a_runtime_classpath_rejects_a_compile_only_variant() {
        // Gradle's `UsageCompatibilityRules` accepts a `java-runtime` producer
        // for a `java-api` request but not the reverse, so `apiElements` is
        // incompatible with a runtime classpath however many of the requested
        // attributes it happens to match. Reading usage as a flat set of
        // acceptable values kept it, and `apiElements` matches four of the
        // requested attributes to `runtimeElements`'s two.
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
    fn the_jvm_version_a_variant_needs_bounds_which_ones_are_usable() {
        let offered = module(vec![
            variant(
                "jdk8Elements",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "java-runtime"),
                    ("org.gradle.jvm.version", "8"),
                ],
            ),
            variant(
                "jdk11Elements",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "java-runtime"),
                    ("org.gradle.jvm.version", "11"),
                ],
            ),
            variant(
                "jdk21Elements",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "java-runtime"),
                    ("org.gradle.jvm.version", "21"),
                ],
            ),
        ]);
        let selected = offered.select_for_jvm(17).expect("a variant is selected");
        assert_eq!(
            selected.name, "jdk11Elements",
            "the highest version this JVM can run, and not 21"
        );
    }

    #[test]
    fn nothing_usable_when_every_variant_needs_a_newer_jvm() {
        let offered = module(vec![variant(
            "jdk21Elements",
            &[
                ("org.gradle.category", "library"),
                ("org.gradle.usage", "java-runtime"),
                ("org.gradle.jvm.version", "21"),
            ],
        )]);
        let error = offered
            .select_for_jvm(8)
            .expect_err("nothing runs on Java 8");
        assert!(error.contains("jdk21Elements"), "{error}");
    }

    #[test]
    fn a_capability_and_an_artifact_selector_are_read_from_the_metadata() {
        let parsed: Module = serde_json::from_str(
            r#"{
                "formatVersion": "1.1",
                "variants": [{
                    "name": "runtimeElements",
                    "attributes": { "org.gradle.category": "library" },
                    "capabilities": [
                        { "group": "org.example", "name": "native", "version": "1.2" }
                    ],
                    "dependencies": [{
                        "group": "com.example", "module": "toolchain",
                        "version": { "requires": "3.0" },
                        "requestedCapabilities": [
                            { "group": "org.example", "name": "native", "version": "1.2" }
                        ],
                        "thirdPartyCompatibility": {
                            "artifactSelector": {
                                "name": "toolchain", "type": "jar",
                                "extension": "jar", "classifier": "linux-x86_64"
                            }
                        }
                    }]
                }]
            }"#,
        )
        .expect("metadata parses");
        let variant = &parsed.variants[0];
        assert_eq!(variant.capabilities.len(), 1);
        assert_eq!(variant.capabilities[0].name, "native");
        assert_eq!(variant.dependencies[0].requested_capabilities.len(), 1);
        let selector = variant.dependencies[0]
            .third_party_compatibility
            .as_ref()
            .and_then(|t| t.artifact_selector.as_ref())
            .expect("the selector is read");
        assert_eq!(selector.classifier.as_deref(), Some("linux-x86_64"));
        assert_eq!(selector.extension.as_deref(), Some("jar"));
    }

    #[test]
    fn a_variant_claiming_java_8_under_either_spelling_is_read() {
        // `1.8` and `8` are the same version and a writer may use either. Read as
        // nothing, a Java-8 variant looks like one with no requirement, so it is
        // never rejected and never preferred over a variant that omits the
        // attribute.
        for spelling in ["1.8", "8"] {
            let offered = module(vec![variant(
                "jdk8Elements",
                &[
                    ("org.gradle.category", "library"),
                    ("org.gradle.usage", "java-runtime"),
                    ("org.gradle.jvm.version", spelling),
                ],
            )]);
            assert!(
                offered.select_for_jvm(8).is_ok(),
                "`{spelling}` reads as a version"
            );
            assert!(
                offered.select_for_jvm(7).is_err(),
                "`{spelling}` is still above Java 7"
            );
        }
    }

    #[test]
    fn an_attribute_value_may_be_a_string_a_boolean_or_an_integer() {
        // The specification allows all three for an attribute, and real modules
        // use all three: `androidx.annotation:annotation:1.3.0` writes
        // `"org.gradle.jvm.version": 8` unquoted. Read as a string only, that
        // discards the module's whole metadata and silently falls back to its
        // POM, which is how a Kotlin Multiplatform stub reached a classpath.
        let parsed: Module = serde_json::from_str(
            r#"{
                "formatVersion": "1.1",
                "variants": [{
                    "name": "runtimeElements",
                    "attributes": {
                        "org.gradle.category": "library",
                        "org.gradle.usage": "java-runtime",
                        "org.gradle.jvm.version": 8,
                        "org.gradle.dependency.bundling": true,
                        "org.gradle.native.debuggable": false
                    },
                    "files": [{"name":"x.jar","url":"x.jar","size":1,"sha1":"ab"}]
                }]
            }"#,
        )
        .expect("an integer or boolean attribute parses");
        let attributes = &parsed.variants[0].attributes;
        assert_eq!(attributes["org.gradle.jvm.version"].as_str(), "8");
        assert_eq!(
            attributes["org.gradle.dependency.bundling"].as_str(),
            "true"
        );
        assert_eq!(attributes["org.gradle.native.debuggable"].as_str(), "false");
        assert_eq!(declared_jvm_version(&parsed.variants[0]), Some(8));
    }

    #[test]
    fn a_platform_variant_is_found_even_though_select_rejects_it() {
        // `select` requests `org.gradle.category=library` and so rejects a
        // platform by design, which would leave a platform module looking like
        // nothing it can use. It is therefore looked for on its own terms, with
        // the same JVM and usage requirements.
        let offered: Module = serde_json::from_str(
            r#"{
                "formatVersion": "1.1",
                "variants": [
                    {
                        "name": "platformRuntimeElements",
                        "attributes": {
                            "org.gradle.category": "platform",
                            "org.gradle.usage": "java-runtime"
                        },
                        "dependencyConstraints": [
                            {"group":"g","module":"lib","version":{"requires":"2.0"}}
                        ]
                    },
                    {
                        "name": "runtimeElements",
                        "attributes": {
                            "org.gradle.category": "library",
                            "org.gradle.usage": "java-runtime"
                        },
                        "files": [{"name":"x.jar","url":"x.jar","size":1,"sha1":"ab"}]
                    }
                ]
            }"#,
        )
        .expect("metadata parses");
        assert_eq!(
            offered.select().expect("a library is selected").name,
            "runtimeElements"
        );
        let platform = offered
            .platform_variant(17)
            .expect("the platform is found anyway");
        assert_eq!(platform.name, "platformRuntimeElements");
        assert_eq!(platform.dependency_constraints.len(), 1);
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
