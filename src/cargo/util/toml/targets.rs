//! This module implements Cargo conventions for directory layout:
//!
//!  * `src/lib.rs` is a library
//!  * `src/main.rs` is a binary
//!  * `src/bin/*.rs` are binaries
//!  * `examples/*.rs` are examples
//!  * `tests/*.rs` are integration tests
//!  * `benches/*.rs` are benchmarks
//!
//! It is a bit tricky because we need match explicit information from `Cargo.toml`
//! with implicit info in directory layout.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::fs::{self, DirEntry};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use cargo_util::paths;
use cargo_util_schemas::manifest::{
    PathValue, StringOrVec, TomlBenchTarget, TomlBinTarget, TomlExampleTarget, TomlLibTarget,
    TomlManifest, TomlPackageBuild, TomlTarget, TomlTestTarget,
};

use crate::core::compiler::{CrateType, rustdoc::RustdocScrapeExamples};
use crate::core::{Edition, Feature, Features, Target};
use crate::util::{
    closest_msg, errors::CargoResult, restricted_names, toml::deprecated_underscore,
};

const DEFAULT_TEST_DIR_NAME: &'static str = "tests";
const DEFAULT_BENCH_DIR_NAME: &'static str = "benches";
const DEFAULT_EXAMPLE_DIR_NAME: &'static str = "examples";

const TARGET_KIND_HUMAN_LIB: &str = "library";
const TARGET_KIND_HUMAN_BIN: &str = "binary";
const TARGET_KIND_HUMAN_EXAMPLE: &str = "example";
const TARGET_KIND_HUMAN_TEST: &str = "test";
const TARGET_KIND_HUMAN_BENCH: &str = "benchmark";

const TARGET_KIND_LIB: &str = "lib";
const TARGET_KIND_BIN: &str = "bin";
const TARGET_KIND_EXAMPLE: &str = "example";
const TARGET_KIND_TEST: &str = "test";
const TARGET_KIND_BENCH: &str = "bench";

const C_SOURCE_EXTENSIONS: &[&str] = &["c"];
const CPP_SOURCE_EXTENSIONS: &[&str] = &["cpp", "cc", "cxx"];
const NATIVE_SOURCE_EXTENSIONS: &[&str] = &["c", "cpp", "cc", "cxx"];
const NATIVE_HEADER_EXTENSIONS: &[&str] = &["h", "hh", "hpp", "hxx", "inc", "ipp", "tpp"];

#[tracing::instrument(skip_all)]
pub(super) fn to_targets(
    features: &Features,
    original_toml: &TomlManifest,
    normalized_toml: &TomlManifest,
    package_root: &Path,
    edition: Edition,
    metabuild: &Option<StringOrVec>,
    warnings: &mut Vec<String>,
) -> CargoResult<Vec<Target>> {
    let mut targets = Vec::new();

    if let Some(target) = to_lib_target(
        original_toml.lib.as_ref(),
        normalized_toml.lib.as_ref(),
        package_root,
        edition,
        warnings,
    )? {
        targets.push(target);
    }

    let package = normalized_toml
        .package
        .as_ref()
        .ok_or_else(|| anyhow::format_err!("manifest has no `package` (or `project`)"))?;

    targets.extend(to_bin_targets(
        features,
        normalized_toml.bin.as_deref().unwrap_or_default(),
        package_root,
        edition,
        warnings,
    )?);

    targets.extend(to_example_targets(
        normalized_toml.example.as_deref().unwrap_or_default(),
        package_root,
        edition,
        warnings,
    )?);

    targets.extend(to_test_targets(
        normalized_toml.test.as_deref().unwrap_or_default(),
        package_root,
        edition,
        warnings,
    )?);

    targets.extend(to_bench_targets(
        normalized_toml.bench.as_deref().unwrap_or_default(),
        package_root,
        edition,
        warnings,
    )?);

    // processing the custom build script
    if let Some(custom_build) = package.normalized_build().expect("previously normalized") {
        if metabuild.is_some() {
            anyhow::bail!("cannot specify both `metabuild` and `build`");
        }
        validate_unique_build_scripts(custom_build)?;
        for script in custom_build {
            let script_path = Path::new(script);
            let name = format!(
                "build-script-{}",
                script_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
            );
            targets.push(Target::custom_build_target(
                &name,
                package_root.join(script_path),
                edition,
            ));
        }
    }
    if let Some(metabuild) = metabuild {
        // Verify names match available build deps.
        let bdeps = normalized_toml.build_dependencies.as_ref();
        for name in &metabuild.0 {
            if !bdeps.map_or(false, |bd| bd.contains_key(name.as_str())) {
                anyhow::bail!(
                    "metabuild package `{}` must be specified in `build-dependencies`",
                    name
                );
            }
        }
        targets.push(Target::metabuild_target(&format!(
            "metabuild-{}",
            package.normalized_name().expect("previously normalized")
        )));
    }

    Ok(targets)
}

#[tracing::instrument(skip_all)]
pub fn normalize_lib(
    original_lib: Option<&TomlLibTarget>,
    package_root: &Path,
    package_name: &str,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
) -> CargoResult<Option<TomlLibTarget>> {
    let inferred = inferred_lib(package_root)?;
    if is_normalized(original_lib, autodiscover) {
        let Some(mut lib) = original_lib.cloned() else {
            return Ok(None);
        };

        // Check early to improve error messages
        validate_lib_name(&lib, warnings)?;

        validate_proc_macro(&lib, TARGET_KIND_HUMAN_LIB, edition, warnings)?;
        validate_crate_types(&lib, TARGET_KIND_HUMAN_LIB, edition, warnings)?;

        if let Some(PathValue(path)) = &lib.path {
            lib.path = Some(PathValue(paths::normalize_path(path).into()));
        }
        normalize_native_manifest_paths(&mut lib);

        Ok(Some(lib))
    } else {
        let lib = original_lib.cloned().or_else(|| {
            inferred.as_ref().map(|lib| TomlTarget {
                path: Some(PathValue(lib.clone())),
                ..TomlTarget::new()
            })
        });
        let Some(mut lib) = lib else { return Ok(None) };
        lib.name
            .get_or_insert_with(|| package_name.replace("-", "_"));

        // Check early to improve error messages
        validate_lib_name(&lib, warnings)?;

        validate_proc_macro(&lib, TARGET_KIND_HUMAN_LIB, edition, warnings)?;
        validate_crate_types(&lib, TARGET_KIND_HUMAN_LIB, edition, warnings)?;

        if lib.path.is_none() {
            if let Some(inferred) = inferred {
                lib.path = Some(PathValue(inferred));
            } else {
                let name = name_or_panic(&lib);
                let legacy_path = Path::new("src").join(format!("{name}.rs"));
                if edition == Edition::Edition2015 && package_root.join(&legacy_path).exists() {
                    warnings.push(format!(
                        "path `{}` was erroneously implicitly accepted for library `{name}`,\n\
                     please rename the file to `src/lib.rs` or set lib.path in Cargo.toml",
                        legacy_path.display(),
                    ));
                    lib.path = Some(PathValue(legacy_path));
                } else {
                    anyhow::bail!(
                        "can't find library `{name}`, \
                     rename file to `src/lib.rs` or `src/lib.cpp`, add `include/` for a header-only library, or specify lib.path",
                    )
                }
            }
        }

        if let Some(PathValue(path)) = lib.path.as_ref() {
            lib.path = Some(PathValue(paths::normalize_path(&path).into()));
        }
        normalize_native_manifest_paths(&mut lib);

        Ok(Some(lib))
    }
}

#[tracing::instrument(skip_all)]
fn to_lib_target(
    original_lib: Option<&TomlLibTarget>,
    normalized_lib: Option<&TomlLibTarget>,
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> CargoResult<Option<Target>> {
    let Some(lib) = normalized_lib else {
        return Ok(None);
    };

    let path = lib.path.as_ref().expect("previously normalized");
    let path = package_root.join(&path.0);

    if is_native_source_path(&path) {
        let source_label = native_source_language(&path);
        let crate_type = match lib.crate_types() {
            None => CrateType::Staticlib,
            Some(crate_types) if crate_types.is_empty() => CrateType::Staticlib,
            Some(crate_types) if crate_types.len() == 1 => match crate_types[0].as_str() {
                "staticlib" => CrateType::Staticlib,
                "cdylib" => CrateType::Cdylib,
                other => anyhow::bail!(
                    "{source_label} library `{}` only supports crate-type `staticlib` or `cdylib`, found `{other}`",
                    name_or_panic(lib)
                ),
            },
            Some(crate_types) => anyhow::bail!(
                "{source_label} library `{}` must specify at most one crate-type, found `{}`",
                name_or_panic(lib),
                crate_types.join(", ")
            ),
        };
        let mut target = Target::native_lib_target(name_or_panic(lib), crate_type, path, edition);
        configure(lib, &mut target, TARGET_KIND_HUMAN_LIB, warnings)?;
        apply_native_manifest_overrides(lib, &mut target, package_root)?;
        target.set_name_inferred(original_lib.map_or(true, |v| v.name.is_none()));
        return Ok(Some(target));
    }

    if is_header_only_native_path(&path) {
        if let Some(crate_types) = lib.crate_types()
            && !crate_types.is_empty()
        {
            anyhow::bail!(
                "header-only native library `{}` cannot set `crate-type`; remove it or point `lib.path` to a compilable source file",
                name_or_panic(lib)
            );
        }
        let mut target = Target::native_header_only_lib_target(name_or_panic(lib), path, edition);
        configure(lib, &mut target, TARGET_KIND_HUMAN_LIB, warnings)?;
        apply_native_manifest_overrides(lib, &mut target, package_root)?;
        target.set_name_inferred(original_lib.map_or(true, |v| v.name.is_none()));
        return Ok(Some(target));
    }

    // Per the Macros 1.1 RFC:
    //
    // > Initially if a crate is compiled with the `proc-macro` crate type
    // > (and possibly others) it will forbid exporting any items in the
    // > crate other than those functions tagged #[proc_macro_derive] and
    // > those functions must also be placed at the crate root.
    //
    // A plugin requires exporting plugin_registrar so a crate cannot be
    // both at once.
    let crate_types = match (lib.crate_types(), lib.proc_macro()) {
        (Some(kinds), _)
            if kinds.contains(&CrateType::Dylib.as_str().to_owned())
                && kinds.contains(&CrateType::Cdylib.as_str().to_owned()) =>
        {
            anyhow::bail!(format!(
                "library `{}` cannot set the crate type of both `dylib` and `cdylib`",
                name_or_panic(lib)
            ));
        }
        (Some(kinds), _) if kinds.contains(&"proc-macro".to_string()) => {
            warnings.push(format!(
                "library `{}` should only specify `proc-macro = true` instead of setting `crate-type`",
                name_or_panic(lib)
            ));
            if kinds.len() > 1 {
                anyhow::bail!("cannot mix `proc-macro` crate type with others");
            }
            vec![CrateType::ProcMacro]
        }
        (Some(kinds), _) => kinds.iter().map(|s| s.into()).collect(),
        (None, Some(true)) => vec![CrateType::ProcMacro],
        (None, _) => vec![CrateType::Lib],
    };

    let mut target = Target::lib_target(name_or_panic(lib), crate_types, path, edition);
    configure(lib, &mut target, TARGET_KIND_HUMAN_LIB, warnings)?;
    target.set_name_inferred(original_lib.map_or(true, |v| v.name.is_none()));
    Ok(Some(target))
}

#[tracing::instrument(skip_all)]
pub fn normalize_bins(
    toml_bins: Option<&Vec<TomlBinTarget>>,
    package_root: &Path,
    package_name: &str,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
    has_lib: bool,
) -> CargoResult<Vec<TomlBinTarget>> {
    if are_normalized(toml_bins, autodiscover) {
        let mut toml_bins = toml_bins.cloned().unwrap_or_default();
        for bin in toml_bins.iter_mut() {
            validate_bin_name(bin, warnings)?;
            validate_bin_crate_types(bin, edition, warnings, errors)?;
            validate_bin_proc_macro(bin, edition, warnings, errors)?;

            if let Some(PathValue(path)) = &bin.path {
                bin.path = Some(PathValue(paths::normalize_path(path).into()));
            }
            normalize_native_manifest_paths(bin);
        }
        Ok(toml_bins)
    } else {
        let inferred = inferred_bins(package_root, package_name)?;

        let mut bins = toml_targets_and_inferred(
            toml_bins,
            &inferred,
            package_root,
            autodiscover,
            edition,
            warnings,
            TARGET_KIND_HUMAN_BIN,
            TARGET_KIND_BIN,
            "autobins",
        );

        for bin in &mut bins {
            // Check early to improve error messages
            validate_bin_name(bin, warnings)?;

            validate_bin_crate_types(bin, edition, warnings, errors)?;
            validate_bin_proc_macro(bin, edition, warnings, errors)?;

            let path = target_path(
                bin,
                &inferred,
                TARGET_KIND_BIN,
                package_root,
                edition,
                &mut |_| {
                    if let Some(legacy_path) =
                        legacy_bin_path(package_root, name_or_panic(bin), has_lib)
                    {
                        warnings.push(format!(
                            "path `{}` was erroneously implicitly accepted for binary `{}`,\n\
                     please set bin.path in Cargo.toml",
                            legacy_path.display(),
                            name_or_panic(bin)
                        ));
                        Some(legacy_path)
                    } else {
                        None
                    }
                },
            );
            let path = match path {
                Ok(path) => paths::normalize_path(&path).into(),
                Err(e) => anyhow::bail!("{}", e),
            };
            bin.path = Some(PathValue(path));
            normalize_native_manifest_paths(bin);
        }

        Ok(bins)
    }
}

#[tracing::instrument(skip_all)]
fn to_bin_targets(
    features: &Features,
    bins: &[TomlBinTarget],
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> CargoResult<Vec<Target>> {
    // This loop performs basic checks on each of the TomlTarget in `bins`.
    for bin in bins {
        // For each binary, check if the `filename` parameter is populated. If it is,
        // check if the corresponding cargo feature has been activated.
        if bin.filename.is_some() {
            features.require(Feature::different_binary_name())?;
        }
    }

    validate_unique_names(&bins, TARGET_KIND_HUMAN_BIN)?;

    let mut result = Vec::new();
    for bin in bins {
        let path = package_root.join(&bin.path.as_ref().expect("previously normalized").0);
        let mut target = if is_native_source_path(&path) {
            Target::native_bin_target(
                name_or_panic(bin),
                bin.filename.clone(),
                path,
                bin.required_features.clone(),
                edition,
            )
        } else {
            Target::bin_target(
                name_or_panic(bin),
                bin.filename.clone(),
                path,
                bin.required_features.clone(),
                edition,
            )
        };

        configure(bin, &mut target, TARGET_KIND_HUMAN_BIN, warnings)?;
        apply_native_manifest_overrides(bin, &mut target, package_root)?;
        result.push(target);
    }
    Ok(result)
}

fn legacy_bin_path(package_root: &Path, name: &str, has_lib: bool) -> Option<PathBuf> {
    if !has_lib {
        let rel_path = Path::new("src").join(format!("{}.rs", name));
        if package_root.join(&rel_path).exists() {
            return Some(rel_path);
        }
    }

    let rel_path = Path::new("src").join("main.rs");
    if package_root.join(&rel_path).exists() {
        return Some(rel_path);
    }

    let default_bin_dir_name = Path::new("src").join("bin");
    let rel_path = default_bin_dir_name.join("main.rs");
    if package_root.join(&rel_path).exists() {
        return Some(rel_path);
    }
    None
}

#[tracing::instrument(skip_all)]
pub fn normalize_examples(
    toml_examples: Option<&Vec<TomlExampleTarget>>,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> CargoResult<Vec<TomlExampleTarget>> {
    let mut inferred = || infer_non_bin_targets(&package_root, Path::new(DEFAULT_EXAMPLE_DIR_NAME));

    let targets = normalize_targets(
        TARGET_KIND_HUMAN_EXAMPLE,
        TARGET_KIND_EXAMPLE,
        toml_examples,
        &mut inferred,
        package_root,
        edition,
        autodiscover,
        warnings,
        errors,
        "autoexamples",
    )?;

    Ok(targets)
}

#[tracing::instrument(skip_all)]
fn to_example_targets(
    targets: &[TomlExampleTarget],
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> CargoResult<Vec<Target>> {
    validate_unique_names(&targets, TARGET_KIND_EXAMPLE)?;

    let mut result = Vec::new();
    for toml in targets {
        let path = package_root.join(&toml.path.as_ref().expect("previously normalized").0);
        if is_native_source_path(&path) {
            let is_bin_example = match toml.crate_types() {
                None => true,
                Some(crate_types) => {
                    crate_types.is_empty()
                        || crate_types
                            .iter()
                            .all(|kind| kind == CrateType::Bin.as_str())
                }
            };
            if !is_bin_example {
                anyhow::bail!(
                    "native C/C++ example `{}` must be an executable target; remove `crate-type` or use only `bin`",
                    name_or_panic(toml)
                );
            }
        }
        let crate_types = match toml.crate_types() {
            Some(kinds) => kinds.iter().map(|s| s.into()).collect(),
            None => Vec::new(),
        };

        let mut target = Target::example_target(
            name_or_panic(&toml),
            crate_types,
            path,
            toml.required_features.clone(),
            edition,
        );
        configure(&toml, &mut target, TARGET_KIND_HUMAN_EXAMPLE, warnings)?;
        apply_native_manifest_overrides(toml, &mut target, package_root)?;
        result.push(target);
    }

    Ok(result)
}

#[tracing::instrument(skip_all)]
pub fn normalize_tests(
    toml_tests: Option<&Vec<TomlTestTarget>>,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> CargoResult<Vec<TomlTestTarget>> {
    let mut inferred = || infer_non_bin_targets(&package_root, Path::new(DEFAULT_TEST_DIR_NAME));

    let targets = normalize_targets(
        TARGET_KIND_HUMAN_TEST,
        TARGET_KIND_TEST,
        toml_tests,
        &mut inferred,
        package_root,
        edition,
        autodiscover,
        warnings,
        errors,
        "autotests",
    )?;

    Ok(targets)
}

#[tracing::instrument(skip_all)]
fn to_test_targets(
    targets: &[TomlTestTarget],
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> CargoResult<Vec<Target>> {
    validate_unique_names(&targets, TARGET_KIND_TEST)?;

    let mut result = Vec::new();
    for toml in targets {
        let path = package_root.join(&toml.path.as_ref().expect("previously normalized").0);
        let mut target = Target::test_target(
            name_or_panic(&toml),
            path,
            toml.required_features.clone(),
            edition,
        );
        configure(&toml, &mut target, TARGET_KIND_HUMAN_TEST, warnings)?;
        apply_native_manifest_overrides(toml, &mut target, package_root)?;
        result.push(target);
    }
    Ok(result)
}

#[tracing::instrument(skip_all)]
pub fn normalize_benches(
    toml_benches: Option<&Vec<TomlBenchTarget>>,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> CargoResult<Vec<TomlBenchTarget>> {
    let mut legacy_warnings = vec![];
    let mut legacy_bench_path = |bench: &TomlTarget| {
        let legacy_path = Path::new("src").join("bench.rs");
        if !(name_or_panic(bench) == "bench" && package_root.join(&legacy_path).exists()) {
            return None;
        }
        legacy_warnings.push(format!(
            "path `{}` was erroneously implicitly accepted for benchmark `{}`,\n\
                 please set bench.path in Cargo.toml",
            legacy_path.display(),
            name_or_panic(bench)
        ));
        Some(legacy_path)
    };

    let mut inferred = || infer_non_bin_targets(&package_root, Path::new(DEFAULT_BENCH_DIR_NAME));

    let targets = normalize_targets_with_legacy_path(
        TARGET_KIND_HUMAN_BENCH,
        TARGET_KIND_BENCH,
        toml_benches,
        &mut inferred,
        package_root,
        edition,
        autodiscover,
        warnings,
        errors,
        &mut legacy_bench_path,
        "autobenches",
    )?;
    warnings.append(&mut legacy_warnings);

    Ok(targets)
}

#[tracing::instrument(skip_all)]
fn to_bench_targets(
    targets: &[TomlBenchTarget],
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> CargoResult<Vec<Target>> {
    validate_unique_names(&targets, TARGET_KIND_BENCH)?;

    let mut result = Vec::new();
    for toml in targets {
        let path = package_root.join(&toml.path.as_ref().expect("previously normalized").0);
        let mut target = Target::bench_target(
            name_or_panic(&toml),
            path,
            toml.required_features.clone(),
            edition,
        );
        configure(&toml, &mut target, TARGET_KIND_HUMAN_BENCH, warnings)?;
        apply_native_manifest_overrides(toml, &mut target, package_root)?;
        result.push(target);
    }

    Ok(result)
}

fn is_normalized(toml_target: Option<&TomlTarget>, autodiscover: Option<bool>) -> bool {
    are_normalized_(toml_target.map(std::slice::from_ref), autodiscover)
}

fn are_normalized(toml_targets: Option<&Vec<TomlTarget>>, autodiscover: Option<bool>) -> bool {
    are_normalized_(toml_targets.map(|v| v.as_slice()), autodiscover)
}

fn are_normalized_(toml_targets: Option<&[TomlTarget]>, autodiscover: Option<bool>) -> bool {
    if autodiscover != Some(false) {
        return false;
    }

    let Some(toml_targets) = toml_targets else {
        return true;
    };
    toml_targets
        .iter()
        .all(|t| t.name.is_some() && t.path.is_some())
}

fn normalize_targets(
    target_kind_human: &str,
    target_kind: &str,
    toml_targets: Option<&Vec<TomlTarget>>,
    inferred: &mut dyn FnMut() -> CargoResult<Vec<(String, PathBuf)>>,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
    autodiscover_flag_name: &str,
) -> CargoResult<Vec<TomlTarget>> {
    normalize_targets_with_legacy_path(
        target_kind_human,
        target_kind,
        toml_targets,
        inferred,
        package_root,
        edition,
        autodiscover,
        warnings,
        errors,
        &mut |_| None,
        autodiscover_flag_name,
    )
}

fn normalize_targets_with_legacy_path(
    target_kind_human: &str,
    target_kind: &str,
    toml_targets: Option<&Vec<TomlTarget>>,
    inferred: &mut dyn FnMut() -> CargoResult<Vec<(String, PathBuf)>>,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
    legacy_path: &mut dyn FnMut(&TomlTarget) -> Option<PathBuf>,
    autodiscover_flag_name: &str,
) -> CargoResult<Vec<TomlTarget>> {
    if are_normalized(toml_targets, autodiscover) {
        let mut toml_targets = toml_targets.cloned().unwrap_or_default();
        for target in toml_targets.iter_mut() {
            // Check early to improve error messages
            validate_target_name(target, target_kind_human, target_kind, warnings)?;

            validate_proc_macro(target, target_kind_human, edition, warnings)?;
            validate_crate_types(target, target_kind_human, edition, warnings)?;

            if let Some(PathValue(path)) = &target.path {
                target.path = Some(PathValue(paths::normalize_path(path).into()));
            }
            normalize_native_manifest_paths(target);
        }
        Ok(toml_targets)
    } else {
        let inferred = inferred()?;
        let toml_targets = toml_targets_and_inferred(
            toml_targets,
            &inferred,
            package_root,
            autodiscover,
            edition,
            warnings,
            target_kind_human,
            target_kind,
            autodiscover_flag_name,
        );

        for target in &toml_targets {
            // Check early to improve error messages
            validate_target_name(target, target_kind_human, target_kind, warnings)?;

            validate_proc_macro(target, target_kind_human, edition, warnings)?;
            validate_crate_types(target, target_kind_human, edition, warnings)?;
        }

        let mut result = Vec::new();
        for mut target in toml_targets {
            let path = target_path(
                &target,
                &inferred,
                target_kind,
                package_root,
                edition,
                legacy_path,
            );
            let path = match path {
                Ok(path) => path,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            target.path = Some(PathValue(paths::normalize_path(&path).into()));
            normalize_native_manifest_paths(&mut target);
            result.push(target);
        }
        Ok(result)
    }
}

fn inferred_lib(package_root: &Path) -> CargoResult<Option<PathBuf>> {
    let rust_lib = Path::new("src").join("lib.rs");
    let native_libs = collect_native_candidates(package_root, Path::new("src"), "lib", false)?;
    let header_only_dir = package_root.join("include");
    let has_rust_lib = package_root.join(&rust_lib).exists();
    if has_rust_lib && !native_libs.is_empty() {
        anyhow::bail!(
            "found conflicting library targets at `src/lib.rs` and `{}`; a package cannot define both a Rust and C++ default library target",
            native_libs[0].display()
        );
    }
    if has_rust_lib {
        Ok(Some(rust_lib))
    } else if let Some(native_lib) = native_libs.into_iter().next() {
        Ok(Some(native_lib))
    } else if header_only_dir.is_dir() {
        Ok(Some(PathBuf::from("include")))
    } else {
        Ok(None)
    }
}

fn inferred_bins(package_root: &Path, package_name: &str) -> CargoResult<Vec<(String, PathBuf)>> {
    let main = Path::new("src").join("main.rs");
    let native_main = collect_native_candidates(package_root, Path::new("src"), "main", false)?;
    let has_rust_main = package_root.join(&main).exists();
    if has_rust_main && !native_main.is_empty() {
        anyhow::bail!(
            "found conflicting binary targets at `src/main.rs` and `{}`; a package cannot define both a Rust and C++ default binary target",
            native_main[0].display()
        );
    }

    let mut result = Vec::new();
    if has_rust_main {
        result.push((package_name.to_string(), main));
    } else if let Some(native_main) = native_main.into_iter().next() {
        result.push((package_name.to_string(), native_main));
    }

    let default_bin_dir_name = Path::new("src").join("bin");
    result.extend(infer_bin_sources(package_root, &default_bin_dir_name)?);

    Ok(result)
}

fn infer_bin_sources(package_root: &Path, relpath: &Path) -> CargoResult<Vec<(String, PathBuf)>> {
    let directory = package_root.join(relpath);
    let entries = match fs::read_dir(directory) {
        Err(_) => return Ok(Vec::new()),
        Ok(dir) => dir,
    };

    let mut seen = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()).filter(is_not_dotfile) {
        if let Some((name, path, language)) = infer_bin_any(package_root, &entry)? {
            if let Some((prev_path, prev_language)) =
                seen.insert(name.clone(), (path.clone(), language))
            {
                let ((first_path, first_language), (second_path, second_language)) =
                    order_conflicting_targets((prev_path, prev_language), (path, language));
                anyhow::bail!(
                    "found conflicting binary targets `{}` at `{}` and `{}`; a package cannot define both a {} and {} target with the same name",
                    name,
                    first_path.display(),
                    second_path.display(),
                    first_language,
                    second_language,
                );
            }
        }
    }

    let mut inferred = seen
        .into_iter()
        .map(|(name, (path, _))| (name, path))
        .collect::<Vec<_>>();
    inferred.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(inferred)
}

fn order_conflicting_targets(
    left: (PathBuf, &'static str),
    right: (PathBuf, &'static str),
) -> ((PathBuf, &'static str), (PathBuf, &'static str)) {
    let left_key = (target_language_priority(left.1), left.0.clone());
    let right_key = (target_language_priority(right.1), right.0.clone());
    if left_key <= right_key {
        (left, right)
    } else {
        (right, left)
    }
}

fn target_language_priority(language: &str) -> u8 {
    match language {
        "Rust" => 0,
        "C" => 1,
        "C++" => 1,
        _ => 2,
    }
}

fn infer_bin_any(
    package_root: &Path,
    entry: &DirEntry,
) -> CargoResult<Option<(String, PathBuf, &'static str)>> {
    if entry.file_type().map_or(false, |t| t.is_dir()) {
        infer_bin_subdirectory(package_root, entry)
    } else if entry.path().extension().and_then(|p| p.to_str()) == Some("rs") {
        Ok(infer_file(package_root, entry).map(|(name, path)| (name, path, "Rust")))
    } else if is_native_source_path(&entry.path()) {
        Ok(infer_file(package_root, entry)
            .map(|(name, path)| (name, path, native_source_language(&entry.path()))))
    } else {
        Ok(None)
    }
}

fn infer_bin_subdirectory(
    package_root: &Path,
    entry: &DirEntry,
) -> CargoResult<Option<(String, PathBuf, &'static str)>> {
    let path = entry.path();
    let name = path.file_name().and_then(|p| p.to_str()).map(str::to_owned);
    let Some(name) = name else {
        return Ok(None);
    };

    let rust_main = path.join("main.rs");
    let native_main = NATIVE_SOURCE_EXTENSIONS
        .iter()
        .map(|ext| path.join(format!("main.{ext}")))
        .filter(|candidate| candidate.exists())
        .collect::<Vec<_>>();

    if rust_main.exists() && !native_main.is_empty() {
        anyhow::bail!(
            "found conflicting binary targets `{}` at `{}` and `{}`; a package cannot define both a Rust and native C/C++ target with the same name",
            name,
            rust_main
                .strip_prefix(package_root)
                .unwrap_or(&rust_main)
                .display(),
            native_main[0]
                .strip_prefix(package_root)
                .unwrap_or(&native_main[0])
                .display(),
        );
    }

    if rust_main.exists() {
        let main = rust_main
            .strip_prefix(package_root)
            .map(|p| p.to_owned())
            .unwrap_or(rust_main);
        return Ok(Some((name, main, "Rust")));
    }

    if let Some(native_main) = native_main.into_iter().next() {
        let language = native_source_language(&native_main);
        let main = native_main
            .strip_prefix(package_root)
            .map(|p| p.to_owned())
            .unwrap_or(native_main);
        return Ok(Some((name, main, language)));
    }

    Ok(None)
}

fn collect_native_candidates(
    package_root: &Path,
    rel_dir: &Path,
    stem: &str,
    allow_multiple: bool,
) -> CargoResult<Vec<PathBuf>> {
    let mut matches = NATIVE_SOURCE_EXTENSIONS
        .iter()
        .map(|ext| rel_dir.join(format!("{stem}.{ext}")))
        .filter(|candidate| package_root.join(candidate).exists())
        .collect::<Vec<_>>();
    if !allow_multiple && matches.len() > 1 {
        let rendered = matches
            .iter()
            .map(|path| format!("`{}`", path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "found multiple native C/C++ targets for `{stem}` at {rendered}; please keep only one default source file"
        );
    }
    matches.sort();
    Ok(matches)
}

fn is_native_source_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| NATIVE_SOURCE_EXTENSIONS.contains(&ext))
}

fn is_header_only_native_path(path: &Path) -> bool {
    path.is_dir()
        || path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| NATIVE_HEADER_EXTENSIONS.contains(&ext))
}

fn native_source_language(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if C_SOURCE_EXTENSIONS.contains(&ext) => "C",
        Some(ext) if CPP_SOURCE_EXTENSIONS.contains(&ext) => "C++",
        _ => "native",
    }
}

fn infer_non_bin_targets(
    package_root: &Path,
    relpath: &Path,
) -> CargoResult<Vec<(String, PathBuf)>> {
    let directory = package_root.join(relpath);
    let entries = match fs::read_dir(directory) {
        Err(_) => return Ok(Vec::new()),
        Ok(dir) => dir,
    };

    let mut seen = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()).filter(is_not_dotfile) {
        if let Some((name, path, language)) = infer_non_bin_any(package_root, &entry)? {
            if let Some((prev_path, prev_language)) =
                seen.insert(name.clone(), (path.clone(), language))
            {
                let ((first_path, first_language), (second_path, second_language)) =
                    order_conflicting_targets((prev_path, prev_language), (path, language));
                anyhow::bail!(
                    "found conflicting targets `{}` at `{}` and `{}`; a package cannot define both a {} and {} target with the same name",
                    name,
                    first_path.display(),
                    second_path.display(),
                    first_language,
                    second_language,
                );
            }
        }
    }

    let mut inferred = seen
        .into_iter()
        .map(|(name, (path, _))| (name, path))
        .collect::<Vec<_>>();
    inferred.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(inferred)
}

fn infer_non_bin_any(
    package_root: &Path,
    entry: &DirEntry,
) -> CargoResult<Option<(String, PathBuf, &'static str)>> {
    if entry.file_type().map_or(false, |t| t.is_dir()) {
        infer_non_bin_subdirectory(package_root, entry)
    } else if entry.path().extension().and_then(|p| p.to_str()) == Some("rs") {
        Ok(infer_file(package_root, entry).map(|(name, path)| (name, path, "Rust")))
    } else if is_native_source_path(&entry.path()) {
        Ok(infer_file(package_root, entry)
            .map(|(name, path)| (name, path, native_source_language(&entry.path()))))
    } else {
        Ok(None)
    }
}

fn infer_non_bin_subdirectory(
    package_root: &Path,
    entry: &DirEntry,
) -> CargoResult<Option<(String, PathBuf, &'static str)>> {
    let path = entry.path();
    let name = path.file_name().and_then(|p| p.to_str()).map(str::to_owned);
    let Some(name) = name else {
        return Ok(None);
    };

    let rust_main = path.join("main.rs");
    let native_main = NATIVE_SOURCE_EXTENSIONS
        .iter()
        .map(|ext| path.join(format!("main.{ext}")))
        .filter(|candidate| candidate.exists())
        .collect::<Vec<_>>();

    if rust_main.exists() && !native_main.is_empty() {
        anyhow::bail!(
            "found conflicting targets `{}` at `{}` and `{}`; a package cannot define both a Rust and native C/C++ target with the same name",
            name,
            rust_main
                .strip_prefix(package_root)
                .unwrap_or(&rust_main)
                .display(),
            native_main[0]
                .strip_prefix(package_root)
                .unwrap_or(&native_main[0])
                .display(),
        );
    }

    if rust_main.exists() {
        let main = rust_main
            .strip_prefix(package_root)
            .map(|p| p.to_owned())
            .unwrap_or(rust_main);
        return Ok(Some((name, main, "Rust")));
    }

    if let Some(native_main) = native_main.into_iter().next() {
        let language = native_source_language(&native_main);
        let main = native_main
            .strip_prefix(package_root)
            .map(|p| p.to_owned())
            .unwrap_or(native_main);
        return Ok(Some((name, main, language)));
    }

    Ok(None)
}

fn infer_file(package_root: &Path, entry: &DirEntry) -> Option<(String, PathBuf)> {
    let path = entry.path();
    let stem = path.file_stem()?.to_str()?.to_owned();
    let path = path
        .strip_prefix(package_root)
        .map(|p| p.to_owned())
        .unwrap_or(path);
    Some((stem, path))
}

fn is_not_dotfile(entry: &DirEntry) -> bool {
    entry.file_name().to_str().map(|s| s.starts_with('.')) == Some(false)
}

fn toml_targets_and_inferred(
    toml_targets: Option<&Vec<TomlTarget>>,
    inferred: &[(String, PathBuf)],
    package_root: &Path,
    autodiscover: Option<bool>,
    edition: Edition,
    warnings: &mut Vec<String>,
    target_kind_human: &str,
    target_kind: &str,
    autodiscover_flag_name: &str,
) -> Vec<TomlTarget> {
    let inferred_targets = inferred_to_toml_targets(inferred);
    let mut toml_targets = match toml_targets {
        None => {
            if let Some(false) = autodiscover {
                vec![]
            } else {
                inferred_targets
            }
        }
        Some(targets) => {
            let mut targets = targets.clone();

            let target_path =
                |target: &TomlTarget| target.path.clone().map(|p| package_root.join(p.0));

            let mut seen_names = HashSet::new();
            let mut seen_paths = HashSet::new();
            for target in targets.iter() {
                seen_names.insert(target.name.clone());
                seen_paths.insert(target_path(target));
            }

            let mut rem_targets = vec![];
            for target in inferred_targets {
                if !seen_names.contains(&target.name) && !seen_paths.contains(&target_path(&target))
                {
                    rem_targets.push(target);
                }
            }

            let autodiscover = match autodiscover {
                Some(autodiscover) => autodiscover,
                None => {
                    if edition == Edition::Edition2015 {
                        if !rem_targets.is_empty() {
                            let mut rem_targets_str = String::new();
                            for t in rem_targets.iter() {
                                if let Some(p) = t.path.clone() {
                                    rem_targets_str.push_str(&format!("* {}\n", p.0.display()))
                                }
                            }
                            warnings.push(format!(
                                "\
An explicit [[{section}]] section is specified in Cargo.toml which currently
disables Cargo from automatically inferring other {target_kind_human} targets.
This inference behavior will change in the Rust 2018 edition and the following
files will be included as a {target_kind_human} target:

{rem_targets_str}
This is likely to break cargo build or cargo test as these files may not be
ready to be compiled as a {target_kind_human} target today. You can future-proof yourself
and disable this warning by adding `{autodiscover_flag_name} = false` to your [package]
section. You may also move the files to a location where Cargo would not
automatically infer them to be a target, such as in subfolders.

For more information on this warning you can consult
https://github.com/rust-lang/cargo/issues/5330",
                                section = target_kind,
                                target_kind_human = target_kind_human,
                                rem_targets_str = rem_targets_str,
                                autodiscover_flag_name = autodiscover_flag_name,
                            ));
                        };
                        false
                    } else {
                        true
                    }
                }
            };

            if autodiscover {
                targets.append(&mut rem_targets);
            }

            targets
        }
    };
    // Ensure target order is deterministic, particularly for `cargo vendor` where re-vendoring
    // should not cause changes.
    //
    // `unstable` should be deterministic because we enforce that `t.name` is unique
    toml_targets.sort_unstable_by_key(|t| t.name.clone());
    toml_targets
}

fn inferred_to_toml_targets(inferred: &[(String, PathBuf)]) -> Vec<TomlTarget> {
    inferred
        .iter()
        .map(|(name, path)| TomlTarget {
            name: Some(name.clone()),
            path: Some(PathValue(path.clone())),
            ..TomlTarget::new()
        })
        .collect()
}

fn normalize_native_manifest_paths(target: &mut TomlTarget) {
    if let Some(PathValue(path)) = &target.native_include_root {
        target.native_include_root = Some(PathValue(paths::normalize_path(path).into()));
    }
    if let Some(PathValue(path)) = &target.native_sources_root {
        target.native_sources_root = Some(PathValue(paths::normalize_path(path).into()));
    }
    if let Some(include_dirs) = &target.native_include_dirs {
        target.native_include_dirs = Some(
            include_dirs
                .iter()
                .map(|PathValue(path)| PathValue(paths::normalize_path(path).into()))
                .collect(),
        );
    }
    if let Some(link_search) = &target.native_link_search {
        target.native_link_search = Some(
            link_search
                .iter()
                .map(|PathValue(path)| PathValue(paths::normalize_path(path).into()))
                .collect(),
        );
    }
}

fn apply_native_manifest_overrides(
    toml: &TomlTarget,
    target: &mut Target,
    package_root: &Path,
) -> CargoResult<()> {
    if toml.native_include_root.is_none()
        && toml.native_sources_root.is_none()
        && toml.native_include_dirs.is_none()
        && toml.native_defines.is_none()
        && toml.native_link_search.is_none()
        && toml.native_link_libraries.is_none()
        && toml.native_link_args.is_none()
    {
        return Ok(());
    }

    if !target.is_native() {
        anyhow::bail!(
            "target `{}` sets native-only manifest keys, but those keys are only supported for native C/C++ targets",
            name_or_panic(toml)
        );
    }

    if let Some(PathValue(path)) = &toml.native_include_root {
        let include_root = package_root.join(path);
        if include_root.exists() && !include_root.is_dir() {
            anyhow::bail!(
                "target `{}` specifies `native-include-root = \"{}\"`, but that path is not a directory",
                name_or_panic(toml),
                path.display()
            );
        }
        target.set_native_include_root(Some(include_root));
    }

    if let Some(PathValue(path)) = &toml.native_sources_root {
        if target.is_header_only() {
            anyhow::bail!(
                "header-only native library `{}` cannot set `native-sources-root`",
                name_or_panic(toml)
            );
        }

        let sources_root = package_root.join(path);
        if !sources_root.is_dir() {
            anyhow::bail!(
                "target `{}` specifies `native-sources-root = \"{}\"`, but that path is not a directory",
                name_or_panic(toml),
                path.display()
            );
        }
        target.set_native_sources_root(Some(sources_root));
    }

    if let Some(include_dirs) = &toml.native_include_dirs {
        if target.is_header_only() {
            anyhow::bail!(
                "header-only native library `{}` cannot set `native-include-dirs`",
                name_or_panic(toml)
            );
        }

        let mut normalized_dirs = Vec::with_capacity(include_dirs.len());
        for PathValue(path) in include_dirs {
            let include_dir = package_root.join(path);
            if !include_dir.is_dir() {
                anyhow::bail!(
                    "target `{}` specifies `native-include-dirs = [\"{}\"]`, but that path is not a directory",
                    name_or_panic(toml),
                    path.display()
                );
            }
            normalized_dirs.push(include_dir);
        }
        target.set_native_include_dirs(normalized_dirs);
    }

    if let Some(native_defines) = &toml.native_defines {
        if target.is_header_only() {
            anyhow::bail!(
                "header-only native library `{}` cannot set `native-defines`",
                name_or_panic(toml)
            );
        }

        if native_defines.iter().any(|define| define.is_empty()) {
            anyhow::bail!(
                "target `{}` specifies `native-defines`, but defines cannot be empty strings",
                name_or_panic(toml)
            );
        }
        target.set_native_defines(native_defines.clone());
    }

    if toml.native_link_search.is_some()
        || toml.native_link_libraries.is_some()
        || toml.native_link_args.is_some()
    {
        if target.is_header_only() {
            anyhow::bail!(
                "header-only native library `{}` cannot set native link manifest keys",
                name_or_panic(toml)
            );
        }
        if target.is_staticlib() {
            anyhow::bail!(
                "static native library `{}` cannot set native link manifest keys because static libraries are archived rather than linked",
                name_or_panic(toml)
            );
        }
    }

    if let Some(link_search) = &toml.native_link_search {
        let mut normalized_dirs = Vec::with_capacity(link_search.len());
        for PathValue(path) in link_search {
            let search_dir = package_root.join(path);
            if !search_dir.is_dir() {
                anyhow::bail!(
                    "target `{}` specifies `native-link-search = [\"{}\"]`, but that path is not a directory",
                    name_or_panic(toml),
                    path.display()
                );
            }
            normalized_dirs.push(search_dir);
        }
        target.set_native_link_search(normalized_dirs);
    }

    if let Some(link_libraries) = &toml.native_link_libraries {
        if link_libraries.iter().any(|library| library.is_empty()) {
            anyhow::bail!(
                "target `{}` specifies `native-link-libraries`, but library names cannot be empty strings",
                name_or_panic(toml)
            );
        }
        target.set_native_link_libraries(link_libraries.clone());
    }

    if let Some(link_args) = &toml.native_link_args {
        if link_args.iter().any(|arg| arg.is_empty()) {
            anyhow::bail!(
                "target `{}` specifies `native-link-args`, but linker arguments cannot be empty strings",
                name_or_panic(toml)
            );
        }
        target.set_native_link_args(link_args.clone());
    }

    Ok(())
}

/// Will check a list of toml targets, and make sure the target names are unique within a vector.
fn validate_unique_names(targets: &[TomlTarget], target_kind: &str) -> CargoResult<()> {
    let mut seen = HashSet::new();
    for name in targets.iter().map(|e| name_or_panic(e)) {
        if !seen.insert(name) {
            anyhow::bail!(
                "found duplicate {target_kind} name {name}, \
                 but all {target_kind} targets must have a unique name",
                target_kind = target_kind,
                name = name
            );
        }
    }
    Ok(())
}

/// Will check a list of build scripts, and make sure script file stems are unique within a vector.
fn validate_unique_build_scripts(scripts: &[String]) -> CargoResult<()> {
    let mut seen = HashMap::new();
    for script in scripts {
        let stem = Path::new(script).file_stem().unwrap().to_str().unwrap();
        seen.entry(stem)
            .or_insert_with(Vec::new)
            .push(script.as_str());
    }
    let mut conflict_file_stem = false;
    let mut err_msg = String::from(
        "found build scripts with duplicate file stems, but all build scripts must have a unique file stem",
    );
    for (stem, paths) in seen {
        if paths.len() > 1 {
            conflict_file_stem = true;
            write!(&mut err_msg, "\n  for stem `{stem}`: {}", paths.join(", "))?;
        }
    }
    if conflict_file_stem {
        anyhow::bail!(err_msg);
    }
    Ok(())
}

fn configure(
    toml: &TomlTarget,
    target: &mut Target,
    target_kind_human: &str,
    warnings: &mut Vec<String>,
) -> CargoResult<()> {
    let t2 = target.clone();
    target
        .set_tested(toml.test.unwrap_or_else(|| t2.tested()))
        .set_doc(toml.doc.unwrap_or_else(|| t2.documented()))
        .set_doctest(toml.doctest.unwrap_or_else(|| t2.doctested()))
        .set_benched(toml.bench.unwrap_or_else(|| t2.benched()))
        .set_harness(toml.harness.unwrap_or_else(|| t2.harness()))
        .set_proc_macro(toml.proc_macro().unwrap_or_else(|| t2.proc_macro()))
        .set_doc_scrape_examples(match toml.doc_scrape_examples {
            None => RustdocScrapeExamples::Unset,
            Some(false) => RustdocScrapeExamples::Disabled,
            Some(true) => RustdocScrapeExamples::Enabled,
        })
        .set_for_host(toml.proc_macro().unwrap_or_else(|| t2.for_host()));

    if let Some(edition) = toml.edition.clone() {
        let name = target.name();
        warnings.push(format!(
            "`edition` is set on {target_kind_human} `{name}` which is deprecated"
        ));
        target.set_edition(
            edition
                .parse()
                .context("failed to parse the `edition` key")?,
        );
    }
    Ok(())
}

/// Build an error message for a target path that cannot be determined either
/// by auto-discovery or specifying.
///
/// This function tries to detect commonly wrong paths for targets:
///
/// test -> tests/*.rs, tests/*/main.rs
/// bench -> benches/*.rs, benches/*/main.rs
/// example -> examples/*.rs, examples/*/main.rs
/// bin -> src/bin/*.rs, src/bin/*/main.rs
///
/// Note that the logic need to sync with non-bin target auto-discovery if changes.
fn target_path_not_found_error_message(
    package_root: &Path,
    target: &TomlTarget,
    target_kind: &str,
    inferred: &[(String, PathBuf)],
) -> String {
    fn possible_target_paths(name: &str, kind: &str, commonly_wrong: bool) -> [PathBuf; 2] {
        let mut target_path = PathBuf::new();
        match (kind, commonly_wrong) {
            // commonly wrong paths
            ("test" | "bench" | "example", true) => target_path.push(kind),
            ("bin", true) => target_path.extend(["src", "bins"]),
            // default inferred paths
            ("test", false) => target_path.push(DEFAULT_TEST_DIR_NAME),
            ("bench", false) => target_path.push(DEFAULT_BENCH_DIR_NAME),
            ("example", false) => target_path.push(DEFAULT_EXAMPLE_DIR_NAME),
            ("bin", false) => target_path.extend(["src", "bin"]),
            _ => unreachable!("invalid target kind: {}", kind),
        }

        let target_path_file = {
            let mut path = target_path.clone();
            path.push(format!("{name}.rs"));
            path
        };
        let target_path_subdir = {
            target_path.extend([name, "main.rs"]);
            target_path
        };
        return [target_path_file, target_path_subdir];
    }

    let target_name = name_or_panic(target);

    let commonly_wrong_paths = possible_target_paths(&target_name, target_kind, true);
    let possible_paths = possible_target_paths(&target_name, target_kind, false);

    let msg = closest_msg(target_name, inferred.iter(), |(n, _p)| n, target_kind);
    if let Some((wrong_path, possible_path)) = commonly_wrong_paths
        .iter()
        .zip(possible_paths.iter())
        .filter(|(wp, _)| package_root.join(wp).exists())
        .next()
    {
        let [wrong_path, possible_path] = [wrong_path, possible_path].map(|p| p.display());
        format!(
            "can't find `{target_name}` {target_kind} at default paths, but found a file at `{wrong_path}`.\n\
             Perhaps rename the file to `{possible_path}` for target auto-discovery, \
             or specify {target_kind}.path if you want to use a non-default path.{msg}",
        )
    } else {
        let [path_file, path_dir] = possible_paths.each_ref().map(|p| p.display());
        format!(
            "can't find `{target_name}` {target_kind} at `{path_file}` or `{path_dir}`. \
             Please specify {target_kind}.path if you want to use a non-default path.{msg}"
        )
    }
}

fn target_path(
    target: &TomlTarget,
    inferred: &[(String, PathBuf)],
    target_kind: &str,
    package_root: &Path,
    edition: Edition,
    legacy_path: &mut dyn FnMut(&TomlTarget) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(ref path) = target.path {
        // Should we verify that this path exists here?
        return Ok(path.0.clone());
    }
    let name = name_or_panic(target).to_owned();

    let mut matching = inferred
        .iter()
        .filter(|(n, _)| n == &name)
        .map(|(_, p)| p.clone());

    let first = matching.next();
    let second = matching.next();
    match (first, second) {
        (Some(path), None) => Ok(path),
        (None, None) => {
            if edition == Edition::Edition2015 {
                if let Some(path) = legacy_path(target) {
                    return Ok(path);
                }
            }
            Err(target_path_not_found_error_message(
                package_root,
                target,
                target_kind,
                inferred,
            ))
        }
        (Some(p0), Some(p1)) => {
            if edition == Edition::Edition2015 {
                if let Some(path) = legacy_path(target) {
                    return Ok(path);
                }
            }
            Err(format!(
                "\
cannot infer path for `{}` {}
Cargo doesn't know which to use because multiple target files found at `{}` and `{}`.",
                name_or_panic(target),
                target_kind,
                p0.strip_prefix(package_root).unwrap_or(&p0).display(),
                p1.strip_prefix(package_root).unwrap_or(&p1).display(),
            ))
        }
        (None, Some(_)) => unreachable!(),
    }
}

/// Returns the path to the build script if one exists for this crate.
#[tracing::instrument(skip_all)]
pub fn normalize_build(
    build: Option<&TomlPackageBuild>,
    package_root: &Path,
) -> CargoResult<Option<TomlPackageBuild>> {
    const BUILD_RS: &str = "build.rs";
    match build {
        None => {
            // If there is a `build.rs` file next to the `Cargo.toml`, assume it is
            // a build script.
            let build_rs = package_root.join(BUILD_RS);
            if build_rs.is_file() {
                Ok(Some(TomlPackageBuild::SingleScript(BUILD_RS.to_owned())))
            } else {
                Ok(Some(TomlPackageBuild::Auto(false)))
            }
        }
        // Explicitly no build script.
        Some(TomlPackageBuild::Auto(false)) => Ok(build.cloned()),
        Some(TomlPackageBuild::SingleScript(build_file)) => {
            let build_file = paths::normalize_path(Path::new(build_file));
            let build = build_file.into_os_string().into_string().expect(
                "`build_file` started as a String and `normalize_path` shouldn't have changed that",
            );
            Ok(Some(TomlPackageBuild::SingleScript(build)))
        }
        Some(TomlPackageBuild::Auto(true)) => {
            Ok(Some(TomlPackageBuild::SingleScript(BUILD_RS.to_owned())))
        }
        Some(TomlPackageBuild::MultipleScript(_scripts)) => Ok(build.cloned()),
    }
}

fn name_or_panic(target: &TomlTarget) -> &str {
    target
        .name
        .as_deref()
        .unwrap_or_else(|| panic!("target name is required"))
}

fn validate_lib_name(target: &TomlTarget, warnings: &mut Vec<String>) -> CargoResult<()> {
    validate_target_name(target, TARGET_KIND_HUMAN_LIB, TARGET_KIND_LIB, warnings)?;
    let name = name_or_panic(target);
    if name.contains('-') {
        anyhow::bail!("library target names cannot contain hyphens: {}", name)
    }

    Ok(())
}

fn validate_bin_name(bin: &TomlTarget, warnings: &mut Vec<String>) -> CargoResult<()> {
    validate_target_name(bin, TARGET_KIND_HUMAN_BIN, TARGET_KIND_BIN, warnings)?;
    let name = name_or_panic(bin).to_owned();
    if restricted_names::is_conflicting_artifact_name(&name) {
        anyhow::bail!(
            "the binary target name `{name}` is forbidden, \
                 it conflicts with cargo's build directory names",
        )
    }

    Ok(())
}

fn validate_target_name(
    target: &TomlTarget,
    target_kind_human: &str,
    target_kind: &str,
    warnings: &mut Vec<String>,
) -> CargoResult<()> {
    match target.name {
        Some(ref name) => {
            if name.trim().is_empty() {
                anyhow::bail!("{} target names cannot be empty", target_kind_human)
            }
            if cfg!(windows) && restricted_names::is_windows_reserved(name) {
                warnings.push(format!(
                    "{} target `{}` is a reserved Windows filename, \
                        this target will not work on Windows platforms",
                    target_kind_human, name
                ));
            }
        }
        None => anyhow::bail!(
            "{} target {}.name is required",
            target_kind_human,
            target_kind
        ),
    }

    Ok(())
}

fn validate_bin_proc_macro(
    target: &TomlTarget,
    edition: Edition,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> CargoResult<()> {
    if target.proc_macro() == Some(true) {
        let name = name_or_panic(target);
        errors.push(format!(
            "the target `{}` is a binary and can't have `proc-macro` \
                 set `true`",
            name
        ));
    } else {
        validate_proc_macro(target, TARGET_KIND_HUMAN_BIN, edition, warnings)?;
    }
    Ok(())
}

fn validate_proc_macro(
    target: &TomlTarget,
    kind: &str,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> CargoResult<()> {
    deprecated_underscore(
        &target.proc_macro2,
        &target.proc_macro,
        "proc-macro",
        name_or_panic(target),
        format!("{kind} target").as_str(),
        edition,
        warnings,
    )
}

fn validate_bin_crate_types(
    target: &TomlTarget,
    edition: Edition,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> CargoResult<()> {
    if let Some(crate_types) = target.crate_types() {
        if !crate_types.is_empty() {
            let name = name_or_panic(target);
            errors.push(format!(
                "the target `{}` is a binary and can't have any \
                     crate-types set (currently \"{}\")",
                name,
                crate_types.join(", ")
            ));
        } else {
            validate_crate_types(target, TARGET_KIND_HUMAN_BIN, edition, warnings)?;
        }
    }
    Ok(())
}

fn validate_crate_types(
    target: &TomlTarget,
    kind: &str,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> CargoResult<()> {
    deprecated_underscore(
        &target.crate_type2,
        &target.crate_type,
        "crate-type",
        name_or_panic(target),
        format!("{kind} target").as_str(),
        edition,
        warnings,
    )
}
