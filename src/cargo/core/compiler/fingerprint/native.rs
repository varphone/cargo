use std::fs;
use std::path::{Path, PathBuf};

use super::dep_info;
use super::{BuildRunner, LocalFingerprint, Unit, dep_info_loc, paths};
use crate::util::errors::CargoResult;
use walkdir::WalkDir;

pub(super) fn local_fingerprints(
    build_runner: &mut BuildRunner<'_, '_>,
    unit: &Unit,
    build_root: &Path,
) -> CargoResult<Vec<LocalFingerprint>> {
    if unit.target.is_header_only() {
        return Ok(vec![LocalFingerprint::Precalculated(
            header_only_fingerprint(unit)?,
        )]);
    }

    let dep_info = dep_info_loc(build_runner, unit);
    let dep_info = dep_info.strip_prefix(build_root).unwrap().to_path_buf();
    let env_config = build_runner.bcx.gctx.env_config()?;
    Ok(vec![
        LocalFingerprint::CheckDepInfo {
            dep_info,
            checksum: false,
        },
        LocalFingerprint::from_env("CXX", &env_config),
        LocalFingerprint::from_env("AR", &env_config),
        LocalFingerprint::Precalculated(super::super::native::toolchain_fingerprint(
            build_runner,
            unit,
        )?),
    ])
}

fn header_only_fingerprint(unit: &Unit) -> CargoResult<String> {
    let Some(source_path) = unit.target.src_path().path() else {
        return Ok("native-header-only:none".to_string());
    };

    let mut entries = Vec::new();
    if source_path.is_dir() {
        for entry in WalkDir::new(source_path) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            entries.push(fingerprint_entry(entry.path())?);
        }
    } else {
        entries.push(fingerprint_entry(source_path)?);
    }
    entries.sort();

    let fingerprint = crate::util::hash_u64((source_path, entries));
    Ok(format!("native-header-only:{fingerprint:016x}"))
}

fn fingerprint_entry(path: &Path) -> CargoResult<(PathBuf, Option<u64>, Option<u128>)> {
    let metadata = fs::metadata(path)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok((path.to_path_buf(), Some(metadata.len()), modified))
}

pub(crate) fn write_dep_info(
    build_root: &Path,
    pkg_root: &Path,
    dep_info_path: &Path,
    tracked_paths: &[PathBuf],
) -> CargoResult<()> {
    let mut paths_to_record = tracked_paths.to_vec();
    paths_to_record.sort();
    paths_to_record.dedup();

    let build_root = crate::util::try_canonicalize(build_root)?;
    let pkg_root = crate::util::try_canonicalize(pkg_root)?;
    let mut encoded = dep_info::EncodedDepInfo::default();

    for path in paths_to_record {
        let canonical = crate::util::try_canonicalize(&path).unwrap_or(path.clone());
        let (path_type, stored_path) = if let Ok(stripped) = canonical.strip_prefix(&build_root) {
            (
                dep_info::DepInfoPathType::BuildRootRelative,
                stripped.to_path_buf(),
            )
        } else if let Ok(stripped) = canonical.strip_prefix(&pkg_root) {
            (
                dep_info::DepInfoPathType::PackageRootRelative,
                stripped.to_path_buf(),
            )
        } else {
            (dep_info::DepInfoPathType::BuildRootRelative, canonical)
        };
        encoded.files.push((path_type, stored_path, None));
    }

    if let Some(parent) = dep_info_path.parent() {
        paths::create_dir_all(parent)?;
    }
    paths::write(dep_info_path, encoded.serialize()?)?;
    Ok(())
}
