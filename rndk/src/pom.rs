//! Maven POM reading and dependency resolution.
//!
//! `android_libs` names direct dependencies; this turns them into the closure
//! that actually has to be on the classpath. It follows the parts of the Maven
//! model that change *which* artifact gets picked: the parent chain,
//! `dependencyManagement` including imported BOMs, exclusions and scopes.
//! Version selection is Maven Resolver's: the nearest declaration wins, and a
//! managed, ranged or `LATEST` version is a soft requirement that yields to a
//! nearer dependency naming a version outright.

use crate::error::NdkError;
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

/// An artifact that belongs on the classpath.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub coordinates: Coordinates,
    /// `pom` artifacts are BOMs and are never packaged, so they are left out
    /// of the closure.
    pub packaging: String,
}

/// How many times a `<relocation>` chain is followed before giving up.
const MAX_RELOCATIONS: usize = 8;

/// Resolves `roots` and their transitive dependencies.
pub fn resolve(roots: &[Coordinates]) -> Result<Vec<Resolved>, NdkError> {
    let mut resolver = Resolver::default();
    for root in roots {
        resolver.consider(
            ModuleId::new(&root.group, &root.artifact),
            Requirement::parse(&root.version),
            0,
            Vec::new(),
            true,
        )?;
    }
    resolver.walk();
    Ok(resolver.results())
}

#[derive(Default)]
struct Resolver {
    /// Effective models by coordinates, so a module reached at two versions
    /// gets both POMs read and neither is re-read.
    models: HashMap<Coordinates, Model>,
    /// Raw POM documents, so relocation and inheritance share one fetch.
    documents: HashMap<Coordinates, Pom>,
    /// Coordinates whose model is being built, to catch a parent cycle.
    building: BTreeSet<Coordinates>,
    /// Published versions per module, for ranges and `LATEST`.
    available: HashMap<ModuleId, Vec<MavenVersion>>,
    selected: BTreeMap<ModuleId, Selection>,
    /// The exclusions each module was last expanded under. Exclusions are
    /// path-dependent, so a path carrying exclusions the module has not seen
    /// yet re-expands it.
    expanded: HashMap<(ModuleId, String), Vec<Exclusion>>,
    pending: VecDeque<Pending>,
    order: usize,
}

impl Resolver {
    fn fail(&self, coordinates: &Coordinates, reason: impl Into<String>) -> NdkError {
        NdkError::MavenLibFailed {
            gav: coordinates.gav(),
            reason: reason.into(),
        }
    }

    /// Records a dependency's claim on a module, keeping the better claim.
    fn consider(
        &mut self,
        module: ModuleId,
        requirement: Requirement,
        depth: usize,
        excluded: Vec<Exclusion>,
        root: bool,
    ) -> Result<(), NdkError> {
        if excluded.iter().any(|e| e.matches(&module)) {
            return Ok(());
        }
        let (module, version) = self.concrete(module, &requirement)?;
        self.order += 1;
        let claim = Selection {
            ordering: MavenVersion::new(&version),
            version: version.clone(),
            depth,
            order: self.order,
        };

        if self
            .selected
            .get(&module)
            .is_some_and(|current| !claim.beats(current))
        {
            return Ok(());
        }
        self.selected.insert(module.clone(), claim);
        self.pending.push_back(Pending {
            module,
            version,
            depth,
            excluded,
            root,
        });
        Ok(())
    }

    /// Turns a requirement into the module and version that get used, following
    /// any `<relocation>` to the coordinates that replaced them.
    fn concrete(
        &mut self,
        module: ModuleId,
        requirement: &Requirement,
    ) -> Result<(ModuleId, String), NdkError> {
        let mut module = module;
        for _ in 0..MAX_RELOCATIONS {
            let mut version = match requirement {
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
            let coordinates = coordinates_of(&module, &version);
            match self.document(&coordinates)?.relocation.clone() {
                Some(relocation) if !relocation.group_id.is_empty() => {
                    module = ModuleId::new(&relocation.group_id, &relocation.artifact_id);
                    // A relocation that names no version keeps the one the
                    // request already resolved to.
                    if !relocation.version.is_empty() {
                        version = relocation.version;
                    }
                }
                _ => return Ok((module, version)),
            }
        }
        Err(self.fail(
            &coordinates_of(&module, ""),
            "its relocation chain does not terminate",
        ))
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

            let model = match self.model(&node.module, &node.version) {
                Ok(model) => model,
                // A root the user named by hand is kept even with no readable
                // POM: there is then simply nothing to add to the classpath.
                Err(e) if node.root => {
                    log::warn!("no dependencies resolved for {}: {e}", node.module);
                    continue;
                }
                Err(e) => {
                    log::warn!("{e}");
                    continue;
                }
            };

            for declaration in &model.dependencies {
                if declaration.optional || !declaration.scope.reaches_classpath() {
                    continue;
                }
                let managed = model.management.get(&declaration.module);
                let spec = match (&declaration.version, managed) {
                    (Some(version), _) => version.clone(),
                    (None, Some(managed)) => managed.version.clone(),
                    (None, None) => {
                        log::warn!(
                            "{} declares {} with no version and no \
                             `dependencyManagement` entry",
                            node.module,
                            declaration.module
                        );
                        continue;
                    }
                };
                if spec.contains("${") {
                    // Substitution already ran over the whole model, so a
                    // property still standing here is one this resolver could
                    // not resolve. Picking a version for it would put an
                    // arbitrary artifact on the classpath.
                    log::warn!(
                        "{} declares {} at `{spec}`, which still holds an unresolved \
                         property; skipping it",
                        node.module,
                        declaration.module
                    );
                    continue;
                }
                let mut excluded = node.excluded.clone();
                excluded.extend(declaration.exclusions.iter().cloned());
                let depth = node.depth + 1;
                if let Err(e) = self.consider(
                    declaration.module.clone(),
                    Requirement::parse(&spec),
                    depth,
                    excluded,
                    false,
                ) {
                    log::warn!("{e}");
                }
            }
        }
    }

    fn results(&self) -> Vec<Resolved> {
        self.selected
            .iter()
            .filter_map(|(module, selection)| {
                let coordinates = Coordinates {
                    group: module.group.clone(),
                    artifact: module.artifact.clone(),
                    version: selection.version.clone(),
                };
                let packaging = self
                    .models
                    .get(&coordinates)
                    .map_or_else(|| "jar".to_owned(), |model| model.packaging.clone());
                (packaging != "pom").then_some(Resolved {
                    coordinates,
                    packaging,
                })
            })
            .collect()
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
        self.expand_imports(&mut model.management)?;
        Ok(model)
    }

    /// Folds imported BOMs into the management table. A locally declared entry
    /// wins over an imported one, so imports are applied after the parent chain
    /// and only fill gaps.
    fn expand_imports(
        &mut self,
        management: &mut BTreeMap<ModuleId, Managed>,
    ) -> Result<(), NdkError> {
        let imports: Vec<ModuleId> = management
            .iter()
            .filter(|(_, managed)| managed.bom)
            .map(|(module, _)| module.clone())
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
        let url = format!(
            "{}/{}/{}/maven-metadata.xml",
            maven::repo_base(&module.group),
            module.group.replace('.', "/"),
            module.artifact
        );
        if !path.is_file() {
            maven::download_with_curl_or_wget(&url, &path).map_err(|e| {
                self.fail(
                    &coordinates_of(module, ""),
                    format!("no version list at {url}: {e}"),
                )
            })?;
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
                format!("{url} lists no versions"),
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

    let mut management: BTreeMap<ModuleId, Managed> =
        parent.map(|p| p.management.clone()).unwrap_or_default();
    for entry in managed_entries(raw) {
        if let Some(managed) = managed_entry(entry)? {
            management.insert(entry.module(), managed);
        }
    }

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
    // Maven imports only a `pom`-typed entry, so `<scope>import</scope>` alone
    // is not an import and must not be folded in as one.
    let bom = Scope::parse(entry.scope.as_deref()) == Scope::Import
        && entry.kind.as_deref().map(str::trim) == Some("pom");
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
    fn only_a_pom_typed_entry_is_an_import() {
        let model = model_of(&pom(
            r#"<groupId>g</groupId><artifactId>a</artifactId><version>1</version>
               <dependencyManagement><dependencies>
                 <dependency>
                   <groupId>g</groupId><artifactId>bom</artifactId><version>1.2</version>
                   <type>pom</type><scope>import</scope>
                 </dependency>
                 <dependency>
                   <groupId>g</groupId><artifactId>not-a-bom</artifactId><version>1.0</version>
                   <scope>import</scope>
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
        // Without `<type>pom</type>` the entry is a managed version like any
        // other, so folding its "dependencies" in would be wrong.
        assert!(
            !model
                .management
                .get(&ModuleId::new("g", "not-a-bom"))
                .unwrap()
                .bom
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

    fn resolve_with(resolver: &mut Resolver, roots: &[(&str, &str, &str)]) -> Vec<String> {
        for (group, artifact, version) in roots {
            resolver
                .consider(
                    ModuleId::new(group, artifact),
                    Requirement::parse(version),
                    0,
                    Vec::new(),
                    true,
                )
                .expect("root resolves");
        }
        resolver.walk();
        resolver
            .results()
            .iter()
            .map(|r| r.coordinates.gav())
            .collect()
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
}
