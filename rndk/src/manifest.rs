use crate::error::NdkError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::Path;

/// Android [manifest element](https://developer.android.com/guide/topics/manifest/manifest-element), containing an [`Application`] element.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename = "manifest")]
pub struct AndroidManifest {
    #[serde(rename(serialize = "@xmlns:android"))]
    #[serde(default = "default_namespace")]
    ns_android: String,
    #[serde(rename(serialize = "@package"), default)]
    pub package: String,
    #[serde(
        rename(serialize = "@android:sharedUserId"),
        skip_serializing_if = "Option::is_none"
    )]
    pub shared_user_id: Option<String>,
    #[serde(
        rename(serialize = "@android:versionCode"),
        skip_serializing_if = "Option::is_none"
    )]
    pub version_code: Option<u32>,
    #[serde(
        rename(serialize = "@android:versionName"),
        skip_serializing_if = "Option::is_none"
    )]
    pub version_name: Option<String>,

    #[serde(rename(serialize = "uses-sdk"))]
    #[serde(default)]
    pub sdk: Sdk,

    #[serde(rename(serialize = "uses-feature"))]
    #[serde(default)]
    pub uses_feature: Vec<Feature>,
    #[serde(rename(serialize = "uses-permission"))]
    #[serde(default)]
    pub uses_permission: Vec<Permission>,

    /// Permissions this manifest *defines*, as opposed to `uses_permission`
    /// which requests one. A library that defines a custom permission (to make
    /// its own non-exported components enforceable) needs the definition merged
    /// too, or the app requests a permission nothing declares.
    #[serde(rename(serialize = "permission"))]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission: Vec<Permission>,

    #[serde(rename(serialize = "grant-uri-permission"))]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_uri_permission: Vec<GrantUriPermission>,

    #[serde(default)]
    pub queries: Option<Queries>,

    #[serde(default)]
    pub application: Application,
}

impl Default for AndroidManifest {
    fn default() -> Self {
        Self {
            ns_android: default_namespace(),
            package: Default::default(),
            shared_user_id: Default::default(),
            version_code: Default::default(),
            version_name: Default::default(),
            sdk: Default::default(),
            uses_feature: Default::default(),
            uses_permission: Default::default(),
            permission: Default::default(),
            grant_uri_permission: Default::default(),
            queries: Default::default(),
            application: Default::default(),
        }
    }
}

impl AndroidManifest {
    pub fn write_to(&self, dir: &Path) -> Result<(), NdkError> {
        let xml = quick_xml::se::to_string(&self).map_err(NdkError::Serialize)?;
        std::fs::write(dir.join("AndroidManifest.xml"), xml.as_bytes())?;
        Ok(())
    }

    /// Folds a library's manifest into this one.
    ///
    /// Only the elements whose absence is fatal are merged: components the
    /// library declares as exported, its permissions, and its `meta-data`. The
    /// library's own `package`, `versionCode`, icon, label and theme are
    /// deliberately dropped — those belong to the app, and a library declaring
    /// them means it was not built for independent merging.
    pub fn merge_library(&mut self, library: &AndroidManifest, application_id: &str) {
        for permission in &library.uses_permission {
            let permission = Permission {
                name: substitute_application_id(&permission.name, application_id),
                max_sdk_version: permission.max_sdk_version,
            };
            if !self
                .uses_permission
                .iter()
                .any(|p| p.name == permission.name)
            {
                self.uses_permission.push(permission);
            }
        }

        for permission in &library.permission {
            let permission = Permission {
                name: substitute_application_id(&permission.name, application_id),
                max_sdk_version: permission.max_sdk_version,
            };
            if !self.permission.iter().any(|p| p.name == permission.name) {
                self.permission.push(permission);
            }
        }

        for meta_data in &library.application.meta_data {
            if self
                .application
                .meta_data
                .iter()
                .any(|m| m.name == meta_data.name)
            {
                continue;
            }
            let mut meta_data = meta_data.clone();
            meta_data.name = substitute_application_id(&meta_data.name, application_id);
            if let Some(value) = &meta_data.value {
                meta_data.value = Some(substitute_application_id(value, application_id));
            }
            self.application.meta_data.push(meta_data);
        }

        for provider in &library.application.provider {
            if removed(provider.tools_node.as_deref()) {
                continue;
            }
            if let Some(existing) = self
                .application
                .provider
                .iter_mut()
                .find(|p| p.name == provider.name)
            {
                if replaces(provider.tools_node.as_deref()) {
                    *existing = provider.clone();
                }
                continue;
            }
            let mut provider = provider.clone();
            // A provider declared with `${applicationId}` in its authorities
            // only resolves once the app id is known.
            if let Some(authorities) = &provider.authorities {
                provider.authorities = Some(substitute_application_id(authorities, application_id));
            }
            for meta_data in &mut provider.meta_data {
                meta_data.name = substitute_application_id(&meta_data.name, application_id);
                if let Some(value) = &meta_data.value {
                    meta_data.value = Some(substitute_application_id(value, application_id));
                }
            }
            self.application.provider.push(provider);
        }

        for receiver in &library.application.receiver {
            if removed(receiver.tools_node.as_deref()) {
                continue;
            }
            if let Some(existing) = self
                .application
                .receiver
                .iter_mut()
                .find(|r| r.name == receiver.name)
            {
                if replaces(receiver.tools_node.as_deref()) {
                    *existing = receiver.clone();
                }
                continue;
            }
            let mut receiver = receiver.clone();
            receiver.name = substitute_application_id(&receiver.name, application_id);
            if let Some(permission) = &receiver.permission {
                receiver.permission = Some(substitute_application_id(permission, application_id));
            }
            for meta_data in &mut receiver.meta_data {
                meta_data.name = substitute_application_id(&meta_data.name, application_id);
            }
            self.application.receiver.push(receiver);
        }

        for activity in &library.application.activity {
            if removed(activity.tools_node.as_deref()) {
                continue;
            }
            if let Some(existing) = self
                .application
                .activity
                .iter_mut()
                .find(|a| a.name == activity.name)
            {
                if replaces(activity.tools_node.as_deref()) {
                    *existing = activity.clone();
                }
                continue;
            }
            let mut activity = activity.clone();
            activity.name = substitute_application_id(&activity.name, application_id);
            for meta_data in &mut activity.meta_data {
                meta_data.name = substitute_application_id(&meta_data.name, application_id);
                if let Some(value) = &meta_data.value {
                    meta_data.value = Some(substitute_application_id(value, application_id));
                }
            }
            self.application.activity.push(activity);
        }

        for service in &library.application.service {
            if removed(service.tools_node.as_deref()) {
                continue;
            }
            if let Some(existing) = self
                .application
                .service
                .iter_mut()
                .find(|s| s.name == service.name)
            {
                if replaces(service.tools_node.as_deref()) {
                    *existing = service.clone();
                }
                continue;
            }
            let mut service = service.clone();
            service.name = substitute_application_id(&service.name, application_id);
            if let Some(permission) = &service.permission {
                service.permission = Some(substitute_application_id(permission, application_id));
            }
            for meta_data in &mut service.meta_data {
                meta_data.name = substitute_application_id(&meta_data.name, application_id);
            }
            self.application.service.push(service);
        }

        for feature in &library.uses_feature {
            let same = |f: &Feature| f.name == feature.name;
            if self.uses_feature.iter().any(same) {
                continue;
            }
            self.uses_feature.push(feature.clone());
        }

        for grant in &library.grant_uri_permission {
            if self.grant_uri_permission.iter().any(|g| g.uri == grant.uri) {
                continue;
            }
            self.grant_uri_permission.push(grant.clone());
        }

        if let Some(queries) = &library.queries {
            let target = self.queries.get_or_insert_with(Queries::default);
            for package in &queries.package {
                if !target.package.iter().any(|p| p.name == package.name) {
                    target.package.push(package.clone());
                }
            }
            for provider in &queries.provider {
                if !target
                    .provider
                    .iter()
                    .any(|p| p.authorities == provider.authorities && p.name == provider.name)
                {
                    target.provider.push(provider.clone());
                }
            }
            for intent in &queries.intent {
                target.intent.push(intent.clone());
            }
        }
    }
}

/// Whether a library component asked to be dropped from the merged manifest.
fn removed(tools_node: Option<&str>) -> bool {
    matches!(tools_node, Some("remove") | Some("removeAll"))
}

/// Whether a library component asked to win over the app's own declaration.
fn replaces(tools_node: Option<&str>) -> bool {
    matches!(tools_node, Some("replace") | Some("replaceStrict"))
}

/// Resolves the placeholders a library manifest may use for the app's own id.
/// `applicationIdSuffix` is dropped rather than substituted, since cargo-rapk
/// does not append a suffix to the package name.
fn substitute_application_id(value: &str, application_id: &str) -> String {
    value
        .replace("${applicationId}", application_id)
        .replace("${applicationIdSuffix}", "")
}

/// Reads a library manifest into the subset that [`AndroidManifest::merge_library`]
/// folds into the app's.
///
/// This cannot use the serde deserializer. Attribute renames in this module are
/// declared with `rename(serialize = ...)`, which affects only output, and the
/// deserializer matches attributes by their exact name, so a field renamed
/// `@android:name` never sees the `android:name` in the document — it silently
/// reads as absent. The event reader's `local_name()` does drop the prefix, so
/// attributes are read here by hand. Serde still does all the writing.
pub fn parse_library_manifest(xml: &str) -> Result<AndroidManifest, String> {
    use quick_xml::events::Event;

    fn attributes(start: &quick_xml::events::BytesStart) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        for attr in start.attributes().with_checks(false) {
            let attr = attr.map_err(|e| e.to_string())?;
            let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
            out.push((
                key,
                attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|e| e.to_string())?
                    .into_owned(),
            ));
        }
        Ok(out)
    }

    fn get<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
        attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn flag(attrs: &[(String, String)], name: &str) -> Option<bool> {
        get(attrs, name).and_then(|v| v.parse().ok())
    }

    fn number(attrs: &[(String, String)], name: &str) -> Option<u32> {
        get(attrs, name).and_then(|v| v.parse().ok())
    }

    fn named(attrs: &[(String, String)]) -> String {
        get(attrs, "name").unwrap_or_default().to_owned()
    }

    fn meta_data(attrs: &[(String, String)]) -> MetaData {
        MetaData {
            name: named(attrs),
            value: get(attrs, "value").map(str::to_owned),
            resource: get(attrs, "resource").map(str::to_owned),
        }
    }

    fn intent_filter_attrs(attrs: &[(String, String)]) -> Vec<String> {
        get(attrs, "name")
            .map(|n| vec![n.to_owned()])
            .unwrap_or_default()
    }

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut manifest = AndroidManifest::default();
    let mut path: Vec<String> = Vec::new();
    let mut activity: Option<Activity> = None;
    let mut service: Option<Service> = None;
    let mut provider: Option<Provider> = None;
    let mut receiver: Option<Receiver> = None;
    let mut filter: Option<IntentFilter> = None;
    let mut queries: Option<Queries> = None;

    loop {
        let (name, attrs, is_start) = match reader.read_event() {
            Ok(Event::Start(e)) => (
                String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
                attributes(&e)?,
                true,
            ),
            Ok(Event::Empty(e)) => (
                String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
                attributes(&e)?,
                false,
            ),
            Ok(Event::End(_)) => {
                let closed = path.pop();
                let owner = path.last().map(String::as_str).unwrap_or_default();
                match closed.as_deref() {
                    Some("provider") => {
                        if let Some(done) = provider.take() {
                            manifest.application.provider.push(done);
                        }
                    }
                    Some("receiver") => {
                        if let Some(done) = receiver.take() {
                            manifest.application.receiver.push(done);
                        }
                    }
                    Some("activity") => {
                        if let Some(done) = activity.take() {
                            manifest.application.activity.push(done);
                        }
                    }
                    Some("service") => {
                        if let Some(done) = service.take() {
                            manifest.application.service.push(done);
                        }
                    }
                    Some("intent-filter") | Some("intent") => {
                        if let Some(done) = filter.take() {
                            if owner == "queries"
                                && let Some(queries) = queries.as_mut()
                            {
                                queries.intent.push(done);
                            } else if let Some(receiver) = receiver.as_mut() {
                                receiver.intent_filter.push(done);
                            }
                        }
                    }
                    Some("queries") => {
                        if let Some(done) = queries.take() {
                            manifest.queries = Some(done);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            Ok(Event::Eof) => break,
            Ok(_) => continue,
            Err(e) => return Err(e.to_string()),
        };

        let parent = path.last().map(String::as_str).unwrap_or_default();

        match (parent, name.as_str()) {
            ("", "manifest") => manifest.package = named(&attrs),
            ("manifest", "uses-permission") => manifest.uses_permission.push(Permission {
                name: named(&attrs),
                max_sdk_version: number(&attrs, "maxSdkVersion"),
            }),
            ("manifest", "permission") => manifest.permission.push(Permission {
                name: named(&attrs),
                max_sdk_version: number(&attrs, "maxSdkVersion"),
            }),
            ("manifest", "queries") if is_start => queries = Some(Queries::default()),
            ("queries", "package") => {
                if let Some(q) = queries.as_mut() {
                    q.package.push(Package {
                        name: named(&attrs),
                    });
                }
            }
            ("queries", "provider") => {
                if let Some(q) = queries.as_mut() {
                    q.provider.push(QueryProvider {
                        authorities: get(&attrs, "authorities").unwrap_or_default().to_owned(),
                        name: get(&attrs, "name").map(str::to_owned),
                    });
                }
            }
            ("queries", "intent") if is_start => filter = Some(IntentFilter::default()),
            ("manifest", "uses-feature") => manifest.uses_feature.push(Feature {
                name: get(&attrs, "name").map(str::to_owned),
                required: flag(&attrs, "required"),
                version: number(&attrs, "version"),
                opengles_version: None,
            }),
            ("manifest", "grant-uri-permission") => manifest
                .grant_uri_permission
                .push(GrantUriPermission { uri: named(&attrs) }),
            ("application", "activity") => {
                let built = Activity {
                    name: named(&attrs),
                    config_changes: get(&attrs, "configChanges").map(str::to_owned),
                    label: get(&attrs, "label").map(str::to_owned),
                    launch_mode: get(&attrs, "launchMode").map(str::to_owned),
                    orientation: get(&attrs, "screenOrientation").map(str::to_owned),
                    exported: flag(&attrs, "exported"),
                    resizeable_activity: flag(&attrs, "resizeableActivity"),
                    always_retain_task_state: flag(&attrs, "alwaysRetainTaskState"),
                    tools_node: get(&attrs, "node").map(str::to_owned),
                    meta_data: Vec::new(),
                    intent_filter: Vec::new(),
                };
                if is_start {
                    activity = Some(built);
                } else {
                    manifest.application.activity.push(built);
                }
            }
            ("activity", "meta-data") => {
                if let Some(a) = activity.as_mut() {
                    a.meta_data.push(meta_data(&attrs));
                }
            }
            ("activity", "intent-filter") if is_start => filter = Some(IntentFilter::default()),
            ("application", "service") => {
                let built = Service {
                    name: named(&attrs),
                    exported: flag(&attrs, "exported"),
                    foreground_service_type: get(&attrs, "foregroundServiceType")
                        .map(str::to_owned),
                    label: get(&attrs, "label").map(str::to_owned),
                    icon: get(&attrs, "icon").map(str::to_owned),
                    permission: get(&attrs, "permission").map(str::to_owned),
                    process: get(&attrs, "process").map(str::to_owned),
                    description: get(&attrs, "description").map(str::to_owned),
                    direct_boot_aware: flag(&attrs, "directBootAware"),
                    tools_node: get(&attrs, "node").map(str::to_owned),
                    meta_data: Vec::new(),
                    intent_filter: Vec::new(),
                };
                if is_start {
                    service = Some(built);
                } else {
                    manifest.application.service.push(built);
                }
            }
            ("service", "meta-data") => {
                if let Some(s) = service.as_mut() {
                    s.meta_data.push(meta_data(&attrs));
                }
            }
            ("service", "intent-filter") if is_start => filter = Some(IntentFilter::default()),
            ("application", "meta-data") => manifest.application.meta_data.push(meta_data(&attrs)),
            ("application", "provider") => {
                let built = Provider {
                    name: named(&attrs),
                    authorities: get(&attrs, "authorities").map(str::to_owned),
                    exported: flag(&attrs, "exported"),
                    enabled: flag(&attrs, "enabled"),
                    init_order: get(&attrs, "initOrder").and_then(|v| v.parse().ok()),
                    multiprocess: flag(&attrs, "multiprocess"),
                    process: get(&attrs, "process").map(str::to_owned),
                    grant_uri_permissions: flag(&attrs, "grantUriPermissions"),
                    tools_node: get(&attrs, "node").map(str::to_owned),
                    meta_data: Vec::new(),
                };
                if is_start {
                    provider = Some(built);
                } else {
                    manifest.application.provider.push(built);
                }
            }
            ("provider", "meta-data") => {
                if let Some(p) = provider.as_mut() {
                    p.meta_data.push(meta_data(&attrs));
                }
            }
            ("application", "receiver") => {
                let built = Receiver {
                    name: named(&attrs),
                    exported: flag(&attrs, "exported"),
                    enabled: flag(&attrs, "enabled"),
                    permission: get(&attrs, "permission").map(str::to_owned),
                    label: get(&attrs, "label").map(str::to_owned),
                    icon: get(&attrs, "icon").map(str::to_owned),
                    direct_boot_aware: flag(&attrs, "directBootAware"),
                    tools_node: get(&attrs, "node").map(str::to_owned),
                    meta_data: Vec::new(),
                    intent_filter: Vec::new(),
                };
                if is_start {
                    receiver = Some(built);
                } else {
                    manifest.application.receiver.push(built);
                }
            }
            ("receiver", "meta-data") => {
                if let Some(r) = receiver.as_mut() {
                    r.meta_data.push(meta_data(&attrs));
                }
            }
            ("receiver", "intent-filter") | ("queries", "intent") if is_start => {
                filter = Some(IntentFilter::default())
            }
            ("intent-filter", "action") => {
                if let Some(f) = filter.as_mut() {
                    f.actions.extend(intent_filter_attrs(&attrs));
                }
            }
            ("intent-filter", "category") => {
                if let Some(f) = filter.as_mut() {
                    f.categories.extend(intent_filter_attrs(&attrs));
                }
            }
            _ => {}
        }

        if is_start {
            path.push(name);
        }
    }

    if let Some(a) = activity.take() {
        manifest.application.activity.push(a);
    }
    if let Some(sv) = service.take() {
        manifest.application.service.push(sv);
    }
    if let Some(p) = provider.take() {
        manifest.application.provider.push(p);
    }
    if let Some(r) = receiver.take() {
        manifest.application.receiver.push(r);
    }
    if let Some(q) = queries.take() {
        manifest.queries = Some(q);
    }
    Ok(manifest)
}

/// Android [application element](https://developer.android.com/guide/topics/manifest/application-element), containing one or more [`Activity`] and [`Service`] elements.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Application {
    #[serde(
        rename(serialize = "@android:debuggable"),
        skip_serializing_if = "Option::is_none"
    )]
    pub debuggable: Option<bool>,
    #[serde(
        rename(serialize = "@android:theme"),
        skip_serializing_if = "Option::is_none"
    )]
    pub theme: Option<String>,
    #[serde(rename(serialize = "@android:hasCode"))]
    #[serde(default)]
    pub has_code: bool,
    #[serde(
        rename(serialize = "@android:icon"),
        skip_serializing_if = "Option::is_none"
    )]
    pub icon: Option<String>,
    #[serde(rename(serialize = "@android:label"))]
    #[serde(default)]
    pub label: String,
    #[serde(
        rename(serialize = "@android:extractNativeLibs"),
        skip_serializing_if = "Option::is_none"
    )]
    pub extract_native_libs: Option<bool>,
    #[serde(
        rename(serialize = "@android:usesCleartextTraffic"),
        skip_serializing_if = "Option::is_none"
    )]
    pub uses_cleartext_traffic: Option<bool>,
    #[serde(
        rename(serialize = "@android:requestLegacyExternalStorage"),
        skip_serializing_if = "Option::is_none"
    )]
    pub request_legacy_external_storage: Option<bool>,
    #[serde(
        rename(serialize = "@android:allowNativeHeapPointerTagging"),
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_native_heap_pointer_tagging: Option<bool>,
    #[serde(
        rename(serialize = "@android:installLocation"),
        skip_serializing_if = "Option::is_none"
    )]
    pub install_location: Option<String>,

    #[serde(rename(serialize = "meta-data"))]
    #[serde(default)]
    pub meta_data: Vec<MetaData>,
    #[serde(default = "default_activities")]
    #[serde(deserialize_with = "deserialize_activities")]
    pub activity: Vec<Activity>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_services")]
    pub service: Vec<Service>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_receivers")]
    pub receiver: Vec<Receiver>,
    #[serde(default)]
    pub provider: Vec<Provider>,
    #[serde(rename(serialize = "profileable"))]
    #[serde(default)]
    pub profileable: Option<Profileable>,
    #[serde(rename(serialize = "uses-native-library"))]
    #[serde(default)]
    pub uses_native_library: Vec<NativeLibrary>,
}

impl Default for Application {
    fn default() -> Self {
        Self {
            debuggable: None,
            theme: None,
            has_code: false,
            icon: None,
            label: String::new(),
            extract_native_libs: None,
            uses_cleartext_traffic: None,
            request_legacy_external_storage: None,
            allow_native_heap_pointer_tagging: None,
            install_location: None,
            meta_data: Vec::new(),
            activity: default_activities(),
            service: Vec::new(),
            receiver: Vec::new(),
            provider: Vec::new(),
            profileable: None,
            uses_native_library: Vec::new(),
        }
    }
}

fn default_activities() -> Vec<Activity> {
    vec![Activity::default()]
}

fn deserialize_activities<'de, D>(deserializer: D) -> Result<Vec<Activity>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        One(T),
        Many(Vec<T>),
    }

    match OneOrMany::<Activity>::deserialize(deserializer)? {
        OneOrMany::One(activity) => Ok(vec![activity]),
        OneOrMany::Many(activities) => Ok(activities),
    }
}

/// Android [activity element](https://developer.android.com/guide/topics/manifest/activity-element).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Activity {
    #[serde(
        rename(serialize = "@android:configChanges"),
        skip_serializing_if = "Option::is_none"
    )]
    #[serde(default = "default_config_changes")]
    pub config_changes: Option<String>,
    #[serde(
        rename(serialize = "@android:label"),
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<String>,
    #[serde(
        rename(serialize = "@android:launchMode"),
        skip_serializing_if = "Option::is_none"
    )]
    pub launch_mode: Option<String>,
    #[serde(rename(serialize = "@android:name"))]
    #[serde(default = "default_activity_name")]
    pub name: String,
    /// `tools:node` from a library manifest. It steers merging and is never
    /// written out: a merger directive has no meaning to the platform, and the
    /// root element declares no `tools` namespace, so emitting it would make
    /// the manifest unparseable.
    #[serde(skip_serializing)]
    pub tools_node: Option<String>,
    #[serde(
        rename(serialize = "@android:screenOrientation"),
        skip_serializing_if = "Option::is_none"
    )]
    pub orientation: Option<String>,
    #[serde(
        rename(serialize = "@android:exported"),
        skip_serializing_if = "Option::is_none"
    )]
    pub exported: Option<bool>,
    #[serde(
        rename(serialize = "@android:resizeableActivity"),
        skip_serializing_if = "Option::is_none"
    )]
    pub resizeable_activity: Option<bool>,
    #[serde(
        rename(serialize = "@android:alwaysRetainTaskState"),
        skip_serializing_if = "Option::is_none"
    )]
    pub always_retain_task_state: Option<bool>,

    #[serde(rename(serialize = "meta-data"))]
    #[serde(default)]
    pub meta_data: Vec<MetaData>,
    /// If no `MAIN` action exists in any intent filter, a default `MAIN` filter is serialized by `cargo-rapk`.
    #[serde(rename(serialize = "intent-filter"))]
    #[serde(default)]
    pub intent_filter: Vec<IntentFilter>,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            config_changes: default_config_changes(),
            label: None,
            launch_mode: None,
            name: default_activity_name(),
            orientation: None,
            exported: None,
            resizeable_activity: None,
            always_retain_task_state: None,
            tools_node: None,
            meta_data: Default::default(),
            intent_filter: Default::default(),
        }
    }
}

/// Android [service element](https://developer.android.com/guide/topics/manifest/service-element).
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct Service {
    #[serde(rename(serialize = "@android:name"))]
    pub name: String,
    /// `tools:node` from a library manifest. It steers merging and is never
    /// written out: a merger directive has no meaning to the platform, and the
    /// root element declares no `tools` namespace, so emitting it would make
    /// the manifest unparseable.
    #[serde(skip_serializing)]
    pub tools_node: Option<String>,
    #[serde(
        rename(serialize = "@android:exported"),
        skip_serializing_if = "Option::is_none"
    )]
    pub exported: Option<bool>,
    #[serde(
        rename(serialize = "@android:foregroundServiceType"),
        skip_serializing_if = "Option::is_none"
    )]
    pub foreground_service_type: Option<String>,
    #[serde(
        rename(serialize = "@android:label"),
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<String>,
    #[serde(
        rename(serialize = "@android:icon"),
        skip_serializing_if = "Option::is_none"
    )]
    pub icon: Option<String>,
    #[serde(
        rename(serialize = "@android:permission"),
        skip_serializing_if = "Option::is_none"
    )]
    pub permission: Option<String>,
    #[serde(
        rename(serialize = "@android:process"),
        skip_serializing_if = "Option::is_none"
    )]
    pub process: Option<String>,
    #[serde(
        rename(serialize = "@android:description"),
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<String>,
    #[serde(
        rename(serialize = "@android:directBootAware"),
        skip_serializing_if = "Option::is_none"
    )]
    pub direct_boot_aware: Option<bool>,

    #[serde(rename(serialize = "meta-data"))]
    #[serde(default)]
    pub meta_data: Vec<MetaData>,
    #[serde(rename(serialize = "intent-filter"))]
    #[serde(default)]
    pub intent_filter: Vec<IntentFilter>,
}

fn deserialize_services<'de, D>(deserializer: D) -> Result<Vec<Service>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        One(T),
        Many(Vec<T>),
    }

    match OneOrMany::<Service>::deserialize(deserializer)? {
        OneOrMany::One(service) => Ok(vec![service]),
        OneOrMany::Many(services) => Ok(services),
    }
}

/// Android [receiver element](https://developer.android.com/guide/topics/manifest/receiver-element).
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct Receiver {
    #[serde(rename(serialize = "@android:name"))]
    pub name: String,
    /// `tools:node` from a library manifest. It steers merging and is never
    /// written out: a merger directive has no meaning to the platform, and the
    /// root element declares no `tools` namespace, so emitting it would make
    /// the manifest unparseable.
    #[serde(skip_serializing)]
    pub tools_node: Option<String>,
    #[serde(
        rename(serialize = "@android:exported"),
        skip_serializing_if = "Option::is_none"
    )]
    pub exported: Option<bool>,
    #[serde(
        rename(serialize = "@android:enabled"),
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled: Option<bool>,
    #[serde(
        rename(serialize = "@android:permission"),
        skip_serializing_if = "Option::is_none"
    )]
    pub permission: Option<String>,
    #[serde(
        rename(serialize = "@android:label"),
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<String>,
    #[serde(
        rename(serialize = "@android:icon"),
        skip_serializing_if = "Option::is_none"
    )]
    pub icon: Option<String>,
    #[serde(
        rename(serialize = "@android:directBootAware"),
        skip_serializing_if = "Option::is_none"
    )]
    pub direct_boot_aware: Option<bool>,

    #[serde(rename(serialize = "meta-data"))]
    #[serde(default)]
    pub meta_data: Vec<MetaData>,
    #[serde(rename(serialize = "intent-filter"))]
    #[serde(default)]
    pub intent_filter: Vec<IntentFilter>,
}

fn deserialize_receivers<'de, D>(deserializer: D) -> Result<Vec<Receiver>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        One(T),
        Many(Vec<T>),
    }

    match OneOrMany::<Receiver>::deserialize(deserializer)? {
        OneOrMany::One(receiver) => Ok(vec![receiver]),
        OneOrMany::Many(receivers) => Ok(receivers),
    }
}

/// Android [profileable element](https://developer.android.com/guide/topics/manifest/profileable-element).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Profileable {
    #[serde(
        rename(serialize = "@android:shell"),
        skip_serializing_if = "Option::is_none"
    )]
    pub shell: Option<bool>,
    #[serde(
        rename(serialize = "@android:enabled"),
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled: Option<bool>,
}

/// Android [uses-native-library element](https://developer.android.com/guide/topics/manifest/uses-native-library-element).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NativeLibrary {
    #[serde(rename(serialize = "@android:name"))]
    pub name: String,
    #[serde(
        rename(serialize = "@android:required"),
        skip_serializing_if = "Option::is_none"
    )]
    pub required: Option<bool>,
}

/// Android [intent filter element](https://developer.android.com/guide/topics/manifest/intent-filter-element).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct IntentFilter {
    /// Serialize strings wrapped in `<action android:name="..." />`
    #[serde(serialize_with = "serialize_actions")]
    #[serde(rename(serialize = "action"))]
    #[serde(default)]
    pub actions: Vec<String>,
    /// Serialize as vector of structs for proper xml formatting
    #[serde(serialize_with = "serialize_catergories")]
    #[serde(rename(serialize = "category"))]
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub data: Vec<IntentFilterData>,
}

fn serialize_actions<S>(actions: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeSeq;

    #[derive(Serialize)]
    struct Action {
        #[serde(rename(serialize = "@android:name"))]
        name: String,
    }
    let mut seq = serializer.serialize_seq(Some(actions.len()))?;
    for action in actions {
        seq.serialize_element(&Action {
            name: action.clone(),
        })?;
    }
    seq.end()
}

fn serialize_catergories<S>(categories: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeSeq;

    #[derive(Serialize)]
    struct Category {
        #[serde(rename(serialize = "@android:name"))]
        pub name: String,
    }

    let mut seq = serializer.serialize_seq(Some(categories.len()))?;
    for category in categories {
        seq.serialize_element(&Category {
            name: category.clone(),
        })?;
    }
    seq.end()
}

/// Android [intent filter data element](https://developer.android.com/guide/topics/manifest/data-element).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct IntentFilterData {
    #[serde(
        rename(serialize = "@android:scheme"),
        skip_serializing_if = "Option::is_none"
    )]
    pub scheme: Option<String>,
    #[serde(
        rename(serialize = "@android:host"),
        skip_serializing_if = "Option::is_none"
    )]
    pub host: Option<String>,
    #[serde(
        rename(serialize = "@android:port"),
        skip_serializing_if = "Option::is_none"
    )]
    pub port: Option<String>,
    #[serde(
        rename(serialize = "@android:path"),
        skip_serializing_if = "Option::is_none"
    )]
    pub path: Option<String>,
    #[serde(
        rename(serialize = "@android:pathPattern"),
        skip_serializing_if = "Option::is_none"
    )]
    pub path_pattern: Option<String>,
    #[serde(
        rename(serialize = "@android:pathPrefix"),
        skip_serializing_if = "Option::is_none"
    )]
    pub path_prefix: Option<String>,
    #[serde(
        rename(serialize = "@android:mimeType"),
        skip_serializing_if = "Option::is_none"
    )]
    pub mime_type: Option<String>,
}

/// Android [meta-data element](https://developer.android.com/guide/topics/manifest/meta-data-element).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MetaData {
    #[serde(rename(serialize = "@android:name"))]
    pub name: String,
    #[serde(
        rename(serialize = "@android:value"),
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<String>,
    /// A reference to a resource, such as the `@xml/device_filter` that a
    /// `USB_DEVICE_ATTACHED` intent filter reads its device list from.
    #[serde(
        rename(serialize = "@android:resource"),
        skip_serializing_if = "Option::is_none"
    )]
    pub resource: Option<String>,
}

/// Android [uses-feature element](https://developer.android.com/guide/topics/manifest/uses-feature-element).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Feature {
    #[serde(
        rename(serialize = "@android:name"),
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<String>,
    #[serde(
        rename(serialize = "@android:required"),
        skip_serializing_if = "Option::is_none"
    )]
    pub required: Option<bool>,
    /// The `version` field is currently used for the following features:
    ///
    /// - `name="android.hardware.vulkan.compute"`: The minimum level of compute features required. See the [Android documentation](https://developer.android.com/reference/android/content/pm/PackageManager#FEATURE_VULKAN_HARDWARE_COMPUTE)
    ///   for available levels and the respective Vulkan features required/provided.
    ///
    /// - `name="android.hardware.vulkan.level"`: The minimum Vulkan requirements. See the [Android documentation](https://developer.android.com/reference/android/content/pm/PackageManager#FEATURE_VULKAN_HARDWARE_LEVEL)
    ///   for available levels and the respective Vulkan features required/provided.
    ///
    /// - `name="android.hardware.vulkan.version"`: Represents the value of Vulkan's `VkPhysicalDeviceProperties::apiVersion`. See the [Android documentation](https://developer.android.com/reference/android/content/pm/PackageManager#FEATURE_VULKAN_HARDWARE_VERSION)
    ///   for available levels and the respective Vulkan features required/provided.
    #[serde(
        rename(serialize = "@android:version"),
        skip_serializing_if = "Option::is_none"
    )]
    pub version: Option<u32>,
    #[serde(
        rename(serialize = "@android:glEsVersion"),
        skip_serializing_if = "Option::is_none"
    )]
    #[serde(serialize_with = "serialize_opengles_version")]
    pub opengles_version: Option<(u8, u8)>,
}

fn serialize_opengles_version<S>(
    version: &Option<(u8, u8)>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match version {
        Some(version) => {
            let opengles_version = format!("0x{:04}{:04}", version.0, version.1);
            serializer.serialize_some(&opengles_version)
        }
        None => serializer.serialize_none(),
    }
}

/// Android [uses-permission element](https://developer.android.com/guide/topics/manifest/uses-permission-element).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Permission {
    #[serde(rename(serialize = "@android:name"))]
    pub name: String,
    #[serde(
        rename(serialize = "@android:maxSdkVersion"),
        skip_serializing_if = "Option::is_none"
    )]
    pub max_sdk_version: Option<u32>,
}

/// Android [grant-uri-permission element](https://developer.android.com/guide/topics/manifest/grant-uri-permission-element).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GrantUriPermission {
    #[serde(rename(serialize = "@android:name"))]
    pub uri: String,
}

/// Android [package element](https://developer.android.com/guide/topics/manifest/queries-element#package).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Package {
    #[serde(rename(serialize = "@android:name"))]
    pub name: String,
}

/// Android [provider element](https://developer.android.com/guide/topics/manifest/queries-element#provider).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct QueryProvider {
    #[serde(rename(serialize = "@android:authorities"))]
    pub authorities: String,

    // Optional per the spec, and aapt2 (which cargo-rapk now uses) accepts a
    // provider without it. It was mandatory while the APK path ran aapt v1.
    #[serde(
        rename(serialize = "@android:name"),
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<String>,
}

/// Android [provider element](https://developer.android.com/guide/topics/manifest/provider-element),
/// as contributed by library manifests.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Provider {
    #[serde(rename(serialize = "@android:name"))]
    pub name: String,
    /// `tools:node` from a library manifest. It steers merging and is never
    /// written out: a merger directive has no meaning to the platform, and the
    /// root element declares no `tools` namespace, so emitting it would make
    /// the manifest unparseable.
    #[serde(skip_serializing)]
    pub tools_node: Option<String>,
    #[serde(
        rename(serialize = "@android:authorities"),
        skip_serializing_if = "Option::is_none"
    )]
    pub authorities: Option<String>,
    #[serde(
        rename(serialize = "@android:exported"),
        skip_serializing_if = "Option::is_none"
    )]
    pub exported: Option<bool>,
    #[serde(
        rename(serialize = "@android:enabled"),
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled: Option<bool>,
    #[serde(
        rename(serialize = "@android:initOrder"),
        skip_serializing_if = "Option::is_none"
    )]
    pub init_order: Option<i32>,
    #[serde(
        rename(serialize = "@android:multiprocess"),
        skip_serializing_if = "Option::is_none"
    )]
    pub multiprocess: Option<bool>,
    #[serde(
        rename(serialize = "@android:process"),
        skip_serializing_if = "Option::is_none"
    )]
    pub process: Option<String>,
    #[serde(
        rename(serialize = "@android:grantUriPermissions"),
        skip_serializing_if = "Option::is_none"
    )]
    pub grant_uri_permissions: Option<bool>,
    #[serde(rename(serialize = "meta-data"), default)]
    pub meta_data: Vec<MetaData>,
}

/// Android [queries element](https://developer.android.com/guide/topics/manifest/queries-element).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Queries {
    #[serde(default)]
    pub package: Vec<Package>,
    #[serde(default)]
    pub intent: Vec<IntentFilter>,
    #[serde(default)]
    pub provider: Vec<QueryProvider>,
}

/// Android [uses-sdk element](https://developer.android.com/guide/topics/manifest/uses-sdk-element).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Sdk {
    #[serde(
        rename(serialize = "@android:minSdkVersion"),
        skip_serializing_if = "Option::is_none"
    )]
    pub min_sdk_version: Option<u32>,
    #[serde(
        rename(serialize = "@android:targetSdkVersion"),
        skip_serializing_if = "Option::is_none"
    )]
    pub target_sdk_version: Option<u32>,
    #[serde(
        rename(serialize = "@android:maxSdkVersion"),
        skip_serializing_if = "Option::is_none"
    )]
    pub max_sdk_version: Option<u32>,
    /// Optional base `version_code` override, read from
    /// `[package.metadata.android.sdk]`. When absent, the base is derived
    /// from the package semver. Never serialized: it is not part of the
    /// `uses-sdk` element.
    #[serde(default, skip_serializing)]
    pub version_code: Option<u32>,
}

impl Default for Sdk {
    fn default() -> Self {
        Self {
            min_sdk_version: Some(23),
            target_sdk_version: None,
            max_sdk_version: None,
            version_code: None,
        }
    }
}

fn default_namespace() -> String {
    "http://schemas.android.com/apk/res/android".to_string()
}

fn default_activity_name() -> String {
    "android.app.NativeActivity".to_string()
}

fn default_config_changes() -> Option<String> {
    Some("orientation|keyboardHidden|screenSize".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity_xml(toml_source: &str) -> String {
        let activity: Activity = toml::from_str(toml_source).unwrap();
        quick_xml::se::to_string(&activity).unwrap()
    }

    #[test]
    fn meta_data_can_reference_a_resource() {
        let xml = activity_xml(
            r#"
            [[meta_data]]
            name = "android.hardware.usb.action.USB_DEVICE_ATTACHED"
            resource = "@xml/device_filter"
            "#,
        );

        assert!(xml.contains(
            r#"<meta-data android:name="android.hardware.usb.action.USB_DEVICE_ATTACHED" android:resource="@xml/device_filter"/>"#
        ));
        assert!(!xml.contains("android:value"));
    }

    #[test]
    fn meta_data_values_are_unchanged() {
        let xml = activity_xml(
            r#"
            [[meta_data]]
            name = "android.app.lib_name"
            value = "example"
            "#,
        );

        assert!(xml.contains(
            r#"<meta-data android:name="android.app.lib_name" android:value="example"/>"#
        ));
        assert!(!xml.contains("android:resource"));
    }
}

#[cfg(test)]
mod library_manifest_tests {
    use super::*;

    /// A library manifest exercising every element kind the merger reads, in
    /// both the self-closing and the container form.
    const LIBRARY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    xmlns:tools="http://schemas.android.com/tools" package="androidx.lib">
  <uses-permission android:name="android.permission.WAKE_LOCK" />
  <uses-feature android:name="android.hardware.vulkan.version" android:version="0x400003" />
  <grant-uri-permission android:name="content://media" />
  <application>
    <service
        android:name="androidx.lib.AlarmService"
        android:exported="false"
        android:enabled="@bool/alarm_default" />
    <service android:name="androidx.lib.JobService" android:exported="true" />
    <activity android:name=".Main" android:exported="true" />
    <receiver android:name=".Boot" android:exported="true" />
    <provider
        android:name="androidx.lib.Init"
        android:authorities="${applicationId}.lib"
        android:exported="false"
        tools:node="merge">
      <meta-data android:name="androidx.lib.Init" android:value="androidx.startup" />
    </provider>
    <service android:name="androidx.lib.Dropped" tools:node="remove" />
    <meta-data android:name="androidx.lib.meta" android:value="v" />
  </application>
</manifest>"#;

    fn merged() -> AndroidManifest {
        let library = parse_library_manifest(LIBRARY).expect("library parses");
        let mut app = AndroidManifest {
            package: "com.example.app".into(),
            ..Default::default()
        };
        app.merge_library(&library, "com.example.app");
        app
    }

    #[test]
    fn self_closing_and_container_components_are_both_read() {
        let app = merged();
        // Self-closing `<service ... />` used to be skipped entirely.
        let services: Vec<&str> = app
            .application
            .service
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            services.contains(&"androidx.lib.AlarmService"),
            "{services:?}"
        );
        assert!(
            services.contains(&"androidx.lib.JobService"),
            "{services:?}"
        );
        assert_eq!(app.application.receiver.len(), 1);
        assert_eq!(app.application.activity.len(), 2); // library's + the default
    }

    #[test]
    fn features_and_grant_uri_permissions_are_merged() {
        let app = merged();
        assert_eq!(app.uses_feature.len(), 1);
        assert_eq!(
            app.uses_feature[0].name.as_deref(),
            Some("android.hardware.vulkan.version")
        );
        assert_eq!(app.grant_uri_permission.len(), 1);
        assert_eq!(app.grant_uri_permission[0].uri, "content://media");
    }

    #[test]
    fn application_id_is_substituted_in_authorities() {
        let app = merged();
        assert_eq!(
            app.application.provider[0].authorities.as_deref(),
            Some("com.example.app.lib")
        );
    }

    #[test]
    fn tools_node_remove_drops_the_element() {
        let app = merged();
        assert!(
            !app.application
                .service
                .iter()
                .any(|s| s.name == "androidx.lib.Dropped"),
            "tools:node=\"remove\" must drop the library's declaration"
        );
    }

    #[test]
    fn tools_directives_never_reach_the_output_manifest() {
        let app = merged();
        let dir = std::env::temp_dir().join("cargo-rapk-tools-node-test");
        std::fs::create_dir_all(&dir).unwrap();
        app.write_to(&dir).unwrap();
        let xml = std::fs::read_to_string(dir.join("AndroidManifest.xml")).unwrap();
        // A `tools:` attribute would need a namespace the root does not
        // declare, making the manifest unparseable by aapt2.
        assert!(
            !xml.contains("tools:"),
            "tools: leaked into the manifest: {xml}"
        );
    }

    #[test]
    fn the_app_declaration_wins_unless_the_library_says_replace() {
        let library = parse_library_manifest(
            r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="l">
                 <application>
                   <service android:name="S" android:exported="false" />
                   <service android:name="R" android:exported="false" tools:node="replace" />
                 </application>
               </manifest>"#,
        )
        .unwrap();
        let mut app = AndroidManifest {
            package: "com.example.app".into(),
            ..Default::default()
        };
        app.application.service.push(Service {
            name: "S".into(),
            exported: Some(true),
            ..Default::default()
        });
        app.application.service.push(Service {
            name: "R".into(),
            exported: Some(true),
            ..Default::default()
        });
        app.merge_library(&library, "com.example.app");
        let s = app
            .application
            .service
            .iter()
            .find(|s| s.name == "S")
            .unwrap();
        let r = app
            .application
            .service
            .iter()
            .find(|s| s.name == "R")
            .unwrap();
        assert_eq!(s.exported, Some(true), "the app's own declaration must win");
        assert_eq!(
            r.exported,
            Some(false),
            "tools:node=\"replace\" must override"
        );
    }
}
