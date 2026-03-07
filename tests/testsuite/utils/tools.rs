//! Common executables that can be reused by various tests.

use crate::prelude::*;
use cargo_test_support::{Project, basic_manifest, paths, project};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

static ECHO_WRAPPER: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static ECHO: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static CLIPPY_DRIVER: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static FAKE_NATIVE_TOOL: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static FAKE_NATIVE_TOOL_NAMED_DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// Returns the path to an executable that works as a wrapper around rustc.
///
/// The wrapper will echo the command line it was called with to stderr.
pub fn echo_wrapper() -> PathBuf {
    let mut lock = ECHO_WRAPPER
        .get_or_init(|| Default::default())
        .lock()
        .unwrap();
    if let Some(path) = &*lock {
        return path.clone();
    }
    let p = project()
        .at(paths::global_root().join("rustc-echo-wrapper"))
        .file("Cargo.toml", &basic_manifest("rustc-echo-wrapper", "1.0.0"))
        .file(
            "src/main.rs",
            r#"
            use std::fs::read_to_string;
            use std::path::PathBuf;
            fn main() {
                // Handle args from `@path` argfile for rustc
                let args = std::env::args()
                    .flat_map(|p| if let Some(p) = p.strip_prefix("@") {
                        read_to_string(p).unwrap().lines().map(String::from).collect()
                    } else {
                        vec![p]
                    })
                    .collect::<Vec<_>>();
                eprintln!("WRAPPER CALLED: {}", args[1..].join(" "));
                let status = std::process::Command::new(&args[1])
                    .args(&args[2..]).status().unwrap();
                std::process::exit(status.code().unwrap_or(1));
            }
            "#,
        )
        .build();
    p.cargo("build").run();
    let path = p.bin("rustc-echo-wrapper");
    *lock = Some(path.clone());
    path
}

/// Returns the path to an executable that prints its arguments.
///
/// Do not expect this to be anything fancy.
pub fn echo() -> PathBuf {
    let mut lock = ECHO.get_or_init(|| Default::default()).lock().unwrap();
    if let Some(path) = &*lock {
        return path.clone();
    }
    if let Ok(path) = cargo_util::paths::resolve_executable(Path::new("echo")) {
        *lock = Some(path.clone());
        return path;
    }
    // Often on Windows, `echo` is not available.
    let p = project()
        .at(paths::global_root().join("basic-echo"))
        .file("Cargo.toml", &basic_manifest("basic-echo", "1.0.0"))
        .file(
            "src/main.rs",
            r#"
                fn main() {
                    let mut s = String::new();
                    let mut it = std::env::args().skip(1).peekable();
                    while let Some(n) = it.next() {
                        s.push_str(&n);
                        if it.peek().is_some() {
                            s.push(' ');
                        }
                    }
                    println!("{}", s);
                }
            "#,
        )
        .build();
    p.cargo("build").run();
    let path = p.bin("basic-echo");
    *lock = Some(path.clone());
    path
}

/// Returns a project which builds a cargo-echo simple subcommand
pub fn echo_subcommand() -> Project {
    let p = project()
        .at("cargo-echo")
        .file("Cargo.toml", &basic_manifest("cargo-echo", "0.0.1"))
        .file(
            "src/main.rs",
            r#"
                fn main() {
                    let args: Vec<_> = ::std::env::args().skip(1).collect();
                    println!("{}", args.join(" "));
                }
            "#,
        )
        .build();
    p.cargo("build").run();
    p
}

/// A wrapper around `rustc` instead of calling `clippy`.
pub fn wrapped_clippy_driver() -> PathBuf {
    let mut lock = CLIPPY_DRIVER
        .get_or_init(|| Default::default())
        .lock()
        .unwrap();
    if let Some(path) = &*lock {
        return path.clone();
    }
    let clippy_driver = project()
        .at(paths::global_root().join("clippy-driver"))
        .file("Cargo.toml", &basic_manifest("clippy-driver", "0.0.1"))
        .file(
            "src/main.rs",
            r#"
            fn main() {
                let mut args = std::env::args_os();
                let _me = args.next().unwrap();
                let rustc = args.next().unwrap();
                let status = std::process::Command::new(rustc).args(args).status().unwrap();
                std::process::exit(status.code().unwrap_or(1));
            }
            "#,
        )
        .build();
    clippy_driver.cargo("build").run();
    let path = clippy_driver.bin("clippy-driver");
    *lock = Some(path.clone());
    path
}

/// Returns the path to a fake native C++ toolchain executable.
///
/// The same binary can act as both `CXX` and `AR`. It records all invocations
/// to the path in `FAKE_NATIVE_TOOL_LOG` and materializes the requested output
/// file so Cargo can continue the build.
pub fn fake_native_tool() -> PathBuf {
    let mut lock = FAKE_NATIVE_TOOL
        .get_or_init(|| Default::default())
        .lock()
        .unwrap();
    if let Some(path) = &*lock {
        return path.clone();
    }
    let p = project()
        .at(paths::global_root().join("fake-native-tool"))
        .file("Cargo.toml", &basic_manifest("fake-native-tool", "1.0.0"))
        .file(
            "src/main.rs",
            r##"
            use std::ffi::OsString;
            use std::fs::{self, OpenOptions};
            use std::io::Write;
            use std::path::{Path, PathBuf};

            fn main() {
                let exe_name = std::env::current_exe()
                    .ok()
                    .and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "fake-native-tool".to_string());
                let args = std::env::args_os().skip(1).collect::<Vec<_>>();

                if let Some(path) = std::env::var_os("FAKE_NATIVE_TOOL_LOG") {
                    let mut file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .unwrap();
                    let rendered = args
                        .iter()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(" ");
                    writeln!(file, "{} {}", exe_name, rendered).unwrap();
                }

                if let Some(depfile) = find_depfile(&args) {
                    write_depfile(&args, &depfile).unwrap();
                }

                if let Some(output) = find_output(&args) {
                    if let Some(parent) = output.parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    if is_executable_output(&output) {
                        materialize_executable(&output).unwrap();
                    } else {
                        fs::write(output, b"fake-native-output").unwrap();
                    }
                    return;
                }

                println!("fake-native-runtime");
            }

            fn find_output(args: &[OsString]) -> Option<PathBuf> {
                let mut iter = args.iter();
                while let Some(arg) = iter.next() {
                    if arg == "-o" {
                        return iter.next().map(PathBuf::from);
                    }
                    let arg = arg.to_string_lossy();
                    if let Some(path) = arg.strip_prefix("/Fo") {
                        return Some(PathBuf::from(path));
                    }
                    if let Some(path) = arg.strip_prefix("/Fe") {
                        return Some(PathBuf::from(path));
                    }
                    if let Some(path) = arg.strip_prefix("/OUT:") {
                        return Some(PathBuf::from(path));
                    }
                }

                match args {
                    [mode, output, ..] if mode == "crs" => Some(PathBuf::from(output)),
                    _ => None,
                }
            }

            fn find_depfile(args: &[OsString]) -> Option<PathBuf> {
                let mut iter = args.iter();
                while let Some(arg) = iter.next() {
                    if arg == "-MF" {
                        return iter.next().map(PathBuf::from);
                    }
                }
                None
            }

            fn is_executable_output(output: &Path) -> bool {
                if cfg!(windows) {
                    return output
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
                }

                output.extension().is_none()
            }

            fn materialize_executable(output: &Path) -> std::io::Result<()> {
                let current_exe = std::env::current_exe()?;
                let bytes = fs::read(current_exe)?;
                fs::write(output, bytes)?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;

                    let mut permissions = fs::metadata(output)?.permissions();
                    permissions.set_mode(0o755);
                    fs::set_permissions(output, permissions)?;
                }

                Ok(())
            }

            fn write_depfile(args: &[OsString], depfile: &Path) -> std::io::Result<()> {
                let Some(source) = find_source(args) else {
                    return Ok(());
                };
                let Some(output) = find_output(args) else {
                    return Ok(());
                };

                let include_dirs = find_include_dirs(args);
                let mut dependencies = vec![source.clone()];
                let mut index = 0;
                while index < dependencies.len() {
                    let path = dependencies[index].clone();
                    index += 1;
                    for include in parse_local_includes(&path)? {
                        if let Some(resolved) = resolve_include(path.parent(), &include_dirs, &include) {
                            if !dependencies.contains(&resolved) {
                                dependencies.push(resolved);
                            }
                        }
                    }
                }

                if let Some(parent) = depfile.parent() {
                    fs::create_dir_all(parent)?;
                }

                let mut rendered = format!("{}:", output.display());
                for dependency in dependencies {
                    rendered.push(' ');
                    rendered.push_str(&dependency.display().to_string());
                }
                rendered.push('\n');
                fs::write(depfile, rendered)
            }

            fn find_source(args: &[OsString]) -> Option<PathBuf> {
                args.iter().find_map(|arg| {
                    let path = PathBuf::from(arg);
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| matches!(ext, "c" | "cpp" | "cc" | "cxx"))
                        .then_some(path)
                })
            }

            fn find_include_dirs(args: &[OsString]) -> Vec<PathBuf> {
                let mut include_dirs = Vec::new();
                let mut iter = args.iter();
                while let Some(arg) = iter.next() {
                    if arg == "-I" {
                        if let Some(path) = iter.next() {
                            include_dirs.push(PathBuf::from(path));
                        }
                        continue;
                    }

                    let value = arg.to_string_lossy();
                    if let Some(path) = value.strip_prefix("-I") {
                        if !path.is_empty() {
                            include_dirs.push(PathBuf::from(path));
                        }
                    }
                }
                include_dirs
            }

            fn parse_local_includes(path: &Path) -> std::io::Result<Vec<String>> {
                let contents = fs::read_to_string(path)?;
                Ok(contents
                    .lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        let rest = line.strip_prefix("#include")?.trim();
                        let rest = rest.strip_prefix('"').or_else(|| rest.strip_prefix('<'))?;
                        let end = rest.find(['"', '>'])?;
                        Some(rest[..end].to_string())
                    })
                    .collect())
            }

            fn resolve_include(
                source_dir: Option<&Path>,
                include_dirs: &[PathBuf],
                include: &str,
            ) -> Option<PathBuf> {
                let relative = Path::new(include);
                if let Some(source_dir) = source_dir {
                    let candidate = source_dir.join(relative);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
                for include_dir in include_dirs {
                    let candidate = include_dir.join(relative);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
                None
            }
            "##,
        )
        .build();
    p.cargo("build").run();
    let path = p.bin("fake-native-tool");
    *lock = Some(path.clone());
    path
}

pub fn fake_native_tool_named(name: &str) -> PathBuf {
    let mut lock = FAKE_NATIVE_TOOL_NAMED_DIR
        .get_or_init(|| Default::default())
        .lock()
        .unwrap();
    let dir = match &*lock {
        Some(path) => path.clone(),
        None => {
            let dir = paths::global_root().join("fake-native-tool-named");
            std::fs::create_dir_all(&dir).unwrap();
            *lock = Some(dir.clone());
            dir
        }
    };

    let named = dir.join(name);
    if !named.exists() {
        std::fs::copy(fake_native_tool(), &named).unwrap();
    }
    named
}
