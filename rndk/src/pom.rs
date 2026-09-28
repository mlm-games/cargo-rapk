//! Maven POM reading and dependency resolution.
//!
//! `android_libs` names direct dependencies; this turns them into the closure
//! that actually has to be on the classpath. It follows the parts of the Maven
//! model that change *which* artifact gets picked: the parent chain,
//! `dependencyManagement` including imported BOMs, exclusions and scopes.
//!
//! When two paths want different versions of one module the newest wins, with
//! distance only breaking a tie between equal versions. That is Gradle's rule
//! rather than Maven's, and it is the one Android projects are written against:
//! the same set of POMs resolves to the same artifacts however `android_libs`
//! is written.
//!
//! A module that publishes [Gradle Module Metadata](crate::gmm) is read from
//! that instead, as Gradle does, and the POM is only a fallback.

use crate::error::NdkError;
use crate::gmm;
use crate::maven::{self, Coordinates};
use crate::range::Requirement;
use crate::version::MavenVersion;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;

/// A `group:artifact` pair. Version selection happens per pair, which is what
/// makes two versions of one module a conflict rather than two modules.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId {
    pub group: String,
    pub artifact: String,
}

impl ModuleId {
    fn new(group: &str, artifact: &str) -> Self {
        Self {
            group: group.trim().to_owned(),
            artifact: artifact.trim().to_owned(),
        }
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.group, self.artifact)
    }
}

/// An `<exclusion>`, either end of which may be the `*` wildcard.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Exclusion {
    group: String,
    artifact: String,
}

impl Exclusion {
    fn matches(&self, module: &ModuleId) -> bool {
        (self.group == "*" || self.group == module.group)
            && (self.artifact == "*" || self.artifact == module.artifact)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Compile,
    Runtime,
    Provided,
    Test,
    System,
    Import,
}

impl Scope {
    fn parse(text: Option<&str>) -> Self {
        match text.unwrap_or("compile").trim() {
            "runtime" => Self::Runtime,
            "provided" => Self::Provided,
            "test" => Self::Test,
            "system" => Self::System,
            "import" => Self::Import,
            _ => Self::Compile,
        }
    }

    /// Whether a dependency in this scope is part of what a `compile`-scope
    /// consumer puts on its classpath. `provided` and `test` are supplied by
    /// the build itself, so they never reach an APK.
    fn reaches_classpath(self) -> bool {
        matches!(self, Self::Compile | Self::Runtime)
    }
}

/// A dependency as declared, before version selection.
#[derive(Debug, Clone)]
struct Declaration {
    module: ModuleId,
    /// `None` when the version comes from `dependencyManagement`.
    version: Option<String>,
    scope: Scope,
    optional: bool,
    exclusions: Vec<Exclusion>,
    /// Which file to fetch for this module, for a dependant whose metadata
    /// says so and whose own does not.
    artifact_selector: Option<gmm::ArtifactSelector>,
    /// `<type>test-jar</type>`, a test fixture rather than a library.
    test_artifact: bool,
    /// Versions the declaring module will not accept, which Gradle filters a
    /// candidate through rather than merely records.
    rejects: Vec<String>,
}

/// A version pinned by `dependencyManagement`, which applies to the
/// dependencies of the POM that declares it and to their subtrees.
#[derive(Debug, Clone)]
struct Managed {
    version: String,
    /// A `pom`-typed entry in `import` scope is a BOM whose own management is
    /// folded in, rather than a version for one module.
    bom: bool,
}

/// A POM with its parent chain, property interpolation and imported BOMs
/// applied. Maven calls this the effective model.
#[derive(Debug, Clone)]
struct Model {
    group: String,
    version: String,
    packaging: String,
    properties: BTreeMap<String, String>,
    management: BTreeMap<ModuleId, Managed>,
    dependencies: Vec<Declaration>,
}

impl Model {
    fn imports_in_order(&self) -> Vec<ModuleId> {
        self.management
            .iter()
            .filter(|(_, managed)| managed.bom)
            .map(|(module, _)| module.clone())
            .collect()
    }
}

/// POM elements carry no namespace prefix — a POM only declares `xmlns:` on
/// its root and never uses one — so quick-xml's serde reads them correctly.
/// The opposite holds for `AndroidManifest.xml`, where every attribute is
/// `android:`-prefixed and the deserializer silently reads none of them.
///
/// Every field carries its wire name, because serde matches element names
/// exactly: a field left as `group_id` silently reads nothing for `<groupId>`
/// rather than failing. `reads_every_element_of_a_fully_populated_pom` pins
/// that.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename = "project")]
struct Pom {
    #[serde(rename = "groupId")]
    group_id: Option<String>,
    #[serde(rename = "version")]
    version: Option<String>,
    #[serde(rename = "packaging", default)]
    packaging: Option<String>,
    #[serde(rename = "parent", default)]
    parent: Option<Parent>,
    #[serde(rename = "properties", default)]
    properties: Option<BTreeMap<String, String>>,
    #[serde(rename = "dependencyManagement", default)]
    dependency_management: Option<Management>,
    #[serde(rename = "dependencies", default)]
    dependencies: Option<DependencyList>,
    #[serde(rename = "relocation", default)]
    relocation: Option<Relocation>,
}

#[derive(Debug, Clone, Deserialize)]
struct Parent {
    #[serde(rename = "groupId", default)]
    group_id: String,
    #[serde(rename = "artifactId", default)]
    artifact_id: String,
    #[serde(rename = "version", default)]
    version: String,
}

/// `<dependencyManagement>` wraps its own `<dependencies>`, and quick-xml needs
/// the intermediate level: pointing the field straight at the entry list makes
/// it look for `<dependency>` where the document has `<dependencies>`, and it
/// then collects nothing without reporting an error.
#[derive(Debug, Clone, Deserialize)]
struct Management {
    #[serde(rename = "dependencies", default)]
    dependencies: Option<DependencyList>,
}

#[derive(Debug, Clone, Deserialize)]
struct DependencyList {
    #[serde(rename = "dependency", default)]
    entries: Vec<Dependency>,
}

#[derive(Debug, Clone, Deserialize)]
struct Dependency {
    #[serde(rename = "groupId", default)]
    group_id: String,
    #[serde(rename = "artifactId", default)]
    artifact_id: String,
    #[serde(rename = "version", default)]
    version: Option<String>,
    /// `<type>`. Read to recognise a `test-jar`, which is a test artifact even
    /// when its scope says compile; every other value is the module's own
    /// artifact.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(rename = "scope", default)]
    scope: Option<String>,
    #[serde(rename = "optional", default)]
    optional: Option<String>,
    #[serde(rename = "exclusions", default)]
    exclusions: Option<Exclusions>,
}

#[derive(Debug, Clone, Deserialize)]
struct Exclusions {
    #[serde(rename = "exclusion", default)]
    entries: Vec<ExclusionXml>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExclusionXml {
    #[serde(rename = "groupId", default)]
    group_id: String,
    #[serde(rename = "artifactId", default)]
    artifact_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Relocation {
    #[serde(rename = "groupId", default)]
    group_id: String,
    #[serde(rename = "artifactId", default)]
    artifact_id: String,
    #[serde(rename = "version", default)]
    version: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Metadata {
    #[serde(rename = "versioning", default)]
    versioning: Option<Versioning>,
}

#[derive(Debug, Clone, Deserialize)]
struct Versioning {
    #[serde(rename = "versions", default)]
    versions: Option<Versions>,
}

#[derive(Debug, Clone, Deserialize)]
struct Versions {
    #[serde(rename = "version", default)]
    entries: Vec<String>,
}

/// One module's winner and the claims it beat.
#[derive(Debug, Clone)]
struct Selection {
    version: String,
    ordering: MavenVersion,
    depth: usize,
    order: usize,
    /// The exclusions in force where this module was reached, so that a later
    /// claim on it expands it under the same rules rather than none.
    excluded: Vec<Exclusion>,
    /// The requirement that produced this version, so that a hard range it
    /// arrived under can be checked against the version that finally wins.
    requirement: Requirement,
}

impl Selection {
    /// The newest version declared for a module wins, whatever its distance.
    ///
    /// This is Gradle's rule rather than Maven's, and it is the one Android
    /// projects are written against: the same POM set resolves to the same
    /// artifacts in the same order however `android_libs` is written. Distance
    /// only breaks a tie between equal versions, where the nearer declaration
    /// is the one that belongs to the artifact actually being used.
    fn beats(&self, other: &Self) -> bool {
        self.ordering
            .cmp(&other.ordering)
            .then_with(|| other.depth.cmp(&self.depth))
            .then_with(|| other.order.cmp(&self.order))
            .is_gt()
    }
}

/// A module queued for its dependencies to be read.
struct Pending {
    module: ModuleId,
    /// The version this entry was queued for. An entry whose version has been
    /// superseded is dropped, because the winner queued its own.
    version: String,
    depth: usize,
    /// Exclusions inherited along the path that reached this module.
    excluded: Vec<Exclusion>,
    /// A module named directly in `android_libs`, which is kept even if it has
    /// no readable POM.
    root: bool,
}

/// How a declaration's version reached it.
enum Claim {
    /// Left out, and filled in from `dependencyManagement` or a BOM.
    Version(String),
    /// Written nowhere, which the resolver reports rather than guesses.
    Undeclared,
    /// A `dependencyConstraint`: advice about a version, not a request for the
    /// artifact. It competes for a module already in the graph and is otherwise
    /// ignored, which is what Gradle's `dependencyConstraints` does.
    Constraint,
}

/// The coordinates a module's runtime variant redirects to, if any.
fn redirect_of(module: &gmm::Module, coordinates: &Coordinates) -> Option<Coordinates> {
    let selected = module.select().ok()?;
    let available = selected.available_at.as_ref()?;
    let mut version = available.version.clone();
    if version.is_empty() {
        version = coordinates.version.clone();
    }
    Some(Coordinates {
        group: available.group.clone(),
        artifact: available.module.clone(),
        version,
    })
}

/// An artifact that belongs on the classpath.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub coordinates: Coordinates,
    pub packaging: String,
    /// The archive to fetch, when the metadata named one. `None` probes for an
    /// AAR and then a jar, which is all a POM can tell us.
    pub extension: Option<String>,
    /// The classifier the archive is published under, when a dependency's
    /// `thirdPartyCompatibility.artifactSelector` names one. Maven publishes
    /// these as `{artifact}-{version}-{classifier}.{extension}`, and without it
    /// a platform-specific or native artifact resolves to the wrong bytes or to
    /// nothing at all.
    pub classifier: Option<String>,
    /// A `pom`-packaged module's own jar, which Maven says is never published
    /// and Gradle probes for anyway.
    ///
    /// `RealisedMavenModuleResolveMetadata.getArtifactsForConfiguration` offers
    /// it as an `optionalArtifact`, so a repository that publishes one has it
    /// on the classpath and a repository that does not is not an error. Maven
    /// suppresses the probe outright, which is the difference being made here:
    /// some aggregators do publish a jar, and dropping it leaves a classpath
    /// missing classes that were there.
    pub optional: bool,
}

/// How many times a `<relocation>` or `available-at` chain is followed before
/// giving up.
const MAX_RELOCATIONS: usize = 8;

/// Resolves `roots` and their transitive dependencies.
pub fn resolve(roots: &[maven::Root]) -> Result<Vec<Resolved>, NdkError> {
    let mut resolver = Resolver::default();
    for root in roots {
        if root.role == maven::Role::Platform {
            resolver.declared_platforms.insert(ModuleId::new(
                &root.coordinates.group,
                &root.coordinates.artifact,
            ));
        }
        if root.role == maven::Role::Library {
            resolver.declared_libraries.insert(ModuleId::new(
                &root.coordinates.group,
                &root.coordinates.artifact,
            ));
        }
        resolver.consider(
            ModuleId::new(&root.coordinates.group, &root.coordinates.artifact),
            Requirement::parse(&root.coordinates.version),
            0,
            Vec::new(),
            true,
            false,
        )?;
    }
    resolver.walk();
    // A platform named in `android_libs` is read during the walk, so its
    // versions are applied afterwards and anything that moves is walked again.
    resolver.apply_platforms()?;
    resolver.check_capabilities()?;
    resolver.report_range_violations();
    let unusable = std::mem::take(&mut resolver.unusable);
    if let Some(e) = unusable.into_iter().next() {
        return Err(e);
    }
    Ok(resolver.results())
}

#[derive(Default)]
struct Resolver {
    /// Modules an `android_libs` entry marked `!platform`, and the ones it
    /// marked `!library`. Gradle reads a platform off the declaration rather
    /// than off the module, so a marker here outranks whatever the packaging
    /// would otherwise suggest.
    declared_platforms: BTreeSet<ModuleId>,
    declared_libraries: BTreeSet<ModuleId>,
    /// Modules whose own metadata says they cannot be put in an APK. Collected
    /// during the walk and raised once it finishes, so a module reached by many
    /// paths is reported once.
    unusable: Vec<NdkError>,
    /// The module each relocated module id is replaced by, so that something
    /// recorded under a declaration's own name can be found under the name the
    /// graph is keyed by.
    relocations: HashMap<ModuleId, ModuleId>,
    /// Effective models by coordinates, so a module reached at two versions
    /// gets both POMs read and neither is re-read.
    models: HashMap<Coordinates, Model>,
    /// Raw POM documents, so relocation and inheritance share one fetch.
    documents: HashMap<Coordinates, Pom>,
    /// Gradle Module Metadata by coordinates. `None` records a module that
    /// publishes none, so the POM path is only tried once.
    metadata: HashMap<Coordinates, Option<gmm::Module>>,
    /// Coordinates whose model is being built, to catch a parent cycle.
    building: BTreeSet<Coordinates>,
    /// Published versions per module, for ranges and `LATEST`.
    available: HashMap<ModuleId, Vec<MavenVersion>>,
    selected: BTreeMap<ModuleId, Selection>,
    /// The newest version any `dependencyConstraint` has recommended for a
    /// module, held until something actually wants that module.
    constrained: BTreeMap<ModuleId, String>,
    /// The versions a platform — a BOM, or a module whose variant is
    /// `org.gradle.category=platform` — coordinates for the whole graph.
    ///
    /// A platform differs from a `dependencyConstraint` in that it also applies
    /// to a module something else already put on the classpath, which is the
    /// whole point of naming one: `kotlin-bom` is how a project says every
    /// `org.jetbrains.kotlin` artifact is the same version. It never *adds* a
    /// module nothing asked for.
    platforms: BTreeMap<ModuleId, String>,
    /// The exclusions each module was last expanded under. Exclusions are
    /// path-dependent, so a path carrying exclusions the module has not seen
    /// yet re-expands it.
    expanded: HashMap<(ModuleId, String), Vec<Exclusion>>,
    pending: VecDeque<Pending>,
    /// The classifier and extension each module was asked for by some
    /// dependency's `thirdPartyCompatibility.artifactSelector`. A module
    /// without metadata of its own has nothing else to say which file to take.
    selectors: HashMap<ModuleId, (Option<String>, Option<String>)>,
    /// Hard ranges that the selected version does not satisfy. Gradle fails
    /// these; they are reported rather than fatal because a skewed patch level
    /// is routine in the AndroidX POM set.
    range_violations: Vec<RangeViolation>,
    order: usize,
}

/// A hard `[x,y]` range whose winner fell outside it.
#[derive(Debug)]
struct RangeViolation {
    module: ModuleId,
    required: String,
    selected: String,
}

impl Resolver {
    /// Reports every hard range the selection does not satisfy. Gradle fails
    /// the build here; a warning is enough to make the choice visible, because
    /// `[1.2.6,1.2.7)`-style skew is common in the AndroidX POM set and failing
    /// outright would make the library unusable.
    fn report_range_violations(&self) {
        let mut reported = BTreeSet::new();
        for violation in &self.range_violations {
            if !reported.insert(violation.module.clone()) {
                continue;
            }
            log::warn!(
                "{}: resolved to {} but something requires the hard range {}; \
                 newest-wins mediation cannot satisfy both",
                violation.module,
                violation.selected,
                violation.required
            );
        }
    }

    fn fail(&self, coordinates: &Coordinates, reason: impl Into<String>) -> NdkError {
        NdkError::MavenLibFailed {
            gav: coordinates.gav(),
            reason: reason.into(),
        }
    }

    /// Records a dependency's claim on a module, keeping the better claim.
    ///
    /// A `constraint` is version advice rather than a request: it competes only
    /// for a module something else already wants, and if that happens later the
    /// advice is applied then.
    fn consider(
        &mut self,
        module: ModuleId,
        requirement: Requirement,
        depth: usize,
        excluded: Vec<Exclusion>,
        root: bool,
        constraint: bool,
    ) -> Result<(), NdkError> {
        if excluded.iter().any(|e| e.matches(&module)) {
            return Ok(());
        }
        let (module, version) = self.concrete(module, &requirement)?;
        if constraint && !self.selected.contains_key(&module) {
            let best = self
                .constrained
                .entry(module)
                .or_insert_with(|| version.clone());
            if MavenVersion::new(&version) > MavenVersion::new(best) {
                *best = version;
            }
            return Ok(());
        }
        self.order += 1;
        let claim = Selection {
            ordering: MavenVersion::new(&version),
            version: version.clone(),
            depth,
            order: self.order,
            excluded: excluded.clone(),
            requirement: requirement.clone(),
        };

        if let Some(current) = self.selected.get(&module)
            && !claim.beats(current)
        {
            // The claim that lost may have arrived under a hard range the
            // winner does not satisfy. Gradle fails such a resolution; warn
            // instead, because a single skewed patch level is routine in the
            // AndroidX POM set and failing outright would make the whole
            // library unusable.
            if let Requirement::Range(range) = &requirement
                && !range.contains(&current.ordering)
            {
                self.range_violations.push(RangeViolation {
                    module: module.clone(),
                    required: requirement.to_string(),
                    selected: current.version.clone(),
                });
            }
            return Ok(());
        }
        self.record_relocation(&module, &version);
        let first_time = match self.selected.insert(module.clone(), claim) {
            Some(displaced) => {
                // The winner may fall outside a hard range the version it
                // displaced arrived under.
                if let Requirement::Range(range) = &displaced.requirement
                    && !range.contains(&MavenVersion::new(&version))
                {
                    self.range_violations.push(RangeViolation {
                        module: module.clone(),
                        required: displaced.requirement.to_string(),
                        selected: version.clone(),
                    });
                }
                false
            }
            None => true,
        };
        self.pending.push_back(Pending {
            module: module.clone(),
            version,
            depth,
            excluded: excluded.clone(),
            root,
        });
        if first_time && let Some(advised) = self.constrained.remove(&module) {
            let requirement = Requirement::parse(&advised);
            let (module, version) = self.concrete(module, &requirement)?;
            self.order += 1;
            let claim = Selection {
                ordering: MavenVersion::new(&version),
                version: version.clone(),
                depth,
                order: self.order,
                excluded: excluded.clone(),
                requirement: requirement.clone(),
            };
            if self
                .selected
                .get(&module)
                .is_none_or(|current| claim.beats(current))
            {
                self.selected.insert(module.clone(), claim);
                self.pending.push_back(Pending {
                    module,
                    version,
                    depth,
                    excluded: excluded.clone(),
                    root,
                });
            }
        }
        Ok(())
    }

    /// Turns a requirement into the module and version that get used, following
    /// any `<relocation>` to the coordinates that replaced them.
    fn concrete(
        &mut self,
        module: ModuleId,
        requirement: &Requirement,
    ) -> Result<(ModuleId, String), NdkError> {
        let version = match requirement {
            Requirement::Exact(version) => version.as_str().to_owned(),
            other => {
                let available = self.available(&module)?;
                other
                    .resolve(&available)
                    .map(|v| v.as_str().to_owned())
                    .ok_or_else(|| {
                        self.fail(
                            &coordinates_of(&module, ""),
                            format!("no published version satisfies `{other}`"),
                        )
                    })?
            }
        };
        // Neither `<relocation>` nor `available-at` moves the graph key. Each
        // says where the artifact is published, not which module the graph
        // should record: the module a dependency names still has to mediate
        // against every other claim on it, and rewriting the key would let one
        // module ship two versions. A relocation target is reached as an edge
        // instead, so it mediates as an ordinary module of its own.
        Ok((module, version))
    }

    /// Notes where a relocated module's artifact is published, so a dependant's
    /// artifact selector can be recorded under the name the graph will key the
    /// target by. Called from `consider` because a dependant records its
    /// selector immediately after placing the module it names, which is before
    /// `walk` reaches that module.
    fn record_relocation(&mut self, module: &ModuleId, version: &str) -> Option<ModuleId> {
        let relocation = self
            .document(&coordinates_of(module, version))
            .ok()?
            .relocation
            .clone()?;
        if relocation.group_id.is_empty() {
            return None;
        }
        let target = ModuleId::new(&relocation.group_id, &relocation.artifact_id);
        self.relocations.insert(module.clone(), target.clone());
        Some(target)
    }

    /// The module a `<relocation>` points at, with the version to ask it for.
    fn relocation_of(
        &mut self,
        coordinates: &Coordinates,
        version: &str,
    ) -> Result<Option<(ModuleId, String)>, NdkError> {
        let Some(relocation) = self.document(coordinates)?.relocation.clone() else {
            return Ok(None);
        };
        if relocation.group_id.is_empty() {
            return Ok(None);
        }
        let target = ModuleId::new(&relocation.group_id, &relocation.artifact_id);
        if target == ModuleId::new(&coordinates.group, &coordinates.artifact) {
            // A relocation to another version of the same coordinates names a
            // module that no longer publishes anything under its own name.
            // Gradle logs an error and takes only the target's dependencies;
            // shipping the target's bytes under the old coordinates is what
            // Gradle refuses to do, and the other half of it — an APK with the
            // classes missing — is a crash that only a device finds. Name the
            // newer version instead.
            return Err(self.fail(
                coordinates,
                format!(
                    "relocates to {}:{}, which is another version of itself. Name {} \
                     directly in `android_libs` instead",
                    coordinates.group, relocation.version, relocation.version
                ),
            ));
        }
        // A relocation that names no version keeps the one the request already
        // resolved to.
        let target_version = if relocation.version.is_empty() {
            version.to_owned()
        } else {
            relocation.version
        };
        Ok(Some((target, target_version)))
    }

    /// Whether a module is a platform: a `pom`-packaged BOM in POM terms, or a
    /// variant marked `org.gradle.category=platform` in metadata terms.
    fn is_platform(&mut self, coordinates: &Coordinates, model: &Model) -> bool {
        let module = ModuleId::new(&coordinates.group, &coordinates.artifact);
        if self.declared_libraries.contains(&module) {
            return false;
        }
        if self.declared_platforms.contains(&module) {
            return true;
        }
        // Unmarked, so the packaging decides. Gradle would apply no
        // `<dependencyManagement>` to a module named as an ordinary dependency,
        // but the packaging is all there is to go on, and a `pom` module that
        // carries nothing but management is a BOM either way. `!library` is how
        // a caller says not to guess.
        if model.packaging == "pom" && !model.management.is_empty() {
            return true;
        }
        self.is_metadata_platform(coordinates)
    }

    /// A GMM platform, and only when nothing else the module offers is a usable
    /// library. A module that publishes both a `platformRuntimeElements` and a
    /// `runtimeElements` is a library that happens to also carry a platform
    /// variant; Gradle only treats it as a platform when one is requested, and so
    /// does the resolution here.
    fn is_metadata_platform(&mut self, coordinates: &Coordinates) -> bool {
        if self.platform_variant(coordinates).is_none() {
            return false;
        }
        let Some(module) = self.metadata(coordinates) else {
            return false;
        };
        module.select().is_err()
    }

    fn platform_variant(&mut self, coordinates: &Coordinates) -> Option<gmm::Variant> {
        let module = self.metadata(coordinates)?;
        module.platform_variant(gmm::target_jvm_version()).cloned()
    }

    /// Records a platform's versions, keeping the newest of each.
    ///
    /// A platform states them as POM `dependencyManagement` or, in metadata
    /// terms, as the variant's `dependencyConstraints`. Both are read, because a
    /// platform that publishes only one of them would otherwise do nothing.
    fn collect_platform(
        &mut self,
        coordinates: &Coordinates,
        management: &BTreeMap<ModuleId, Managed>,
    ) {
        let mut versions: BTreeMap<ModuleId, String> = management
            .iter()
            .filter(|(_, managed)| !managed.bom)
            .map(|(module, managed)| (module.clone(), managed.version.clone()))
            .collect();
        if let Some(variant) = self.platform_variant(coordinates) {
            for constraint in &variant.dependency_constraints {
                if let Some(version) = constraint.version.as_ref().and_then(|c| c.requirement()) {
                    versions
                        .entry(ModuleId::new(&constraint.group, &constraint.module))
                        .or_insert_with(|| version.to_owned());
                }
            }
        }
        for (module, version) in versions {
            if version.is_empty() {
                continue;
            }
            let entry = self.platforms.entry(module.clone()).or_insert_with(|| {
                log::debug!("a platform coordinates {module} to {version}");
                version.clone()
            });
            if MavenVersion::new(&version) > MavenVersion::new(entry) {
                *entry = version;
            }
        }
    }

    /// Applies every platform's versions, and keeps going while they keep moving
    /// the selection, since a re-selected module can reach a further platform.
    fn apply_platforms(&mut self) -> Result<bool, NdkError> {
        let mut moved = false;
        while !self.platforms.is_empty() {
            let batch = std::mem::take(&mut self.platforms);
            for (module, version) in batch {
                let requirement = Requirement::parse(&version);
                // Checked before the coordinates are made concrete, so advice
                // about a module nothing wants costs no request: a platform
                // coordinates versions, it does not pull artifacts in.
                let placed = self
                    .selected
                    .get(&module)
                    .map(|c| (c.excluded.clone(), c.depth, c.ordering.clone()));
                let Some((excluded, depth, placed_at)) = placed else {
                    let entry = self
                        .constrained
                        .entry(module)
                        .or_insert_with(|| version.clone());
                    if MavenVersion::new(&version) > MavenVersion::new(entry) {
                        *entry = version;
                    }
                    continue;
                };
                let (module, version) = self.concrete(module, &requirement)?;
                if MavenVersion::new(&version) <= placed_at {
                    continue;
                }
                self.consider(module, requirement, depth, excluded, false, false)?;
                moved = true;
            }
            self.walk();
        }
        Ok(moved)
    }

    /// Reports the capabilities two different modules both provide, and the ones
    /// a dependency required that the resolved variant does not have.
    ///
    /// A capability is a feature rather than a coordinate, so two modules
    /// providing the same one under different names is a real conflict — the
    /// relocated `asm` is the documented example — and Gradle fails it rather
    /// than picking a winner, because there is nothing to pick on.
    fn check_capabilities(&mut self) -> Result<(), NdkError> {
        // (group, name) -> the resolved artifacts providing it, and the version
        // each declares. Every component provides its own coordinates implicitly,
        // and `selected` is keyed by module, so one module cannot be two
        // providers of the same capability.
        let mut providers: BTreeMap<(String, String), BTreeMap<String, String>> = BTreeMap::new();
        let selection: Vec<(ModuleId, String)> = self
            .selected
            .iter()
            .map(|(module, selection)| (module.clone(), selection.version.clone()))
            .collect();
        for (module, version) in &selection {
            let coordinates = coordinates_of(module, version);
            let mut declared = vec![gmm::Capability::implicit(
                &coordinates.group,
                &coordinates.artifact,
                &coordinates.version,
            )];
            if let Some(metadata) = self.metadata(&coordinates)
                && let Ok(variant) = metadata.select()
            {
                declared.extend(variant.capabilities.iter().cloned());
            }
            for capability in declared {
                providers
                    .entry((capability.group.clone(), capability.name.clone()))
                    .or_default()
                    .insert(coordinates.gav(), capability.version);
            }
        }

        // Gradle registers a conflict as soon as two *selected* components provide
        // the same capability — there is no `requestedCapabilities` test in that
        // path — and then resolves it by version: `UpgradeCapabilityResolver`
        // evicts every candidate but the highest when they declare different
        // versions, and `LastCandidateCapabilityResolver` has nothing to choose
        // between when they agree, so every candidate is rejected and the build
        // fails. So the condition is "more than one provider", and the outcome
        // is "fine while they disagree on a version, fatal once they agree".
        for ((group, name), modules) in &providers {
            if modules.len() < 2 {
                continue;
            }
            let versions: BTreeSet<&String> = modules.values().collect();
            if versions.len() > 1 {
                log::warn!(
                    "{}:{} is provided by {} at {} different versions ({}); the higher one                      is kept, as Gradle's `UpgradeCapabilityResolver` would",
                    group,
                    name,
                    modules.len(),
                    versions.len(),
                    modules
                        .iter()
                        .map(|(gav, version)| format!("{gav} as {version}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                continue;
            }
            let owners: Vec<String> = modules
                .iter()
                .map(|(gav, version)| format!("{gav} (as {version})"))
                .collect();
            return Err(self.fail(
                &Coordinates {
                    group: group.clone(),
                    artifact: name.clone(),
                    version: String::new(),
                },
                format!(
                    "is provided by {} resolved artifacts, all of them declaring it at {}, \
                     so there is no version to choose between and Gradle rejects them all: {}",
                    modules.len(),
                    versions.iter().next().copied().cloned().unwrap_or_default(),
                    owners.join(", ")
                ),
            ));
        }
        Ok(())
    }

    /// Reads every queued module's dependencies until the winners stop moving.
    fn walk(&mut self) {
        while let Some(node) = self.pending.pop_front() {
            if self
                .selected
                .get(&node.module)
                .is_none_or(|current| current.version != node.version)
            {
                continue;
            }
            let key = (node.module.clone(), node.version.clone());
            if self
                .expanded
                .get(&key)
                .is_some_and(|seen| seen.iter().all(|e| node.excluded.contains(e)))
            {
                continue;
            }
            self.expanded.insert(key, node.excluded.clone());

            let coordinates = coordinates_of(&node.module, &node.version);
            // Gradle reads the `.module` file in preference to the POM, and the
            // two do not always agree: a module's runtime variant can name
            // dependencies the POM's compile scope does not, and can point
            // somewhere else entirely.
            // A platform coordinates versions rather than requesting
            // artifacts, so its entries are constraints on the whole graph and
            // are collected instead of walked.
            if let Ok(model) = self.model(&node.module, &node.version)
                && self.is_platform(&coordinates, &model)
            {
                let management = model.management.clone();
                let has_dependencies = !model.dependencies.is_empty();
                self.collect_platform(&coordinates, &management);
                // A platform asks for nothing, so there is nothing to walk. A
                // `pom`-packaged module can still declare real dependencies —
                // Maven calls that an aggregator — and skipping it here would
                // drop its whole subtree.
                if !has_dependencies {
                    continue;
                }
            }
            // A relocated module publishes no artifact, so the only thing it
            // contributes is an edge to the module it was moved to. The target
            // is resolved as a module in its own right, which is how Gradle
            // hangs it off `GradlePomModuleDescriptorBuilder
            // .addDependencyForRelocation`.
            match self.relocation_of(&coordinates, &node.version) {
                Ok(Some((target, target_version))) => {
                    if let Err(e) = self.consider(
                        target,
                        Requirement::parse(&target_version),
                        node.depth + 1,
                        node.excluded.clone(),
                        false,
                        false,
                    ) {
                        log::warn!("{e}");
                    }
                }
                Err(e) => {
                    self.unusable.push(e);
                    continue;
                }
                Ok(None) => {}
            }
            let declared = match self.declarations(&coordinates) {
                Ok(declared) => declared,
                // A root the user named by hand is kept even with no readable
                // metadata: there is then simply nothing to add to the
                // classpath.
                Err(e) if node.root => {
                    log::warn!("no dependencies resolved for {}: {e}", node.module);
                    continue;
                }
                Err(e) => {
                    log::warn!("{e}");
                    continue;
                }
            };

            for (declaration, claim) in declared {
                if declaration.optional || !declaration.scope.reaches_classpath() {
                    continue;
                }
                if declaration.test_artifact {
                    continue;
                }
                let (spec, constraint) = match (&declaration.version, claim) {
                    (_, Claim::Constraint) => match &declaration.version {
                        Some(version) => (version.clone(), true),
                        None => continue,
                    },
                    (Some(version), _) => (version.clone(), false),
                    (None, Claim::Version(version)) => (version, false),
                    (None, Claim::Undeclared) => {
                        log::warn!(
                            "{} declares {} with no version and no \
                             `dependencyManagement` entry",
                            node.module,
                            declaration.module
                        );
                        continue;
                    }
                };
                if let Some(selector) = &declaration.artifact_selector {
                    self.selectors
                        .entry(declaration.module.clone())
                        .or_insert_with(|| {
                            (selector.classifier.clone(), selector.extension.clone())
                        });
                }
                let mut excluded = node.excluded.clone();
                excluded.extend(declaration.exclusions.iter().cloned());
                let depth = node.depth + 1;
                let module = declaration.module.clone();
                if let Err(e) = self.consider(
                    module.clone(),
                    Requirement::rejecting(&spec, &declaration.rejects),
                    depth,
                    excluded,
                    false,
                    constraint,
                ) {
                    log::warn!("{e}");
                }
                if let Some(selector) = &declaration.artifact_selector {
                    // Recorded under the name the graph is keyed by, which for a
                    // relocated module is the relocation target: the dependant
                    // wrote the selector against the old name, but the target is
                    // the node that produces the artifact.
                    let target = self
                        .relocations
                        .get(&module)
                        .cloned()
                        .unwrap_or_else(|| module.clone());
                    self.selectors.entry(target).or_insert_with(|| {
                        (selector.classifier.clone(), selector.extension.clone())
                    });
                }
            }
        }
    }

    /// The artifacts to fetch, one per selected module.
    ///
    /// A module whose runtime variant redirects is fetched from where the
    /// metadata points, so `androidx.annotation:annotation` yields
    /// `annotation-jvm`'s jar rather than the metadata stub published under its
    /// own coordinates.
    fn results(&mut self) -> Vec<Resolved> {
        let mut out: BTreeMap<Coordinates, Resolved> = BTreeMap::new();
        for (module, selection) in self.selected.clone() {
            let mut coordinates = coordinates_of(&module, &selection.version);
            let mut extension = None;
            // A module marked `!platform` ships nothing even when it publishes an
            // archive, because its versions are constraints rather than a
            // dependency. Gradle's `withConstraintsOnly()` drops a platform's
            // artifacts for the same reason.
            let mut platform = self.declared_platforms.contains(&module);
            for _ in 0..MAX_RELOCATIONS {
                let Some(metadata) = self.metadata(&coordinates) else {
                    break;
                };
                let selected = match metadata.select() {
                    Ok(selected) => selected,
                    Err(_) => {
                        // Nothing in the module is a usable library. If what it
                        // does offer is a platform, then it ships nothing at all.
                        if metadata
                            .platform_variant(gmm::target_jvm_version())
                            .is_some()
                        {
                            platform = true;
                        }
                        break;
                    }
                };
                // A redirect is checked before anything else, because the
                // metadata format forbids a redirecting variant from carrying
                // files: its files are the target's, so an empty `files` here
                // says nothing about whether there is an artifact to ship.
                if let Some(target) = redirect_of(&metadata, &coordinates) {
                    coordinates = target;
                    continue;
                }
                // A variant that ships no files and points nowhere is a
                // platform or a documentation bundle, so there is nothing to
                // put in an APK.
                if !selected.ships_files() {
                    platform = true;
                    break;
                }
                extension = gmm::library_extension(&selected.attributes)
                    .map(str::to_owned)
                    .or_else(|| {
                        selected
                            .files
                            .iter()
                            .find_map(|file| file.extension())
                            .map(str::to_owned)
                    });
                break;
            }
            if platform {
                continue;
            }
            // A relocated module is a pointer, not a payload. Its target is a
            // node of its own in `selected` and contributes the artifact, so
            // emitting one here would ship the same bytes twice.
            if self
                .relocation_of(&coordinates, &selection.version)
                .is_ok_and(|relocation| relocation.is_some())
            {
                continue;
            }
            // The metadata names the archive, so it also settles what this is
            // packaged as; a POM's `<packaging>` only has to be believed when
            // there is no metadata to ask.
            let packaging = match &extension {
                Some(extension) => extension.clone(),
                None => self
                    .models
                    .get(&coordinates)
                    .map_or_else(|| "jar".to_owned(), |model| model.packaging.clone()),
            };
            let (classifier, selected_extension) =
                self.selectors.get(&module).cloned().unwrap_or((None, None));
            let extension = selected_extension.or(extension);
            let packaging = match &extension {
                Some(extension) => extension.clone(),
                None => packaging,
            };
            // `<packaging>pom</packaging>` says the module publishes no archive
            // of its own, but it may anyway, and Gradle takes the jar when one
            // is there rather than treating the absence as a failure.
            let optional = packaging == "pom";
            let packaging = if optional { "jar" } else { &packaging }.to_owned();
            out.entry(coordinates.clone()).or_insert_with(|| Resolved {
                coordinates,
                packaging,
                extension,
                classifier,
                optional,
            });
        }
        out.into_values().collect()
    }

    /// What a module declares, paired with the version `dependencyManagement`
    /// pins for it. Each element is `(declaration, kind)`.
    fn declarations(
        &mut self,
        coordinates: &Coordinates,
    ) -> Result<Vec<(Declaration, Claim)>, NdkError> {
        if self.metadata(coordinates).is_some() {
            return self.metadata_declarations(coordinates);
        }
        self.pom_declarations(coordinates)
    }

    /// What the POM declares, with `dependencyManagement` filling in any version
    /// the document left out.
    fn pom_declarations(
        &mut self,
        coordinates: &Coordinates,
    ) -> Result<Vec<(Declaration, Claim)>, NdkError> {
        let module = ModuleId::new(&coordinates.group, &coordinates.artifact);
        let model = self.model(&module, &coordinates.version)?;
        Ok(model
            .dependencies
            .iter()
            .map(|declaration| {
                let claim = model
                    .management
                    .get(&declaration.module)
                    .map(|managed| Claim::Version(managed.version.clone()))
                    .unwrap_or(Claim::Undeclared);
                (declaration.clone(), claim)
            })
            .collect())
    }

    /// The runtime variant's dependencies, plus the constraints it publishes.
    fn metadata_declarations(
        &mut self,
        coordinates: &Coordinates,
    ) -> Result<Vec<(Declaration, Claim)>, NdkError> {
        let Some(module) = self.metadata(coordinates) else {
            return Ok(Vec::new());
        };
        // A variant that redirects carries no dependencies of its own, so the
        // module it points at is what actually declares them. A failure to read
        // the target is reported rather than treated as "no dependencies",
        // which would quietly strip the module's whole subtree.
        if let Some(redirect) = redirect_of(&module, coordinates) {
            log::debug!(
                "{} redirects to {}; reading its dependencies there",
                coordinates.gav(),
                redirect.gav()
            );
            return self.declarations(&redirect);
        }
        let selected = match module.select() {
            Ok(selected) => selected,
            Err(reason) => {
                log::warn!("{}: {reason}; using its POM instead", coordinates.gav());
                return self.pom_declarations(coordinates);
            }
        };
        let mut out = Vec::new();
        for dependency in &selected.dependencies {
            let rejects = dependency
                .version
                .as_ref()
                .map(|c| c.rejects.clone())
                .unwrap_or_default();
            out.push((
                Declaration {
                    module: ModuleId::new(&dependency.group, &dependency.module),
                    version: dependency
                        .version
                        .as_ref()
                        .and_then(|c| c.requirement().map(str::to_owned)),
                    rejects,
                    scope: Scope::Compile,
                    test_artifact: false,
                    optional: false,
                    exclusions: dependency
                        .excludes
                        .iter()
                        .map(|e| Exclusion {
                            group: e.group.clone(),
                            artifact: e.module.clone(),
                        })
                        .collect(),
                    artifact_selector: dependency
                        .third_party_compatibility
                        .as_ref()
                        .and_then(|t| t.artifact_selector.clone()),
                },
                Claim::Undeclared,
            ));
        }
        for constraint in &selected.dependency_constraints {
            out.push((
                Declaration {
                    module: ModuleId::new(&constraint.group, &constraint.module),
                    version: constraint
                        .version
                        .as_ref()
                        .and_then(|c| c.requirement().map(str::to_owned)),
                    rejects: Vec::new(),
                    scope: Scope::Compile,
                    test_artifact: false,
                    optional: false,
                    exclusions: Vec::new(),
                    artifact_selector: None,
                },
                // Version advice, not a dependency. A platform's constraint
                // list names every version it coordinates, most of which the
                // consumer never asked for, so following one as a dependency
                // would add Compose, test fixtures and `guava` to an APK that
                // uses none of them.
                Claim::Constraint,
            ));
        }
        Ok(out)
    }

    /// Fetches and parses a module's Gradle Module Metadata, if it publishes
    /// any. Gradle ignores the POM when a `.module` file is present, so this is
    /// checked first and the POM is only a fallback.
    fn metadata(&mut self, coordinates: &Coordinates) -> Option<gmm::Module> {
        if let Some(cached) = self.metadata.get(coordinates) {
            return cached.clone();
        }
        // A module that publishes no `.module` is the common case on Maven
        // Central, so a missing one is not a failure. It is recorded either way:
        // leaving the negative case uncached re-probes for it on every call, and
        // several code paths ask per module.
        let module = self.read_metadata(coordinates);
        self.metadata.insert(coordinates.clone(), module.clone());
        module
    }

    fn read_metadata(&self, coordinates: &Coordinates) -> Option<gmm::Module> {
        let path = maven::fetch_artifact_inner(
            &maven::artifact_dir(coordinates),
            &coordinates.group,
            &coordinates.artifact,
            &coordinates.version,
            "module",
            None,
            true,
        )
        .ok()
        .flatten()?;
        let json = match std::fs::read_to_string(&path) {
            Ok(json) => json,
            Err(e) => {
                log::warn!("cannot read metadata for {}: {e}", coordinates.gav());
                return None;
            }
        };
        match serde_json::from_str::<gmm::Module>(&json) {
            Ok(module) => Some(module),
            Err(e) => {
                log::warn!("cannot parse metadata for {}: {e}", coordinates.gav());
                None
            }
        }
    }

    /// Fetches and parses a POM, once.
    fn document(&mut self, coordinates: &Coordinates) -> Result<Pom, NdkError> {
        if let Some(document) = self.documents.get(coordinates) {
            return Ok(document.clone());
        }
        let path = maven::fetch_artifact(
            &maven::artifact_dir(coordinates),
            &coordinates.group,
            &coordinates.artifact,
            &coordinates.version,
            "pom",
        )
        .map_err(|reason| self.fail(coordinates, reason))?
        .ok_or_else(|| self.fail(coordinates, "no POM published"))?;
        let xml = std::fs::read_to_string(&path)
            .map_err(|e| self.fail(coordinates, format!("cannot read POM: {e}")))?;
        let document: Pom = quick_xml::de::from_str(&xml)
            .map_err(|e| self.fail(coordinates, format!("cannot parse POM: {e}")))?;
        self.documents.insert(coordinates.clone(), document.clone());
        Ok(document)
    }

    /// The effective model, built once per set of coordinates.
    fn model(&mut self, module: &ModuleId, version: &str) -> Result<Model, NdkError> {
        let coordinates = Coordinates {
            group: module.group.clone(),
            artifact: module.artifact.clone(),
            version: version.to_owned(),
        };
        if let Some(model) = self.models.get(&coordinates) {
            return Ok(model.clone());
        }
        if !self.building.insert(coordinates.clone()) {
            return Err(self.fail(&coordinates, "its POM's parent chain is cyclic"));
        }
        let built = self.build(&coordinates);
        self.building.remove(&coordinates);
        let model = built?;
        self.models.insert(coordinates, model.clone());
        Ok(model)
    }

    fn build(&mut self, coordinates: &Coordinates) -> Result<Model, NdkError> {
        let raw = self.document(coordinates)?;

        let parent = match &raw.parent {
            Some(parent) if !parent.artifact_id.is_empty() => Some(self.model(
                &ModuleId::new(&parent.group_id, &parent.artifact_id),
                &parent.version,
            )?),
            _ => None,
        };

        let mut model = inherit(&raw, parent.as_ref())?;
        self.expand_imports(&mut model)?;
        Ok(model)
    }

    /// Folds imported BOMs into the management table. A locally declared entry
    /// wins over an imported one, so imports are applied after the parent chain
    /// and only fill gaps.
    fn expand_imports(&mut self, model: &mut Model) -> Result<(), NdkError> {
        let order = model.imports_in_order();
        let management = &mut model.management;
        // The order the POM declares its imports in, not the map's: two BOMs
        // managing the same module resolve to whichever is declared first, and a
        // `BTreeMap` made that the alphabetically first.
        let imports: Vec<ModuleId> = order
            .iter()
            .filter(|module| management.get(module).is_some_and(|managed| managed.bom))
            .cloned()
            .collect();
        for module in imports {
            let version = management
                .get(&module)
                .map(|managed| managed.version.clone())
                .unwrap_or_default();
            let imported = self.model(&module, &version)?;
            for (module, managed) in imported.management {
                management.entry(module).or_insert(managed);
            }
        }
        // An import is a request for the BOM's versions, never a version for the
        // BOM itself. Gradle's `addDependencies` skips import-scoped entries for
        // exactly this reason; leaving one in place would pin a module the
        // consumer also asks for directly to the BOM's own version.
        management.retain(|_, managed| !managed.bom);
        Ok(())
    }

    /// The versions a module has published, from `maven-metadata.xml`.
    fn available(&mut self, module: &ModuleId) -> Result<Vec<MavenVersion>, NdkError> {
        if let Some(known) = self.available.get(module) {
            return Ok(known.clone());
        }
        let dir = maven::cache_dir()
            .join(module.group.replace('.', "/"))
            .join(&module.artifact);
        let path = dir.join("maven-metadata.xml");
        let mut last = String::new();
        if !path.is_file() {
            for repository in maven::repo_bases(&module.group) {
                let url = format!(
                    "{}/{}/{}/maven-metadata.xml",
                    repository.url.trim_end_matches('/'),
                    module.group.replace('.', "/"),
                    module.artifact
                );
                match maven::download_authenticated(&url, &path, repository) {
                    Ok(()) => break,
                    Err(reason) => last = format!("no version list at {url}: {reason}"),
                }
            }
            if !path.is_file() {
                return Err(self.fail(&coordinates_of(module, ""), last));
            }
        }
        let xml = std::fs::read_to_string(&path).map_err(|e| {
            self.fail(
                &coordinates_of(module, ""),
                format!("cannot read version list: {e}"),
            )
        })?;
        let metadata = quick_xml::de::from_str::<Metadata>(&xml).map_err(|e| {
            self.fail(
                &coordinates_of(module, ""),
                format!("cannot parse version list: {e}"),
            )
        })?;
        let versions: Vec<MavenVersion> = metadata
            .versioning
            .and_then(|v| v.versions)
            .map(|v| v.entries)
            .unwrap_or_default()
            .iter()
            .map(|v| MavenVersion::new(v.trim()))
            .collect();
        if versions.is_empty() {
            return Err(self.fail(
                &coordinates_of(module, ""),
                format!("{} lists no versions", path.display()),
            ));
        }
        self.available.insert(module.clone(), versions.clone());
        Ok(versions)
    }
}

/// Merges a POM with its already-resolved parent.
///
/// The child supplies whatever it states and inherits the rest, and where both
/// declare the same module the child's entry wins. Every string is
/// interpolated only at the end, from the merged properties, which is what
/// lets a parent pin a dependency at a version the child chooses.
fn inherit(raw: &Pom, parent: Option<&Model>) -> Result<Model, NdkError> {
    let mut properties = parent.map(|p| p.properties.clone()).unwrap_or_default();
    if let Some(own) = &raw.properties {
        properties.extend(own.clone());
    }

    let group = raw
        .group_id
        .clone()
        .filter(|g| !g.is_empty())
        .or_else(|| parent.map(|p| p.group.clone()))
        .unwrap_or_default();
    let version = raw
        .version
        .clone()
        .filter(|v| !v.is_empty())
        .or_else(|| parent.map(|p| p.version.clone()))
        .unwrap_or_default();
    seed_properties(&mut properties, &group, &version, &raw.parent);

    // `local > import > parent`, the order Gradle's `PomReader.resolveDependencyMgt`
    // applies them in, so an imported BOM outranks an inherited version and the
    // POM's own entries outrank both. The import entries stay marked so that
    // `expand_imports` can fill them in and a platform's own coordinates are not
    // turned into a constraint on themselves.
    let mut management = BTreeMap::new();
    let mut imports = Vec::new();
    for entry in managed_entries(raw) {
        match managed_entry(entry)? {
            Some(managed) if managed.bom => imports.push((entry.module(), managed)),
            Some(managed) => {
                management.insert(entry.module(), managed);
            }
            None => {}
        }
    }
    if let Some(parent) = parent {
        for (module, managed) in parent.management.clone() {
            management.entry(module).or_insert(managed);
        }
    }
    management.extend(imports);

    let mut dependencies: Vec<Declaration> = raw
        .dependencies
        .iter()
        .flat_map(|list| &list.entries)
        .map(Declaration::from)
        .collect();
    if let Some(parent) = parent {
        let declared: BTreeSet<&ModuleId> = dependencies.iter().map(|d| &d.module).collect();
        let inherited: Vec<Declaration> = parent
            .dependencies
            .iter()
            .filter(|d| !declared.contains(&d.module))
            .cloned()
            .collect();
        dependencies.extend(inherited);
    }

    // Substitution runs over the merged model, not over each document as it is
    // read, and that includes what the parent contributed: a parent pinning a
    // dependency at `${revision}` is resolved with the *child's* property, which
    // is how a single version is set for a whole family of modules.
    for managed in management.values_mut() {
        managed.version = interpolate(&managed.version, &properties);
    }
    for declaration in &mut dependencies {
        declaration.substitute(&properties);
    }
    let management = management
        .into_iter()
        .map(|(module, managed)| {
            (
                ModuleId::new(
                    &interpolate(&module.group, &properties),
                    &interpolate(&module.artifact, &properties),
                ),
                managed,
            )
        })
        .collect();

    Ok(Model {
        group: interpolate(&group, &properties),
        version: interpolate(&version, &properties),
        packaging: raw
            .packaging
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "jar".to_owned()),
        properties,
        management,
        dependencies,
    })
}

fn managed_entries(raw: &Pom) -> impl Iterator<Item = &Dependency> {
    raw.dependency_management
        .iter()
        .flat_map(|management| management.dependencies.iter())
        .flat_map(|list| &list.entries)
}

impl Dependency {
    /// `<type>test-jar</type>` publishes a test fixture, not a library, and it
    /// is absent from the main artifact a fetch would look for.
    fn is_test_artifact(&self) -> bool {
        self.kind.as_deref().map(str::trim) == Some("test-jar")
    }

    fn module(&self) -> ModuleId {
        ModuleId::new(&self.group_id, &self.artifact_id)
    }
}

fn coordinates_of(module: &ModuleId, version: &str) -> Coordinates {
    Coordinates {
        group: module.group.clone(),
        artifact: module.artifact.clone(),
        version: version.to_owned(),
    }
}

impl Declaration {
    fn from(entry: &Dependency) -> Self {
        Self {
            module: entry.module(),
            version: entry.version.clone(),
            scope: Scope::parse(entry.scope.as_deref()),
            test_artifact: entry.is_test_artifact(),
            rejects: Vec::new(),
            optional: entry
                .optional
                .as_deref()
                .is_some_and(|o| o.trim() == "true"),
            exclusions: entry
                .exclusions
                .iter()
                .flat_map(|list| &list.entries)
                .map(|e| Exclusion {
                    group: e.group_id.trim().to_owned(),
                    artifact: e.artifact_id.trim().to_owned(),
                })
                .collect(),
            artifact_selector: None,
        }
    }

    fn substitute(&mut self, properties: &BTreeMap<String, String>) {
        self.module = ModuleId::new(
            &interpolate(&self.module.group, properties),
            &interpolate(&self.module.artifact, properties),
        );
        if let Some(version) = &self.version {
            self.version = Some(interpolate(version, properties));
        }
    }
}

/// Reads a `<dependencyManagement>` entry, or `None` when it pins no version.
fn managed_entry(entry: &Dependency) -> Result<Option<Managed>, NdkError> {
    // `<scope>import</scope>` alone makes an import. Gradle's
    // `isDependencyImportScoped` tests nothing else, and Maven's
    // `DefaultModelBuilder` does the same, so requiring `<type>pom</type>` as
    // well discarded a working BOM's versions and left a different resolution.
    let bom = Scope::parse(entry.scope.as_deref()) == Scope::Import;
    let Some(version) = entry.version.clone() else {
        if bom {
            return Err(NdkError::MavenLibFailed {
                gav: entry.module().to_string(),
                reason: "imports a BOM with no `<version>`".to_owned(),
            });
        }
        return Ok(None);
    };
    Ok(Some(Managed { version, bom }))
}

/// Adds the properties Maven makes available without a `<properties>` block.
fn seed_properties(
    properties: &mut BTreeMap<String, String>,
    group: &str,
    version: &str,
    parent: &Option<Parent>,
) {
    for name in ["project.groupId", "pom.groupId", "groupId"] {
        properties.insert(name.to_owned(), group.to_owned());
    }
    for name in ["project.artifactId", "pom.artifactId"] {
        properties.insert(name.to_owned(), "artifactId".to_owned());
    }
    for name in ["project.version", "pom.version", "version"] {
        properties.insert(name.to_owned(), version.to_owned());
    }
    if let Some(parent) = parent {
        properties.insert("project.parent.version".to_owned(), parent.version.clone());
    }
}

/// Substitutes `${...}` from the effective properties.
///
/// An unknown name is left as written, matching Maven, so an unsubstituted
/// `${basedir}` in a build-only section does not fail the read. Repetition
/// covers properties that refer to other properties, which `revision` does.
fn interpolate(text: &str, properties: &BTreeMap<String, String>) -> String {
    let mut current = text.to_owned();
    for _ in 0..5 {
        let mut next = String::with_capacity(current.len());
        let mut rest = current.as_str();
        let mut changed = false;
        while let Some(start) = rest.find("${") {
            next.push_str(&rest[..start]);
            rest = &rest[start + 2..];
            match rest.find('}') {
                Some(end) => {
                    let name = &rest[..end];
                    match properties.get(name) {
                        Some(value) => {
                            next.push_str(value);
                            changed = true;
                        }
                        None => {
                            next.push_str("${");
                            next.push_str(name);
                            next.push('}');
                        }
                    }
                    rest = &rest[end + 1..];
                }
                None => {
                    // No closing brace, so the rest of the string is literal.
                    next.push_str("${");
                    break;
                }
            }
        }
        next.push_str(rest);
        if !changed || next == current {
            return next;
        }
        current = next;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A POM body with the boilerplate stripped, so a test reads as the model
    /// it is about.
    fn pom(body: &str) -> String {
        format!(r#"<project><modelVersion>4.0.0</modelVersion>{body}</project>"#)
    }

    /// Parses one POM with no parent and no imported BOM, through the same
    /// inheritance the resolver uses.
    fn model_of(xml: &str) -> Model {
        let document: Pom = quick_xml::de::from_str(xml).expect("POM parses");
        inherit(&document, None).expect("POM inherits")
    }

    fn versions_of(model: &Model) -> Vec<&str> {
        model
            .dependencies
            .iter()
            .map(|d| d.version.as_deref().unwrap_or("<managed>"))
            .collect()
    }

    #[test]
    fn reads_every_element_of_a_fully_populated_pom() {
        // Every field is asserted here because serde matches element names
        // exactly: a field that misses its `rename` reads as absent and the
        // rest of the document still parses, so a POM can look fine while
        // every group and artifact is empty.
        let document: Pom = quick_xml::de::from_str(
            r#"<project>
                 <groupId>com.example</groupId>
                 <version>1.2.3</version>
                 <packaging>aar</packaging>
                 <parent>
                   <groupId>com.example.parent</groupId>
                   <artifactId>root</artifactId>
                   <version>9</version>
                 </parent>
                 <properties><revision>1.2.3</revision></properties>
                 <dependencyManagement>
                   <dependencies>
                     <dependency>
                       <groupId>com.example.managed</groupId>
                       <artifactId>pinned</artifactId>
                       <version>5.0</version>
                       <type>pom</type>
                       <scope>import</scope>
                     </dependency>
                   </dependencies>
                 </dependencyManagement>
                 <dependencies>
                   <dependency>
                     <groupId>com.example.dep</groupId>
                     <artifactId>leaf</artifactId>
                     <version>3.0</version>
                     <type>aar</type>
                     <scope>runtime</scope>
                     <optional>true</optional>
                     <exclusions>
                       <exclusion>
                         <groupId>com.example.gone</groupId>
                         <artifactId>dropped</artifactId>
                       </exclusion>
                     </exclusions>
                   </dependency>
                 </dependencies>
                 <relocation>
                   <groupId>com.example.moved</groupId>
                   <artifactId>here</artifactId>
                   <version>4.0</version>
                 </relocation>
               </project>"#,
        )
        .expect("POM parses");

        assert_eq!(document.group_id.as_deref(), Some("com.example"));
        assert_eq!(document.version.as_deref(), Some("1.2.3"));
        assert_eq!(document.packaging.as_deref(), Some("aar"));
        assert_eq!(document.properties.as_ref().unwrap()["revision"], "1.2.3");

        let parent = document.parent.as_ref().expect("a parent");
        assert_eq!(parent.group_id, "com.example.parent");
        assert_eq!(parent.artifact_id, "root");
        assert_eq!(parent.version, "9");

        let managed = managed_entries(&document)
            .next()
            .expect("one managed entry");
        assert_eq!(
            managed.module(),
            ModuleId::new("com.example.managed", "pinned")
        );
        assert_eq!(managed.version.as_deref(), Some("5.0"));
        assert_eq!(managed.kind.as_deref(), Some("pom"));
        assert_eq!(managed.scope.as_deref(), Some("import"));

        let leaf = document
            .dependencies
            .as_ref()
            .and_then(|list| list.entries.first())
            .expect("one dependency");
        assert_eq!(leaf.module(), ModuleId::new("com.example.dep", "leaf"));
        assert_eq!(leaf.version.as_deref(), Some("3.0"));
        assert_eq!(leaf.kind.as_deref(), Some("aar"));
        assert_eq!(leaf.scope.as_deref(), Some("runtime"));
        assert_eq!(leaf.optional.as_deref(), Some("true"));
        let exclusion = &leaf.exclusions.as_ref().unwrap().entries[0];
        assert_eq!(exclusion.group_id, "com.example.gone");
        assert_eq!(exclusion.artifact_id, "dropped");

        let relocation = document.relocation.as_ref().expect("a relocation");
        assert_eq!(relocation.group_id, "com.example.moved");
        assert_eq!(relocation.artifact_id, "here");
        assert_eq!(relocation.version, "4.0");
    }

    #[test]
    fn reads_a_version_list() {
        let metadata: Metadata = quick_xml::de::from_str(
            r#"<metadata>
                 <groupId>g</groupId>
                 <versioning>
                   <latest>1.2</latest>
                   <release>1.1</release>
                   <versions>
                     <version>1.0</version>
                     <version>1.1</version>
                     <version>1.2-SNAPSHOT</version>
                   </versions>
                 </versioning>
               </metadata>"#,
        )
        .expect("metadata parses");
        let versions = metadata.versioning.unwrap().versions.unwrap();
        assert_eq!(versions.entries, ["1.0", "1.1", "1.2-SNAPSHOT"]);
    }

    #[test]
    fn dependencies_keep_the_order_the_pom_declares_them_in() {
        let model = model_of(&pom(
            r#"<groupId>g</groupId><artifactId>a</artifactId><version>1</version>
               <dependencies>
                 <dependency><groupId>g</groupId><artifactId>first</artifactId><version>1.0</version></dependency>
                 <dependency><groupId>g</groupId><artifactId>second</artifactId><version>2.0</version></dependency>
               </dependencies>"#,
        ));
        assert_eq!(versions_of(&model), ["1.0", "2.0"]);
    }

    #[test]
    fn a_version_left_for_dependency_management_is_not_filled_in_here() {
        let model = model_of(&pom(
            r#"<groupId>g</groupId><artifactId>a</artifactId><version>1</version>
               <dependencies>
                 <dependency><groupId>g</groupId><artifactId>b</artifactId></dependency>
               </dependencies>"#,
        ));
        assert_eq!(model.dependencies[0].version, None);
    }

    #[test]
    fn dependency_management_is_keyed_by_module() {
        let model = model_of(&pom(
            r#"<groupId>g</groupId><artifactId>a</artifactId><version>1</version>
               <dependencyManagement><dependencies>
                 <dependency>
                   <groupId>g</groupId><artifactId>b</artifactId><version>3.0</version>
                   <scope>runtime</scope>
                 </dependency>
               </dependencies></dependencyManagement>"#,
        ));
        let managed = model.management.get(&ModuleId::new("g", "b")).unwrap();
        assert_eq!(managed.version, "3.0");
        assert!(!managed.bom);
    }

    #[test]
    fn import_scope_alone_makes_an_import() {
        // Gradle's `isDependencyImportScoped` tests the scope and nothing else,
        // and Maven's `DefaultModelBuilder` does the same. Requiring
        // `<type>pom</type>` as well discarded a working BOM's versions and left
        // a different resolution behind.
        let model = model_of(&pom(
            r#"<groupId>g</groupId><artifactId>a</artifactId><version>1</version>
               <dependencyManagement><dependencies>
                 <dependency>
                   <groupId>g</groupId><artifactId>bom</artifactId><version>1.2</version>
                   <type>pom</type><scope>import</scope>
                 </dependency>
                 <dependency>
                   <groupId>g</groupId><artifactId>other-bom</artifactId><version>1.0</version>
                   <scope>import</scope>
                 </dependency>
                 <dependency>
                   <groupId>g</groupId><artifactId>pinned</artifactId><version>3.0</version>
                 </dependency>
               </dependencies></dependencyManagement>"#,
        ));
        assert!(
            model
                .management
                .get(&ModuleId::new("g", "bom"))
                .unwrap()
                .bom
        );
        assert!(
            model
                .management
                .get(&ModuleId::new("g", "other-bom"))
                .unwrap()
                .bom,
            "`<scope>import</scope>` on its own is an import"
        );
        assert!(
            !model
                .management
                .get(&ModuleId::new("g", "pinned"))
                .unwrap()
                .bom,
            "no scope is a plain managed version"
        );
    }

    #[test]
    fn properties_interpolate_in_both_a_version_and_a_module_id() {
        let model = model_of(&pom(
            r#"<groupId>g</groupId><artifactId>a</artifactId><version>${revision}</version>
               <properties>
                 <revision>2.6.1</revision>
                 <lib>shared</lib>
                 <lib.version>4.0.0</lib.version>
               </properties>
               <dependencies>
                 <dependency>
                   <groupId>g</groupId><artifactId>${lib}</artifactId>
                   <version>${lib.version}</version>
                 </dependency>
               </dependencies>"#,
        ));
        assert_eq!(model.version, "2.6.1");
        assert_eq!(model.dependencies[0].module, ModuleId::new("g", "shared"));
        assert_eq!(model.dependencies[0].version.as_deref(), Some("4.0.0"));
    }

    #[test]
    fn a_property_may_refer_to_another_property() {
        let properties = BTreeMap::from([
            ("revision".to_owned(), "1.0.0".to_owned()),
            ("version".to_owned(), "${revision}".to_owned()),
        ]);
        assert_eq!(interpolate("${version}", &properties), "1.0.0");
    }

    #[test]
    fn an_unknown_property_is_left_alone() {
        let properties = BTreeMap::from([("a".to_owned(), "1".to_owned())]);
        assert_eq!(interpolate("${a}-${b}", &properties), "1-${b}");
        assert_eq!(interpolate("${unterminated", &properties), "${unterminated");
    }

    #[test]
    fn only_compile_and_runtime_reach_the_classpath() {
        assert!(Scope::parse(None).reaches_classpath());
        assert!(Scope::parse(Some("compile")).reaches_classpath());
        assert!(Scope::parse(Some("runtime")).reaches_classpath());
        for scope in ["provided", "test", "system", "import"] {
            assert!(
                !Scope::parse(Some(scope)).reaches_classpath(),
                "{scope} should not reach the classpath"
            );
        }
    }

    #[test]
    fn a_wildcard_exclusion_matches_any_module() {
        let wildcard = Exclusion {
            group: "*".to_owned(),
            artifact: "*".to_owned(),
        };
        assert!(wildcard.matches(&ModuleId::new("any", "thing")));

        let named = Exclusion {
            group: "g".to_owned(),
            artifact: "b".to_owned(),
        };
        assert!(named.matches(&ModuleId::new("g", "b")));
        assert!(!named.matches(&ModuleId::new("g", "c")));
        assert!(!named.matches(&ModuleId::new("h", "b")));
    }

    #[test]
    fn an_exclusion_is_read_with_both_its_endpoints() {
        let model = model_of(&pom(
            r#"<groupId>g</groupId><artifactId>a</artifactId><version>1</version>
               <dependencies>
                 <dependency>
                   <groupId>g</groupId><artifactId>b</artifactId><version>1</version>
                   <exclusions>
                     <exclusion><groupId>org.x</groupId><artifactId>*</artifactId></exclusion>
                     <exclusion><groupId>*</groupId><artifactId>annotations</artifactId></exclusion>
                   </exclusions>
                 </dependency>
               </dependencies>"#,
        ));
        let exclusions = &model.dependencies[0].exclusions;
        assert_eq!(exclusions.len(), 2);
        assert!(exclusions[0].matches(&ModuleId::new("org.x", "anything")));
        assert!(exclusions[1].matches(&ModuleId::new("anything", "annotations")));
    }

    #[test]
    fn optional_and_unknown_scopes_are_read_as_written() {
        let model = model_of(&pom(
            r#"<groupId>g</groupId><artifactId>a</artifactId><version>1</version>
               <dependencies>
                 <dependency>
                   <groupId>g</groupId><artifactId>b</artifactId><version>1</version>
                   <optional>true</optional>
                 </dependency>
               </dependencies>"#,
        ));
        assert!(model.dependencies[0].optional);
    }

    /// A POM that declares no dependencies, for a module that is only ever a
    /// leaf in a test.
    fn leaf() -> String {
        pom("<groupId>g</groupId><artifactId>leaf</artifactId><version>1</version>")
    }

    /// A resolver whose POM documents are supplied instead of downloaded, so
    /// mediation can be exercised without a repository.
    fn offline(poms: &[(&str, &str, &str, String)]) -> Resolver {
        let mut resolver = Resolver::default();
        for (group, artifact, version, xml) in poms {
            resolver.documents.insert(
                Coordinates {
                    group: (*group).to_owned(),
                    artifact: (*artifact).to_owned(),
                    version: (*version).to_owned(),
                },
                quick_xml::de::from_str(xml).expect("fixture POM parses"),
            );
        }
        resolver
    }

    fn seed_roots(resolver: &mut Resolver, roots: &[(&str, &str, &str)]) {
        for (group, artifact, version) in roots {
            resolver
                .consider(
                    ModuleId::new(group, artifact),
                    Requirement::parse(version),
                    0,
                    Vec::new(),
                    true,
                    false,
                )
                .expect("root resolves");
        }
    }

    fn resolve_with(resolver: &mut Resolver, roots: &[(&str, &str, &str)]) -> Vec<String> {
        seed_roots(resolver, roots);
        resolver.walk();
        let mut gavs: Vec<String> = resolver
            .results()
            .iter()
            .map(|r| r.coordinates.gav())
            .collect();
        gavs.sort();
        gavs
    }

    #[test]
    fn the_newest_declared_version_wins_however_far_away_it_is() {
        // `b` is wanted at 1.0 from one hop away and 2.0 from three, so 2.0
        // wins: distance is not a tiebreak against the version itself.
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>near</artifactId><version>1.0</version></dependency>
                         <dependency><groupId>g</groupId><artifactId>far</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "near",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>near</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "far",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>far</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>mid</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "mid",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>mid</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>deeper</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "deeper",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>deeper</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>2.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "b", "1.0", leaf()),
            ("g", "b", "2.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert!(resolved.contains(&"g:b:2.0".to_owned()), "{resolved:?}");
        assert!(!resolved.contains(&"g:b:1.0".to_owned()), "{resolved:?}");
    }

    #[test]
    fn the_order_roots_are_written_in_does_not_change_the_result() {
        // The reason this resolver prefers the newest version rather than the
        // nearest: otherwise reordering `android_libs` silently changes which
        // artifacts a build gets.
        let graph = || -> Vec<(&'static str, &'static str, &'static str, String)> {
            vec![
                (
                    "g",
                    "root",
                    "1.0",
                    pom(
                        r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                           <dependencies>
                             <dependency><groupId>g</groupId><artifactId>left</artifactId><version>1.0</version></dependency>
                             <dependency><groupId>g</groupId><artifactId>right</artifactId><version>1.0</version></dependency>
                           </dependencies>"#,
                    ),
                ),
                (
                    "g",
                    "left",
                    "1.0",
                    pom(
                        r#"<groupId>g</groupId><artifactId>left</artifactId><version>1.0</version>
                           <dependencies>
                             <dependency><groupId>g</groupId><artifactId>b</artifactId><version>1.0</version></dependency>
                           </dependencies>"#,
                    ),
                ),
                (
                    "g",
                    "right",
                    "1.0",
                    pom(
                        r#"<groupId>g</groupId><artifactId>right</artifactId><version>1.0</version>
                           <dependencies>
                             <dependency><groupId>g</groupId><artifactId>b</artifactId><version>2.0</version></dependency>
                           </dependencies>"#,
                    ),
                ),
                ("g", "b", "1.0", leaf()),
                ("g", "b", "2.0", leaf()),
            ]
        };

        let mut forwards = offline(&graph());
        let mut backwards = offline(&graph());
        let a = resolve_with(&mut forwards, &[("g", "root", "1.0")]);
        let b = resolve_with(&mut backwards, &[("g", "root", "1.0")]);
        assert_eq!(a, b);
        assert!(a.contains(&"g:b:2.0".to_owned()), "{a:?}");
    }

    #[test]
    fn a_dependency_management_version_competes_like_any_other() {
        // `root` pins `b` at 9.0 for its own dependency on `b`, and a
        // transitive path asks for 1.0. Under newest-wins the pin wins, which
        // is what Gradle does with a platform's version.
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencyManagement><dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>9.0</version></dependency>
                       </dependencies></dependencyManagement>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId></dependency>
                         <dependency><groupId>g</groupId><artifactId>other</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "other",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>other</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "b", "1.0", leaf()),
            ("g", "b", "9.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert!(resolved.contains(&"g:b:9.0".to_owned()), "{resolved:?}");
        assert!(!resolved.contains(&"g:b:1.0".to_owned()), "{resolved:?}");
    }

    #[test]
    fn a_prerelease_never_outranks_its_own_release() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>2.0-rc1</version></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "b", "2.0-rc1", leaf()),
            ("g", "b", "2.0", leaf()),
        ]);
        // The release is not declared anywhere, so the resolver cannot invent
        // it; what this pins is that `2.0` sorts above `2.0-rc1` when both are
        // present, which `MavenVersion` owns.
        assert!(MavenVersion::new("2.0") > MavenVersion::new("2.0-rc1"));
        let _ = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
    }

    #[test]
    fn a_dependency_management_version_fills_in_one_left_out() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencyManagement><dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>9.0</version></dependency>
                       </dependencies></dependencyManagement>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "b", "9.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(resolved, ["g:b:9.0", "g:root:1.0"]);
    }

    #[test]
    fn an_imported_bom_supplies_versions() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencyManagement><dependencies>
                         <dependency>
                           <groupId>g</groupId><artifactId>bom</artifactId><version>1.0</version>
                           <type>pom</type><scope>import</scope>
                         </dependency>
                       </dependencies></dependencyManagement>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "used",
                "1.0",
                pom("<groupId>g</groupId><artifactId>used</artifactId><version>1.0</version>"),
            ),
            (
                "g",
                "used",
                "2.0",
                pom("<groupId>g</groupId><artifactId>used</artifactId><version>2.0</version>"),
            ),
            (
                "g",
                "bom",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>bom</artifactId><version>1.0</version>
                       <packaging>pom</packaging>
                       <dependencyManagement><dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>4.0</version></dependency>
                       </dependencies></dependencyManagement>"#,
                ),
            ),
            ("g", "b", "4.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        // The BOM itself is `pom`-packaged, so it is not an artifact to ship.
        assert_eq!(resolved, ["g:b:4.0", "g:root:1.0"]);
    }

    #[test]
    fn a_managed_entry_declared_locally_shadows_the_one_an_imported_bom_gives() {
        // `dependencyManagement` holds one entry per module, and Maven's
        // importer only fills gaps, so the local 2.0 is what the dependency
        // gets; the BOM's 4.0 never becomes a competing claim.
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencyManagement><dependencies>
                         <dependency>
                           <groupId>g</groupId><artifactId>bom</artifactId><version>1.0</version>
                           <type>pom</type><scope>import</scope>
                         </dependency>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>2.0</version></dependency>
                       </dependencies></dependencyManagement>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "bom",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>bom</artifactId><version>1.0</version>
                       <packaging>pom</packaging>
                       <dependencyManagement><dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>4.0</version></dependency>
                       </dependencies></dependencyManagement>"#,
                ),
            ),
            ("g", "b", "2.0", leaf()),
            ("g", "b", "4.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(resolved, ["g:b:2.0", "g:root:1.0"]);
    }

    #[test]
    fn an_exclusion_prunes_the_whole_subtree_below_it() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency>
                           <groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>
                           <exclusions>
                             <exclusion><groupId>g</groupId><artifactId>pruned</artifactId></exclusion>
                           </exclusions>
                         </dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "a",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>pruned</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "pruned",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>pruned</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>below</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "below", "1.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(resolved, ["g:a:1.0", "g:root:1.0"]);
    }

    #[test]
    fn an_exclusion_on_one_path_does_not_remove_the_module_from_another() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency>
                           <groupId>g</groupId><artifactId>excluder</artifactId><version>1.0</version>
                           <exclusions>
                             <exclusion><groupId>g</groupId><artifactId>shared</artifactId></exclusion>
                           </exclusions>
                         </dependency>
                         <dependency><groupId>g</groupId><artifactId>keeper</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "excluder",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>excluder</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>shared</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "keeper",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>keeper</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>shared</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "shared", "1.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(
            resolved,
            [
                "g:excluder:1.0",
                "g:keeper:1.0",
                "g:root:1.0",
                "g:shared:1.0"
            ]
        );
    }

    #[test]
    fn optional_and_non_classpath_scopes_are_left_out() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency>
                           <groupId>g</groupId><artifactId>wanted</artifactId><version>1.0</version>
                         </dependency>
                         <dependency>
                           <groupId>g</groupId><artifactId>optional-one</artifactId><version>1.0</version>
                           <optional>true</optional>
                         </dependency>
                         <dependency>
                           <groupId>g</groupId><artifactId>provided-one</artifactId><version>1.0</version>
                           <scope>provided</scope>
                         </dependency>
                         <dependency>
                           <groupId>g</groupId><artifactId>test-one</artifactId><version>1.0</version>
                           <scope>test</scope>
                         </dependency>
                         <dependency>
                           <groupId>g</groupId><artifactId>runtime-one</artifactId><version>1.0</version>
                           <scope>runtime</scope>
                         </dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "wanted", "1.0", leaf()),
            ("g", "optional-one", "1.0", leaf()),
            ("g", "provided-one", "1.0", leaf()),
            ("g", "test-one", "1.0", leaf()),
            ("g", "runtime-one", "1.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(
            resolved,
            ["g:root:1.0", "g:runtime-one:1.0", "g:wanted:1.0"]
        );
    }

    #[test]
    fn a_dependency_cycle_terminates() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>a</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "a",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "b",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>b</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>a</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(resolved, ["g:a:1.0", "g:b:1.0", "g:root:1.0"]);
    }

    #[test]
    fn a_parent_supplies_what_the_child_leaves_out() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <parent>
                         <groupId>g</groupId><artifactId>base</artifactId><version>1.0</version>
                       </parent>
                       <properties><dep.version>7.0</dep.version></properties>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "base",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>base</artifactId><version>1.0</version>
                       <dependencyManagement><dependencies>
                         <dependency>
                           <groupId>g</groupId><artifactId>b</artifactId>
                           <version>${dep.version}</version>
                         </dependency>
                       </dependencies></dependencyManagement>"#,
                ),
            ),
            ("g", "b", "7.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(resolved, ["g:b:7.0", "g:root:1.0"]);
    }

    /// A resolver whose POMs and Gradle Module Metadata are both supplied, for
    /// the cases where the two disagree.
    fn offline_with_gmm(
        poms: &[(&str, &str, &str, String)],
        modules: &[(&str, &str, &str, &str)],
    ) -> Resolver {
        let mut resolver = offline(poms);
        for (group, artifact, version, json) in modules {
            let coordinates = Coordinates {
                group: (*group).to_owned(),
                artifact: (*artifact).to_owned(),
                version: (*version).to_owned(),
            };
            let parsed: gmm::Module = serde_json::from_str(json).expect("fixture metadata parses");
            resolver.metadata.insert(coordinates, Some(parsed));
        }
        resolver
    }

    /// A module file whose runtime variant names `depends` and ships one
    /// archive of the given kind.
    fn gmm_with(group: &str, artifact: &str, depends: &str, elements: &str) -> String {
        format!(
            r#"{{
                 "formatVersion": "1.1",
                 "component": {{"group":"{group}","module":"{artifact}","version":"1.0"}},
                 "variants": [
                   {{
                     "name": "runtimeElements",
                     "attributes": {{
                       "org.gradle.category": "library",
                       "org.gradle.usage": "java-runtime",
                       "org.gradle.libraryelements": "{elements}"
                     }},
                     "files": [{{"name":"x.{elements}","url":"x.{elements}","size":1,"sha1":"ab"}}],
                     "dependencies": [{depends}]
                   }}
                 ]
               }}"#
        )
    }

    #[test]
    fn a_dependency_constraint_does_not_pull_the_module_into_the_closure() {
        // A platform's constraint list names every version it coordinates, most
        // of which the consumer never asked for. Following one as a dependency
        // adds Compose, test fixtures and `guava` to an APK using none of them.
        // The shape is a real one: a BOM that also publishes a library variant.
        let mut resolver = offline_with_gmm(
            &[("g", "root", "1.0", leaf())],
            &[(
                "g",
                "root",
                "1.0",
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
                           {"group":"g","module":"advised","version":{"requires":"4.0"}}
                         ]
                       },
                       {
                         "name": "runtimeElements",
                         "attributes": {
                           "org.gradle.category": "library",
                           "org.gradle.usage": "java-runtime",
                           "org.gradle.libraryelements": "jar"
                         },
                         "files": [{"name":"x.jar","url":"x.jar","size":1,"sha1":"ab"}]
                       }
                     ]
                   }"#,
            )],
        );
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(resolved, ["g:root:1.0"]);
    }

    #[test]
    fn a_pom_packaged_module_still_brings_its_own_dependencies() {
        // Maven calls that an aggregator. Treating it as a platform and stopping
        // there would drop its whole subtree with no warning.
        let mut resolver = offline(&[
            (
                "g",
                "app",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>app</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>bundle</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "bundle",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>bundle</artifactId><version>1.0</version>
                       <packaging>pom</packaging>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>realdep</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "realdep",
                "1.0",
                pom("<groupId>g</groupId><artifactId>realdep</artifactId><version>1.0</version>"),
            ),
        ]);
        seed_roots(&mut resolver, &[("g", "app", "1.0")]);
        resolver.walk();
        resolver.apply_platforms().expect("platforms apply");
        let artifacts: Vec<String> = resolver
            .results()
            .into_iter()
            .map(|r| r.coordinates.artifact)
            .collect();
        assert!(
            artifacts.contains(&"realdep".to_owned()),
            "an aggregator's own dependencies are still resolved: {artifacts:?}"
        );
    }

    #[test]
    fn a_gmm_only_platform_still_coordinates() {
        // Some platforms publish their versions only as `dependencyConstraints`
        // in the `.module`, with no POM management at all. Reading only the POM
        // would leave such a platform with no effect whatsoever, and
        // `select` rejects a platform variant by design, so it has to be found
        // on its own terms.
        let mut resolver = offline_with_gmm(
            &[
                (
                    "g",
                    "lib",
                    "1.0",
                    pom("<groupId>g</groupId><artifactId>lib</artifactId><version>1.0</version>"),
                ),
                (
                    "g",
                    "lib",
                    "2.0",
                    pom("<groupId>g</groupId><artifactId>lib</artifactId><version>2.0</version>"),
                ),
                (
                    "g",
                    "platform",
                    "1.0",
                    pom(
                        "<groupId>g</groupId><artifactId>platform</artifactId><version>1.0</version><packaging>pom</packaging>",
                    ),
                ),
            ],
            &[(
                "g",
                "platform",
                "1.0",
                r#"{"formatVersion":"1.1","variants":[{"name":"platformRuntimeElements",
                    "attributes":{"org.gradle.category":"platform","org.gradle.usage":"java-runtime"},
                    "dependencyConstraints":[{"group":"g","module":"lib","version":{"requires":"2.0"}}]}]}"#,
            )],
        );
        seed_roots(
            &mut resolver,
            &[("g", "lib", "1.0"), ("g", "platform", "1.0")],
        );
        resolver.walk();
        resolver.apply_platforms().expect("platforms apply");
        assert_eq!(
            resolver
                .selected
                .get(&ModuleId::new("g", "lib"))
                .map(|s| s.version.clone()),
            Some("2.0".to_owned()),
            "a platform whose versions are only in its .module still applies"
        );
    }

    #[test]
    fn a_rejected_version_is_not_a_candidate() {
        // Gradle enforces `rejects` rather than recording it: a static version is
        // checked too, so a module published only at a rejected version fails the
        // build naming the version. A published rejection is a statement the
        // publisher expects to hold — it is how a known-broken version is kept
        // out of a classpath.
        let mut resolver = offline_with_gmm(
            &[
                (
                    "g",
                    "app",
                    "1.0",
                    pom("<groupId>g</groupId><artifactId>app</artifactId><version>1.0</version>"),
                ),
                (
                    "g",
                    "lib",
                    "1.5",
                    pom("<groupId>g</groupId><artifactId>lib</artifactId><version>1.5</version>"),
                ),
                (
                    "g",
                    "lib",
                    "1.6",
                    pom("<groupId>g</groupId><artifactId>lib</artifactId><version>1.6</version>"),
                ),
            ],
            &[(
                "g",
                "app",
                "1.0",
                r#"{"formatVersion":"1.1","variants":[{"name":"runtimeElements",
                    "attributes":{"org.gradle.category":"library","org.gradle.usage":"java-runtime"},
                    "files":[{"name":"app-1.0.jar","url":"app-1.0.jar","size":1,"sha1":"ab"}],
                    "dependencies":[{"group":"g","module":"lib",
                        "version":{"requires":"[1.0,2.0)","rejects":["1.6"]}}]}]}"#,
            )],
        );
        seed_available(&mut resolver, "g", "lib", &["1.4", "1.5", "1.6"]);
        seed_roots(&mut resolver, &[("g", "app", "1.0")]);
        resolver.walk();
        assert_eq!(
            resolver
                .selected
                .get(&ModuleId::new("g", "lib"))
                .map(|s| s.version.clone()),
            Some("1.5".to_owned()),
            "1.6 is rejected, so the next newest is chosen"
        );
    }

    #[test]
    fn a_static_version_that_is_rejected_is_not_selected() {
        let mut resolver = offline_with_gmm(
            &[
                (
                    "g",
                    "app",
                    "1.0",
                    pom("<groupId>g</groupId><artifactId>app</artifactId><version>1.0</version>"),
                ),
                (
                    "g",
                    "lib",
                    "1.6",
                    pom("<groupId>g</groupId><artifactId>lib</artifactId><version>1.6</version>"),
                ),
            ],
            &[(
                "g",
                "app",
                "1.0",
                r#"{"formatVersion":"1.1","variants":[{"name":"runtimeElements",
                    "attributes":{"org.gradle.category":"library","org.gradle.usage":"java-runtime"},
                    "files":[{"name":"app-1.0.jar","url":"app-1.0.jar","size":1,"sha1":"ab"}],
                    "dependencies":[{"group":"g","module":"lib",
                        "version":{"requires":"1.6","rejects":["1.6"]}}]}]}"#,
            )],
        );
        seed_roots(&mut resolver, &[("g", "app", "1.0")]);
        resolver.walk();
        assert!(
            !resolver.selected.contains_key(&ModuleId::new("g", "lib")),
            "a version that is both required and rejected is not selected"
        );
    }

    #[test]
    fn a_module_that_publishes_only_a_platform_ships_nothing() {
        // `platform()` names a module that coordinates versions and carries no
        // artifact, which is what a BOM is. There is nothing to put in an APK.
        let mut resolver = offline_with_gmm(
            &[("g", "bom", "1.0", leaf())],
            &[(
                "g",
                "bom",
                "1.0",
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
                           {"group":"g","module":"advised","version":{"requires":"4.0"}}
                         ]
                       }
                     ]
                   }"#,
            )],
        );
        let resolved = resolve_with(&mut resolver, &[("g", "bom", "1.0")]);
        assert!(
            resolved.is_empty(),
            "a platform contributes constraints, not an artifact: {resolved:?}"
        );
    }

    #[test]
    fn a_dependency_constraint_still_raises_a_version_something_else_wanted() {
        let mut resolver = offline_with_gmm(
            &[
                (
                    "g",
                    "root",
                    "1.0",
                    pom(
                        r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                           <dependencies>
                             <dependency><groupId>g</groupId><artifactId>b</artifactId><version>1.0</version></dependency>
                           </dependencies>"#,
                    ),
                ),
                ("g", "b", "1.0", leaf()),
                ("g", "b", "4.0", leaf()),
            ],
            &[],
        );
        // Advice recorded before anything wants the module, then applied when
        // the module does turn up.
        resolver
            .constrained
            .insert(ModuleId::new("g", "b"), "4.0".to_owned());
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert!(resolved.contains(&"g:b:4.0".to_owned()), "{resolved:?}");
    }

    /// A module file whose runtime variant redirects to another module.
    fn gmm_redirect(target: &str) -> String {
        format!(
            r#"{{
                 "formatVersion": "1.1",
                 "variants": [
                   {{
                     "name": "runtimeElements",
                     "attributes": {{
                       "org.gradle.category": "library",
                       "org.gradle.usage": "java-runtime",
                       "org.gradle.libraryelements": "jar"
                     }},
                     "available-at": {{
                       "url": "../{target}/{target}.module",
                       "group": "g",
                       "module": "{target}",
                       "version": ""
                     }}
                   }}
                 ]
               }}"#
        )
    }

    #[test]
    fn a_redirect_fetches_the_target_without_letting_two_versions_ship() {
        // `androidx.annotation:annotation` publishes a stub jar of its own and
        // points its JVM variants at `annotation-jvm`. The graph must still
        // record one module with one version, and the artifact fetched is the
        // target's.
        let mut resolver = offline_with_gmm(
            &[
                (
                    "g",
                    "root",
                    "1.0",
                    pom(
                        r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                           <dependencies>
                             <dependency><groupId>g</groupId><artifactId>wanted</artifactId><version>1.0</version></dependency>
                             <dependency><groupId>g</groupId><artifactId>other</artifactId><version>1.0</version></dependency>
                           </dependencies>"#,
                    ),
                ),
                ("g", "other", "1.0", leaf()),
                ("g", "wanted", "1.0", leaf()),
                ("g", "wanted", "1.6.0", leaf()),
            ],
            &[
                (
                    "g",
                    "other",
                    "1.0",
                    &gmm_with(
                        "g",
                        "other",
                        r#"{"group":"g","module":"wanted","version":{"requires":"1.6.0"}}"#,
                        "aar",
                    ),
                ),
                ("g", "wanted", "1.0", &gmm_redirect("wanted-jvm")),
                ("g", "wanted", "1.6.0", &gmm_redirect("wanted-jvm")),
                (
                    "g",
                    "wanted-jvm",
                    "1.0",
                    &gmm_with("g", "wanted-jvm", "", "jar"),
                ),
                (
                    "g",
                    "wanted-jvm",
                    "1.6.0",
                    &gmm_with("g", "wanted-jvm", "", "jar"),
                ),
            ],
        );
        seed_roots(&mut resolver, &[("g", "root", "1.0")]);
        resolver.walk();
        let resolved = resolver.results();

        // One artifact, at the newest version, under the target's coordinates.
        let wanted: Vec<&Resolved> = resolved
            .iter()
            .filter(|r| {
                r.coordinates.artifact == "wanted" || r.coordinates.artifact == "wanted-jvm"
            })
            .collect();
        assert_eq!(wanted.len(), 1, "{resolved:?}");
        assert_eq!(wanted[0].coordinates.gav(), "g:wanted-jvm:1.6.0");
        assert_eq!(wanted[0].extension.as_deref(), Some("jar"));
        // The packaging follows the archive that will actually be fetched, so
        // a `libraryelements: jar` module is not reported as an AAR.
        assert_eq!(wanted[0].packaging, "jar");
        // And the redirecting module's own coordinates are not fetched, which
        // is what keeps a stub jar off the classpath.
        assert!(
            !resolved
                .iter()
                .any(|r| r.coordinates.gav() == "g:wanted:1.6.0"),
            "{resolved:?}"
        );
    }

    #[test]
    fn a_variant_with_no_files_is_left_out_of_the_closure() {
        let mut resolver = offline_with_gmm(
            &[("g", "root", "1.0", leaf())],
            &[(
                "g",
                "root",
                "1.0",
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
                           {"group":"g","module":"advised","version":{"requires":"1.0"}}
                         ]
                       },
                       {
                         "name": "runtimeElements",
                         "attributes": {
                           "org.gradle.category": "library",
                           "org.gradle.usage": "java-runtime",
                           "org.gradle.libraryelements": "aar"
                         },
                         "files": [{"name":"x.aar","url":"x.aar","size":1,"sha1":"ab"}]
                       }
                     ]
                   }"#,
            )],
        );
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(resolved, ["g:root:1.0"]);
        assert!(resolver.results().first().unwrap().extension.is_some());
    }

    #[test]
    fn a_dependency_with_no_version_and_no_management_is_reported_not_guessed() {
        let mut resolver = offline(&[
            (
                "g",
                "root",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>root</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "b", "1.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "root", "1.0")]);
        assert_eq!(resolved, ["g:root:1.0"]);
    }
    /// A range is resolved against the repository's version list, which is
    /// normally fetched; tests seed it instead of going to the network.
    fn seed_available(resolver: &mut Resolver, group: &str, artifact: &str, versions: &[&str]) {
        resolver.available.insert(
            ModuleId::new(group, artifact),
            versions.iter().map(|v| MavenVersion::new(v)).collect(),
        );
    }

    #[test]
    fn a_hard_range_the_winner_falls_outside_is_reported() {
        // Two surviving paths reach `c`: one asks for the hard range [1.0], the
        // other for 2.0. Newest-wins takes 2.0, which `[1.0]` does not allow.
        let mut resolver = offline(&[
            (
                "g",
                "a",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>c</artifactId><version>[1.0]</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "b",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>b</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>c</artifactId><version>2.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "c", "1.0", leaf()),
            ("g", "c", "2.0", leaf()),
        ]);
        seed_available(&mut resolver, "g", "c", &["1.0", "2.0"]);
        let resolved = resolve_with(&mut resolver, &[("g", "a", "1.0"), ("g", "b", "1.0")]);
        assert!(resolved.contains(&"g:c:2.0".to_owned()), "{resolved:?}");
        assert_eq!(
            resolver.range_violations.len(),
            1,
            "{:?}",
            resolver.range_violations
        );
        assert_eq!(resolver.range_violations[0].required, "[1.0]");
        assert_eq!(resolver.range_violations[0].selected, "2.0");
    }

    #[test]
    fn a_range_the_winner_satisfies_is_not_reported() {
        let mut resolver = offline(&[
            (
                "g",
                "a",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>c</artifactId><version>[1.0,3.0)</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "b",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>b</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>c</artifactId><version>2.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "c", "2.0", leaf()),
        ]);
        seed_available(&mut resolver, "g", "c", &["2.0"]);
        let _ = resolve_with(&mut resolver, &[("g", "a", "1.0"), ("g", "b", "1.0")]);
        assert!(
            resolver.range_violations.is_empty(),
            "{:?}",
            resolver.range_violations
        );
    }

    #[test]
    fn a_platform_moves_a_version_already_on_the_classpath() {
        // The Kotlin case: `stdlib-jdk7:1.6.21` carries classes that
        // `stdlib:1.8.22` also carries, so d8 refuses the pair. Newest-wins
        // cannot reconcile two different modules, and only the platform can,
        // because it is the one artifact that speaks for the whole family.
        let mut resolver = offline(&[
            (
                "g",
                "a",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib-jdk7</artifactId><version>1.6.21</version></dependency>
                         <dependency><groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib</artifactId><version>1.8.22</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "org.jetbrains.kotlin",
                "kotlin-bom",
                "1.8.22",
                pom(
                    r#"<groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-bom</artifactId><version>1.8.22</version>
                       <packaging>pom</packaging>
                       <dependencyManagement><dependencies>
                         <dependency><groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib-jdk7</artifactId><version>1.8.22</version></dependency>
                         <dependency><groupId>org.jetbrains.kotlin</groupId><artifactId>kotlin-stdlib-jdk8</artifactId><version>1.8.22</version></dependency>
                       </dependencies></dependencyManagement>"#,
                ),
            ),
        ]);
        seed_roots(
            &mut resolver,
            &[
                ("g", "a", "1.0"),
                ("org.jetbrains.kotlin", "kotlin-bom", "1.8.22"),
            ],
        );
        resolver.walk();
        resolver.apply_platforms().expect("platforms apply");
        let selected = |artifact: &str| {
            resolver
                .selected
                .get(&ModuleId::new("org.jetbrains.kotlin", artifact))
                .map(|s| s.version.clone())
        };
        assert_eq!(selected("kotlin-stdlib-jdk7").as_deref(), Some("1.8.22"));
        assert_eq!(selected("kotlin-stdlib"), Some("1.8.22".to_owned()));
    }

    #[test]
    fn a_platform_does_not_pull_in_a_module_nothing_asked_for() {
        // A BOM coordinates every version it lists, most of which the app never
        // reaches. Adding those would put a jar in the APK that nothing uses.
        let mut resolver = offline(&[
            (
                "g",
                "a",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>used</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "used",
                "1.0",
                pom("<groupId>g</groupId><artifactId>used</artifactId><version>1.0</version>"),
            ),
            (
                "g",
                "used",
                "2.0",
                pom("<groupId>g</groupId><artifactId>used</artifactId><version>2.0</version>"),
            ),
            (
                "g",
                "bom",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>bom</artifactId><version>1.0</version>
                       <packaging>pom</packaging>
                       <dependencyManagement><dependencies>
                         <dependency><groupId>g</groupId><artifactId>used</artifactId><version>2.0</version></dependency>
                         <dependency><groupId>g</groupId><artifactId>unwanted</artifactId><version>3.0</version></dependency>
                       </dependencies></dependencyManagement>"#,
                ),
            ),
        ]);
        seed_available(&mut resolver, "g", "unwanted", &["3.0"]);
        seed_roots(&mut resolver, &[("g", "a", "1.0"), ("g", "bom", "1.0")]);
        resolver.walk();
        resolver.apply_platforms().expect("platforms apply");
        assert_eq!(
            resolver
                .selected
                .get(&ModuleId::new("g", "used"))
                .map(|s| s.version.clone()),
            Some("2.0".to_owned()),
            "the platform moves a module that is wanted"
        );
        assert!(
            !resolver
                .selected
                .contains_key(&ModuleId::new("g", "unwanted")),
            "the platform advises, it does not add"
        );
    }
    /// Two modules, where the first declares a capability the second provides
    /// implicitly, and both end up claiming the same capability version.
    fn capability_pair(asm_version: &str, ow2_version: &str) -> Resolver {
        let provides = r#"{"formatVersion":"1.1","variants":[{"name":"runtimeElements",
            "attributes":{"org.gradle.category":"library","org.gradle.usage":"java-runtime"},
            "capabilities":[{"group":"org.ow2.asm","name":"asm","version":"9.7"}]}]}"#;
        let mut resolver = offline_with_gmm(
            &[
                (
                    "asm",
                    "asm",
                    asm_version,
                    pom(
                        "<groupId>asm</groupId><artifactId>asm</artifactId><version>3.3.1</version>",
                    ),
                ),
                (
                    "org.ow2.asm",
                    "asm",
                    ow2_version,
                    pom(
                        "<groupId>org.ow2.asm</groupId><artifactId>asm</artifactId><version>9.7</version>",
                    ),
                ),
            ],
            &[("asm", "asm", asm_version, provides)],
        );
        seed_roots(
            &mut resolver,
            &[
                ("asm", "asm", asm_version),
                ("org.ow2.asm", "asm", ow2_version),
            ],
        );
        resolver.walk();
        resolver
    }

    #[test]
    fn two_providers_agreeing_on_a_capability_version_are_a_conflict() {
        // Gradle's `DependencyGraphBuilder.registerCapabilities` registers a
        // conflict as soon as two selected components provide the same
        // capability, and `LastCandidateCapabilityResolver` has nothing to choose
        // between when they declare one version, so every candidate is rejected
        // and the build fails. The relocated `asm` is the documented case.
        let mut resolver = capability_pair("3.3.1", "9.7");
        let error = resolver
            .check_capabilities()
            .expect_err("two providers of one capability version");
        assert!(format!("{error}").contains("org.ow2.asm:asm"), "{error}");
    }

    #[test]
    fn two_providers_disagreeing_on_a_capability_version_resolve_to_the_higher() {
        // `UpgradeCapabilityResolver` sorts the declared capability versions and
        // evicts every candidate but the highest, so a version disagreement is
        // resolved rather than reported.
        let mut resolver = capability_pair("3.3.1", "9.8");
        resolver
            .check_capabilities()
            .expect("a higher capability version wins");
    }

    #[test]
    fn one_provider_of_a_capability_is_never_a_conflict() {
        let mut resolver = offline_with_gmm(
            &[(
                "com.google.guava",
                "guava",
                "33.0.0-android",
                pom(
                    "<groupId>com.google.guava</groupId><artifactId>guava</artifactId><version>33.0.0-android</version>",
                ),
            )],
            &[(
                "com.google.guava",
                "guava",
                "33.0.0-android",
                r#"{"formatVersion":"1.1","variants":[{"name":"runtimeElements",
                    "attributes":{"org.gradle.category":"library","org.gradle.usage":"java-runtime"},
                    "capabilities":[{"group":"com.google.collections","name":"google-collections","version":"1.0"}]}]}"#,
            )],
        );
        seed_roots(
            &mut resolver,
            &[("com.google.guava", "guava", "33.0.0-android")],
        );
        resolver.walk();
        resolver
            .check_capabilities()
            .expect("guava's legacy shim capability on its own is fine");
    }

    /// `old:1.0` moved to `new:1.0`; `old:2.0` stayed where it is.
    fn relocation_fixture() -> [(&'static str, &'static str, &'static str, String); 3] {
        [
            (
                "g",
                "new",
                "1.0",
                pom("<groupId>g</groupId><artifactId>new</artifactId><version>1.0</version>"),
            ),
            (
                "g",
                "old",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>old</artifactId><version>1.0</version>
                       <relocation><groupId>g</groupId><artifactId>new</artifactId><version>1.0</version></relocation>"#,
                ),
            ),
            (
                "g",
                "old",
                "2.0",
                pom("<groupId>g</groupId><artifactId>old</artifactId><version>2.0</version>"),
            ),
        ]
    }

    #[test]
    fn a_relocated_module_ships_its_target_and_nothing_of_itself() {
        // Gradle marks a relocated module `setRelocated(true)`, and
        // `RealisedMavenModuleResolveMetadata.getArtifactsForConfiguration`
        // returns no artifacts for it, so the bytes come from the target alone.
        let mut resolver = offline(&relocation_fixture());
        seed_roots(&mut resolver, &[("g", "old", "1.0")]);
        resolver.walk();
        resolver.apply_platforms().expect("platforms apply");
        let artifacts: Vec<String> = resolver
            .results()
            .into_iter()
            .map(|r| r.coordinates.gav())
            .collect();
        assert_eq!(artifacts, vec!["g:new:1.0".to_owned()]);
    }

    #[test]
    fn a_module_relocated_at_one_version_and_not_another_ships_one_artifact() {
        // Re-keying the graph onto the relocation target put the relocated claim
        // under `g:new` and the un-relocated one under `g:old`, so mediation
        // never compared the two and both artifacts shipped. Gradle keys every
        // claim on the module a dependency names, so `2.0` wins outright and
        // `new` is never reached at all.
        let mut resolver = offline(&relocation_fixture());
        seed_roots(&mut resolver, &[("g", "old", "1.0"), ("g", "old", "2.0")]);
        resolver.walk();
        resolver.apply_platforms().expect("platforms apply");
        let artifacts: Vec<String> = resolver
            .results()
            .into_iter()
            .map(|r| r.coordinates.gav())
            .collect();
        assert_eq!(artifacts, vec!["g:old:2.0".to_owned()]);
    }

    #[test]
    fn a_relocation_to_another_version_of_itself_is_refused() {
        // The other coordinates name a module that publishes nothing under this
        // name. Gradle logs an error, takes the target's dependencies and ships
        // no artifact; the other half of that is an APK whose classes are
        // missing, which only a device finds, so the build stops here instead.
        let mut resolver = offline(&[(
            "g",
            "old",
            "1.0",
            pom(
                r#"<groupId>g</groupId><artifactId>old</artifactId><version>1.0</version>
                   <relocation><groupId>g</groupId><artifactId>old</artifactId><version>2.0</version></relocation>"#,
            ),
        )]);
        seed_roots(&mut resolver, &[("g", "old", "1.0")]);
        resolver.walk();
        let unusable = std::mem::take(&mut resolver.unusable);
        let error = unusable
            .into_iter()
            .next()
            .expect("the relocation is refused");
        assert!(format!("{error}").contains("Name 2.0 directly"), "{error}");
    }

    #[test]
    fn an_artifact_selector_survives_a_relocation() {
        // The selector is recorded under the name the dependant wrote, but the
        // graph is keyed by the relocation target, so the two have to be joined.
        let mut resolver = offline_with_gmm(
            &[
                (
                    "g",
                    "app",
                    "1.0",
                    pom("<groupId>g</groupId><artifactId>app</artifactId><version>1.0</version>"),
                ),
                (
                    "g",
                    "new",
                    "1.0",
                    pom("<groupId>g</groupId><artifactId>new</artifactId><version>1.0</version>"),
                ),
                (
                    "g",
                    "old",
                    "1.0",
                    pom(
                        r#"<groupId>g</groupId><artifactId>old</artifactId><version>1.0</version>
                           <relocation><groupId>g</groupId><artifactId>new</artifactId><version>1.0</version></relocation>"#,
                    ),
                ),
            ],
            &[(
                "g",
                "app",
                "1.0",
                r#"{"formatVersion":"1.1","variants":[{"name":"runtimeElements",
                    "attributes":{"org.gradle.category":"library","org.gradle.usage":"java-runtime"},
                    "dependencies":[{"group":"g","module":"old","version":{"requires":"1.0"},
                        "thirdPartyCompatibility":{"artifactSelector":{
                            "extension":"jar","classifier":"linux-x86_64"}}}]}]}"#,
            )],
        );
        seed_roots(&mut resolver, &[("g", "app", "1.0")]);
        resolver.walk();
        resolver.apply_platforms().expect("platforms apply");
        let relocated = resolver
            .results()
            .into_iter()
            .find(|r| r.coordinates.artifact == "new")
            .expect("the relocated module is what is fetched");
        assert_eq!(relocated.classifier.as_deref(), Some("linux-x86_64"));
    }

    #[test]
    fn an_artifact_selector_reaches_the_artifact_to_fetch() {
        let mut resolver = offline_with_gmm(
            &[
                (
                    "g",
                    "a",
                    "1.0",
                    pom("<groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>"),
                ),
                (
                    "g",
                    "toolchain",
                    "1.0",
                    pom(
                        "<groupId>g</groupId><artifactId>toolchain</artifactId><version>1.0</version>",
                    ),
                ),
            ],
            &[(
                "g",
                "a",
                "1.0",
                r#"{"formatVersion":"1.1","variants":[{"name":"runtimeElements",
                    "attributes":{"org.gradle.category":"library","org.gradle.usage":"java-runtime"},
                    "dependencies":[{"group":"g","module":"toolchain","version":{"requires":"1.0"},
                        "thirdPartyCompatibility":{"artifactSelector":{
                            "extension":"jar","classifier":"linux-x86_64"}}}]}]}"#,
            )],
        );
        seed_roots(&mut resolver, &[("g", "a", "1.0")]);
        resolver.walk();
        resolver.apply_platforms().expect("platforms apply");
        let toolchain = resolver
            .results()
            .into_iter()
            .find(|r| r.coordinates.artifact == "toolchain")
            .expect("the selected dependency is an artifact to fetch");
        assert_eq!(toolchain.classifier.as_deref(), Some("linux-x86_64"));
        assert_eq!(toolchain.extension.as_deref(), Some("jar"));
    }

    #[test]
    fn a_losing_pom_is_never_expanded_so_its_range_never_competes() {
        // `a` wants `b:1.0`, which requires the hard range `c:[1.0]`, but
        // `b:2.0` is also wanted and wins. Only the winner is expanded, so
        // `[1.0]` never becomes a live constraint. This is why the AndroidX
        // closure resolves quietly: a POM that lost is not read at all.
        let mut resolver = offline(&[
            (
                "g",
                "a",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>a</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>b</artifactId><version>1.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "b",
                "1.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>b</artifactId><version>1.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>c</artifactId><version>[1.0]</version></dependency>
                       </dependencies>"#,
                ),
            ),
            (
                "g",
                "b",
                "2.0",
                pom(
                    r#"<groupId>g</groupId><artifactId>b</artifactId><version>2.0</version>
                       <dependencies>
                         <dependency><groupId>g</groupId><artifactId>c</artifactId><version>2.0</version></dependency>
                       </dependencies>"#,
                ),
            ),
            ("g", "c", "1.0", leaf()),
            ("g", "c", "2.0", leaf()),
        ]);
        let resolved = resolve_with(&mut resolver, &[("g", "a", "1.0"), ("g", "b", "2.0")]);
        assert!(resolved.contains(&"g:c:2.0".to_owned()), "{resolved:?}");
        assert!(
            resolver.range_violations.is_empty(),
            "{:?}",
            resolver.range_violations
        );
    }
}
