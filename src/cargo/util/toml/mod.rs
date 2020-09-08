use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str;

use anyhow::{anyhow, bail};
use cargo_platform::Platform;
use log::{debug, trace};
use semver::{self, VersionReq};
use serde::de::{self, IntoDeserializer};
use serde::ser;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::core::dependency::DepKind;
use crate::core::manifest::{ManifestMetadata, TargetSourcePath, Warnings};
use crate::core::profiles::Strip;
use crate::core::resolver::ResolveBehavior;
use crate::core::workspace::find_workspace_root;
use crate::core::{Dependency, Manifest, PackageId, Summary, Target};
use crate::core::{Edition, EitherManifest, Feature, Features, VirtualManifest, Workspace};
use crate::core::{GitReference, PackageIdSpec, SourceId, WorkspaceConfig, WorkspaceRootConfig};
use crate::sources::{CRATES_IO_INDEX, CRATES_IO_REGISTRY};
use crate::util::errors::{CargoResult, CargoResultExt, ManifestError};
use crate::util::interning::InternedString;
use crate::util::{self, paths, validate_package_name, Config, IntoUrl};

mod targets;
use self::targets::targets;
mod manifest_cache;
pub use manifest_cache::{parse_manifest, ManifestCache, ParseOutput};

pub fn read_manifest(
    path: &Path,
    source_id: SourceId,
    config: &Config,
) -> Result<(EitherManifest, Vec<PathBuf>), ManifestError> {
    trace!(
        "read_manifest; path={}; source-id={}",
        path.display(),
        source_id
    );

    let output = parse_manifest(path, config)?;

    do_read_manifest(&output, path, source_id, config)
        .chain_err(|| format!("failed to parse manifest at `{}`", path.display()))
        .map_err(|err| ManifestError::new(err, path.into()))
}

fn do_read_manifest(
    output: &ParseOutput,
    manifest_file: &Path,
    source_id: SourceId,
    config: &Config,
) -> CargoResult<(EitherManifest, Vec<PathBuf>)> {
    let manifest = DefinedTomlManifest::from_toml_manifest(
        TomlManifest::clone(&output.manifest),
        manifest_file,
        config,
    )?;

    let add_unused = |warnings: &mut Warnings| {
        for key in output.unused.iter() {
            warnings.add_warning(format!("unused manifest key: {}", key));
            if key == "profiles.debug" {
                warnings.add_warning("use `[profile.dev]` to configure debug builds".to_string());
            }
        }
    };

    if let Some(deps) = manifest
        .workspace
        .as_ref()
        .and_then(|ws| ws.dependencies.as_ref())
    {
        for (name, dep) in deps {
            if dep.is_optional() {
                bail!(
                    "{} is optional, but workspace dependencies cannot be optional",
                    name
                );
            }
        }
    }

    return if manifest.package.is_some() {
        let (mut manifest, paths) =
            manifest.into_real_manifest(source_id, manifest_file, config)?;
        add_unused(manifest.warnings_mut());
        if manifest.targets().iter().all(|t| t.is_custom_build()) {
            bail!(
                "no targets specified in the manifest\n\
                 either src/lib.rs, src/main.rs, a [lib] section, or \
                 [[bin]] section must be present"
            )
        }
        Ok((EitherManifest::Real(manifest), paths))
    } else {
        let (mut m, paths) = manifest.into_virtual_manifest(source_id, manifest_file, config)?;
        add_unused(m.warnings_mut());
        Ok((EitherManifest::Virtual(m), paths))
    };
}

pub fn parse(toml: &str, file: &Path, config: &Config) -> CargoResult<toml::Value> {
    let first_error = match toml.parse() {
        Ok(ret) => return Ok(ret),
        Err(e) => e,
    };

    let mut second_parser = toml::de::Deserializer::new(toml);
    second_parser.set_require_newline_after_table(false);
    if let Ok(ret) = toml::Value::deserialize(&mut second_parser) {
        let msg = format!(
            "\
TOML file found which contains invalid syntax and will soon not parse
at `{}`.

The TOML spec requires newlines after table definitions (e.g., `[a] b = 1` is
invalid), but this file has a table header which does not have a newline after
it. A newline needs to be added and this warning will soon become a hard error
in the future.",
            file.display()
        );
        config.shell().warn(&msg)?;
        return Ok(ret);
    }

    let mut third_parser = toml::de::Deserializer::new(toml);
    third_parser.set_allow_duplicate_after_longer_table(true);
    if let Ok(ret) = toml::Value::deserialize(&mut third_parser) {
        let msg = format!(
            "\
TOML file found which contains invalid syntax and will soon not parse
at `{}`.

The TOML spec requires that each table header is defined at most once, but
historical versions of Cargo have erroneously accepted this file. The table
definitions will need to be merged together with one table header to proceed,
and this will become a hard error in the future.",
            file.display()
        );
        config.shell().warn(&msg)?;
        return Ok(ret);
    }

    let first_error = anyhow::Error::from(first_error);
    Err(first_error.context("could not parse input as TOML"))
}

type TomlLibTarget = TomlTarget;
type TomlBinTarget = TomlTarget;
type TomlExampleTarget = TomlTarget;
type TomlTestTarget = TomlTarget;
type TomlBenchTarget = TomlTarget;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged, try_from = "TomlDependency")]
pub enum DefinedTomlDependency {
    Simple(String),
    Detailed(TomlDependencyDetails),
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum TomlDependency {
    Simple(String),
    Workspace(WorkspaceDetails),
    Detailed(TomlDependencyDetails),
}

#[derive(Clone, Debug, Serialize)]
#[serde(into = "TomlWorkspaceDetails")]
pub struct WorkspaceDetails {
    features: Option<Vec<String>>,
    optional: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct TomlWorkspaceDetails {
    workspace: bool,
    features: Option<Vec<String>>,
    optional: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DefinedTomlDependencyWrapper {
    Simple(String),
    Workspace(TomlWorkspaceDetails),
    Detailed(TomlDependencyDetails),
}

impl From<WorkspaceDetails> for TomlWorkspaceDetails {
    fn from(workspace_details: WorkspaceDetails) -> Self {
        Self {
            workspace: true,
            features: workspace_details.features,
            optional: workspace_details.optional,
        }
    }
}

impl std::convert::TryFrom<TomlDependency> for DefinedTomlDependency {
    type Error = anyhow::Error;

    fn try_from(toml_workspace_dependency: TomlDependency) -> CargoResult<Self> {
        match toml_workspace_dependency {
            TomlDependency::Simple(simple) => Ok(Self::Simple(simple)),
            TomlDependency::Detailed(details) => Ok(Self::Detailed(details)),
            TomlDependency::Workspace(_) => Err(anyhow!("cannot specify workspace dependency")),
        }
    }
}

// This implementation of `Deserialize`, along with many others in this file, exist entirely to
// provide error handling. The error message in derived implementations of `Deserialize` are
// sometimes too opaque to expose to end users. These implementations should otherwise behave
// exactly like their derived counterparts.
impl<'de> de::Deserialize<'de> for TomlDependency {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct TomlDependencyVisitor;

        impl<'de> de::Visitor<'de> for TomlDependencyVisitor {
            type Value = TomlDependency;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "a version string like \"0.9.8\" or a \
                     detailed dependency like { version = \"0.9.8\" }",
                )
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(Self::Value::Simple(s.to_owned()))
            }

            fn visit_map<V>(self, map: V) -> Result<Self::Value, V::Error>
            where
                V: de::MapAccess<'de>,
            {
                let mvd = de::value::MapAccessDeserializer::new(map);

                Ok(match DefinedTomlDependencyWrapper::deserialize(mvd) {
                    Ok(DefinedTomlDependencyWrapper::Simple(version)) => Self::Value::Simple(version),
                    Ok(DefinedTomlDependencyWrapper::Detailed(details)) => Self::Value::Detailed(details),
                    Ok(DefinedTomlDependencyWrapper::Workspace(ws)) if ws.workspace => {
                        Self::Value::Workspace(WorkspaceDetails {
                            features: ws.features,
                            optional: ws.optional,
                        })
                    }

                    Ok(DefinedTomlDependencyWrapper::Workspace(_)) => {
                        return Err(de::Error::custom("workspace cannot be false"));
                    }

                    Err(_) => return Err(de::Error::custom(
                        "a version string like \"0.9.8\" or a detailed dependency like { version = \"0.9.8\" }",
                    )),
                })
            }
        }

        deserializer.deserialize_any(TomlDependencyVisitor)
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub struct TomlDependencyDetails {
    version: Option<String>,
    registry: Option<String>,
    /// The URL of the `registry` field.
    /// This is an internal implementation detail. When Cargo creates a
    /// package, it replaces `registry` with `registry-index` so that the
    /// manifest contains the correct URL. All users won't have the same
    /// registry names configured, so Cargo can't rely on just the name for
    /// crates published by other users.
    registry_index: Option<String>,
    path: Option<String>,
    git: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
    rev: Option<String>,
    features: Option<Vec<String>>,
    optional: Option<bool>,
    default_features: Option<bool>,
    #[serde(rename = "default_features")]
    default_features2: Option<bool>,
    package: Option<String>,
    public: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlManifest {
    cargo_features: Option<Vec<String>>,
    package: Option<Box<TomlProject>>,
    project: Option<Box<TomlProject>>,
    profile: Option<TomlProfiles>,
    lib: Option<TomlLibTarget>,
    bin: Option<Vec<TomlBinTarget>>,
    example: Option<Vec<TomlExampleTarget>>,
    test: Option<Vec<TomlTestTarget>>,
    bench: Option<Vec<TomlTestTarget>>,
    dependencies: Option<BTreeMap<String, TomlDependency>>,
    dev_dependencies: Option<BTreeMap<String, TomlDependency>>,
    #[serde(rename = "dev_dependencies")]
    dev_dependencies2: Option<BTreeMap<String, TomlDependency>>,
    build_dependencies: Option<BTreeMap<String, TomlDependency>>,
    #[serde(rename = "build_dependencies")]
    build_dependencies2: Option<BTreeMap<String, TomlDependency>>,
    features: Option<BTreeMap<String, Vec<String>>>,
    target: Option<BTreeMap<String, TomlPlatform>>,
    replace: Option<BTreeMap<String, DefinedTomlDependency>>,
    patch: Option<BTreeMap<String, BTreeMap<String, DefinedTomlDependency>>>,
    workspace: Option<TomlWorkspace>,
    #[serde(deserialize_with = "deserialize_workspace_badges", default)]
    badges: Option<MaybeWorkspace<BTreeMap<String, BTreeMap<String, String>>>>,
}

impl TomlManifest {
    pub fn workspace_config(
        &self,
        package_root: &Path,
        config: &Config,
    ) -> CargoResult<WorkspaceConfig> {
        let workspace = self.workspace.as_ref();
        let project_workspace = self
            .project
            .as_ref()
            .or_else(|| self.package.as_ref())
            .and_then(|p| p.workspace.as_ref());

        Ok(match (workspace, project_workspace) {
            (Some(toml_workspace), None) => WorkspaceConfig::Root(
                WorkspaceRootConfig::from_toml_workspace(package_root, &config, toml_workspace)?,
            ),
            (None, root) => WorkspaceConfig::Member {
                root: root.cloned(),
            },
            (Some(..), Some(..)) => bail!(
                "cannot configure both `package.workspace` and \
                 `[workspace]`, only one can be specified"
            ),
        })
    }
}

#[derive(Debug, Clone)]
pub struct DefinedTomlManifest {
    cargo_features: Option<Vec<String>>,
    package: Option<DefinedTomlPackage>,
    profile: Option<TomlProfiles>,
    lib: Option<TomlLibTarget>,
    bin: Option<Vec<TomlBinTarget>>,
    example: Option<Vec<TomlExampleTarget>>,
    test: Option<Vec<TomlTestTarget>>,
    bench: Option<Vec<TomlTestTarget>>,
    dependencies: Option<BTreeMap<String, DefinedTomlDependency>>,
    dev_dependencies: Option<BTreeMap<String, DefinedTomlDependency>>,
    build_dependencies: Option<BTreeMap<String, DefinedTomlDependency>>,
    features: Option<BTreeMap<String, Vec<String>>>,
    target: Option<BTreeMap<String, DefinedTomlPlatform>>,
    replace: Option<BTreeMap<String, DefinedTomlDependency>>,
    patch: Option<BTreeMap<String, BTreeMap<String, DefinedTomlDependency>>>,
    workspace: Option<TomlWorkspace>,
    badges: Option<BTreeMap<String, BTreeMap<String, String>>>,
}

impl DefinedTomlManifest {
    fn from_toml_manifest(
        manifest: TomlManifest,
        manifest_file: &Path,
        config: &Config,
    ) -> CargoResult<Self> {
        let package_root = manifest_file.parent().unwrap();
        let root_path = find_workspace_root(manifest_file, config)?;
        let root_path = root_path.as_deref();

        let output = root_path
            .map(|root_path| parse_manifest(root_path, config))
            .transpose()?;

        let workspace = output.as_ref().and_then(|output| output.workspace());

        let package = manifest
            .package
            .or(manifest.project)
            .map(|p| *p)
            .map(|p| DefinedTomlPackage::from_toml_project(p, workspace, root_path, package_root))
            .transpose()?;

        let badges = ws_default(manifest.badges, workspace, |ws| &ws.badges, "badges")?;

        let ws_deps = workspace.map(|ws| ws.dependencies.as_ref()).flatten();
        let dependencies =
            to_defined_dependencies(manifest.dependencies.as_ref(), ws_deps, root_path)?;
        let dev_dependencies = to_defined_dependencies(
            manifest
                .dev_dependencies
                .or(manifest.dev_dependencies2)
                .as_ref(),
            ws_deps,
            root_path,
        )?;

        let build_dependencies = to_defined_dependencies(
            manifest
                .build_dependencies
                .or(manifest.build_dependencies2)
                .as_ref(),
            ws_deps,
            root_path,
        )?;

        let target = to_defined_platform(manifest.target, ws_deps, root_path)?;

        Ok(Self {
            cargo_features: manifest.cargo_features,
            package,
            profile: manifest.profile,
            lib: manifest.lib,
            bin: manifest.bin,
            example: manifest.example,
            test: manifest.test,
            bench: manifest.bench,
            dependencies,
            dev_dependencies,
            build_dependencies,
            features: manifest.features,
            target,
            replace: manifest.replace,
            patch: manifest.patch,
            workspace: manifest.workspace,
            badges,
        })
    }
}

fn to_defined_dependencies(
    dependencies: Option<&BTreeMap<String, TomlDependency>>,
    ws_dependencies: Option<&BTreeMap<String, DefinedTomlDependency>>,
    root_path: Option<&Path>,
) -> CargoResult<Option<BTreeMap<String, DefinedTomlDependency>>> {
    let empty = BTreeMap::new();
    let ws_deps = ws_dependencies.unwrap_or(&empty);

    map_btree(dependencies, |key, dep| {
        DefinedTomlDependency::from_toml_dependency(dep, &key, &ws_deps, root_path)
    })
}

fn to_toml_dependencies(
    dependencies: Option<&BTreeMap<String, DefinedTomlDependency>>,
) -> Option<BTreeMap<String, TomlDependency>> {
    map_btree(dependencies, |_key, dep| {
        Ok(TomlDependency::from_defined_dependency(dep))
    })
    .unwrap()
}

fn to_defined_platform(
    toml_platform: Option<BTreeMap<String, TomlPlatform>>,
    ws_dependencies: Option<&BTreeMap<String, DefinedTomlDependency>>,
    root_path: Option<&Path>,
) -> CargoResult<Option<BTreeMap<String, DefinedTomlPlatform>>> {
    let empty = BTreeMap::new();
    let ws_deps = ws_dependencies.unwrap_or(&empty);
    map_btree(toml_platform.as_ref(), |_key, toml_platform| {
        DefinedTomlPlatform::from_toml_platform(toml_platform, ws_deps, root_path)
    })
}

fn to_toml_platform(
    defined_platform: Option<BTreeMap<String, DefinedTomlPlatform>>,
) -> Option<BTreeMap<String, TomlPlatform>> {
    map_btree(defined_platform.as_ref(), |_key, defined_platform| {
        Ok(TomlPlatform::from_defined_platform(defined_platform))
    })
    .unwrap()
}

pub fn map_deps(
    config: &Config,
    deps: Option<&BTreeMap<String, DefinedTomlDependency>>,
    filter: impl Fn(&DefinedTomlDependency) -> bool,
) -> CargoResult<Option<BTreeMap<String, DefinedTomlDependency>>> {
    let deps = match deps {
        Some(deps) => deps,
        None => return Ok(None),
    };
    let deps = deps
        .iter()
        .filter(|(_k, v)| filter(v))
        .map(|(k, v)| Ok((k.clone(), map_dependency(config, v)?)))
        .collect::<CargoResult<BTreeMap<_, _>>>()?;
    Ok(Some(deps))
}

fn map_dependency(
    config: &Config,
    dep: &DefinedTomlDependency,
) -> CargoResult<DefinedTomlDependency> {
    match dep {
        DefinedTomlDependency::Detailed(d) => {
            let mut d = d.clone();
            // Path dependencies become crates.io deps.
            d.path.take();
            // Same with git dependencies.
            d.git.take();
            d.branch.take();
            d.tag.take();
            d.rev.take();
            // registry specifications are elaborated to the index URL
            if let Some(registry) = d.registry.take() {
                let src = SourceId::alt_registry(config, &registry)?;
                d.registry_index = Some(src.url().to_string());
            }
            Ok(DefinedTomlDependency::Detailed(d))
        }
        DefinedTomlDependency::Simple(s) => {
            Ok(DefinedTomlDependency::Detailed(TomlDependencyDetails {
                version: Some(s.clone()),
                ..Default::default()
            }))
        }
    }
}

fn map_btree<T, R>(
    tree: Option<&BTreeMap<String, T>>,
    f: impl Fn(&str, &T) -> CargoResult<R>,
) -> CargoResult<Option<BTreeMap<String, R>>> {
    match tree {
        None => Ok(None),
        Some(deps) => Ok(Some(
            deps.iter()
                .map(|(key, val)| Ok((key.clone(), f(&*key, val)?)))
                .collect::<CargoResult<BTreeMap<_, _>>>()?,
        )),
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct TomlProfiles(BTreeMap<InternedString, TomlProfile>);

impl TomlProfiles {
    pub fn get_all(&self) -> &BTreeMap<InternedString, TomlProfile> {
        &self.0
    }

    pub fn get(&self, name: &str) -> Option<&TomlProfile> {
        self.0.get(name)
    }

    pub fn validate(&self, features: &Features, warnings: &mut Vec<String>) -> CargoResult<()> {
        for (name, profile) in &self.0 {
            profile.validate(name, features, warnings)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TomlOptLevel(pub String);

impl<'de> de::Deserialize<'de> for TomlOptLevel {
    fn deserialize<D>(d: D) -> Result<TomlOptLevel, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = TomlOptLevel;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an optimization level")
            }

            fn visit_i64<E>(self, value: i64) -> Result<TomlOptLevel, E>
            where
                E: de::Error,
            {
                Ok(TomlOptLevel(value.to_string()))
            }

            fn visit_str<E>(self, value: &str) -> Result<TomlOptLevel, E>
            where
                E: de::Error,
            {
                if value == "s" || value == "z" {
                    Ok(TomlOptLevel(value.to_string()))
                } else {
                    Err(E::custom(format!(
                        "must be an integer, `z`, or `s`, \
                         but found the string: \"{}\"",
                        value
                    )))
                }
            }
        }

        d.deserialize_any(Visitor)
    }
}

impl ser::Serialize for TomlOptLevel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        match self.0.parse::<u32>() {
            Ok(n) => n.serialize(serializer),
            Err(_) => self.0.serialize(serializer),
        }
    }
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum U32OrBool {
    U32(u32),
    Bool(bool),
}

impl<'de> de::Deserialize<'de> for U32OrBool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = U32OrBool;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a boolean or an integer")
            }

            fn visit_bool<E>(self, b: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(U32OrBool::Bool(b))
            }

            fn visit_i64<E>(self, u: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(U32OrBool::U32(u as u32))
            }

            fn visit_u64<E>(self, u: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(U32OrBool::U32(u as u32))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, Eq, PartialEq)]
#[serde(default, rename_all = "kebab-case")]
pub struct TomlProfile {
    pub opt_level: Option<TomlOptLevel>,
    pub lto: Option<StringOrBool>,
    pub codegen_units: Option<u32>,
    pub debug: Option<U32OrBool>,
    pub debug_assertions: Option<bool>,
    pub rpath: Option<bool>,
    pub panic: Option<String>,
    pub overflow_checks: Option<bool>,
    pub incremental: Option<bool>,
    pub package: Option<BTreeMap<ProfilePackageSpec, TomlProfile>>,
    pub build_override: Option<Box<TomlProfile>>,
    pub dir_name: Option<InternedString>,
    pub inherits: Option<InternedString>,
    pub strip: Option<Strip>,
}

#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum ProfilePackageSpec {
    Spec(PackageIdSpec),
    All,
}

impl ser::Serialize for ProfilePackageSpec {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        match *self {
            ProfilePackageSpec::Spec(ref spec) => spec.serialize(s),
            ProfilePackageSpec::All => "*".serialize(s),
        }
    }
}

impl<'de> de::Deserialize<'de> for ProfilePackageSpec {
    fn deserialize<D>(d: D) -> Result<ProfilePackageSpec, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let string = String::deserialize(d)?;
        if string == "*" {
            Ok(ProfilePackageSpec::All)
        } else {
            PackageIdSpec::parse(&string)
                .map_err(de::Error::custom)
                .map(ProfilePackageSpec::Spec)
        }
    }
}

impl TomlProfile {
    pub fn validate(
        &self,
        name: &str,
        features: &Features,
        warnings: &mut Vec<String>,
    ) -> CargoResult<()> {
        if name == "debug" {
            warnings.push("use `[profile.dev]` to configure debug builds".to_string());
        }

        if let Some(ref profile) = self.build_override {
            features.require(Feature::profile_overrides())?;
            profile.validate_override("build-override")?;
        }
        if let Some(ref packages) = self.package {
            features.require(Feature::profile_overrides())?;
            for profile in packages.values() {
                profile.validate_override("package")?;
            }
        }

        // Feature gate definition of named profiles
        match name {
            "dev" | "release" | "bench" | "test" | "doc" => {}
            _ => {
                features.require(Feature::named_profiles())?;
            }
        }

        // Profile name validation
        Self::validate_name(name, "profile name")?;

        // Feature gate on uses of keys related to named profiles
        if self.inherits.is_some() {
            features.require(Feature::named_profiles())?;
        }

        if self.dir_name.is_some() {
            features.require(Feature::named_profiles())?;
        }

        // `dir-name` validation
        match &self.dir_name {
            None => {}
            Some(dir_name) => {
                Self::validate_name(dir_name, "dir-name")?;
            }
        }

        // `inherits` validation
        match &self.inherits {
            None => {}
            Some(inherits) => {
                Self::validate_name(inherits, "inherits")?;
            }
        }

        match name {
            "doc" => {
                warnings.push("profile `doc` is deprecated and has no effect".to_string());
            }
            "test" | "bench" => {
                if self.panic.is_some() {
                    warnings.push(format!("`panic` setting is ignored for `{}` profile", name))
                }
            }
            _ => {}
        }

        if let Some(panic) = &self.panic {
            if panic != "unwind" && panic != "abort" {
                bail!(
                    "`panic` setting of `{}` is not a valid setting,\
                     must be `unwind` or `abort`",
                    panic
                );
            }
        }

        if self.strip.is_some() {
            features.require(Feature::strip())?;
        }
        Ok(())
    }

    /// Validate dir-names and profile names according to RFC 2678.
    pub fn validate_name(name: &str, what: &str) -> CargoResult<()> {
        if let Some(ch) = name
            .chars()
            .find(|ch| !ch.is_alphanumeric() && *ch != '_' && *ch != '-')
        {
            bail!("Invalid character `{}` in {}: `{}`", ch, what, name);
        }

        match name {
            "package" | "build" => {
                bail!("Invalid {}: `{}`", what, name);
            }
            "debug" if what == "profile" => {
                if what == "profile name" {
                    // Allowed, but will emit warnings
                } else {
                    bail!("Invalid {}: `{}`", what, name);
                }
            }
            "doc" if what == "dir-name" => {
                bail!("Invalid {}: `{}`", what, name);
            }
            _ => {}
        }

        Ok(())
    }

    fn validate_override(&self, which: &str) -> CargoResult<()> {
        if self.package.is_some() {
            bail!("package-specific profiles cannot be nested");
        }
        if self.build_override.is_some() {
            bail!("build-override profiles cannot be nested");
        }
        if self.panic.is_some() {
            bail!("`panic` may not be specified in a `{}` profile", which)
        }
        if self.lto.is_some() {
            bail!("`lto` may not be specified in a `{}` profile", which)
        }
        if self.rpath.is_some() {
            bail!("`rpath` may not be specified in a `{}` profile", which)
        }
        Ok(())
    }

    /// Overwrite self's values with the given profile.
    pub fn merge(&mut self, profile: &TomlProfile) {
        if let Some(v) = &profile.opt_level {
            self.opt_level = Some(v.clone());
        }

        if let Some(v) = &profile.lto {
            self.lto = Some(v.clone());
        }

        if let Some(v) = profile.codegen_units {
            self.codegen_units = Some(v);
        }

        if let Some(v) = &profile.debug {
            self.debug = Some(v.clone());
        }

        if let Some(v) = profile.debug_assertions {
            self.debug_assertions = Some(v);
        }

        if let Some(v) = profile.rpath {
            self.rpath = Some(v);
        }

        if let Some(v) = &profile.panic {
            self.panic = Some(v.clone());
        }

        if let Some(v) = profile.overflow_checks {
            self.overflow_checks = Some(v);
        }

        if let Some(v) = profile.incremental {
            self.incremental = Some(v);
        }

        if let Some(other_package) = &profile.package {
            match &mut self.package {
                Some(self_package) => {
                    for (spec, other_pkg_profile) in other_package {
                        match self_package.get_mut(spec) {
                            Some(p) => p.merge(other_pkg_profile),
                            None => {
                                self_package.insert(spec.clone(), other_pkg_profile.clone());
                            }
                        }
                    }
                }
                None => self.package = Some(other_package.clone()),
            }
        }

        if let Some(other_bo) = &profile.build_override {
            match &mut self.build_override {
                Some(self_bo) => self_bo.merge(other_bo),
                None => self.build_override = Some(other_bo.clone()),
            }
        }

        if let Some(v) = &profile.inherits {
            self.inherits = Some(*v);
        }

        if let Some(v) = &profile.dir_name {
            self.dir_name = Some(*v);
        }

        if let Some(v) = profile.strip {
            self.strip = Some(v);
        }
    }
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub struct StringOrVec(Vec<String>);

impl<'de> de::Deserialize<'de> for StringOrVec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = StringOrVec;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("string or list of strings")
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringOrVec(vec![s.to_string()]))
            }

            fn visit_seq<V>(self, v: V) -> Result<Self::Value, V::Error>
            where
                V: de::SeqAccess<'de>,
            {
                let seq = de::value::SeqAccessDeserializer::new(v);
                Vec::deserialize(seq).map(StringOrVec)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum StringOrBool {
    String(String),
    Bool(bool),
}

impl StringOrBool {
    fn string_or_default(&self, default_value: &str) -> Option<String> {
        match self {
            Self::String(value) => Some(String::from(value)),
            Self::Bool(true) => Some(String::from(default_value)),
            Self::Bool(false) => None,
        }
    }
}

impl<'de> de::Deserialize<'de> for StringOrBool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = StringOrBool;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a boolean or a string")
            }

            fn visit_bool<E>(self, b: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringOrBool::Bool(b))
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StringOrBool::String(s.to_string()))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(PartialEq, Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum VecStringOrBool {
    VecString(Vec<String>),
    Bool(bool),
}

impl<'de> de::Deserialize<'de> for VecStringOrBool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = VecStringOrBool;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a boolean or vector of strings")
            }

            fn visit_seq<V>(self, v: V) -> Result<Self::Value, V::Error>
            where
                V: de::SeqAccess<'de>,
            {
                let seq = de::value::SeqAccessDeserializer::new(v);
                Vec::deserialize(seq).map(VecStringOrBool::VecString)
            }

            fn visit_bool<E>(self, b: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(VecStringOrBool::Bool(b))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(untagged)]
pub enum MaybeWorkspace<T> {
    Workspace,
    Defined(T),
}

impl<'de, T> de::Deserialize<'de> for MaybeWorkspace<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct MaybeWorkspaceVisitor<T> {
            marker: PhantomData<fn() -> MaybeWorkspace<T>>,
        }

        impl<'de, T> de::Visitor<'de> for MaybeWorkspaceVisitor<T>
        where
            T: de::Deserialize<'de>,
        {
            type Value = MaybeWorkspace<T>;

            /// The `visit_foo` methods should cover all the possibilities, so we should in theory
            /// never fallback to this error message.
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("{ workspace: true } or a valid value")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                T::deserialize(v.into_deserializer()).map(MaybeWorkspace::Defined)
            }

            fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                T::deserialize(s.into_deserializer()).map(MaybeWorkspace::Defined)
            }

            fn visit_i64<E>(self, numeric: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                T::deserialize(numeric.into_deserializer()).map(MaybeWorkspace::Defined)
            }

            fn visit_f64<E>(self, numeric: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                T::deserialize(numeric.into_deserializer()).map(MaybeWorkspace::Defined)
            }

            fn visit_u64<E>(self, numeric: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                T::deserialize(numeric.into_deserializer()).map(MaybeWorkspace::Defined)
            }

            fn visit_seq<V>(self, seq: V) -> Result<Self::Value, V::Error>
            where
                V: de::SeqAccess<'de>,
            {
                let svd = de::value::SeqAccessDeserializer::new(seq);
                T::deserialize(svd).map(MaybeWorkspace::Defined)
            }

            fn visit_map<V>(self, map: V) -> Result<Self::Value, V::Error>
            where
                V: de::MapAccess<'de>,
            {
                let mvd = de::value::MapAccessDeserializer::new(map);
                TomlWorkspaceField::deserialize(mvd).and_then(|t| {
                    if t.workspace {
                        Ok(MaybeWorkspace::Workspace)
                    } else {
                        Err(de::Error::custom("workspace cannot be false"))
                    }
                })
            }
        }

        deserializer.deserialize_any(MaybeWorkspaceVisitor {
            marker: PhantomData,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MaybeWorkspaceBadge {
    Workspace(TomlWorkspaceField),
    Defined(BTreeMap<String, BTreeMap<String, String>>),
}

/// This exists only to provide a nicer error message.
fn deserialize_workspace_badges<'de, D>(
    deserializer: D,
) -> Result<Option<MaybeWorkspace<BTreeMap<String, BTreeMap<String, String>>>>, D::Error>
where
    D: de::Deserializer<'de>,
{
    match Option::deserialize(deserializer) {
        Ok(None) => Ok(None),
        Ok(Some(MaybeWorkspaceBadge::Defined(badges))) => Ok(Some(MaybeWorkspace::Defined(badges))),
        Ok(Some(MaybeWorkspaceBadge::Workspace(ws))) if ws.workspace => {
            Ok(Some(MaybeWorkspace::Workspace))
        }
        Ok(Some(MaybeWorkspaceBadge::Workspace(_))) => {
            Err(de::Error::custom("workspace cannot be false"))
        }

        Err(_) => Err(de::Error::custom(
            "expected a table of badges or { workspace = true }",
        )),
    }
}

#[derive(Deserialize, Serialize, Debug)]
struct TomlWorkspaceField {
    workspace: bool,
}

impl<T> MaybeWorkspace<T>
where
    T: std::fmt::Debug + Clone,
{
    fn from_option(value: &Option<T>) -> Option<Self> {
        match value {
            Some(value) => Some(Self::Defined(value.clone())),
            None => None,
        }
    }
}

/// Parses an optional field, defaulting to the workspace's value.
fn ws_default<T, F>(
    value: Option<MaybeWorkspace<T>>,
    workspace: Option<&TomlWorkspace>,
    f: F,
    label: &str,
) -> CargoResult<Option<T>>
where
    T: std::fmt::Debug + Clone,
    F: FnOnce(&TomlWorkspace) -> &Option<T>,
{
    match (value, workspace) {
        (None, _) => Ok(None),
        (Some(MaybeWorkspace::Defined(value)), _) => Ok(Some(value)),
        (Some(MaybeWorkspace::Workspace), Some(ws)) => f(ws)
            .clone()
            .ok_or_else(|| {
                anyhow!(
                    "error reading {0}: workspace root does not define [workspace.{0}]",
                    label
                )
            })
            .map(|value| Some(value)),

        (Some(MaybeWorkspace::Workspace), None) => Err(anyhow!(
            "error reading {}: could not read workspace root",
            label
        )),
    }
}

/// Represents the `package`/`project` sections of a `Cargo.toml`.
///
/// Note that the order of the fields matters, since this is the order they
/// are serialized to a TOML file. For example, you cannot have values after
/// the field `metadata`, since it is a table and values cannot appear after
/// tables.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct TomlProject {
    edition: Option<MaybeWorkspace<String>>,
    name: InternedString,
    version: MaybeWorkspace<semver::Version>,
    authors: Option<MaybeWorkspace<Vec<String>>>,
    build: Option<StringOrBool>,
    metabuild: Option<StringOrVec>,
    links: Option<String>,
    exclude: Option<Vec<String>>,
    include: Option<Vec<String>>,
    publish: Option<MaybeWorkspace<VecStringOrBool>>,
    #[serde(rename = "publish-lockfile")]
    publish_lockfile: Option<bool>,
    workspace: Option<String>,
    #[serde(rename = "im-a-teapot")]
    im_a_teapot: Option<bool>,
    autobins: Option<bool>,
    autoexamples: Option<bool>,
    autotests: Option<bool>,
    autobenches: Option<bool>,
    #[serde(rename = "namespaced-features")]
    namespaced_features: Option<bool>,
    #[serde(rename = "default-run")]
    default_run: Option<String>,

    // Package metadata.
    description: Option<MaybeWorkspace<String>>,
    homepage: Option<MaybeWorkspace<String>>,
    documentation: Option<MaybeWorkspace<String>>,
    readme: Option<MaybeWorkspace<StringOrBool>>,
    keywords: Option<MaybeWorkspace<Vec<String>>>,
    categories: Option<MaybeWorkspace<Vec<String>>>,
    license: Option<MaybeWorkspace<String>>,
    #[serde(rename = "license-file")]
    license_file: Option<MaybeWorkspace<String>>,
    repository: Option<MaybeWorkspace<String>>,
    metadata: Option<toml::Value>,
    resolver: Option<String>,
}

#[derive(Clone, Debug)]
struct DefinedTomlPackage {
    edition: Option<String>,
    name: InternedString,
    version: semver::Version,
    authors: Option<Vec<String>>,
    build: Option<StringOrBool>,
    metabuild: Option<StringOrVec>,
    links: Option<String>,
    exclude: Option<Vec<String>>,
    include: Option<Vec<String>>,
    publish: Option<VecStringOrBool>,
    publish_lockfile: Option<bool>,
    pub workspace: Option<String>,
    im_a_teapot: Option<bool>,
    autobins: Option<bool>,
    autoexamples: Option<bool>,
    autotests: Option<bool>,
    autobenches: Option<bool>,
    namespaced_features: Option<bool>,
    default_run: Option<String>,

    // Package metadata.
    description: Option<String>,
    homepage: Option<String>,
    documentation: Option<String>,
    readme: Option<String>,
    keywords: Option<Vec<String>>,
    categories: Option<Vec<String>>,
    license: Option<String>,
    license_file: Option<String>,
    repository: Option<String>,
    metadata: Option<toml::Value>,
    resolver: Option<String>,
}

impl DefinedTomlPackage {
    fn from_toml_project(
        project: TomlProject,
        ws: Option<&TomlWorkspace>,
        root_path: Option<&Path>,
        package_root: &Path,
    ) -> CargoResult<Self> {
        let version = ws_default(Some(project.version), ws, |ws| &ws.version, "version")?
            .ok_or_else(|| anyhow!("no version specified"))?;
        let edition = ws_default(project.edition, ws, |ws| &ws.edition, "edition")?;
        let authors = ws_default(project.authors, ws, |ws| &ws.authors, "authors")?;
        let publish = ws_default(project.publish, ws, |ws| &ws.publish, "publish")?;
        let description = ws_default(project.description, ws, |ws| &ws.description, "description")?;
        let homepage = ws_default(project.homepage, ws, |ws| &ws.homepage, "homepage")?;
        let documentation = ws_default(
            project.documentation,
            ws,
            |ws| &ws.documentation,
            "documentation",
        )?;

        let readme = match (project.readme, ws.and_then(|ws| ws.readme.as_ref())) {
            (None, _) => default_readme_from_package_root(package_root),
            (Some(MaybeWorkspace::Defined(defined)), _) => defined.string_or_default("README.md"),
            (Some(MaybeWorkspace::Workspace), None) => {
                bail!("error reading readme: workspace root does not defined [workspace.readme]")
            }
            (Some(MaybeWorkspace::Workspace), Some(defined)) => {
                match defined.string_or_default("README.md") {
                    Some(ws_readme) => Some(join_relative_path(root_path, &ws_readme)?),
                    None => None,
                }
            }
        };

        let license_file = match (
            project.license_file,
            ws.and_then(|ws| ws.license_file.as_ref()),
        ) {
            (None, _) => None,
            (Some(MaybeWorkspace::Defined(defined)), _) => Some(defined),
            (Some(MaybeWorkspace::Workspace), None) => {
                bail!("error reading license-file: workspace root does not defined [workspace.license-file]");
            }
            (Some(MaybeWorkspace::Workspace), Some(ws_license_file)) => {
                Some(join_relative_path(root_path, ws_license_file)?)
            }
        };

        let keywords = ws_default(project.keywords, ws, |ws| &ws.keywords, "keywords")?;
        let categories = ws_default(project.categories, ws, |ws| &ws.categories, "categories")?;
        let license = ws_default(project.license, ws, |ws| &ws.license, "license")?;
        let repository = ws_default(project.repository, ws, |ws| &ws.repository, "repository")?;

        Ok(Self {
            version,
            edition,
            name: project.name,
            authors,
            build: project.build,
            metabuild: project.metabuild,
            links: project.links,
            exclude: project.exclude,
            include: project.include,
            publish,
            publish_lockfile: project.publish_lockfile,
            workspace: project.workspace,
            im_a_teapot: project.im_a_teapot,
            autobins: project.autobins,
            autoexamples: project.autoexamples,
            autotests: project.autotests,
            autobenches: project.autobenches,
            namespaced_features: project.namespaced_features,
            default_run: project.default_run,

            // Package metadata.
            description,
            homepage,
            documentation,
            readme,
            keywords,
            categories,
            license,
            license_file,
            repository,
            metadata: project.metadata,
            resolver: project.resolver,
        })
    }
}

fn join_relative_path(root_path: Option<&Path>, relative_path: &str) -> CargoResult<String> {
    root_path
        .unwrap()
        .parent()
        .unwrap()
        .join(relative_path)
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow!("could not convert path into `String`"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TomlWorkspace {
    pub members: Option<Vec<String>>,
    #[serde(rename = "default-members")]
    pub default_members: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub metadata: Option<toml::Value>,
    resolver: Option<String>,

    // Properties that can be inherited by members.
    pub dependencies: Option<BTreeMap<String, DefinedTomlDependency>>,
    pub version: Option<semver::Version>,
    pub authors: Option<Vec<String>>,
    pub description: Option<String>,
    pub documentation: Option<String>,
    pub readme: Option<StringOrBool>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    #[serde(rename = "license-file")]
    pub license_file: Option<String>,
    pub keywords: Option<Vec<String>>,
    pub categories: Option<Vec<String>>,
    pub publish: Option<VecStringOrBool>,
    pub edition: Option<String>,
    pub badges: Option<BTreeMap<String, BTreeMap<String, String>>>,
}

struct Context<'a, 'b> {
    pkgid: Option<PackageId>,
    deps: &'a mut Vec<Dependency>,
    source_id: SourceId,
    nested_paths: &'a mut Vec<PathBuf>,
    config: &'b Config,
    warnings: &'a mut Vec<String>,
    platform: Option<Platform>,
    root: &'a Path,
    features: &'a Features,
}

impl DefinedTomlManifest {
    pub fn prepare_for_publish(
        &self,
        ws: &Workspace<'_>,
        manifest_file: &Path,
    ) -> CargoResult<DefinedTomlManifest> {
        let package_root = manifest_file.parent().unwrap();
        let config = ws.config();
        let mut package = self.package.clone().unwrap();

        package.workspace = None;
        let mut cargo_features = self.cargo_features.clone();
        package.resolver = ws.resolve_behavior().to_manifest();
        if package.resolver.is_some() {
            // This should be removed when stabilizing.
            match &mut cargo_features {
                None => cargo_features = Some(vec!["resolver".to_string()]),
                Some(feats) => {
                    if !feats.iter().any(|feat| feat == "resolver") {
                        feats.push("resolver".to_string());
                    }
                }
            }
        }

        if let Some(license_file) = &package.license_file {
            let license_path = Path::new(&license_file);
            let abs_license_path = paths::normalize_path(&package_root.join(license_path));
            if abs_license_path.strip_prefix(package_root).is_err() {
                // This path points outside of the package root. `cargo package`
                // will copy it into the root, so adjust the path to this location.
                package.license_file = Some(
                    license_path
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                );
            }
        }
        let all = |_d: &DefinedTomlDependency| true;
        Ok(DefinedTomlManifest {
            package: Some(package),
            profile: self.profile.clone(),
            lib: self.lib.clone(),
            bin: self.bin.clone(),
            example: self.example.clone(),
            test: self.test.clone(),
            bench: self.bench.clone(),
            dependencies: map_deps(config, self.dependencies.as_ref(), all)?,
            dev_dependencies: map_deps(
                config,
                self.dev_dependencies.as_ref(),
                DefinedTomlDependency::is_version_specified,
            )?,
            build_dependencies: map_deps(config, self.build_dependencies.as_ref(), all)?,
            features: self.features.clone(),
            target: match self.target.as_ref().map(|target_map| {
                target_map
                    .iter()
                    .map(|(k, v)| {
                        Ok((
                            k.clone(),
                            DefinedTomlPlatform {
                                dependencies: map_deps(config, v.dependencies.as_ref(), all)?,
                                dev_dependencies: map_deps(
                                    config,
                                    v.dev_dependencies
                                        .as_ref()
                                        .or_else(|| v.dev_dependencies2.as_ref()),
                                    DefinedTomlDependency::is_version_specified,
                                )?,
                                dev_dependencies2: None,
                                build_dependencies: map_deps(
                                    config,
                                    v.build_dependencies
                                        .as_ref()
                                        .or_else(|| v.build_dependencies2.as_ref()),
                                    all,
                                )?,
                                build_dependencies2: None,
                            },
                        ))
                    })
                    .collect()
            }) {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => return Err(e),
                None => None,
            },
            replace: None,
            patch: None,
            workspace: None,
            badges: self.badges.clone(),
            cargo_features,
        })
    }

    pub fn into_real_manifest(
        self,
        source_id: SourceId,
        manifest_file: &Path,
        config: &Config,
    ) -> CargoResult<(Manifest, Vec<PathBuf>)> {
        let me = Rc::new(self);
        let package_root = manifest_file.parent().unwrap();
        let mut nested_paths = vec![];
        let mut warnings = vec![];
        let mut errors = vec![];

        // Parse features first so they will be available when parsing other parts of the TOML.
        let empty = Vec::new();
        let cargo_features = me.cargo_features.as_ref().unwrap_or(&empty);
        let features = Features::new(&cargo_features, &mut warnings)?;

        let project = me.package.as_ref().unwrap();

        let package_name = project.name.trim();
        if package_name.is_empty() {
            bail!("package name cannot be an empty string")
        }

        validate_package_name(package_name, "package name", "")?;

        let pkgid = PackageId::new(project.name, &project.version, source_id)?;

        let edition = if let Some(ref edition) = project.edition {
            features
                .require(Feature::edition())
                .chain_err(|| "editions are unstable")?;
            edition
                .parse()
                .chain_err(|| "failed to parse the `edition` key")?
        } else {
            Edition::Edition2015
        };

        if project.metabuild.is_some() {
            features.require(Feature::metabuild())?;
        }

        if project.resolver.is_some()
            || me
                .workspace
                .as_ref()
                .map_or(false, |ws| ws.resolver.is_some())
        {
            features.require(Feature::resolver())?;
        }

        let resolve_behavior = match (
            project.resolver.as_ref(),
            me.workspace.as_ref().and_then(|ws| ws.resolver.as_ref()),
        ) {
            (None, None) => None,
            (Some(s), None) | (None, Some(s)) => Some(ResolveBehavior::from_manifest(s)?),
            (Some(_), Some(_)) => {
                bail!("cannot specify `resolver` field in both `[workspace]` and `[package]`")
            }
        };

        // If we have no lib at all, use the inferred lib, if available.
        // If we have a lib with a path, we're done.
        // If we have a lib with no path, use the inferred lib or else the package name.
        let targets = targets(
            &features,
            &me,
            package_name,
            package_root,
            edition,
            &project.build,
            &project.metabuild,
            &mut warnings,
            &mut errors,
        )?;

        if targets.is_empty() {
            debug!("manifest has no build targets");
        }

        if let Err(e) = unique_build_targets(&targets, package_root) {
            warnings.push(format!(
                "file found to be present in multiple \
                 build targets: {}",
                e
            ));
        }

        if let Some(links) = &project.links {
            if !targets.iter().any(|t| t.is_custom_build()) {
                bail!(
                    "package `{}` specifies that it links to `{}` but does not \
                     have a custom build script",
                    pkgid,
                    links
                )
            }
        }

        let mut deps = Vec::new();
        let replace;
        let patch;

        {
            let mut cx = Context {
                pkgid: Some(pkgid),
                deps: &mut deps,
                source_id,
                nested_paths: &mut nested_paths,
                config,
                warnings: &mut warnings,
                features: &features,
                platform: None,
                root: package_root,
            };

            fn process_dependencies(
                cx: &mut Context<'_, '_>,
                new_deps: Option<&BTreeMap<String, DefinedTomlDependency>>,
                kind: Option<DepKind>,
            ) -> CargoResult<()> {
                let dependencies = match new_deps {
                    Some(dependencies) => dependencies,
                    None => return Ok(()),
                };
                for (n, v) in dependencies.iter() {
                    let dep = v.to_dependency(n, cx, kind)?;
                    validate_package_name(dep.name_in_toml().as_str(), "dependency name", "")?;
                    cx.deps.push(dep);
                }

                Ok(())
            }

            // Collect the workspace's [workspace.dependencies], if any.
            let output = find_workspace_root(manifest_file, config)?
                .map(|root_path| parse_manifest(&root_path, config))
                .transpose()?;

            let workspace = output.as_ref().and_then(|ws| ws.workspace());

            process_dependencies(
                &mut cx,
                workspace.and_then(|ws| ws.dependencies.as_ref()),
                None,
            )?;

            // Collect the dependencies.
            process_dependencies(&mut cx, me.dependencies.as_ref(), None)?;
            let dev_deps = me.dev_dependencies.as_ref();
            process_dependencies(&mut cx, dev_deps, Some(DepKind::Development))?;
            let build_deps = me.build_dependencies.as_ref();
            process_dependencies(&mut cx, build_deps, Some(DepKind::Build))?;

            for (name, platform) in me.target.iter().flatten() {
                cx.platform = {
                    let platform: Platform = name.parse()?;
                    platform.check_cfg_attributes(&mut cx.warnings);
                    Some(platform)
                };
                process_dependencies(&mut cx, platform.dependencies.as_ref(), None)?;
                let build_deps = platform
                    .build_dependencies
                    .as_ref()
                    .or_else(|| platform.build_dependencies2.as_ref());
                process_dependencies(&mut cx, build_deps, Some(DepKind::Build))?;
                let dev_deps = platform
                    .dev_dependencies
                    .as_ref()
                    .or_else(|| platform.dev_dependencies2.as_ref());
                process_dependencies(&mut cx, dev_deps, Some(DepKind::Development))?;
            }

            replace = me.replace(&mut cx)?;
            patch = me.patch(&mut cx)?;
        }

        {
            let mut names_sources = BTreeMap::new();
            for dep in &deps {
                let name = dep.name_in_toml();
                let prev = names_sources.insert(name.to_string(), dep.source_id());
                if prev.is_some() && prev != Some(dep.source_id()) {
                    bail!(
                        "Dependency '{}' has different source paths depending on the build \
                         target. Each dependency must have a single canonical source path \
                         irrespective of build target.",
                        name
                    );
                }
            }
        }

        let exclude = project.exclude.clone().unwrap_or_default();
        let include = project.include.clone().unwrap_or_default();
        if project.namespaced_features.is_some() {
            features.require(Feature::namespaced_features())?;
        }

        let summary_features = me
            .features
            .as_ref()
            .map(|x| {
                x.iter()
                    .map(|(k, v)| (k.as_str(), v.iter().collect()))
                    .collect()
            })
            .unwrap_or_else(BTreeMap::new);

        let summary = Summary::new(
            pkgid,
            deps,
            &summary_features,
            project.links.as_deref(),
            project.namespaced_features.unwrap_or(false),
        )?;

        let metadata = ManifestMetadata {
            description: project.description.clone(),
            homepage: project.homepage.clone(),
            documentation: project.documentation.clone(),
            readme: project.readme.clone(),
            authors: project.authors.clone().unwrap_or_default(),
            license: project.license.clone(),
            license_file: project.license_file.clone(),
            repository: project.repository.clone(),
            keywords: project.keywords.clone().unwrap_or_default(),
            categories: project.categories.clone().unwrap_or_default(),
            badges: me.badges.clone().unwrap_or_default(),
            links: project.links.clone(),
        };

        let workspace_config = me.workspace_config(package_root, &config)?;

        let profiles = me.profile.clone();
        if let Some(profiles) = &profiles {
            profiles.validate(&features, &mut warnings)?;
        }

        let publish = match project.publish {
            Some(VecStringOrBool::VecString(ref vecstring)) => Some(vecstring.clone()),
            Some(VecStringOrBool::Bool(false)) => Some(vec![]),
            None | Some(VecStringOrBool::Bool(true)) => None,
        };

        let publish_lockfile = match project.publish_lockfile {
            Some(b) => {
                features.require(Feature::publish_lockfile())?;
                warnings.push(
                    "The `publish-lockfile` feature is deprecated and currently \
                     has no effect. It may be removed in a future version."
                        .to_string(),
                );
                b
            }
            None => features.is_enabled(Feature::publish_lockfile()),
        };

        if summary.features().contains_key("default-features") {
            warnings.push(
                "`default-features = [\"..\"]` was found in [features]. \
                 Did you mean to use `default = [\"..\"]`?"
                    .to_string(),
            )
        }

        if let Some(run) = &project.default_run {
            if !targets
                .iter()
                .filter(|t| t.is_bin())
                .any(|t| t.name() == run)
            {
                let suggestion =
                    util::closest_msg(run, targets.iter().filter(|t| t.is_bin()), |t| t.name());
                bail!("default-run target `{}` not found{}", run, suggestion);
            }
        }

        let custom_metadata = project.metadata.clone();
        let mut manifest = Manifest::new(
            summary,
            targets,
            exclude,
            include,
            project.links.clone(),
            metadata,
            custom_metadata,
            profiles,
            publish,
            publish_lockfile,
            replace,
            patch,
            Rc::new(workspace_config),
            features,
            edition,
            project.im_a_teapot,
            project.default_run.clone(),
            Rc::clone(&me),
            project.metabuild.clone().map(|sov| sov.0),
            resolve_behavior,
        );
        if project.license_file.is_some() && project.license.is_some() {
            manifest.warnings_mut().add_warning(
                "only one of `license` or \
                 `license-file` is necessary"
                    .to_string(),
            );
        }
        for warning in warnings {
            manifest.warnings_mut().add_warning(warning);
        }
        for error in errors {
            manifest.warnings_mut().add_critical_warning(error);
        }

        manifest.feature_gate()?;

        Ok((manifest, nested_paths))
    }

    fn into_virtual_manifest(
        self,
        source_id: SourceId,
        manifest_file: &Path,
        config: &Config,
    ) -> CargoResult<(VirtualManifest, Vec<PathBuf>)> {
        let root = manifest_file.parent().unwrap();
        if self.package.is_some() {
            bail!("this virtual manifest specifies a [project] section, which is not allowed");
        }
        if self.lib.is_some() {
            bail!("this virtual manifest specifies a [lib] section, which is not allowed");
        }
        if self.bin.is_some() {
            bail!("this virtual manifest specifies a [[bin]] section, which is not allowed");
        }
        if self.example.is_some() {
            bail!("this virtual manifest specifies a [[example]] section, which is not allowed");
        }
        if self.test.is_some() {
            bail!("this virtual manifest specifies a [[test]] section, which is not allowed");
        }
        if self.bench.is_some() {
            bail!("this virtual manifest specifies a [[bench]] section, which is not allowed");
        }
        if self.dependencies.is_some() {
            bail!("this virtual manifest specifies a [dependencies] section, which is not allowed");
        }
        if self.dev_dependencies.is_some() {
            bail!("this virtual manifest specifies a [dev-dependencies] section, which is not allowed");
        }
        if self.build_dependencies.is_some() {
            bail!("this virtual manifest specifies a [build-dependencies] section, which is not allowed");
        }
        if self.features.is_some() {
            bail!("this virtual manifest specifies a [features] section, which is not allowed");
        }
        if self.target.is_some() {
            bail!("this virtual manifest specifies a [target] section, which is not allowed");
        }
        if self.badges.is_some() {
            bail!("this virtual manifest specifies a [badges] section, which is not allowed");
        }

        let mut nested_paths = Vec::new();
        let mut warnings = Vec::new();
        let mut deps = Vec::new();
        let empty = Vec::new();
        let cargo_features = self.cargo_features.as_ref().unwrap_or(&empty);
        let features = Features::new(cargo_features, &mut warnings)?;

        let (replace, patch) = {
            let mut cx = Context {
                pkgid: None,
                deps: &mut deps,
                source_id,
                nested_paths: &mut nested_paths,
                config,
                warnings: &mut warnings,
                platform: None,
                features: &features,
                root,
            };
            (self.replace(&mut cx)?, self.patch(&mut cx)?)
        };
        let profiles = self.profile.clone();
        if let Some(profiles) = &profiles {
            profiles.validate(&features, &mut warnings)?;
        }
        if self
            .workspace
            .as_ref()
            .map_or(false, |ws| ws.resolver.is_some())
        {
            features.require(Feature::resolver())?;
        }
        let resolve_behavior = self
            .workspace
            .as_ref()
            .and_then(|ws| ws.resolver.as_deref())
            .map(|r| ResolveBehavior::from_manifest(r))
            .transpose()?;

        let workspace_config = self.workspace_config(root, config)?;
        if !workspace_config.is_root() {
            bail!("virtual manifests must be configured with [workspace]");
        }

        Ok((
            VirtualManifest::new(
                replace,
                patch,
                Rc::new(workspace_config),
                profiles,
                features,
                resolve_behavior,
            ),
            nested_paths,
        ))
    }

    pub fn into_toml_manifest(self) -> TomlManifest {
        let project = self.package.unwrap();

        TomlManifest {
            cargo_features: self.cargo_features,
            package: Some(Box::new(TomlProject {
                edition: MaybeWorkspace::from_option(&project.edition),
                name: project.name,
                version: MaybeWorkspace::Defined(project.version),
                authors: MaybeWorkspace::from_option(&project.authors),
                build: project.build,
                metabuild: project.metabuild,
                links: project.links,
                exclude: project.exclude,
                include: project.include,
                publish: MaybeWorkspace::from_option(&project.publish),
                publish_lockfile: project.publish_lockfile,
                workspace: None,
                im_a_teapot: project.im_a_teapot,
                autobins: project.autobins,
                autoexamples: project.autoexamples,
                autotests: project.autotests,
                autobenches: project.autobenches,
                namespaced_features: project.namespaced_features,
                default_run: project.default_run,

                description: MaybeWorkspace::from_option(&project.description),
                homepage: MaybeWorkspace::from_option(&project.homepage),
                documentation: MaybeWorkspace::from_option(&project.documentation),
                readme: MaybeWorkspace::from_option(&project.readme.map(StringOrBool::String)),
                keywords: MaybeWorkspace::from_option(&project.keywords),
                categories: MaybeWorkspace::from_option(&project.categories),
                license: MaybeWorkspace::from_option(&project.license),
                license_file: MaybeWorkspace::from_option(&project.license_file),
                repository: MaybeWorkspace::from_option(&project.repository),
                metadata: project.metadata,
                resolver: project.resolver,
            })),
            project: None,
            profile: self.profile,
            lib: self.lib,
            bin: self.bin,
            example: self.example,
            test: self.test,
            bench: self.bench,
            dependencies: to_toml_dependencies(self.dependencies.as_ref()),
            dev_dependencies: to_toml_dependencies(self.dev_dependencies.as_ref()),
            dev_dependencies2: None,
            build_dependencies: to_toml_dependencies(self.build_dependencies.as_ref()),
            build_dependencies2: None,
            features: self.features,
            target: to_toml_platform(self.target),
            replace: self.replace,
            patch: self.patch,
            workspace: self.workspace,
            badges: MaybeWorkspace::from_option(&self.badges),
        }
    }

    pub fn workspace_config(
        &self,
        package_root: &Path,
        config: &Config,
    ) -> CargoResult<WorkspaceConfig> {
        let workspace = self.workspace.as_ref();
        let project_workspace = self.package.as_ref().and_then(|p| p.workspace.as_ref());

        Ok(match (workspace, project_workspace) {
            (Some(toml_workspace), None) => WorkspaceConfig::Root(
                WorkspaceRootConfig::from_toml_workspace(package_root, &config, toml_workspace)?,
            ),
            (None, root) => WorkspaceConfig::Member {
                root: root.cloned(),
            },
            (Some(..), Some(..)) => bail!(
                "cannot configure both `package.workspace` and \
                 `[workspace]`, only one can be specified"
            ),
        })
    }

    fn replace(&self, cx: &mut Context<'_, '_>) -> CargoResult<Vec<(PackageIdSpec, Dependency)>> {
        if self.patch.is_some() && self.replace.is_some() {
            bail!("cannot specify both [replace] and [patch]");
        }
        let mut replace = Vec::new();
        for (spec, replacement) in self.replace.iter().flatten() {
            let mut spec = PackageIdSpec::parse(spec).chain_err(|| {
                format!(
                    "replacements must specify a valid semver \
                     version to replace, but `{}` does not",
                    spec
                )
            })?;
            if spec.url().is_none() {
                spec.set_url(CRATES_IO_INDEX.parse().unwrap());
            }

            if replacement.is_version_specified() {
                bail!(
                    "replacements cannot specify a version \
                     requirement, but found one for `{}`",
                    spec
                );
            }

            let mut dep = replacement.to_dependency(spec.name().as_str(), cx, None)?;
            {
                let version = spec.version().ok_or_else(|| {
                    anyhow!(
                        "replacements must specify a version \
                         to replace, but `{}` does not",
                        spec
                    )
                })?;
                dep.set_version_req(VersionReq::exact(version));
            }
            replace.push((spec, dep));
        }
        Ok(replace)
    }

    fn patch(&self, cx: &mut Context<'_, '_>) -> CargoResult<HashMap<Url, Vec<Dependency>>> {
        let mut patch = HashMap::new();
        for (url, deps) in self.patch.iter().flatten() {
            let url = match &url[..] {
                CRATES_IO_REGISTRY => CRATES_IO_INDEX.parse().unwrap(),
                _ => cx
                    .config
                    .get_registry_index(url)
                    .or_else(|_| url.into_url())
                    .chain_err(|| {
                        format!("[patch] entry `{}` should be a URL or registry name", url)
                    })?,
            };
            patch.insert(
                url,
                deps.iter()
                    .map(|(name, dep)| dep.to_dependency(name, cx, None))
                    .collect::<CargoResult<Vec<_>>>()?,
            );
        }
        Ok(patch)
    }

    fn maybe_custom_build(
        &self,
        build: &Option<StringOrBool>,
        package_root: &Path,
    ) -> Option<PathBuf> {
        let build_rs = package_root.join("build.rs");
        match *build {
            // Explicitly no build script.
            Some(StringOrBool::Bool(false)) => None,
            Some(StringOrBool::Bool(true)) => Some(build_rs),
            Some(StringOrBool::String(ref s)) => Some(PathBuf::from(s)),
            None => {
                // If there is a `build.rs` file next to the `Cargo.toml`, assume it is
                // a build script.
                if build_rs.is_file() {
                    Some(build_rs)
                } else {
                    None
                }
            }
        }
    }

    pub fn has_profiles(&self) -> bool {
        self.profile.is_some()
    }
}

const DEFAULT_README_FILES: [&str; 3] = ["README.md", "README.txt", "README"];

/// Checks if a file with any of the default README file names exists in the package root.
/// If so, returns a `String` representing that name.
fn default_readme_from_package_root(package_root: &Path) -> Option<String> {
    for &readme_filename in DEFAULT_README_FILES.iter() {
        if package_root.join(readme_filename).is_file() {
            return Some(readme_filename.to_string());
        }
    }

    None
}

/// Checks a list of build targets, and ensures the target names are unique within a vector.
/// If not, the name of the offending build target is returned.
fn unique_build_targets(targets: &[Target], package_root: &Path) -> Result<(), String> {
    let mut seen = HashSet::new();
    for target in targets {
        if let TargetSourcePath::Path(path) = target.src_path() {
            let full = package_root.join(path);
            if !seen.insert(full.clone()) {
                return Err(full.display().to_string());
            }
        }
    }
    Ok(())
}

impl TomlDependency {
    fn from_defined_dependency(dep: &DefinedTomlDependency) -> Self {
        match dep {
            DefinedTomlDependency::Simple(s) => Self::Simple(s.clone()),
            DefinedTomlDependency::Detailed(detailed) => Self::Detailed(detailed.clone()),
        }
    }
}

impl DefinedTomlDependency {
    fn from_toml_dependency(
        dep: &TomlDependency,
        name: &str,
        ws_deps: &BTreeMap<String, Self>,
        root_path: Option<&Path>,
    ) -> CargoResult<Self> {
        match dep {
            TomlDependency::Simple(s) => Ok(Self::Simple(s.clone())),
            TomlDependency::Detailed(detailed) => Ok(Self::Detailed(detailed.clone())),
            TomlDependency::Workspace(ws) => {
                let ws_dep = ws_deps.get(name).ok_or_else(|| {
                    anyhow!(
                        "could not find entry in [workspace.dependencies] for \"{}\"",
                        name
                    )
                })?;

                Ok(Self::from_workspace_dependency(ws, ws_dep, root_path)?)
            }
        }
    }

    fn from_workspace_dependency(
        details: &WorkspaceDetails,
        ws_dep: &Self,
        root_path: Option<&Path>,
    ) -> CargoResult<Self> {
        let details = match ws_dep {
            Self::Simple(s) => TomlDependencyDetails {
                version: Some(s.clone()),
                features: details
                    .features
                    .clone()
                    .or_else(|| ws_dep.features().cloned()),
                optional: details.optional.or_else(|| Some(ws_dep.is_optional())),
                ..Default::default()
            },

            Self::Detailed(d) => TomlDependencyDetails {
                version: d.version.clone(),
                registry: d.registry.clone(),
                registry_index: d.registry_index.clone(),
                path: d
                    .path
                    .clone()
                    .map(|p| join_relative_path(root_path, &p))
                    .transpose()?,
                git: d.git.clone(),
                branch: d.branch.clone(),
                tag: d.tag.clone(),
                rev: d.rev.clone(),
                features: match (&details.features, &d.features) {
                    (None, None) => None,
                    (Some(features), None) | (None, Some(features)) => Some(features.clone()),
                    (Some(ws_features), Some(features)) => {
                        let mut result = ws_features.clone();
                        for f in features {
                            if !result.contains(&f) {
                                result.push(f.clone());
                            }
                        }
                        Some(result)
                    }
                },
                optional: details.optional.or_else(|| d.optional.clone()),
                default_features: d.default_features.clone(),
                default_features2: d.default_features2.clone(),
                package: d.package.clone(),
                public: d.public.clone(),
            },
        };

        Ok(Self::Detailed(details))
    }
    fn to_dependency(
        &self,
        name: &str,
        cx: &mut Context<'_, '_>,
        kind: Option<DepKind>,
    ) -> CargoResult<Dependency> {
        match *self {
            DefinedTomlDependency::Simple(ref version) => TomlDependencyDetails {
                version: Some(version.clone()),
                ..Default::default()
            }
            .to_dependency(name, cx, kind),
            DefinedTomlDependency::Detailed(ref details) => details.to_dependency(name, cx, kind),
        }
    }

    fn is_version_specified(&self) -> bool {
        match self {
            DefinedTomlDependency::Detailed(d) => d.version.is_some(),
            DefinedTomlDependency::Simple(..) => true,
        }
    }

    fn is_optional(&self) -> bool {
        match self {
            DefinedTomlDependency::Detailed(d) => d.optional.unwrap_or(false),
            DefinedTomlDependency::Simple(..) => false,
        }
    }

    fn features(&self) -> Option<&Vec<String>> {
        match self {
            DefinedTomlDependency::Detailed(d) => d.features.as_ref(),
            DefinedTomlDependency::Simple(..) => None,
        }
    }
}

impl TomlDependencyDetails {
    fn to_dependency(
        &self,
        name_in_toml: &str,
        cx: &mut Context<'_, '_>,
        kind: Option<DepKind>,
    ) -> CargoResult<Dependency> {
        if self.version.is_none() && self.path.is_none() && self.git.is_none() {
            let msg = format!(
                "dependency ({}) specified without \
                 providing a local path, Git repository, or \
                 version to use. This will be considered an \
                 error in future versions",
                name_in_toml
            );
            cx.warnings.push(msg);
        }

        if let Some(version) = &self.version {
            if version.contains('+') {
                cx.warnings.push(format!(
                    "version requirement `{}` for dependency `{}` \
                     includes semver metadata which will be ignored, removing the \
                     metadata is recommended to avoid confusion",
                    version, name_in_toml
                ));
            }
        }

        if self.git.is_none() {
            let git_only_keys = [
                (&self.branch, "branch"),
                (&self.tag, "tag"),
                (&self.rev, "rev"),
            ];

            for &(key, key_name) in &git_only_keys {
                if key.is_some() {
                    let msg = format!(
                        "key `{}` is ignored for dependency ({}). \
                         This will be considered an error in future versions",
                        key_name, name_in_toml
                    );
                    cx.warnings.push(msg)
                }
            }
        }

        let new_source_id = match (
            self.git.as_ref(),
            self.path.as_ref(),
            self.registry.as_ref(),
            self.registry_index.as_ref(),
        ) {
            (Some(_), _, Some(_), _) | (Some(_), _, _, Some(_)) => bail!(
                "dependency ({}) specification is ambiguous. \
                 Only one of `git` or `registry` is allowed.",
                name_in_toml
            ),
            (_, _, Some(_), Some(_)) => bail!(
                "dependency ({}) specification is ambiguous. \
                 Only one of `registry` or `registry-index` is allowed.",
                name_in_toml
            ),
            (Some(git), maybe_path, _, _) => {
                if maybe_path.is_some() {
                    let msg = format!(
                        "dependency ({}) specification is ambiguous. \
                         Only one of `git` or `path` is allowed. \
                         This will be considered an error in future versions",
                        name_in_toml
                    );
                    cx.warnings.push(msg)
                }

                let n_details = [&self.branch, &self.tag, &self.rev]
                    .iter()
                    .filter(|d| d.is_some())
                    .count();

                if n_details > 1 {
                    let msg = format!(
                        "dependency ({}) specification is ambiguous. \
                         Only one of `branch`, `tag` or `rev` is allowed. \
                         This will be considered an error in future versions",
                        name_in_toml
                    );
                    cx.warnings.push(msg)
                }

                let reference = self
                    .branch
                    .clone()
                    .map(GitReference::Branch)
                    .or_else(|| self.tag.clone().map(GitReference::Tag))
                    .or_else(|| self.rev.clone().map(GitReference::Rev))
                    .unwrap_or_else(|| GitReference::DefaultBranch);
                let loc = git.into_url()?;

                if let Some(fragment) = loc.fragment() {
                    let msg = format!(
                        "URL fragment `#{}` in git URL is ignored for dependency ({}). \
                        If you were trying to specify a specific git revision, \
                        use `rev = \"{}\"` in the dependency declaration.",
                        fragment, name_in_toml, fragment
                    );
                    cx.warnings.push(msg)
                }

                SourceId::for_git(&loc, reference)?
            }
            (None, Some(path), _, _) => {
                cx.nested_paths.push(PathBuf::from(path));
                // If the source ID for the package we're parsing is a path
                // source, then we normalize the path here to get rid of
                // components like `..`.
                //
                // The purpose of this is to get a canonical ID for the package
                // that we're depending on to ensure that builds of this package
                // always end up hashing to the same value no matter where it's
                // built from.
                if cx.source_id.is_path() {
                    let path = cx.root.join(path);
                    let path = util::normalize_path(&path);
                    SourceId::for_path(&path)?
                } else {
                    cx.source_id
                }
            }
            (None, None, Some(registry), None) => SourceId::alt_registry(cx.config, registry)?,
            (None, None, None, Some(registry_index)) => {
                let url = registry_index.into_url()?;
                SourceId::for_registry(&url)?
            }
            (None, None, None, None) => SourceId::crates_io(cx.config)?,
        };

        let (pkg_name, explicit_name_in_toml) = match self.package {
            Some(ref s) => (&s[..], Some(name_in_toml)),
            None => (name_in_toml, None),
        };

        let version = self.version.as_deref();
        let mut dep = match cx.pkgid {
            Some(id) => Dependency::parse(pkg_name, version, new_source_id, id, cx.config)?,
            None => Dependency::parse_no_deprecated(pkg_name, version, new_source_id)?,
        };
        dep.set_features(self.features.iter().flatten())
            .set_default_features(
                self.default_features
                    .or(self.default_features2)
                    .unwrap_or(true),
            )
            .set_optional(self.optional.unwrap_or(false))
            .set_platform(cx.platform.clone());
        if let Some(registry) = &self.registry {
            let registry_id = SourceId::alt_registry(cx.config, registry)?;
            dep.set_registry_id(registry_id);
        }
        if let Some(registry_index) = &self.registry_index {
            let url = registry_index.into_url()?;
            let registry_id = SourceId::for_registry(&url)?;
            dep.set_registry_id(registry_id);
        }

        if let Some(kind) = kind {
            dep.set_kind(kind);
        }
        if let Some(name_in_toml) = explicit_name_in_toml {
            cx.features.require(Feature::rename_dependency())?;
            dep.set_explicit_name_in_toml(name_in_toml);
        }

        if let Some(p) = self.public {
            cx.features.require(Feature::public_dependency())?;

            if dep.kind() != DepKind::Normal {
                bail!("'public' specifier can only be used on regular dependencies, not {:?} dependencies", dep.kind());
            }

            dep.set_public(p);
        }
        Ok(dep)
    }
}

#[derive(Default, Serialize, Deserialize, Debug, Clone)]
struct TomlTarget {
    name: Option<String>,

    // The intention was to only accept `crate-type` here but historical
    // versions of Cargo also accepted `crate_type`, so look for both.
    #[serde(rename = "crate-type")]
    crate_type: Option<Vec<String>>,
    #[serde(rename = "crate_type")]
    crate_type2: Option<Vec<String>>,

    path: Option<PathValue>,
    test: Option<bool>,
    doctest: Option<bool>,
    bench: Option<bool>,
    doc: Option<bool>,
    plugin: Option<bool>,
    #[serde(rename = "proc-macro")]
    proc_macro_raw: Option<bool>,
    #[serde(rename = "proc_macro")]
    proc_macro_raw2: Option<bool>,
    harness: Option<bool>,
    #[serde(rename = "required-features")]
    required_features: Option<Vec<String>>,
    edition: Option<String>,
}

#[derive(Clone)]
struct PathValue(PathBuf);

impl<'de> de::Deserialize<'de> for PathValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        Ok(PathValue(String::deserialize(deserializer)?.into()))
    }
}

impl ser::Serialize for PathValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        self.0.serialize(serializer)
    }
}

/// Corresponds to a `target` entry, but `TomlTarget` is already used.
#[derive(Clone, Serialize, Deserialize, Debug)]
struct DefinedTomlPlatform {
    dependencies: Option<BTreeMap<String, DefinedTomlDependency>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<BTreeMap<String, DefinedTomlDependency>>,
    #[serde(rename = "build_dependencies")]
    build_dependencies2: Option<BTreeMap<String, DefinedTomlDependency>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<BTreeMap<String, DefinedTomlDependency>>,
    #[serde(rename = "dev_dependencies")]
    dev_dependencies2: Option<BTreeMap<String, DefinedTomlDependency>>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
struct TomlPlatform {
    dependencies: Option<BTreeMap<String, TomlDependency>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<BTreeMap<String, TomlDependency>>,
    #[serde(rename = "build_dependencies")]
    build_dependencies2: Option<BTreeMap<String, TomlDependency>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<BTreeMap<String, TomlDependency>>,
    #[serde(rename = "dev_dependencies")]
    dev_dependencies2: Option<BTreeMap<String, TomlDependency>>,
}

impl TomlPlatform {
    fn from_defined_platform(defined_platform: &DefinedTomlPlatform) -> Self {
        Self {
            dependencies: to_toml_dependencies(defined_platform.dependencies.as_ref()),
            build_dependencies: to_toml_dependencies(defined_platform.build_dependencies.as_ref()),
            build_dependencies2: None,
            dev_dependencies: to_toml_dependencies(defined_platform.dev_dependencies.as_ref()),
            dev_dependencies2: None,
        }
    }
}

impl DefinedTomlPlatform {
    fn from_toml_platform(
        toml_platform: &TomlPlatform,
        ws_deps: &BTreeMap<String, DefinedTomlDependency>,
        root_path: Option<&Path>,
    ) -> CargoResult<Self> {
        let build_dependencies = toml_platform
            .build_dependencies
            .as_ref()
            .or(toml_platform.build_dependencies2.as_ref());

        let dev_dependencies = toml_platform
            .dev_dependencies
            .as_ref()
            .or(toml_platform.dev_dependencies2.as_ref());

        Ok(Self {
            dependencies: to_defined_dependencies(
                toml_platform.dependencies.as_ref(),
                Some(ws_deps),
                root_path,
            )?,
            build_dependencies: to_defined_dependencies(
                build_dependencies,
                Some(ws_deps),
                root_path,
            )?,
            build_dependencies2: None,
            dev_dependencies: to_defined_dependencies(dev_dependencies, Some(ws_deps), root_path)?,
            dev_dependencies2: None,
        })
    }
}

impl TomlTarget {
    fn new() -> TomlTarget {
        TomlTarget::default()
    }

    fn name(&self) -> String {
        match self.name {
            Some(ref name) => name.clone(),
            None => panic!("target name is required"),
        }
    }

    fn proc_macro(&self) -> Option<bool> {
        self.proc_macro_raw.or(self.proc_macro_raw2).or_else(|| {
            if let Some(types) = self.crate_types() {
                if types.contains(&"proc-macro".to_string()) {
                    return Some(true);
                }
            }
            None
        })
    }

    fn crate_types(&self) -> Option<&Vec<String>> {
        self.crate_type
            .as_ref()
            .or_else(|| self.crate_type2.as_ref())
    }
}

impl fmt::Debug for PathValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
