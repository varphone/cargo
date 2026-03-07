use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::btree_map::Entry;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use cargo_platform::Cfg;
use cargo_util::{ProcessBuilder, paths};
use serde::Serialize;
use walkdir::WalkDir;

use super::build_config::CompileMode;
use super::build_context::FileFlavor;
use super::custom_build;
use super::custom_build::{LibraryPath, LinkArgTarget};
use super::fingerprint;
use super::job_queue::{JobState, Work};
use super::{BuildOutput, BuildRunner, BuildScriptOutputs, Unit, UnitHash};
use crate::core::manifest::TargetSourcePath;
use crate::core::profiles::{Lto, StripInner};
use crate::util::context::StringList;
use crate::util::errors::CargoResult;
use crate::util::internal;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum NativeCompilerFamily {
    GnuLike,
    MsvcLike,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeSourceLanguage {
    C,
    Cpp,
}

const NATIVE_SOURCE_EXTENSIONS: &[&str] = &["c", "cpp", "cc", "cxx"];
const NATIVE_HEADER_EXTENSIONS: &[&str] = &["h", "hh", "hpp", "hxx", "inc", "ipp", "tpp"];

#[derive(Clone, Debug)]
struct NativeToolchain {
    cc: PathBuf,
    cxx: PathBuf,
    ar: Option<PathBuf>,
    family: NativeCompilerFamily,
    env: BTreeMap<String, std::ffi::OsString>,
    init_script: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CompileCommand {
    directory: PathBuf,
    file: PathBuf,
    arguments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct CompileCommandCandidate {
    key: (PathBuf, PathBuf),
    command: CompileCommand,
    priority: (u8, u8, PathBuf),
}

#[derive(Clone, Debug, Default)]
struct NativeLinkInputs {
    dependency_libraries: Vec<PathBuf>,
    manifest_library_search_dirs: Vec<PathBuf>,
    manifest_library_links: Vec<String>,
    manifest_link_args: Vec<String>,
    build_outputs: Vec<BuildOutput>,
}

#[derive(Clone, Debug)]
struct NativeDependencyDescriptor {
    output_path: Option<PathBuf>,
    public_include_dir: Option<PathBuf>,
    build_script_metadatas: Option<Vec<UnitHash>>,
    previous_build_outputs: Vec<BuildOutput>,
}

#[derive(Clone, Debug)]
struct NativeLinkContext {
    is_bin: bool,
    is_cdylib: bool,
    is_test: bool,
    is_bench: bool,
    is_example: bool,
    target_name: String,
    mode: CompileMode,
}

pub(crate) fn compile(build_runner: &mut BuildRunner<'_, '_>, unit: &Unit) -> CargoResult<Work> {
    if unit.target.is_header_only() {
        return Ok(Work::noop());
    }

    let toolchain = detect_toolchain(build_runner, unit)?;
    let outputs = build_runner.outputs(unit)?;
    let dep_info_loc = fingerprint::dep_info_loc(build_runner, unit);
    let build_root = build_runner.bcx.ws.build_dir().into_path_unlocked();
    let dependency_descriptors = transitive_native_dependency_descriptors(build_runner, unit)?;
    let build_script_outputs = Arc::clone(&build_runner.build_script_outputs);
    let declared_include_metadata = build_runner.find_build_script_metadatas(unit);
    let previous_declared_include_outputs =
        custom_build::previous_build_outputs(build_runner, unit);
    let link_context = NativeLinkContext {
        is_bin: unit.target.is_bin(),
        is_cdylib: unit.target.is_cdylib(),
        is_test: unit.target.is_test(),
        is_bench: unit.target.is_bench(),
        is_example: unit.target.is_exe_example(),
        target_name: unit.target.name().to_string(),
        mode: unit.mode,
    };
    let sbom_files = build_runner.sbom_output_files(unit)?;
    let sbom = (!sbom_files.is_empty())
        .then(|| super::output_sbom::build_sbom(build_runner, unit))
        .transpose()?;
    let unit = unit.clone();
    let source = match unit.target.src_path() {
        TargetSourcePath::Path(path) => path.clone(),
        TargetSourcePath::Metabuild => anyhow::bail!("native targets require a source path"),
    };
    let package_root = unit.pkg.root().to_path_buf();
    let target_triple = build_runner
        .bcx
        .target_data
        .short_name(&unit.kind)
        .to_string();
    let target_name = unit.target.name().to_string();
    let is_staticlib = unit.target.is_staticlib();
    let is_sharedlib = unit.target.is_cdylib() || unit.target.is_dylib();
    let native_include_root = unit.target.native_include_root(&package_root);
    let native_sources_root = unit.target.native_sources_root(&package_root);
    let native_manifest_include_dirs = unit.target.native_include_dirs().to_vec();
    let native_manifest_define_args =
        native_define_args(unit.target.native_defines(), toolchain.family);
    let native_manifest_link_inputs = NativeLinkInputs {
        dependency_libraries: Vec::new(),
        manifest_library_search_dirs: unit.target.native_link_search().to_vec(),
        manifest_library_links: unit.target.native_link_libraries().to_vec(),
        manifest_link_args: unit.target.native_link_args().to_vec(),
        build_outputs: Vec::new(),
    };
    let sources = collect_target_sources(&source, native_sources_root.as_deref())?;
    let profile = unit.profile.clone();
    let cpp_config_args = native_cppflags_config_flags(build_runner.bcx.gctx, &target_triple)?;
    let c_config_args = native_c_compile_config_flags(build_runner.bcx.gctx, &target_triple)?;
    let cxx_config_args = native_cxx_compile_config_flags(build_runner.bcx.gctx, &target_triple)?;
    let link_config_args = native_link_config_flags(build_runner.bcx.gctx, &target_triple)?;
    let feature_args = native_feature_args(&unit, toolchain.family);
    let cfg_args = native_cfg_args(build_runner, &unit, toolchain.family);
    let crt_static_enabled = native_crt_static_enabled(build_runner, &unit);

    Ok(Work::new(move |state| {
        let declared_include_dirs = declared_include_dirs_from_metadata(
            &build_script_outputs,
            declared_include_metadata.as_deref(),
            &previous_declared_include_outputs,
        );
        let dependency_include_dirs = dependency_include_dirs_from_descriptors(
            &build_script_outputs,
            &dependency_descriptors,
        );
        let native_link_inputs = native_link_inputs_from_descriptors(
            &build_script_outputs,
            &dependency_descriptors,
            declared_include_metadata.as_deref(),
            &previous_declared_include_outputs,
        );
        let mut native_link_inputs = native_link_inputs;
        native_link_inputs.manifest_library_search_dirs.extend(
            native_manifest_link_inputs
                .manifest_library_search_dirs
                .iter()
                .cloned(),
        );
        native_link_inputs.manifest_library_links.extend(
            native_manifest_link_inputs
                .manifest_library_links
                .iter()
                .cloned(),
        );
        native_link_inputs.manifest_link_args.extend(
            native_manifest_link_inputs
                .manifest_link_args
                .iter()
                .cloned(),
        );
        let include_dirs = include_dirs(
            &package_root,
            &sources,
            native_include_root.as_deref(),
            &native_manifest_include_dirs,
            &declared_include_dirs,
            &dependency_include_dirs,
        );

        for output in outputs.iter() {
            if let Some(parent) = output.path.parent() {
                paths::create_dir_all(parent)?;
            }
            if let Some(parent) = output.hardlink.as_ref().and_then(|path| path.parent()) {
                paths::create_dir_all(parent)?;
            }
            if let Some(parent) = output.export_path.as_ref().and_then(|path| path.parent()) {
                paths::create_dir_all(parent)?;
            }
        }

        let primary_output = outputs
            .iter()
            .find(|output| output.flavor == FileFlavor::Normal)
            .map(|output| output.path.clone())
            .ok_or_else(|| internal("native target is missing a primary output"))?;
        let objects_dir = primary_output.with_file_name(format!(
            "{}.native",
            primary_output.file_name().unwrap().to_string_lossy()
        ));
        let objects = object_paths(&package_root, &sources, &objects_dir, toolchain.family);
        let has_cpp_sources = sources
            .iter()
            .any(|source| source_language(source) == Some(NativeSourceLanguage::Cpp));
        let mut compile_profile_args = native_profile_compile_args(&profile, toolchain.family);
        compile_profile_args.extend(native_crt_compile_args(
            crt_static_enabled,
            &profile,
            toolchain.family,
        ));
        compile_profile_args.extend(native_shared_compile_args(is_sharedlib, toolchain.family));
        let mut link_profile_args = native_profile_link_args(&profile, toolchain.family);
        link_profile_args.extend(native_crt_link_args(
            crt_static_enabled,
            &target_triple,
            toolchain.family,
            has_cpp_sources,
        ));
        let link_env_args = native_link_env_flags(&target_triple, &link_config_args);

        for object in &objects {
            if let Some(parent) = object.parent() {
                paths::create_dir_all(parent)?;
            }
        }

        let depfiles = depfile_paths(&objects, toolchain.family);

        for ((source, object), depfile) in sources.iter().zip(objects.iter()).zip(depfiles.iter()) {
            let language = source_language(source)
                .ok_or_else(|| internal("native target contains a non-native source file"))?;
            let compile_env_args = native_compile_env_flags(
                &target_triple,
                language,
                &cpp_config_args,
                &c_config_args,
                &cxx_config_args,
            );
            compile_object(
                state,
                &toolchain,
                &compile_profile_args,
                &compile_env_args,
                &feature_args,
                &cfg_args,
                &native_manifest_define_args,
                language,
                source,
                object,
                depfile.as_deref(),
                &include_dirs,
                &target_name,
            )?;
        }

        if is_staticlib {
            archive_library(state, &toolchain, &objects, &primary_output, &target_name)?;
        } else if is_sharedlib {
            link_shared_library(
                state,
                &toolchain,
                &link_profile_args,
                &link_env_args,
                has_cpp_sources,
                &objects,
                &primary_output,
                &native_link_inputs,
                link_context,
                &target_name,
            )?;
        } else {
            link_binary(
                state,
                &toolchain,
                &link_profile_args,
                &link_env_args,
                has_cpp_sources,
                &objects,
                &primary_output,
                &native_link_inputs,
                link_context,
                &target_name,
            )?;
        }

        let tracked_paths = tracked_input_paths(&toolchain, &sources, &depfiles, &include_dirs)?;

        fingerprint::write_native_dep_info(
            &build_root,
            &package_root,
            &dep_info_loc,
            &tracked_paths,
        )?;

        if let Some(sbom) = &sbom {
            for file in &sbom_files {
                tracing::debug!("writing sbom to {}", file.display());
                let outfile = io::BufWriter::new(paths::create(file)?);
                serde_json::to_writer(outfile, sbom)?;
            }
        }

        Ok(())
    }))
}

pub(crate) fn toolchain_fingerprint(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<String> {
    let toolchain = detect_toolchain(build_runner, unit)?;
    let target_triple = build_runner.bcx.target_data.short_name(&unit.kind);
    let crt_static_enabled = native_crt_static_enabled(build_runner, unit);
    let cpp_config_args = native_cppflags_config_flags(build_runner.bcx.gctx, target_triple)?;
    let c_config_args = native_c_compile_config_flags(build_runner.bcx.gctx, target_triple)?;
    let cxx_config_args = native_cxx_compile_config_flags(build_runner.bcx.gctx, target_triple)?;
    let link_config_args = native_link_config_flags(build_runner.bcx.gctx, target_triple)?;
    let flag_env_hash = crate::util::hash_u64(flag_env_values(target_triple));
    let config_flag_hash = crate::util::hash_u64((
        cpp_config_args,
        c_config_args,
        cxx_config_args,
        link_config_args,
    ));
    let fingerprint = crate::util::hash_u64((
        (
            (
                build_runner.bcx.target_data.short_name(&unit.kind),
                crt_static_enabled,
                target_native_tool_env(target_triple, "CC"),
                target_native_tool_env(target_triple, "CXX"),
                target_native_tool_env(target_triple, "AR"),
                env::var_os("CC"),
                env::var_os("CXX"),
                env::var_os("AR"),
            ),
            (
                build_native_tool_config_value(build_runner.bcx.gctx, "cc")?,
                build_native_tool_config_value(build_runner.bcx.gctx, "cxx")?,
                build_native_tool_config_value(build_runner.bcx.gctx, "ar")?,
                target_native_tool_config_value(build_runner.bcx.gctx, target_triple, "cc")?,
                target_native_tool_config_value(build_runner.bcx.gctx, target_triple, "cxx")?,
                target_native_tool_config_value(build_runner.bcx.gctx, target_triple, "ar")?,
            ),
            (
                target_cargo_native_env(target_triple, "MSVC_VCVARSALL"),
                env::var_os("CARGO_NATIVE_MSVC_VCVARSALL"),
                flag_env_hash,
                config_flag_hash,
            ),
        ),
        (
            toolchain.family,
            crate::util::hash_u64(&toolchain.env),
            tool_file_fingerprint(&toolchain.cc)?,
            tool_file_fingerprint(&toolchain.cxx)?,
            toolchain
                .ar
                .as_deref()
                .map(tool_file_fingerprint)
                .transpose()?,
            toolchain
                .init_script
                .as_deref()
                .map(tool_file_fingerprint)
                .transpose()?,
        ),
    ));
    Ok(format!("native-toolchain:{fingerprint:016x}"))
}

pub(crate) fn write_compile_commands(build_runner: &BuildRunner<'_, '_>) -> CargoResult<()> {
    let mut candidates = build_runner
        .bcx
        .unit_graph
        .keys()
        .filter(|unit| {
            unit.target.is_native() && !unit.mode.is_doc() && !unit.mode.is_run_custom_build()
        })
        .map(|unit| compile_commands_for_unit(build_runner, unit))
        .collect::<CargoResult<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| left.priority.cmp(&right.priority));

    let mut deduped = BTreeMap::new();
    for candidate in candidates {
        match deduped.entry(candidate.key.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(candidate.command);
            }
            Entry::Occupied(_) => {}
        }
    }

    let commands = deduped.into_values().collect::<Vec<_>>();

    let path = build_runner.bcx.ws.root().join("compile_commands.json");
    let mut json = serde_json::to_string_pretty(&commands)?;
    json.push('\n');
    paths::write(&path, json)?;
    Ok(())
}

fn compile_commands_for_unit(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<Vec<CompileCommandCandidate>> {
    if unit.target.is_header_only() {
        return Ok(Vec::new());
    }

    let toolchain = detect_toolchain(build_runner, unit)?;
    let outputs = build_runner.outputs(unit)?;
    let declared_include_metadata = build_runner.find_build_script_metadatas(unit);
    let previous_declared_include_outputs =
        custom_build::previous_build_outputs(build_runner, unit);
    let source = match unit.target.src_path() {
        TargetSourcePath::Path(path) => path.clone(),
        TargetSourcePath::Metabuild => anyhow::bail!("native targets require a source path"),
    };
    let package_root = unit.pkg.root().to_path_buf();
    let native_include_root = unit.target.native_include_root(&package_root);
    let native_sources_root = unit.target.native_sources_root(&package_root);
    let native_manifest_include_dirs = unit.target.native_include_dirs().to_vec();
    let native_manifest_define_args =
        native_define_args(unit.target.native_defines(), toolchain.family);
    let sources = collect_target_sources(&source, native_sources_root.as_deref())?;
    let declared_include_dirs = declared_include_dirs_from_metadata(
        &build_runner.build_script_outputs,
        declared_include_metadata.as_deref(),
        &previous_declared_include_outputs,
    );
    let dependency_include_dirs = dependency_include_dirs_from_descriptors(
        &build_runner.build_script_outputs,
        &transitive_native_dependency_descriptors(build_runner, unit)?,
    );
    let include_dirs = include_dirs(
        &package_root,
        &sources,
        native_include_root.as_deref(),
        &native_manifest_include_dirs,
        &declared_include_dirs,
        &dependency_include_dirs,
    );

    let primary_output = outputs
        .iter()
        .find(|output| output.flavor == FileFlavor::Normal)
        .map(|output| output.path.clone())
        .ok_or_else(|| internal("native target is missing a primary output"))?;
    let objects_dir = primary_output.with_file_name(format!(
        "{}.native",
        primary_output.file_name().unwrap().to_string_lossy()
    ));
    let objects = object_paths(&package_root, &sources, &objects_dir, toolchain.family);
    let depfiles = depfile_paths(&objects, toolchain.family);
    let mut compile_profile_args = native_profile_compile_args(&unit.profile, toolchain.family);
    compile_profile_args.extend(native_shared_compile_args(
        unit.target.is_cdylib() || unit.target.is_dylib(),
        toolchain.family,
    ));
    let feature_args = native_feature_args(unit, toolchain.family);
    let cfg_args = native_cfg_args(build_runner, unit, toolchain.family);
    let target_triple = build_runner.bcx.target_data.short_name(&unit.kind);
    let cpp_config_args = native_cppflags_config_flags(build_runner.bcx.gctx, target_triple)?;
    let c_config_args = native_c_compile_config_flags(build_runner.bcx.gctx, target_triple)?;
    let cxx_config_args = native_cxx_compile_config_flags(build_runner.bcx.gctx, target_triple)?;
    let package_priority = if build_runner
        .bcx
        .roots
        .iter()
        .any(|root| root.pkg.package_id() == unit.pkg.package_id())
    {
        0
    } else {
        1
    };
    let mode_priority = compile_mode_priority(unit.mode);

    sources
        .into_iter()
        .zip(objects)
        .zip(depfiles)
        .map(|((source, object), depfile)| {
            let language = source_language(&source)
                .ok_or_else(|| internal("native target contains a non-native source file"))?;
            let compile_env_args = native_compile_env_flags(
                target_triple,
                language,
                &cpp_config_args,
                &c_config_args,
                &cxx_config_args,
            );
            let cmd = build_compile_object_command(
                &toolchain,
                &compile_profile_args,
                &compile_env_args,
                &feature_args,
                &cfg_args,
                &native_manifest_define_args,
                language,
                &source,
                &object,
                depfile.as_deref(),
                &include_dirs,
            );
            Ok(CompileCommandCandidate {
                key: (package_root.clone(), source.clone()),
                priority: (package_priority, mode_priority, object.clone()),
                command: CompileCommand {
                    directory: package_root.clone(),
                    file: source,
                    arguments: command_arguments(&cmd),
                    output: Some(object),
                },
            })
        })
        .collect::<CargoResult<Vec<_>>>()
}

fn transitive_native_dependency_descriptors(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<Vec<NativeDependencyDescriptor>> {
    let mut descriptors = Vec::new();

    for dep_unit in transitive_native_dependency_units(build_runner, unit) {
        let output_path = build_runner
            .outputs(&dep_unit)?
            .iter()
            .find(|output| output.flavor == FileFlavor::Normal)
            .map(|output| output.path.clone());

        descriptors.push(NativeDependencyDescriptor {
            output_path,
            public_include_dir: dep_unit.target.native_include_root(dep_unit.pkg.root()),
            build_script_metadatas: build_runner.find_build_script_metadatas(&dep_unit),
            previous_build_outputs: custom_build::previous_build_outputs(build_runner, &dep_unit),
        });
    }

    Ok(descriptors)
}

fn transitive_native_dependency_units(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut visited = BTreeSet::new();
    collect_transitive_native_dependency_units(build_runner, unit, &mut visited, &mut units);
    units
}

fn collect_transitive_native_dependency_units(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
    visited: &mut BTreeSet<Unit>,
    units: &mut Vec<Unit>,
) {
    for dep in build_runner.unit_deps(unit) {
        if dep.unit.mode.is_doc()
            || dep.unit.mode.is_run_custom_build()
            || !dep.unit.target.is_native()
            || (!dep.unit.target.is_linkable() && !dep.unit.target.is_header_only())
            || !visited.insert(dep.unit.clone())
        {
            continue;
        }

        units.push(dep.unit.clone());
        collect_transitive_native_dependency_units(build_runner, &dep.unit, visited, units);
    }
}

fn dependency_include_dirs_from_descriptors(
    build_script_outputs: &Arc<Mutex<BuildScriptOutputs>>,
    dependency_descriptors: &[NativeDependencyDescriptor],
) -> Vec<PathBuf> {
    let mut include_dirs = Vec::new();

    for descriptor in dependency_descriptors {
        let declared_include_dirs = declared_include_dirs_from_metadata(
            build_script_outputs,
            descriptor.build_script_metadatas.as_deref(),
            &descriptor.previous_build_outputs,
        );

        if declared_include_dirs.is_empty() {
            if let Some(public_include_dir) = &descriptor.public_include_dir
                && public_include_dir.is_dir()
            {
                push_unique_path(&mut include_dirs, public_include_dir.clone());
            }
        } else {
            for include_dir in declared_include_dirs {
                push_unique_path(&mut include_dirs, include_dir);
            }
        }
    }

    include_dirs
}

fn native_link_inputs_from_descriptors(
    build_script_outputs: &Arc<Mutex<BuildScriptOutputs>>,
    dependency_descriptors: &[NativeDependencyDescriptor],
    current_build_script_metadatas: Option<&[UnitHash]>,
    current_previous_build_outputs: &[BuildOutput],
) -> NativeLinkInputs {
    let mut inputs = NativeLinkInputs::default();

    for descriptor in dependency_descriptors {
        if let Some(output_path) = &descriptor.output_path {
            push_unique_path(&mut inputs.dependency_libraries, output_path.clone());
        }
        inputs.build_outputs.extend(build_outputs_from_metadata(
            build_script_outputs,
            descriptor.build_script_metadatas.as_deref(),
            &descriptor.previous_build_outputs,
        ));
    }

    inputs.build_outputs.extend(build_outputs_from_metadata(
        build_script_outputs,
        current_build_script_metadatas,
        current_previous_build_outputs,
    ));
    inputs.build_outputs.sort();
    inputs.build_outputs.dedup();
    inputs
}

fn build_outputs_from_metadata(
    build_script_outputs: &Arc<Mutex<BuildScriptOutputs>>,
    metadata_vec: Option<&[UnitHash]>,
    previous_outputs: &[BuildOutput],
) -> Vec<BuildOutput> {
    let mut outputs = Vec::new();

    if let Some(metadata_vec) = metadata_vec {
        let build_script_outputs = build_script_outputs.lock().unwrap();
        for metadata in metadata_vec {
            if let Some(output) = build_script_outputs.get(*metadata) {
                outputs.push(output.clone());
            }
        }
    }

    if outputs.is_empty() {
        outputs.extend_from_slice(previous_outputs);
    }

    outputs.sort();
    outputs.dedup();
    outputs
}

fn detect_toolchain(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
) -> CargoResult<NativeToolchain> {
    let triple = build_runner.bcx.target_data.short_name(&unit.kind);
    let host = build_runner.bcx.host_triple();
    let gctx = build_runner.bcx.gctx;
    let mut tool_env = BTreeMap::new();
    let mut init_script = None;
    let cc_env = target_native_tool_env(triple, "CC");
    let cxx_env = target_native_tool_env(triple, "CXX");
    let cc_config = target_native_tool_config(gctx, triple, "cc")?
        .or_else(|| build_native_tool_config(gctx, "cc"));
    let cxx_config = target_native_tool_config(gctx, triple, "cxx")?
        .or_else(|| build_native_tool_config(gctx, "cxx"));
    let ar_config = target_native_tool_config(gctx, triple, "ar")?
        .or_else(|| build_native_tool_config(gctx, "ar"));
    if cc_env.is_none()
        && cxx_env.is_none()
        && cc_config.is_none()
        && cxx_config.is_none()
        && triple.contains("windows-msvc")
        && resolve_tool(preferred_cc_candidates(triple)).is_none()
        && resolve_tool(preferred_cxx_candidates(triple)).is_none()
    {
        if let Some((env_map, script)) = load_msvc_environment(triple, &host)? {
            tool_env = env_map;
            init_script = Some(script);
        }
    }

    let cc = if let Some(cc) = cc_env
        .or_else(|| env::var_os("CC"))
        .map(PathBuf::from)
        .or(cc_config)
    {
        cc
    } else {
        resolve_tool_with_env(preferred_cc_candidates(triple), &tool_env).ok_or_else(|| {
            anyhow::format_err!(
                "failed to find a C compiler for target `{}`; set CC, `build.native-cc`, `target.{triple}.native-cc`, or install one of: {}",
                triple,
                preferred_cc_candidates(triple).join(", ")
            )
        })?
    };

    let cxx = if let Some(cxx) = cxx_env
        .or_else(|| env::var_os("CXX"))
        .map(PathBuf::from)
        .or(cxx_config)
    {
        cxx
    } else {
        resolve_tool_with_env(preferred_cxx_candidates(triple), &tool_env)
            .unwrap_or_else(|| cc.clone())
    };

    let family = compiler_family(&cxx);

    let ar = if let Some(ar) = target_native_tool_env(triple, "AR")
        .or_else(|| env::var_os("AR"))
        .map(PathBuf::from)
        .or(ar_config)
    {
        Some(ar)
    } else {
        match family {
            NativeCompilerFamily::MsvcLike => {
                resolve_tool_with_env(&["lib.exe", "llvm-lib.exe"], &tool_env)
            }
            NativeCompilerFamily::GnuLike => resolve_tool_with_env(&["ar", "llvm-ar"], &tool_env),
        }
    };

    Ok(NativeToolchain {
        cc,
        cxx,
        ar,
        family,
        env: tool_env,
        init_script,
    })
}

fn preferred_cc_candidates(triple: &str) -> &'static [&'static str] {
    if triple.contains("windows-msvc") {
        &["cl.exe", "clang-cl.exe", "clang.exe", "gcc.exe"]
    } else if triple.contains("windows-gnullvm") {
        &["clang.exe", "gcc.exe", "clang-cl.exe", "cl.exe"]
    } else if triple.contains("windows-gnu") {
        &["gcc.exe", "clang.exe", "cl.exe", "clang-cl.exe"]
    } else if triple.contains("windows") {
        &["cl.exe", "clang-cl.exe", "clang.exe", "gcc.exe"]
    } else {
        &["cc", "gcc", "clang"]
    }
}

fn preferred_cxx_candidates(triple: &str) -> &'static [&'static str] {
    if triple.contains("windows-msvc") {
        &["cl.exe", "clang-cl.exe", "clang++.exe", "g++.exe"]
    } else if triple.contains("windows-gnullvm") {
        &["clang++.exe", "g++.exe", "clang-cl.exe", "cl.exe"]
    } else if triple.contains("windows-gnu") {
        &["g++.exe", "clang++.exe", "cl.exe", "clang-cl.exe"]
    } else if triple.contains("windows") {
        &["cl.exe", "clang-cl.exe", "clang++.exe", "g++.exe"]
    } else {
        &["c++", "g++", "clang++"]
    }
}

fn resolve_tool(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .find_map(|tool| paths::resolve_executable(Path::new(tool)).ok())
}

fn resolve_tool_with_env(
    candidates: &[&str],
    tool_env: &BTreeMap<String, std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(path) = tool_env.get("PATH") {
        for dir in env::split_paths(path) {
            for tool in candidates {
                let candidate = dir.join(tool);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    resolve_tool(candidates)
}

fn load_msvc_environment(
    target_triple: &str,
    host_triple: &str,
) -> CargoResult<Option<(BTreeMap<String, std::ffi::OsString>, PathBuf)>> {
    let Some(vcvarsall) = find_vcvarsall()? else {
        return Ok(None);
    };

    let arch = vcvarsall_arch(target_triple, host_triple);
    let cmd = env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    let mut process = ProcessBuilder::new(cmd);
    process.arg("/d").arg("/c").arg(format!(
        "call \"{}\" {} >nul && set",
        vcvarsall.display(),
        arch
    ));
    let output = process.exec_with_output().with_context(|| {
        format!(
            "failed to initialize MSVC environment with `{}`",
            vcvarsall.display()
        )
    })?;

    let mut env_map = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.is_empty() || key.starts_with('=') {
            continue;
        }
        env_map.insert(key.to_string(), value.into());
    }
    Ok(Some((env_map, vcvarsall)))
}

fn find_vcvarsall() -> CargoResult<Option<PathBuf>> {
    if let Some(override_path) = env::var_os("CARGO_NATIVE_MSVC_VCVARSALL") {
        return Ok(Some(PathBuf::from(override_path)));
    }

    let Some(vswhere) = find_vswhere() else {
        return Ok(None);
    };
    let output = ProcessBuilder::new(vswhere)
        .args(&[
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-find",
            r"VC\Auxiliary\Build\vcvarsall.bat",
        ])
        .exec_with_output()?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from))
}

fn find_vswhere() -> Option<PathBuf> {
    paths::resolve_executable(Path::new("vswhere.exe"))
        .ok()
        .or_else(|| {
            let program_files_x86 =
                env::var_os("ProgramFiles(x86)").or_else(|| env::var_os("ProgramFiles"))?;
            let path = PathBuf::from(program_files_x86)
                .join("Microsoft Visual Studio")
                .join("Installer")
                .join("vswhere.exe");
            path.is_file().then_some(path)
        })
}

fn vcvarsall_arch(target_triple: &str, host_triple: &str) -> &'static str {
    let host_is_x64 = host_triple.starts_with("x86_64");
    if target_triple.starts_with("x86_64") {
        "x64"
    } else if target_triple.starts_with("i686") || target_triple.starts_with("i586") {
        if host_is_x64 { "amd64_x86" } else { "x86" }
    } else if target_triple.starts_with("aarch64") {
        if host_is_x64 { "amd64_arm64" } else { "arm64" }
    } else {
        "x64"
    }
}

fn compiler_family(cxx: &Path) -> NativeCompilerFamily {
    let cxx_name = cxx
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if cxx_name == "cl.exe"
        || cxx_name == "cl"
        || cxx_name == "clang-cl.exe"
        || cxx_name == "clang-cl"
        || cxx_name == "llvm-cl.exe"
        || cxx_name == "llvm-cl"
    {
        NativeCompilerFamily::MsvcLike
    } else {
        NativeCompilerFamily::GnuLike
    }
}

fn tool_file_fingerprint(path: &Path) -> CargoResult<(PathBuf, Option<u64>, Option<u128>)> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to read native tool metadata for `{}`",
                    path.display()
                )
            });
        }
    };
    let len = metadata.as_ref().map(|metadata| metadata.len());
    let modified = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok((path.to_path_buf(), len, modified))
}

fn declared_include_dirs_from_metadata(
    build_script_outputs: &Arc<Mutex<BuildScriptOutputs>>,
    metadata_vec: Option<&[UnitHash]>,
    previous_outputs: &[BuildOutput],
) -> Vec<PathBuf> {
    let mut include_dirs = Vec::new();
    if let Some(metadata_vec) = metadata_vec {
        let build_script_outputs = build_script_outputs.lock().unwrap();
        for metadata in metadata_vec {
            let Some(output) = build_script_outputs.get(*metadata) else {
                continue;
            };
            for (key, value) in &output.metadata {
                if key == "include" {
                    include_dirs.push(PathBuf::from(value));
                }
            }
        }
    }

    if include_dirs.is_empty() {
        for previous_output in previous_outputs {
            for (key, value) in &previous_output.metadata {
                if key == "include" {
                    include_dirs.push(PathBuf::from(value));
                }
            }
        }
    }

    include_dirs.sort();
    include_dirs.dedup();
    include_dirs
}

fn include_dirs(
    package_root: &Path,
    sources: &[PathBuf],
    native_include_root: Option<&Path>,
    native_manifest_include_dirs: &[PathBuf],
    declared_include_dirs: &[PathBuf],
    dependency_include_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut include_dirs = Vec::new();
    include_dirs.extend(declared_include_dirs.iter().cloned());
    if let Some(native_include_root) = native_include_root {
        include_dirs.push(native_include_root.to_path_buf());
    } else {
        let public_include = package_root.join("include");
        if public_include.is_dir() {
            include_dirs.push(public_include);
        }
    }
    include_dirs.extend(native_manifest_include_dirs.iter().cloned());
    for source in sources {
        if let Some(parent) = source.parent() {
            include_dirs.push(parent.to_path_buf());
        }
    }
    include_dirs.extend(dependency_include_dirs.iter().cloned());
    include_dirs.sort();
    include_dirs.dedup();
    include_dirs
}

fn collect_target_sources(source: &Path, sources_root: Option<&Path>) -> CargoResult<Vec<PathBuf>> {
    let mut sources = vec![source.to_path_buf()];
    if let Some(companion_root) = sources_root {
        if !companion_root.is_dir() {
            anyhow::bail!(
                "native companion source root `{}` is not a directory",
                companion_root.display()
            );
        }
        for entry in WalkDir::new(companion_root) {
            let entry = entry.with_context(|| {
                format!(
                    "failed to walk native source directory while building `{}`",
                    source.display()
                )
            })?;
            let path = entry.path();
            if !entry.file_type().is_file() || path == source || !is_native_source_path(path) {
                continue;
            }
            sources.push(path.to_path_buf());
        }
    }
    sources.sort();
    sources.dedup();
    Ok(sources)
}

fn source_language(path: &Path) -> Option<NativeSourceLanguage> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("c") => Some(NativeSourceLanguage::C),
        Some("cpp" | "cc" | "cxx") => Some(NativeSourceLanguage::Cpp),
        _ => None,
    }
}

fn is_native_source_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| NATIVE_SOURCE_EXTENSIONS.contains(&ext))
}

fn object_paths(
    package_root: &Path,
    sources: &[PathBuf],
    objects_dir: &Path,
    family: NativeCompilerFamily,
) -> Vec<PathBuf> {
    let object_extension = match family {
        NativeCompilerFamily::MsvcLike => "obj",
        NativeCompilerFamily::GnuLike => "o",
    };

    sources
        .iter()
        .map(|source| {
            let rel_source = source.strip_prefix(package_root).unwrap_or(source);
            objects_dir
                .join(rel_source)
                .with_extension(object_extension)
        })
        .collect()
}

fn depfile_paths(objects: &[PathBuf], family: NativeCompilerFamily) -> Vec<Option<PathBuf>> {
    match family {
        NativeCompilerFamily::GnuLike => objects
            .iter()
            .map(|object| {
                let file_name = object.file_name()?.to_string_lossy();
                Some(object.with_file_name(format!("{file_name}.d")))
            })
            .collect(),
        NativeCompilerFamily::MsvcLike => vec![None; objects.len()],
    }
}

fn compile_object(
    state: &JobState<'_, '_>,
    toolchain: &NativeToolchain,
    profile_args: &[OsString],
    env_args: &[OsString],
    feature_args: &[OsString],
    cfg_args: &[OsString],
    native_define_args: &[OsString],
    language: NativeSourceLanguage,
    source: &Path,
    object: &Path,
    depfile: Option<&Path>,
    include_dirs: &[PathBuf],
    target_name: &str,
) -> CargoResult<()> {
    let cmd = build_compile_object_command(
        toolchain,
        profile_args,
        env_args,
        feature_args,
        cfg_args,
        native_define_args,
        language,
        source,
        object,
        depfile,
        include_dirs,
    );
    run_command(state, cmd, target_name)
}

fn build_compile_object_command(
    toolchain: &NativeToolchain,
    profile_args: &[OsString],
    env_args: &[OsString],
    feature_args: &[OsString],
    cfg_args: &[OsString],
    native_define_args: &[OsString],
    language: NativeSourceLanguage,
    source: &Path,
    object: &Path,
    depfile: Option<&Path>,
    include_dirs: &[PathBuf],
) -> ProcessBuilder {
    let mut cmd = ProcessBuilder::new(compiler_for_source(toolchain, language));
    apply_toolchain_env(&mut cmd, toolchain);
    match toolchain.family {
        NativeCompilerFamily::GnuLike => {
            append_args(&mut cmd, profile_args);
            append_args(&mut cmd, env_args);
            append_args(&mut cmd, feature_args);
            append_args(&mut cmd, cfg_args);
            append_args(&mut cmd, native_define_args);
            cmd.arg("-c").arg(source).arg("-o").arg(object);
            if let Some(depfile) = depfile {
                cmd.arg("-MMD").arg("-MF").arg(depfile);
            }
            for include_dir in include_dirs {
                cmd.arg("-I").arg(include_dir);
            }
        }
        NativeCompilerFamily::MsvcLike => {
            cmd.arg("/nologo");
            append_args(&mut cmd, profile_args);
            append_args(&mut cmd, env_args);
            append_args(&mut cmd, feature_args);
            append_args(&mut cmd, cfg_args);
            append_args(&mut cmd, native_define_args);
            cmd.arg("/c")
                .arg(source)
                .arg(format!("/Fo{}", object.display()));
            for include_dir in include_dirs {
                cmd.arg(format!("/I{}", include_dir.display()));
            }
        }
    }
    cmd
}

fn archive_library(
    state: &JobState<'_, '_>,
    toolchain: &NativeToolchain,
    objects: &[PathBuf],
    library: &Path,
    target_name: &str,
) -> CargoResult<()> {
    let ar = toolchain.ar.as_ref().ok_or_else(|| {
        anyhow::format_err!(
            "failed to find an archiver for native C++ target `{}`; set the AR environment variable",
            target_name
        )
    })?;
    let mut cmd = ProcessBuilder::new(ar);
    apply_toolchain_env(&mut cmd, toolchain);
    match toolchain.family {
        NativeCompilerFamily::GnuLike => {
            cmd.arg("crs").arg(library).args(objects);
        }
        NativeCompilerFamily::MsvcLike => {
            cmd.arg("/nologo")
                .arg(format!("/OUT:{}", library.display()))
                .args(objects);
        }
    }
    run_command(state, cmd, target_name)
}

fn link_binary(
    state: &JobState<'_, '_>,
    toolchain: &NativeToolchain,
    profile_args: &[OsString],
    env_args: &[OsString],
    has_cpp_sources: bool,
    objects: &[PathBuf],
    executable: &Path,
    link_inputs: &NativeLinkInputs,
    link_context: NativeLinkContext,
    target_name: &str,
) -> CargoResult<()> {
    let mut cmd = ProcessBuilder::new(linker_for_target(toolchain, has_cpp_sources));
    apply_toolchain_env(&mut cmd, toolchain);
    match toolchain.family {
        NativeCompilerFamily::GnuLike => {
            cmd.args(objects);
            append_args(&mut cmd, profile_args);
            append_args(&mut cmd, env_args);
            apply_build_output_link_inputs(&mut cmd, toolchain.family, link_inputs, link_context);
            cmd.arg("-o").arg(executable);
            for library in &link_inputs.dependency_libraries {
                cmd.arg(library);
            }
        }
        NativeCompilerFamily::MsvcLike => {
            cmd.arg("/nologo").args(objects);
            append_args(&mut cmd, profile_args);
            append_args(&mut cmd, env_args);
            apply_build_output_link_inputs(&mut cmd, toolchain.family, link_inputs, link_context);
            cmd.arg(format!("/Fe{}", executable.display()));
            for library in &link_inputs.dependency_libraries {
                cmd.arg(library);
            }
        }
    }
    run_command(state, cmd, target_name)
}

fn link_shared_library(
    state: &JobState<'_, '_>,
    toolchain: &NativeToolchain,
    profile_args: &[OsString],
    env_args: &[OsString],
    has_cpp_sources: bool,
    objects: &[PathBuf],
    library: &Path,
    link_inputs: &NativeLinkInputs,
    link_context: NativeLinkContext,
    target_name: &str,
) -> CargoResult<()> {
    let mut cmd = ProcessBuilder::new(linker_for_target(toolchain, has_cpp_sources));
    apply_toolchain_env(&mut cmd, toolchain);
    match toolchain.family {
        NativeCompilerFamily::GnuLike => {
            cmd.args(objects);
            append_args(&mut cmd, profile_args);
            append_args(&mut cmd, env_args);
            apply_build_output_link_inputs(&mut cmd, toolchain.family, link_inputs, link_context);
            cmd.arg("-shared").arg("-o").arg(library);
            for dep in &link_inputs.dependency_libraries {
                cmd.arg(dep);
            }
        }
        NativeCompilerFamily::MsvcLike => {
            cmd.arg("/nologo").arg("/LD").args(objects);
            append_args(&mut cmd, profile_args);
            append_args(&mut cmd, env_args);
            apply_build_output_link_inputs(&mut cmd, toolchain.family, link_inputs, link_context);
            cmd.arg(format!("/Fe{}", library.display()));
            for dep in &link_inputs.dependency_libraries {
                cmd.arg(dep);
            }
        }
    }
    run_command(state, cmd, target_name)
}

fn apply_build_output_link_inputs(
    cmd: &mut ProcessBuilder,
    family: NativeCompilerFamily,
    link_inputs: &NativeLinkInputs,
    link_context: NativeLinkContext,
) {
    let mut library_paths = Vec::new();
    let mut library_links = Vec::new();
    let mut linker_args = Vec::new();

    for output in &link_inputs.build_outputs {
        library_paths.extend(output.library_paths.iter().cloned());
        library_links.extend(output.library_links.iter().cloned());
        linker_args.extend(
            output
                .linker_args
                .iter()
                .filter(|(link_type, _)| link_arg_applies_to(link_type, &link_context))
                .map(|(_, arg)| arg.clone()),
        );
    }

    library_paths.extend(
        link_inputs
            .manifest_library_search_dirs
            .iter()
            .cloned()
            .map(LibraryPath::External),
    );
    library_links.extend(link_inputs.manifest_library_links.iter().cloned());
    linker_args.extend(link_inputs.manifest_link_args.iter().cloned());

    library_paths.sort_by_key(|path| match path {
        LibraryPath::CargoArtifact(_) => 0,
        LibraryPath::External(_) => 1,
    });

    for path in library_paths {
        match family {
            NativeCompilerFamily::GnuLike => {
                cmd.arg("-L").arg(path.into_path_buf());
            }
            NativeCompilerFamily::MsvcLike => {
                cmd.arg(format!("/LIBPATH:{}", path.into_path_buf().display()));
            }
        }
    }

    for arg in linker_args {
        cmd.arg(arg);
    }

    for library in library_links {
        match family {
            NativeCompilerFamily::GnuLike => {
                cmd.arg(format!("-l{}", native_link_library_name(&library)));
            }
            NativeCompilerFamily::MsvcLike => {
                cmd.arg(format!("{}.lib", native_link_library_name(&library)));
            }
        }
    }
}

fn native_link_library_name(library: &str) -> &str {
    let library = library
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or(library);
    library
        .split_once(':')
        .map(|(name, _)| name)
        .unwrap_or(library)
}

fn link_arg_applies_to(link_type: &LinkArgTarget, link_context: &NativeLinkContext) -> bool {
    let is_test = link_context.mode.is_any_test();
    match link_type {
        LinkArgTarget::All => true,
        LinkArgTarget::Cdylib => !is_test && link_context.is_cdylib,
        LinkArgTarget::Bin => link_context.is_bin,
        LinkArgTarget::SingleBin(name) => link_context.is_bin && link_context.target_name == *name,
        LinkArgTarget::Test => link_context.is_test,
        LinkArgTarget::Bench => link_context.is_bench,
        LinkArgTarget::Example => link_context.is_example,
    }
}

fn compiler_for_source(toolchain: &NativeToolchain, language: NativeSourceLanguage) -> &Path {
    match language {
        NativeSourceLanguage::C => &toolchain.cc,
        NativeSourceLanguage::Cpp => &toolchain.cxx,
    }
}

fn linker_for_target(toolchain: &NativeToolchain, has_cpp_sources: bool) -> &Path {
    if has_cpp_sources {
        &toolchain.cxx
    } else {
        &toolchain.cc
    }
}

fn run_command(
    state: &JobState<'_, '_>,
    cmd: ProcessBuilder,
    target_name: &str,
) -> CargoResult<()> {
    state.running(&cmd);
    cmd.exec_with_streaming(
        &mut |line| state.stdout(line.to_owned()),
        &mut |line| state.stderr(line.to_owned()),
        false,
    )
    .map(drop)
    .map_err(|err| {
        err.context(format!(
            "failed to build native C++ target `{}`",
            target_name
        ))
    })
}

fn apply_toolchain_env(cmd: &mut ProcessBuilder, toolchain: &NativeToolchain) {
    for (key, value) in &toolchain.env {
        cmd.env(key, value);
    }
}

fn append_args(cmd: &mut ProcessBuilder, args: &[OsString]) {
    for arg in args {
        cmd.arg(arg);
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn native_feature_args(unit: &Unit, family: NativeCompilerFamily) -> Vec<OsString> {
    unit.features
        .iter()
        .map(|feature| native_feature_define_arg(feature, family))
        .collect()
}

fn native_feature_define_arg(feature: &str, family: NativeCompilerFamily) -> OsString {
    let feature = super::envify(feature);
    match family {
        NativeCompilerFamily::GnuLike => OsString::from(format!("-DCARGO_FEATURE_{feature}=1")),
        NativeCompilerFamily::MsvcLike => OsString::from(format!("/DCARGO_FEATURE_{feature}=1")),
    }
}

fn native_define_args(defines: &[String], family: NativeCompilerFamily) -> Vec<OsString> {
    defines
        .iter()
        .map(|define| native_define_arg(define, family))
        .collect()
}

fn native_define_arg(define: &str, family: NativeCompilerFamily) -> OsString {
    match family {
        NativeCompilerFamily::GnuLike => OsString::from(format!("-D{define}")),
        NativeCompilerFamily::MsvcLike => OsString::from(format!("/D{define}")),
    }
}

fn native_crt_static_enabled(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> bool {
    build_runner
        .bcx
        .target_data
        .cfg(unit.kind)
        .iter()
        .any(|cfg| matches!(cfg, Cfg::KeyPair(key, value) if key.as_str() == "target_feature" && value == "crt-static"))
}

fn native_crt_compile_args(
    crt_static_enabled: bool,
    profile: &crate::core::profiles::Profile,
    family: NativeCompilerFamily,
) -> Vec<OsString> {
    match family {
        NativeCompilerFamily::GnuLike => Vec::new(),
        NativeCompilerFamily::MsvcLike => {
            let runtime = match (crt_static_enabled, profile.debug_assertions) {
                (true, true) => "/MTd",
                (true, false) => "/MT",
                (false, true) => "/MDd",
                (false, false) => "/MD",
            };
            vec![runtime.into()]
        }
    }
}

fn native_crt_link_args(
    crt_static_enabled: bool,
    target_triple: &str,
    family: NativeCompilerFamily,
    has_cpp_sources: bool,
) -> Vec<OsString> {
    if !crt_static_enabled {
        return Vec::new();
    }

    match family {
        NativeCompilerFamily::GnuLike => native_gnu_crt_link_args(target_triple, has_cpp_sources),
        NativeCompilerFamily::MsvcLike => Vec::new(),
    }
}

fn native_gnu_crt_link_args(target_triple: &str, has_cpp_sources: bool) -> Vec<OsString> {
    if target_triple.contains("windows-gnullvm") {
        return Vec::new();
    }

    let mut args = vec![OsString::from("-static-libgcc")];
    if has_cpp_sources {
        args.push("-static-libstdc++".into());
    }
    args
}

fn native_cfg_args(
    build_runner: &BuildRunner<'_, '_>,
    unit: &Unit,
    family: NativeCompilerFamily,
) -> Vec<OsString> {
    let mut defines = BTreeSet::new();

    if unit.profile.debug_assertions {
        defines.insert(native_cfg_define_name("debug_assertions", family));
    }

    for cfg in build_runner.bcx.target_data.cfg(unit.kind) {
        match cfg {
            Cfg::Name(name) => {
                if name.as_str() == "debug_assertions" {
                    continue;
                }
                defines.insert(native_cfg_define_name(name.as_str(), family));
            }
            Cfg::KeyPair(key, value) => {
                defines.insert(native_cfg_define_key_pair(
                    key.as_str(),
                    value.as_str(),
                    family,
                ));
            }
        }
    }

    defines.into_iter().collect()
}

fn native_cfg_define_name(name: &str, family: NativeCompilerFamily) -> OsString {
    let name = super::envify(name);
    match family {
        NativeCompilerFamily::GnuLike => OsString::from(format!("-DCARGO_CFG_{name}=1")),
        NativeCompilerFamily::MsvcLike => OsString::from(format!("/DCARGO_CFG_{name}=1")),
    }
}

fn native_cfg_define_key_pair(key: &str, value: &str, family: NativeCompilerFamily) -> OsString {
    if value.is_empty() {
        return native_cfg_define_name(key, family);
    }

    let key = super::envify(key);
    let value = super::envify(value);
    match family {
        NativeCompilerFamily::GnuLike => OsString::from(format!("-DCARGO_CFG_{key}_{value}=1")),
        NativeCompilerFamily::MsvcLike => OsString::from(format!("/DCARGO_CFG_{key}_{value}=1")),
    }
}

fn command_arguments(cmd: &ProcessBuilder) -> Vec<String> {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn compile_mode_priority(mode: CompileMode) -> u8 {
    match mode {
        CompileMode::Build => 0,
        CompileMode::Test => 1,
        CompileMode::Check { test: false } => 2,
        CompileMode::Check { test: true } => 3,
        CompileMode::Doctest => 4,
        CompileMode::Doc | CompileMode::Docscrape | CompileMode::RunCustomBuild => 5,
    }
}

fn tracked_input_paths(
    toolchain: &NativeToolchain,
    sources: &[PathBuf],
    depfiles: &[Option<PathBuf>],
    include_dirs: &[PathBuf],
) -> CargoResult<Vec<PathBuf>> {
    let mut tracked_paths = sources.to_vec();

    match toolchain.family {
        NativeCompilerFamily::GnuLike => {
            for depfile in depfiles.iter().flatten() {
                let dep_info = fingerprint::parse_rustc_dep_info(depfile).with_context(|| {
                    format!("failed to parse native depfile `{}`", depfile.display())
                })?;
                tracked_paths.extend(dep_info.files.into_keys());
            }
        }
        NativeCompilerFamily::MsvcLike => {
            tracked_paths.extend(scan_include_dirs(include_dirs)?);
        }
    }

    tracked_paths.sort();
    tracked_paths.dedup();
    Ok(tracked_paths)
}

fn scan_include_dirs(include_dirs: &[PathBuf]) -> CargoResult<Vec<PathBuf>> {
    let mut headers = Vec::new();
    for include_dir in include_dirs {
        if !include_dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(include_dir) {
            let entry = entry.with_context(|| {
                format!(
                    "failed to walk native include directory `{}` while collecting dependencies",
                    include_dir.display()
                )
            })?;
            let path = entry.path();
            if entry.file_type().is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| NATIVE_HEADER_EXTENSIONS.contains(&ext))
            {
                headers.push(path.to_path_buf());
            }
        }
    }
    Ok(headers)
}

fn native_profile_compile_args(
    profile: &crate::core::profiles::Profile,
    family: NativeCompilerFamily,
) -> Vec<OsString> {
    let mut args = Vec::new();
    let opt_level = profile.opt_level.as_str();
    match family {
        NativeCompilerFamily::GnuLike => {
            args.push(match opt_level {
                "0" => "-O0".into(),
                "s" | "z" => "-Os".into(),
                level => format!("-O{level}").into(),
            });
            if profile.debuginfo.is_turned_on() {
                args.push("-g".into());
            }
            match profile.lto {
                Lto::Off | Lto::Bool(false) => {}
                Lto::Bool(true) => args.push("-flto".into()),
                Lto::Named(name) => args.push(format!("-flto={name}").into()),
            }
            if profile.debug_assertions {
                args.push("-UNDEBUG".into());
            } else {
                args.push("-DNDEBUG".into());
            }
        }
        NativeCompilerFamily::MsvcLike => {
            args.push(match opt_level {
                "0" => "/Od".into(),
                "1" | "s" | "z" => "/O1".into(),
                _ => "/O2".into(),
            });
            if profile.debuginfo.is_turned_on() {
                args.push("/Z7".into());
            }
            if !matches!(profile.lto, Lto::Off | Lto::Bool(false)) {
                args.push("/GL".into());
            }
            if profile.debug_assertions {
                args.push("/UNDEBUG".into());
            } else {
                args.push("/DNDEBUG".into());
            }
        }
    }
    args
}

fn native_shared_compile_args(is_sharedlib: bool, family: NativeCompilerFamily) -> Vec<OsString> {
    if is_sharedlib && matches!(family, NativeCompilerFamily::GnuLike) {
        vec!["-fPIC".into()]
    } else {
        Vec::new()
    }
}

fn native_profile_link_args(
    profile: &crate::core::profiles::Profile,
    family: NativeCompilerFamily,
) -> Vec<OsString> {
    let mut args = Vec::new();
    match family {
        NativeCompilerFamily::GnuLike => {
            match profile.lto {
                Lto::Off | Lto::Bool(false) => {}
                Lto::Bool(true) => args.push("-flto".into()),
                Lto::Named(name) => args.push(format!("-flto={name}").into()),
            }
            match profile.strip.into_inner() {
                StripInner::None => {}
                StripInner::Named(name) if name == "debuginfo" => args.push("-Wl,-S".into()),
                StripInner::Named(name) if name == "symbols" => args.push("-s".into()),
                StripInner::Named(name) => args.push(format!("-Wl,--strip-{name}").into()),
            }
        }
        NativeCompilerFamily::MsvcLike => {
            if profile.debuginfo.is_turned_on()
                && !matches!(profile.strip.into_inner(), StripInner::Named(name) if name == "debuginfo")
            {
                args.push("/DEBUG".into());
            }
            if !matches!(profile.lto, Lto::Off | Lto::Bool(false)) {
                args.push("/LTCG".into());
            }
        }
    }
    args
}

fn native_compile_env_flags(
    target_triple: &str,
    language: NativeSourceLanguage,
    cpp_config_args: &[OsString],
    c_config_args: &[OsString],
    cxx_config_args: &[OsString],
) -> Vec<OsString> {
    let mut args = cpp_config_args.to_vec();
    args.extend(
        match language {
            NativeSourceLanguage::C => c_config_args,
            NativeSourceLanguage::Cpp => cxx_config_args,
        }
        .iter()
        .cloned(),
    );
    args.extend(read_target_and_global_native_flag_env(
        target_triple,
        "ENCODED_NATIVE_CPPFLAGS",
        "NATIVE_CPPFLAGS",
        "CPPFLAGS",
    ));
    match language {
        NativeSourceLanguage::C => {
            args.extend(read_target_and_global_native_flag_env(
                target_triple,
                "ENCODED_NATIVE_CFLAGS",
                "NATIVE_CFLAGS",
                "CFLAGS",
            ));
        }
        NativeSourceLanguage::Cpp => {
            args.extend(read_target_and_global_native_flag_env(
                target_triple,
                "ENCODED_NATIVE_CXXFLAGS",
                "NATIVE_CXXFLAGS",
                "CXXFLAGS",
            ));
        }
    }
    args
}

fn native_link_env_flags(target_triple: &str, config_args: &[OsString]) -> Vec<OsString> {
    let mut args = config_args.to_vec();
    args.extend(read_target_and_global_native_flag_env(
        target_triple,
        "ENCODED_NATIVE_LDFLAGS",
        "NATIVE_LDFLAGS",
        "LDFLAGS",
    ));
    args
}

fn native_cppflags_config_flags(
    gctx: &crate::util::context::GlobalContext,
    target_triple: &str,
) -> CargoResult<Vec<OsString>> {
    Ok(
        native_config_flag_values(gctx, target_triple, "native-cppflags")?
            .into_iter()
            .map(OsString::from)
            .collect(),
    )
}

fn native_c_compile_config_flags(
    gctx: &crate::util::context::GlobalContext,
    target_triple: &str,
) -> CargoResult<Vec<OsString>> {
    Ok(
        native_config_flag_values(gctx, target_triple, "native-cflags")?
            .into_iter()
            .map(OsString::from)
            .collect(),
    )
}

fn native_cxx_compile_config_flags(
    gctx: &crate::util::context::GlobalContext,
    target_triple: &str,
) -> CargoResult<Vec<OsString>> {
    Ok(
        native_config_flag_values(gctx, target_triple, "native-cxxflags")?
            .into_iter()
            .map(OsString::from)
            .collect(),
    )
}

fn native_link_config_flags(
    gctx: &crate::util::context::GlobalContext,
    target_triple: &str,
) -> CargoResult<Vec<OsString>> {
    Ok(
        native_config_flag_values(gctx, target_triple, "native-ldflags")?
            .into_iter()
            .map(OsString::from)
            .collect(),
    )
}

fn native_config_flag_values(
    gctx: &crate::util::context::GlobalContext,
    target_triple: &str,
    key: &str,
) -> CargoResult<Vec<String>> {
    let mut values = build_config_string_list(gctx, key)?;
    let target_config = gctx.target_cfg_triple(target_triple)?;
    values.extend(match key {
        "native-cflags" => opt_string_list(&target_config.native_cflags),
        "native-cppflags" => opt_string_list(&target_config.native_cppflags),
        "native-cxxflags" => opt_string_list(&target_config.native_cxxflags),
        "native-ldflags" => opt_string_list(&target_config.native_ldflags),
        _ => Vec::new(),
    });
    Ok(values)
}

fn build_config_string_list(
    gctx: &crate::util::context::GlobalContext,
    key: &str,
) -> CargoResult<Vec<String>> {
    let build = gctx.build_config()?;
    Ok(match key {
        "native-cflags" => build
            .native_cflags
            .as_ref()
            .map(|list| list.as_slice().to_vec())
            .unwrap_or_default(),
        "native-cppflags" => build
            .native_cppflags
            .as_ref()
            .map(|list| list.as_slice().to_vec())
            .unwrap_or_default(),
        "native-cxxflags" => build
            .native_cxxflags
            .as_ref()
            .map(|list| list.as_slice().to_vec())
            .unwrap_or_default(),
        "native-ldflags" => build
            .native_ldflags
            .as_ref()
            .map(|list| list.as_slice().to_vec())
            .unwrap_or_default(),
        _ => Vec::new(),
    })
}

fn build_native_tool_config(
    gctx: &crate::util::context::GlobalContext,
    key: &str,
) -> Option<PathBuf> {
    let build = gctx.build_config().ok()?;
    match key {
        "cc" => build
            .native_cc
            .as_ref()
            .map(|path| path.resolve_program(gctx)),
        "cxx" => build
            .native_cxx
            .as_ref()
            .map(|path| path.resolve_program(gctx)),
        "ar" => build
            .native_ar
            .as_ref()
            .map(|path| path.resolve_program(gctx)),
        _ => None,
    }
}

fn build_native_tool_config_value(
    gctx: &crate::util::context::GlobalContext,
    key: &str,
) -> CargoResult<Option<OsString>> {
    Ok(match key {
        "cc" => gctx
            .build_config()?
            .native_cc
            .as_ref()
            .map(|path| OsString::from(path.raw_value())),
        "cxx" => gctx
            .build_config()?
            .native_cxx
            .as_ref()
            .map(|path| OsString::from(path.raw_value())),
        "ar" => gctx
            .build_config()?
            .native_ar
            .as_ref()
            .map(|path| OsString::from(path.raw_value())),
        _ => None,
    })
}

fn target_native_tool_config(
    gctx: &crate::util::context::GlobalContext,
    target_triple: &str,
    key: &str,
) -> CargoResult<Option<PathBuf>> {
    let target_config = gctx.target_cfg_triple(target_triple)?;
    Ok(match key {
        "cc" => target_config
            .native_cc
            .as_ref()
            .map(|path| path.val.resolve_program(gctx)),
        "cxx" => target_config
            .native_cxx
            .as_ref()
            .map(|path| path.val.resolve_program(gctx)),
        "ar" => target_config
            .native_ar
            .as_ref()
            .map(|path| path.val.resolve_program(gctx)),
        _ => None,
    })
}

fn target_native_tool_config_value(
    gctx: &crate::util::context::GlobalContext,
    target_triple: &str,
    key: &str,
) -> CargoResult<Option<OsString>> {
    let target_config = gctx.target_cfg_triple(target_triple)?;
    Ok(match key {
        "cc" => target_config
            .native_cc
            .as_ref()
            .map(|path| OsString::from(path.val.raw_value())),
        "cxx" => target_config
            .native_cxx
            .as_ref()
            .map(|path| OsString::from(path.val.raw_value())),
        "ar" => target_config
            .native_ar
            .as_ref()
            .map(|path| OsString::from(path.val.raw_value())),
        _ => None,
    })
}

fn opt_string_list(value: &Option<crate::util::context::Value<StringList>>) -> Vec<String> {
    value
        .as_ref()
        .map(|list| list.val.as_slice().to_vec())
        .unwrap_or_default()
}

fn read_native_flag_env(encoded: &str, plain: &str) -> Vec<OsString> {
    let encoded_value = env::var_os(encoded)
        .map(split_encoded_flags)
        .unwrap_or_default();
    if !encoded_value.is_empty() {
        return encoded_value;
    }
    read_plain_flag_env(plain)
}

fn read_target_and_global_native_flag_env(
    target_triple: &str,
    cargo_encoded_key: &str,
    cargo_plain_key: &str,
    plain_key: &str,
) -> Vec<OsString> {
    let mut args = read_native_flag_env(
        &format!("CARGO_ENCODED_{}", cargo_plain_key),
        &format!("CARGO_{}", cargo_plain_key),
    );
    args.extend(read_plain_flag_env(plain_key));
    args.extend(read_target_native_flag_env(
        target_triple,
        cargo_encoded_key,
        cargo_plain_key,
        plain_key,
    ));
    args
}

fn read_target_native_flag_env(
    target_triple: &str,
    cargo_encoded_key: &str,
    cargo_plain_key: &str,
    plain_key: &str,
) -> Vec<OsString> {
    let encoded = target_cargo_native_env(target_triple, cargo_encoded_key)
        .map(split_encoded_flags)
        .unwrap_or_default();
    if !encoded.is_empty() {
        return encoded;
    }
    if let Some(value) = target_cargo_native_env(target_triple, cargo_plain_key) {
        let split = split_shell_like_flags(value);
        if !split.is_empty() {
            return split;
        }
    }
    target_plain_flag_env(target_triple, plain_key)
}

fn target_native_tool_env(target_triple: &str, key: &str) -> Option<OsString> {
    target_cargo_native_env(target_triple, &format!("NATIVE_{key}"))
        .or_else(|| env::var_os(format!("{key}_{}", target_env_suffix(target_triple))))
}

fn target_cargo_native_env(target_triple: &str, key: &str) -> Option<OsString> {
    env::var_os(format!(
        "CARGO_TARGET_{}_{}",
        target_env_suffix(target_triple),
        key
    ))
}

fn target_plain_flag_env(target_triple: &str, key: &str) -> Vec<OsString> {
    env::var_os(format!("{key}_{}", target_env_suffix(target_triple)))
        .map(split_shell_like_flags)
        .unwrap_or_default()
}

fn target_env_suffix(target_triple: &str) -> String {
    target_triple
        .chars()
        .flat_map(|c| c.to_uppercase())
        .map(|c| if matches!(c, '-' | '.') { '_' } else { c })
        .collect()
}

fn split_shell_like_flags(value: OsString) -> Vec<OsString> {
    value
        .to_string_lossy()
        .split_whitespace()
        .map(OsString::from)
        .collect()
}

fn flag_env_values(target_triple: &str) -> Vec<Option<OsString>> {
    let mut values = vec![
        env::var_os("CFLAGS"),
        env::var_os("CPPFLAGS"),
        env::var_os("CXXFLAGS"),
        env::var_os("LDFLAGS"),
        env::var_os("CARGO_NATIVE_CFLAGS"),
        env::var_os("CARGO_NATIVE_CPPFLAGS"),
        env::var_os("CARGO_NATIVE_CXXFLAGS"),
        env::var_os("CARGO_NATIVE_LDFLAGS"),
        env::var_os("CARGO_ENCODED_NATIVE_CFLAGS"),
        env::var_os("CARGO_ENCODED_NATIVE_CPPFLAGS"),
        env::var_os("CARGO_ENCODED_NATIVE_CXXFLAGS"),
        env::var_os("CARGO_ENCODED_NATIVE_LDFLAGS"),
    ];
    for key in [
        "CFLAGS",
        "CPPFLAGS",
        "CXXFLAGS",
        "LDFLAGS",
        "NATIVE_CFLAGS",
        "NATIVE_CPPFLAGS",
        "NATIVE_CXXFLAGS",
        "NATIVE_LDFLAGS",
        "ENCODED_NATIVE_CFLAGS",
        "ENCODED_NATIVE_CPPFLAGS",
        "ENCODED_NATIVE_CXXFLAGS",
        "ENCODED_NATIVE_LDFLAGS",
    ] {
        let value = if key.starts_with("ENCODED_") || key.starts_with("NATIVE_") {
            target_cargo_native_env(target_triple, key)
        } else {
            env::var_os(format!("{key}_{}", target_env_suffix(target_triple)))
        };
        values.push(value);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::native_gnu_crt_link_args;
    use std::ffi::OsString;

    #[test]
    fn gnu_crt_link_args_include_runtime_flags_on_windows_gnu() {
        assert_eq!(
            native_gnu_crt_link_args("x86_64-pc-windows-gnu", true),
            vec![
                OsString::from("-static-libgcc"),
                OsString::from("-static-libstdc++"),
            ]
        );
    }

    #[test]
    fn gnu_crt_link_args_skip_runtime_flags_on_windows_gnullvm() {
        assert!(native_gnu_crt_link_args("x86_64-pc-windows-gnullvm", true).is_empty());
    }

    #[test]
    fn gnu_crt_link_args_include_libgcc_without_cpp_runtime_for_c_only_targets() {
        assert_eq!(
            native_gnu_crt_link_args("x86_64-unknown-linux-gnu", false),
            vec![OsString::from("-static-libgcc")]
        );
    }
}

fn read_plain_flag_env(var: &str) -> Vec<OsString> {
    env::var(var)
        .ok()
        .map(|value| value.split_whitespace().map(OsString::from).collect())
        .unwrap_or_default()
}

fn split_encoded_flags(value: OsString) -> Vec<OsString> {
    value
        .to_string_lossy()
        .split('\u{1f}')
        .filter(|flag| !flag.is_empty())
        .map(OsString::from)
        .collect()
}
