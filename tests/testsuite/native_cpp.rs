//! Tests for native C++ target support.

use crate::prelude::*;
use crate::utils::tools;
use cargo_test_support::cross_compile::alternate as cross_compile_alternate;
use cargo_test_support::{basic_bin_manifest, project, rustc_host, sleep_ms, str};
use std::process::Command;

const SBOM_FILE_EXTENSION: &str = ".cargo-sbom.json";

fn native_library_basename(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    }
}

fn native_shared_library_basename(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

fn native_library_deps_fragment(name: &str) -> String {
    if cfg!(windows) {
        format!("\\{name}-")
    } else {
        format!("/lib{name}-")
    }
}

fn source_fragment(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

fn normalize_json_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn toml_path(path: &std::path::Path) -> String {
    if cfg!(windows) {
        path.display().to_string().replace('\\', "\\\\")
    } else {
        path.display().to_string()
    }
}

fn collect_sbom_files(dir: &std::path::Path, sbom_files: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_sbom_files(&path, sbom_files);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(SBOM_FILE_EXTENSION))
        {
            sbom_files.push(path);
        }
    }
}

fn target_env_suffix(target: &str) -> String {
    target
        .chars()
        .flat_map(|c| c.to_uppercase())
        .map(|c| if matches!(c, '-' | '.') { '_' } else { c })
        .collect()
}

fn available_cross_target() -> Option<&'static str> {
    if matches!(std::env::var("CFG_DISABLE_CROSS_TESTS"), Ok(value) if value == "1") {
        return None;
    }
    if !(cfg!(target_os = "macos") || cfg!(target_os = "linux") || cfg!(target_env = "msvc")) {
        return None;
    }

    let target = cross_compile_alternate();
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()?;
    let installed = String::from_utf8_lossy(&output.stdout);
    installed
        .lines()
        .any(|line| line.trim() == target)
        .then_some(target)
}

fn installed_target(target: &'static str) -> Option<&'static str> {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()?;
    let installed = String::from_utf8_lossy(&output.stdout);
    installed
        .lines()
        .any(|line| line.trim() == target)
        .then_some(target)
}

#[cargo_test]
fn cpp_bin_links_same_package_native_lib() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    assert!(p.bin("foo").is_file());
    assert!(
        p.root()
            .join("target/debug")
            .join(native_library_basename("foo"))
            .is_file()
    );

    let log = p.read_file("native-tool.log");
    assert!(log.contains(&source_fragment("src/main.cpp")));
    let lib_fragment = native_library_deps_fragment("foo");
    assert!(
        log.contains(&lib_fragment),
        "missing same-package native lib in link line: {log}"
    );
}

#[cargo_test]
fn cargo_run_executes_native_bin() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/main.cpp", "int main() { return 0; }\n")
        .build();

    p.cargo("run")
        .env("CXX", &tool)
        .env("AR", &tool)
        .with_stdout_contains("fake-native-runtime")
        .run();
}

#[cargo_test]
fn cargo_test_executes_native_test_target() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [[test]]
                name = "smoke"
                path = "tests/smoke.cpp"
                harness = false
            "#,
        )
        .file("tests/smoke.cpp", "int main() { return 0; }\n")
        .build();

    p.cargo("test --test smoke")
        .env("CXX", &tool)
        .env("AR", &tool)
        .with_stdout_contains("fake-native-runtime")
        .run();
}

#[cargo_test]
fn cargo_bench_executes_native_bench_target() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [[bench]]
                name = "micro"
                path = "benches/micro.cpp"
                harness = false
            "#,
        )
        .file("benches/micro.cpp", "int main() { return 0; }\n")
        .build();

    p.cargo("bench --bench micro")
        .env("CXX", &tool)
        .env("AR", &tool)
        .with_stdout_contains("fake-native-runtime")
        .run();
}

#[cargo_test]
fn cpp_lib_builds_companion_sources_from_directory() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file(
            "src/lib.cpp",
            "int primary_answer() { return helper_answer(); }\n",
        )
        .file("src/lib/helper.cpp", "int helper_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int primary_answer();\nint main() { return primary_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    assert!(log.contains(&source_fragment("src/lib.cpp")));
    assert!(log.contains(&source_fragment("src/lib/helper.cpp")));
}

#[cargo_test]
fn cpp_cdylib_builds_shared_library() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [lib]
                crate-type = ["cdylib"]
            "#,
        )
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    assert!(
        p.root()
            .join("target/debug")
            .join(native_shared_library_basename("foo"))
            .is_file()
    );

    let log = p.read_file("native-tool.log");
    assert!(
        log.contains("-shared"),
        "missing shared library flag: {log}"
    );
    assert!(
        log.contains("-fPIC"),
        "missing PIC flag for shared library: {log}"
    );
}

#[cargo_test]
fn cpp_bin_links_path_dependency_native_lib() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <bar/answer.hpp>\nint main() { return dep_answer(); }\n",
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("bar/include/bar/answer.hpp", "int dep_answer();\n")
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    assert!(p.bin("foo").is_file());

    let log = p.read_file("native-tool.log");
    let lib_fragment = native_library_deps_fragment("bar");
    let include_path = source_fragment(&p.root().join("bar/include").display().to_string());
    assert!(
        log.contains(&lib_fragment),
        "missing dependency native lib in link line: {log}"
    );
    assert!(
        log.contains(&include_path),
        "missing dependency include dir in compile line: {log}"
    );
}

#[cargo_test]
fn cpp_header_change_rebuilds_native_targets() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <bar/answer.hpp>\nint main() { return dep_answer(); }\n",
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("bar/include/bar/answer.hpp", "int dep_answer();\n")
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    let build = || {
        p.cargo("build -v")
            .env("CXX", &tool)
            .env("AR", &tool)
            .env("FAKE_NATIVE_TOOL_LOG", &log_path)
            .run();
    };

    build();
    let first_count = p.read_file("native-tool.log").lines().count();

    build();
    let second_count = p.read_file("native-tool.log").lines().count();
    assert_eq!(
        second_count, first_count,
        "native compile unexpectedly reran on a fresh build"
    );

    sleep_ms(1000);
    p.change_file(
        "bar/include/bar/answer.hpp",
        "int dep_answer();\nint dep_twice();\n",
    );

    build();
    let third_count = p.read_file("native-tool.log").lines().count();
    assert!(
        third_count > second_count,
        "changing a native header should trigger recompilation"
    );
}

#[cargo_test]
fn cpp_transitive_native_dependency_propagates_public_headers_and_links() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                mid = { path = "mid" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <mid/api.hpp>\nint main() { return mid_answer(); }\n",
        )
        .file(
            "mid/Cargo.toml",
            r#"
                [package]
                name = "mid"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                leaf = { path = "../leaf" }
            "#,
        )
        .file(
            "mid/include/mid/api.hpp",
            "#include <leaf/value.hpp>\nint mid_answer();\n",
        )
        .file(
            "mid/src/lib.cpp",
            "#include <mid/api.hpp>\nint mid_answer() { return leaf_value(); }\n",
        )
        .file(
            "leaf/Cargo.toml",
            r#"
                [package]
                name = "leaf"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("leaf/include/leaf/value.hpp", "int leaf_value();\n")
        .file("leaf/src/lib.cpp", "int leaf_value() { return 5; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let compile_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing app compile line in log: {log}"));
    let leaf_include = source_fragment(&p.root().join("leaf/include").display().to_string());
    assert!(
        compile_line.contains(&leaf_include),
        "missing transitive leaf include dir in compile line: {compile_line}"
    );

    let app_link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing app link line in log: {log}"));
    assert!(
        app_link_line.contains(&native_library_deps_fragment("mid")),
        "missing direct native dependency in app link line: {app_link_line}"
    );
    assert!(
        app_link_line.contains(&native_library_deps_fragment("leaf")),
        "missing transitive native dependency in app link line: {app_link_line}"
    );
}

#[cargo_test]
fn cpp_explicit_header_only_dependency_propagates_includes_without_native_artifact() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                headers = { path = "headers" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <answer.hpp>\nint main() { return header_answer() == 11 ? 0 : 1; }\n",
        )
        .file(
            "headers/Cargo.toml",
            r#"
                [package]
                name = "headers"
                version = "0.1.0"
                edition = "2024"

                [lib]
                path = "include/answer.hpp"
            "#,
        )
        .file(
            "headers/include/answer.hpp",
            "inline int header_answer() { return 11; }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    assert!(p.bin("app").is_file());
    assert_eq!(p.glob("target/debug/deps/libheaders-*.a").count(), 0);

    let log = p.read_file("native-tool.log");
    let compile_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing app compile line in log: {log}"));
    let include_path = source_fragment(&p.root().join("headers/include").display().to_string());
    assert!(
        compile_line.contains(&include_path),
        "missing explicit header-only include dir in compile line: {compile_line}"
    );

    let app_link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing app link line in log: {log}"));
    assert!(
        !app_link_line.contains(&native_library_deps_fragment("headers")),
        "header-only dependency should not contribute a native library artifact: {app_link_line}"
    );
}

#[cargo_test]
fn cpp_implicit_header_only_dependency_is_inferred_and_propagates_includes() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                helper = { path = "helper" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <answer.hpp>\nint main() { return helper_answer() == 7 ? 0 : 1; }\n",
        )
        .file(
            "helper/Cargo.toml",
            r#"
                [package]
                name = "helper"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file(
            "helper/include/answer.hpp",
            "inline int helper_answer() { return 7; }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    assert!(p.bin("app").is_file());
    assert_eq!(p.glob("target/debug/deps/libhelper-*.a").count(), 0);

    let log = p.read_file("native-tool.log");
    let compile_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing app compile line in log: {log}"));
    let include_path = source_fragment(&p.root().join("helper/include").display().to_string());
    assert!(
        compile_line.contains(&include_path),
        "missing inferred header-only include dir in compile line: {compile_line}"
    );
}

#[cargo_test]
fn cpp_native_link_consumes_build_script_link_metadata() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file(
            "src/main.cpp",
            "int dep_answer();\nint main() { return dep_answer(); }\n",
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
                build = "build.rs"
                links = "bar"
            "#,
        )
        .file(
            "bar/build.rs",
            r#"
                use std::env;
                use std::fs;
                use std::path::PathBuf;

                fn main() {
                    let lib_dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("native-libs");
                    fs::create_dir_all(&lib_dir).unwrap();
                    println!("cargo::rustc-link-search={}", lib_dir.display());
                    println!("cargo::rustc-link-lib=generated_dep");
                    println!("cargo::rustc-link-arg=-Wl,--export-dynamic");
                }
            "#,
        )
        .file("bar/src/lib.cpp", "int dep_answer() { return 9; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let app_link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing app link line in log: {log}"));
    assert!(
        app_link_line.contains("-lgenerated_dep"),
        "missing build script rustc-link-lib in native link line: {app_link_line}"
    );
    assert!(
        app_link_line.contains("-Wl,--export-dynamic"),
        "missing build script rustc-link-arg in native link line: {app_link_line}"
    );
    assert!(
        app_link_line.contains("/native-libs") || app_link_line.contains("\\native-libs"),
        "missing build script rustc-link-search path in native link line: {app_link_line}"
    );
}

#[cargo_test]
fn cpp_toolchain_change_rebuilds_native_targets() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let alt_tool = p.root().join(if cfg!(windows) {
        "fake-native-tool-alt.exe"
    } else {
        "fake-native-tool-alt"
    });
    std::fs::copy(&tool, &alt_tool).unwrap();

    let log_path = p.root().join("native-tool.log");
    let build = |cxx: &std::path::Path, ar: &std::path::Path| {
        p.cargo("build -v")
            .env("CXX", cxx)
            .env("AR", ar)
            .env("FAKE_NATIVE_TOOL_LOG", &log_path)
            .run();
    };

    build(&tool, &tool);
    let first_count = p.read_file("native-tool.log").lines().count();

    build(&tool, &tool);
    let second_count = p.read_file("native-tool.log").lines().count();
    assert_eq!(
        second_count, first_count,
        "native compile unexpectedly reran on a fresh build"
    );

    build(&alt_tool, &alt_tool);
    let third_count = p.read_file("native-tool.log").lines().count();
    assert!(
        third_count > second_count,
        "changing the native toolchain should trigger recompilation"
    );
}

#[cargo_test]
fn cpp_directory_bin_builds_companion_sources() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file(
            "src/bin/tool/main.cpp",
            "int helper_answer();\nint main() { return helper_answer(); }\n",
        )
        .file(
            "src/bin/tool/helper.cpp",
            "int helper_answer() { return 7; }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v --bin tool")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    assert!(p.bin("tool").is_file());

    let log = p.read_file("native-tool.log");
    assert!(log.contains(&source_fragment("src/bin/tool/main.cpp")));
    assert!(log.contains(&source_fragment("src/bin/tool/helper.cpp")));
}

#[cargo_test]
fn cpp_debug_and_release_profiles_affect_native_flags() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            &format!(
                "{}\n[profile.release]\nlto = \"thin\"\nstrip = \"debuginfo\"\ndebug-assertions = true\n",
                basic_bin_manifest("foo")
            ),
        )
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let debug_log_path = p.root().join("native-tool-debug.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &debug_log_path)
        .run();

    let debug_log = p.read_file("native-tool-debug.log");
    assert!(
        debug_log.contains("-O0"),
        "missing debug opt-level flag: {debug_log}"
    );
    assert!(
        debug_log.contains("-g"),
        "missing debug info flag: {debug_log}"
    );
    assert!(
        debug_log.contains("-UNDEBUG"),
        "missing debug assertions flag: {debug_log}"
    );

    let release_log_path = p.root().join("native-tool-release.log");
    p.cargo("build -v --release")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &release_log_path)
        .run();

    let release_log = p.read_file("native-tool-release.log");
    assert!(
        release_log.contains("-O3"),
        "missing release opt-level flag: {release_log}"
    );
    assert!(
        release_log.contains("-flto=thin"),
        "missing release lto flag: {release_log}"
    );
    assert!(
        release_log.contains("-Wl,-S"),
        "missing release strip flag: {release_log}"
    );
    assert!(
        release_log.contains("-UNDEBUG"),
        "missing release debug assertions override: {release_log}"
    );
}

#[cargo_test]
fn cpp_env_flags_are_passed_to_native_compile_and_link() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .env(
            "CARGO_ENCODED_NATIVE_CPPFLAGS",
            "-DCPP_FEATURE=1\u{1f}-Winvalid-pch",
        )
        .env("CARGO_ENCODED_NATIVE_CXXFLAGS", "-std=c++20\u{1f}-Wextra")
        .env("CARGO_ENCODED_NATIVE_LDFLAGS", "-Wl,--as-needed")
        .run();

    let log = p.read_file("native-tool.log");
    assert!(
        log.contains("-DCPP_FEATURE=1"),
        "missing CPPFLAGS entry: {log}"
    );
    assert!(
        log.contains("-Winvalid-pch"),
        "missing encoded CPPFLAGS entry: {log}"
    );
    assert!(log.contains("-std=c++20"), "missing CXXFLAGS entry: {log}");
    assert!(
        log.contains("-Wextra"),
        "missing encoded CXXFLAGS entry: {log}"
    );
    assert!(
        log.contains("-Wl,--as-needed"),
        "missing LDFLAGS entry: {log}"
    );
}

#[cargo_test]
fn cpp_config_flags_are_passed_to_native_compile_and_link() {
    let tool = tools::fake_native_tool();
    let host = rustc_host();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file(
            ".cargo/config.toml",
            &format!(
                "[build]\nnative-cppflags = [\"-DBUILD_CPP=1\"]\nnative-cxxflags = [\"-std=c++23\"]\nnative-ldflags = [\"-Wl,--gc-sections\"]\n\n[target.\"{host}\"]\nnative-cxxflags = [\"-Winvalid-pch\"]\nnative-ldflags = [\"-Wl,--as-needed\"]\n"
            ),
        )
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    assert!(
        log.contains("-DBUILD_CPP=1"),
        "missing build.native-cppflags entry: {log}"
    );
    assert!(
        log.contains("-std=c++23"),
        "missing build.native-cxxflags entry: {log}"
    );
    assert!(
        log.contains("-Winvalid-pch"),
        "missing target.native-cxxflags entry: {log}"
    );
    assert!(
        log.contains("-Wl,--gc-sections"),
        "missing build.native-ldflags entry: {log}"
    );
    assert!(
        log.contains("-Wl,--as-needed"),
        "missing target.native-ldflags entry: {log}"
    );
}

#[cargo_test]
fn c_bin_links_same_package_native_lib() {
    let cc = tools::fake_native_tool_named("fake-cc");
    let cxx = tools::fake_native_tool_named("fake-cxx");
    let ar = tools::fake_native_tool_named("fake-ar");
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.c", "int native_answer(void) { return 42; }\n")
        .file(
            "src/main.c",
            "int native_answer(void);\nint main(void) { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CC", &cc)
        .env("CXX", &cxx)
        .env("AR", &ar)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    assert!(p.bin("foo").is_file());
    assert!(
        p.root()
            .join("target/debug")
            .join(native_library_basename("foo"))
            .is_file()
    );

    let log = p.read_file("native-tool.log");
    assert!(log.contains(&source_fragment("src/lib.c")));
    assert!(log.contains(&source_fragment("src/main.c")));
    assert!(
        log.lines().any(|line| line.starts_with("fake-cc ")),
        "expected CC to compile and link C sources, log was: {log}"
    );
    assert!(
        !log.lines().any(|line| line.starts_with("fake-cxx ")),
        "unexpected CXX usage for pure C target, log was: {log}"
    );
}

#[cargo_test]
fn mixed_c_and_cpp_sources_use_language_specific_tools_and_flags() {
    let cc = tools::fake_native_tool_named("fake-cc");
    let cxx = tools::fake_native_tool_named("fake-cxx");
    let ar = tools::fake_native_tool_named("fake-ar");
    let host = rustc_host();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file(
            ".cargo/config.toml",
            &format!(
                "[build]\nnative-cflags = [\"-Winvalid-c\"]\nnative-cxxflags = [\"-Winvalid-pch\"]\n\n[target.\"{host}\"]\nnative-cflags = [\"-DBUILD_C=1\"]\n"
            ),
        )
        .file(
            "src/main.cpp",
            "extern \"C\" int helper(void);\nint main() { return helper(); }\n",
        )
        .file("src/main/helper.c", "int helper(void) { return 0; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CC", &cc)
        .env("CXX", &cxx)
        .env("AR", &ar)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .env(
            "CARGO_ENCODED_NATIVE_CFLAGS",
            "-DC_FROM_ENV=1\u{1f}-std=c17",
        )
        .env(
            "CARGO_ENCODED_NATIVE_CXXFLAGS",
            "-DCPP_FROM_ENV=1\u{1f}-std=c++20",
        )
        .run();

    let log = p.read_file("native-tool.log");
    let c_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main/helper.c")))
        .unwrap_or_else(|| panic!("missing C compile line in log: {log}"));
    assert!(
        c_line.starts_with("fake-cc "),
        "expected CC for C source, log was: {log}"
    );
    assert!(
        c_line.contains("-Winvalid-c"),
        "missing build.native-cflags entry: {c_line}"
    );
    assert!(
        c_line.contains("-DBUILD_C=1"),
        "missing target.native-cflags entry: {c_line}"
    );
    assert!(
        c_line.contains("-DC_FROM_ENV=1"),
        "missing C env flag entry: {c_line}"
    );
    assert!(
        c_line.contains("-std=c17"),
        "missing C standard flag entry: {c_line}"
    );
    assert!(
        !c_line.contains("-DCPP_FROM_ENV=1"),
        "C compile line should not include CXX flags: {c_line}"
    );

    let cpp_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing C++ compile line in log: {log}"));
    assert!(
        cpp_line.starts_with("fake-cxx "),
        "expected CXX for C++ source, log was: {log}"
    );
    assert!(
        cpp_line.contains("-Winvalid-pch"),
        "missing build.native-cxxflags entry: {cpp_line}"
    );
    assert!(
        cpp_line.contains("-DCPP_FROM_ENV=1"),
        "missing CXX env flag entry: {cpp_line}"
    );
    assert!(
        cpp_line.contains("-std=c++20"),
        "missing C++ standard flag entry: {cpp_line}"
    );
    assert!(
        !cpp_line.contains("-DC_FROM_ENV=1"),
        "C++ compile line should not include C flags: {cpp_line}"
    );

    let fake_cxx_lines = log
        .lines()
        .filter(|line| line.starts_with("fake-cxx "))
        .count();
    assert!(
        fake_cxx_lines >= 2,
        "expected mixed-language link step to use CXX, log was: {log}"
    );
}

#[cargo_test]
fn native_manifest_explicit_paths_build_nondefault_targets() {
    let cc = tools::fake_native_tool_named("fake-cc");
    let cxx = tools::fake_native_tool_named("fake-cxx");
    let ar = tools::fake_native_tool_named("fake-ar");
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [lib]
                name = "foo"
                path = "native/lib/native_lib.cpp"
                crate-type = ["staticlib"]

                [[bin]]
                name = "runner"
                path = "native/bin/runner.c"

                [[example]]
                name = "demo"
                path = "native/examples/demo.cpp"

                [[test]]
                name = "smoke"
                path = "native/tests/smoke.cpp"
                harness = false

                [[bench]]
                name = "micro"
                path = "native/benches/micro.cpp"
                harness = false
            "#,
        )
        .file(
            "native/lib/native_lib.cpp",
            "int native_answer() { return 42; }\n",
        )
        .file(
            "native/bin/runner.c",
            "int native_answer(void);\nint main(void) { return native_answer(); }\n",
        )
        .file("native/examples/demo.cpp", "int main() { return 0; }\n")
        .file("native/tests/smoke.cpp", "int main() { return 0; }\n")
        .file("native/benches/micro.cpp", "int main() { return 0; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v --bin runner --example demo --test smoke --bench micro")
        .env("CC", &cc)
        .env("CXX", &cxx)
        .env("AR", &ar)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    assert!(p.bin("runner").is_file());
    let log = p.read_file("native-tool.log");
    assert!(log.contains(&source_fragment("native/lib/native_lib.cpp")));
    assert!(log.contains(&source_fragment("native/bin/runner.c")));
    assert!(log.contains(&source_fragment("native/examples/demo.cpp")));
    assert!(log.contains(&source_fragment("native/tests/smoke.cpp")));
    assert!(log.contains(&source_fragment("native/benches/micro.cpp")));
}

#[cargo_test]
fn native_manifest_roots_override_default_public_headers_and_companion_sources() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                dep = { path = "dep" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <dep/answer.hpp>\nint dep_export();\nint main() { return dep_export(); }\n",
        )
        .file(
            "dep/Cargo.toml",
            r#"
                [package]
                name = "dep"
                version = "0.1.0"
                edition = "2024"

                [lib]
                path = "native/entry/bridge.cpp"
                crate-type = ["staticlib"]
                native-include-root = "public"
                native-sources-root = "native/all-src"
            "#,
        )
        .file(
            "dep/public/dep/answer.hpp",
            "int dep_helper();\ninline int dep_answer() { return dep_helper(); }\n",
        )
        .file(
            "dep/native/entry/bridge.cpp",
            "#include <dep/answer.hpp>\nint dep_export() { return dep_answer(); }\n",
        )
        .file(
            "dep/native/all-src/helper.cpp",
            "int dep_helper() { return 7; }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let include_path = source_fragment(&p.root().join("dep/public").display().to_string());
    let dep_compile_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("dep/native/entry/bridge.cpp")))
        .unwrap_or_else(|| panic!("missing dep compile line in log: {log}"));
    assert!(
        dep_compile_line.contains(&include_path),
        "missing overridden public header root in dep compile line: {dep_compile_line}"
    );

    let app_compile_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing app compile line in log: {log}"));
    assert!(
        app_compile_line.contains(&include_path),
        "missing overridden dependency include root in app compile line: {app_compile_line}"
    );
    assert!(
        log.contains(&source_fragment("dep/native/all-src/helper.cpp")),
        "missing overridden native companion source in build log: {log}"
    );
}

#[cargo_test]
fn native_manifest_include_dirs_and_defines_customize_only_local_native_compile() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                dep = { path = "dep" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <dep/api.hpp>\nint dep_export();\nint main() { return dep_export(); }\n",
        )
        .file(
            "dep/Cargo.toml",
            r#"
                [package]
                name = "dep"
                version = "0.1.0"
                edition = "2024"

                [lib]
                path = "native/entry/bridge.cpp"
                crate-type = ["staticlib"]
                native-include-root = "public"
                native-include-dirs = ["native/private"]
                native-defines = ["DEP_LOCAL_DEFINE=1", "DEP_PLAIN_DEFINE"]
            "#,
        )
        .file("dep/public/dep/api.hpp", "int dep_export();\n")
        .file(
            "dep/native/private/detail/helper.hpp",
            "inline int dep_private_answer() { return 17; }\n",
        )
        .file(
            "dep/native/entry/bridge.cpp",
            "#include <detail/helper.hpp>\n#ifndef DEP_LOCAL_DEFINE\n#error missing DEP_LOCAL_DEFINE\n#endif\n#ifndef DEP_PLAIN_DEFINE\n#error missing DEP_PLAIN_DEFINE\n#endif\nint dep_export() { return dep_private_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let private_include_path =
        source_fragment(&p.root().join("dep/native/private").display().to_string());
    let dep_compile_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("dep/native/entry/bridge.cpp")))
        .unwrap_or_else(|| panic!("missing dep compile line in log: {log}"));
    assert!(
        dep_compile_line.contains(&private_include_path),
        "missing private native include dir in dep compile line: {dep_compile_line}"
    );
    assert!(
        dep_compile_line.contains("DEP_LOCAL_DEFINE=1"),
        "missing manifest native define in dep compile line: {dep_compile_line}"
    );
    assert!(
        dep_compile_line.contains("DEP_PLAIN_DEFINE"),
        "missing valueless manifest native define in dep compile line: {dep_compile_line}"
    );

    let app_compile_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing app compile line in log: {log}"));
    assert!(
        !app_compile_line.contains(&private_include_path),
        "private native include dirs must not propagate downstream: {app_compile_line}"
    );
    assert!(
        !app_compile_line.contains("DEP_LOCAL_DEFINE=1"),
        "manifest native defines must remain target-local: {app_compile_line}"
    );
}

#[cargo_test]
fn native_manifest_link_search_and_libraries_customize_local_link_step() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [[bin]]
                name = "foo"
                path = "src/main.cpp"
                native-link-search = ["native/libs"]
                native-link-libraries = ["extra_dep"]
            "#,
        )
        .file("src/main.cpp", "int main() { return 0; }\n")
        .file("native/libs/.keep", "")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing link line in log: {log}"));
    let search_dir = source_fragment(&p.root().join("native/libs").display().to_string());
    assert!(
        link_line.contains(&search_dir),
        "missing manifest native link search dir in link line: {link_line}"
    );
    assert!(
        link_line.contains("-lextra_dep"),
        "missing manifest native link library in link line: {link_line}"
    );
}

#[cargo_test]
fn native_manifest_link_args_customize_local_link_step() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [[bin]]
                name = "foo"
                path = "src/main.cpp"
                native-link-args = ["-Wl,--custom-native-link-arg", "-Wl,--second-link-arg"]
            "#,
        )
        .file("src/main.cpp", "int main() { return 0; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing link line in log: {log}"));
    assert!(
        link_line.contains("-Wl,--custom-native-link-arg"),
        "missing first manifest native link arg in link line: {link_line}"
    );
    assert!(
        link_line.contains("-Wl,--second-link-arg"),
        "missing second manifest native link arg in link line: {link_line}"
    );
}

#[cargo_test]
fn native_target_feature_crt_static_adds_gnu_runtime_link_flags() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("RUSTFLAGS", "-Ctarget-feature=+crt-static")
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing link line in log: {log}"));
    assert!(
        link_line.contains("-static-libgcc"),
        "missing crt-static libgcc flag in link line: {link_line}"
    );
    assert!(
        link_line.contains("-static-libstdc++"),
        "missing crt-static libstdc++ flag in link line: {link_line}"
    );
}

#[cargo_test]
fn native_non_crt_target_feature_does_not_add_gnu_runtime_link_flags() {
    let Some(feature) = host_non_crt_target_feature() else {
        return;
    };

    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("RUSTFLAGS", format!("-Ctarget-feature=+{feature}"))
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing link line in log: {log}"));
    assert!(
        !link_line.contains("-static-libgcc"),
        "non-crt-static feature unexpectedly added libgcc runtime flag: {link_line}"
    );
    assert!(
        !link_line.contains("-static-libstdc++"),
        "non-crt-static feature unexpectedly added libstdc++ runtime flag: {link_line}"
    );
}

#[cargo_test]
fn native_target_feature_crt_static_skips_gnullvm_gnu_runtime_link_flags() {
    let Some(target) = installed_target("x86_64-pc-windows-gnullvm") else {
        return;
    };

    let cxx = tools::fake_native_tool_named("clang++.exe");
    let ar = tools::fake_native_tool_named("llvm-ar.exe");
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo(&format!("build -v --target {target}"))
        .env_remove("CC")
        .env("CXX", &cxx)
        .env("AR", &ar)
        .env("RUSTFLAGS", "-Ctarget-feature=+crt-static")
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing link line in log: {log}"));
    assert!(
        !link_line.contains("-static-libgcc"),
        "gnullvm unexpectedly inherited libgcc runtime flag: {link_line}"
    );
    assert!(
        !link_line.contains("-static-libstdc++"),
        "gnullvm unexpectedly inherited libstdc++ runtime flag: {link_line}"
    );
}

#[cargo_test]
fn native_target_feature_crt_static_adds_windows_gnu_runtime_link_flags() {
    let Some(target) = installed_target("x86_64-pc-windows-gnu") else {
        return;
    };

    let cxx = tools::fake_native_tool_named("g++.exe");
    let ar = tools::fake_native_tool_named("ar.exe");
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo(&format!("build -v --target {target}"))
        .env_remove("CC")
        .env("CXX", &cxx)
        .env("AR", &ar)
        .env("RUSTFLAGS", "-Ctarget-feature=+crt-static")
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let link_line = log
        .lines()
        .last()
        .unwrap_or_else(|| panic!("missing link line in log: {log}"));
    assert!(
        link_line.contains("-static-libgcc"),
        "windows-gnu missing libgcc runtime flag: {link_line}"
    );
    assert!(
        link_line.contains("-static-libstdc++"),
        "windows-gnu missing libstdc++ runtime flag: {link_line}"
    );
}

#[cargo_test]
fn native_target_feature_crt_static_adds_msvc_runtime_compile_flags() {
    let cxx = tools::fake_native_tool_named("cl.exe");
    let ar = tools::fake_native_tool_named("lib.exe");
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env_remove("CC")
        .env("CXX", &cxx)
        .env("AR", &ar)
        .env("RUSTFLAGS", "-Ctarget-feature=+crt-static")
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let compile_line = log
        .lines()
        .find(|line| line.starts_with("cl.exe ") && line.contains(" /c "))
        .unwrap_or_else(|| panic!("missing MSVC compile line in log: {log}"));
    assert!(
        compile_line.contains("/MTd"),
        "missing crt-static MSVC runtime flag in compile line: {compile_line}"
    );
    assert!(
        !compile_line.contains("/MD"),
        "unexpected dynamic CRT flag in compile line: {compile_line}"
    );
}

#[cargo_test]
fn native_target_feature_without_crt_static_uses_msvc_dynamic_runtime_flags() {
    let cxx = tools::fake_native_tool_named("cl.exe");
    let ar = tools::fake_native_tool_named("lib.exe");
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env_remove("CC")
        .env("CXX", &cxx)
        .env("AR", &ar)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let compile_line = log
        .lines()
        .find(|line| line.starts_with("cl.exe ") && line.contains(" /c "))
        .unwrap_or_else(|| panic!("missing MSVC compile line in log: {log}"));
    assert!(
        compile_line.contains("/MDd"),
        "missing dynamic MSVC runtime flag in compile line: {compile_line}"
    );
    assert!(
        !compile_line.contains("/MT"),
        "unexpected static CRT flag in compile line: {compile_line}"
    );
}

#[cargo_test]
fn native_non_crt_target_feature_does_not_change_msvc_runtime_flags() {
    let Some(feature) = host_non_crt_target_feature() else {
        return;
    };

    let cxx = tools::fake_native_tool_named("cl.exe");
    let ar = tools::fake_native_tool_named("lib.exe");
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env_remove("CC")
        .env("CXX", &cxx)
        .env("AR", &ar)
        .env("RUSTFLAGS", format!("-Ctarget-feature=+{feature}"))
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    let compile_line = log
        .lines()
        .find(|line| line.starts_with("cl.exe ") && line.contains(" /c "))
        .unwrap_or_else(|| panic!("missing MSVC compile line in log: {log}"));
    assert!(
        compile_line.contains("/MDd"),
        "non-crt-static feature unexpectedly changed MSVC runtime flag: {compile_line}"
    );
    assert!(
        !compile_line.contains("/MT"),
        "non-crt-static feature unexpectedly enabled static CRT: {compile_line}"
    );
}

#[cargo_test]
fn native_manifest_bin_required_features_are_honored() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [features]
                cli = []

                [[bin]]
                name = "runner"
                path = "native/runner.cpp"
                required-features = ["cli"]
            "#,
        )
        .file("native/runner.cpp", "int main() { return 0; }\n")
        .build();

    p.cargo("build --bin runner")
        .env("CXX", &tool)
        .env("AR", &tool)
        .with_status(101)
        .with_stderr_data(str![[r#"
[ERROR] target `runner` in package `foo` requires the features: `cli`
Consider enabling them by passing, e.g., `--features="cli"`

"#]])
        .run();

    p.cargo("build --bin runner --features cli")
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();

    assert!(p.bin("runner").is_file());
}

#[cargo_test]
fn native_cross_compile_uses_target_scoped_tool_env() {
    let Some(target) = available_cross_target() else {
        return;
    };
    let target_env = target_env_suffix(target);
    let cc_key = format!("CARGO_TARGET_{target_env}_NATIVE_CC");
    let cxx_key = format!("CARGO_TARGET_{target_env}_NATIVE_CXX");
    let ar_key = format!("CARGO_TARGET_{target_env}_NATIVE_AR");
    let cc = tools::fake_native_tool_named("fake-cc-cross");
    let cxx = tools::fake_native_tool_named("fake-cxx-cross");
    let ar = tools::fake_native_tool_named("fake-ar-cross");
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return helper(); }\n")
        .file("src/lib/helper.c", "int helper(void) { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo(&format!("build -v --target {target}"))
        .env_remove("CC")
        .env_remove("CXX")
        .env_remove("AR")
        .env(&cc_key, &cc)
        .env(&cxx_key, &cxx)
        .env(&ar_key, &ar)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    assert!(
        log.lines().any(|line| line.starts_with("fake-cc-cross ")),
        "expected target-scoped CC to be used for cross build, log was: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("fake-cxx-cross ")),
        "expected target-scoped CXX to be used for cross build, log was: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("fake-ar-cross ")),
        "expected target-scoped AR to be used for cross build, log was: {log}"
    );
}

#[cargo_test]
fn native_cross_compile_uses_target_scoped_tool_config() {
    let Some(target) = available_cross_target() else {
        return;
    };
    let global_cc = tools::fake_native_tool_named("fake-cc-global");
    let global_cxx = tools::fake_native_tool_named("fake-cxx-global");
    let global_ar = tools::fake_native_tool_named("fake-ar-global");
    let target_cc = tools::fake_native_tool_named("fake-cc-config");
    let target_cxx = tools::fake_native_tool_named("fake-cxx-config");
    let target_ar = tools::fake_native_tool_named("fake-ar-config");
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file(
            ".cargo/config.toml",
            &format!(
                r#"
                    [build]
                    native-cc = "{}"
                    native-cxx = "{}"
                    native-ar = "{}"

                    [target."{target}"]
                    native-cc = "{}"
                    native-cxx = "{}"
                    native-ar = "{}"
                "#,
                toml_path(&global_cc),
                toml_path(&global_cxx),
                toml_path(&global_ar),
                toml_path(&target_cc),
                toml_path(&target_cxx),
                toml_path(&target_ar),
            ),
        )
        .file("src/lib.cpp", "int native_answer() { return helper(); }\n")
        .file("src/lib/helper.c", "int helper(void) { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo(&format!("build -v --target {target}"))
        .env_remove("CC")
        .env_remove("CXX")
        .env_remove("AR")
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    assert!(
        log.lines().any(|line| line.starts_with("fake-cc-config ")),
        "expected target-scoped native-cc config to be used for cross build, log was: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("fake-cxx-config ")),
        "expected target-scoped native-cxx config to be used for cross build, log was: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("fake-ar-config ")),
        "expected target-scoped native-ar config to be used for cross build, log was: {log}"
    );
    assert!(
        !log.lines().any(|line| line.starts_with("fake-cc-global ")),
        "target-scoped native-cc config should override build.native-cc, log was: {log}"
    );
}

#[cargo_test]
fn native_cross_compile_uses_target_scoped_flag_env() {
    let Some(target) = available_cross_target() else {
        return;
    };
    let target_env = target_env_suffix(target);
    let cppflags_key = format!("CPPFLAGS_{target_env}");
    let cxxflags_key = format!("CXXFLAGS_{target_env}");
    let ldflags_key = format!("CARGO_TARGET_{target_env}_ENCODED_NATIVE_LDFLAGS");
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo(&format!("build -v --target {target}"))
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .env(&cppflags_key, "-DTARGET_CPP=1")
        .env(&cxxflags_key, "-std=c++23 -Winvalid-pch")
        .env(&ldflags_key, "-Wl,--as-needed\u{1f}-Wl,--gc-sections")
        .run();

    let log = p.read_file("native-tool.log");
    assert!(
        log.contains("-DTARGET_CPP=1"),
        "missing target-scoped CPPFLAGS entry: {log}"
    );
    assert!(
        log.contains("-std=c++23"),
        "missing target-scoped CXXFLAGS entry: {log}"
    );
    assert!(
        log.contains("-Winvalid-pch"),
        "missing second target-scoped CXXFLAGS entry: {log}"
    );
    assert!(
        log.contains("-Wl,--as-needed"),
        "missing target-scoped encoded LDFLAGS entry: {log}"
    );
    assert!(
        log.contains("-Wl,--gc-sections"),
        "missing second target-scoped encoded LDFLAGS entry: {log}"
    );
}

#[cargo_test]
fn native_compilation_receives_active_feature_defines() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [features]
                default = ["default-mode"]
                default-mode = []
                simd-mode = []
            "#,
        )
        .file("src/main.cpp", "int main() { return 0; }\n")
        .build();

    let default_log_path = p.root().join("native-tool-default.log");
    p.cargo("build -v --features simd-mode")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &default_log_path)
        .run();

    let default_log = p.read_file("native-tool-default.log");
    let default_line = default_log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing compile line in log: {default_log}"));
    assert!(
        default_line.contains("CARGO_FEATURE_DEFAULT_MODE=1"),
        "missing default feature define: {default_line}"
    );
    assert!(
        default_line.contains("CARGO_FEATURE_SIMD_MODE=1"),
        "missing explicit feature define: {default_line}"
    );

    let no_default_log_path = p.root().join("native-tool-no-default.log");
    p.cargo("build -v --no-default-features --features simd-mode")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &no_default_log_path)
        .run();

    let no_default_log = p.read_file("native-tool-no-default.log");
    let no_default_line = no_default_log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing compile line in log: {no_default_log}"));
    assert!(
        !no_default_line.contains("CARGO_FEATURE_DEFAULT_MODE=1"),
        "default feature define should be removed with --no-default-features: {no_default_line}"
    );
    assert!(
        no_default_line.contains("CARGO_FEATURE_SIMD_MODE=1"),
        "explicit feature define should remain enabled: {no_default_line}"
    );
}

#[cargo_test]
fn native_compilation_receives_target_cfg_defines() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/main.cpp", "int main() { return 0; }\n")
        .build();

    let log_path = p.root().join("native-tool-cfg.log");
    p.cargo("build -v")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool-cfg.log");
    let compile_line = log
        .lines()
        .find(|line| line.contains(&source_fragment("src/main.cpp")))
        .unwrap_or_else(|| panic!("missing compile line in log: {log}"));

    let target_os_define = match std::env::consts::OS {
        "windows" => "CARGO_CFG_TARGET_OS_WINDOWS=1",
        "macos" => "CARGO_CFG_TARGET_OS_MACOS=1",
        "linux" => "CARGO_CFG_TARGET_OS_LINUX=1",
        other => panic!("unexpected host OS for native cfg test: {other}"),
    };
    assert!(
        compile_line.contains(target_os_define),
        "missing target_os define `{target_os_define}`: {compile_line}"
    );

    let target_arch_define = format!(
        "CARGO_CFG_TARGET_ARCH_{}=1",
        std::env::consts::ARCH.replace('-', "_").to_uppercase()
    );
    assert!(
        compile_line.contains(&target_arch_define),
        "missing target_arch define `{target_arch_define}`: {compile_line}"
    );

    if cfg!(unix) {
        assert!(
            compile_line.contains("CARGO_CFG_UNIX=1"),
            "missing unix cfg define: {compile_line}"
        );
    }
    if cfg!(windows) {
        assert!(
            compile_line.contains("CARGO_CFG_WINDOWS=1"),
            "missing windows cfg define: {compile_line}"
        );
    }
}

#[cargo_test]
fn cpp_examples_tests_and_benches_are_auto_discovered() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/main.rs", "fn main() {}\n")
        .file("examples/demo.cpp", "int main() { return 0; }\n")
        .file("tests/smoke/main.cpp", "int main() { return 0; }\n")
        .file("benches/micro.cpp", "int main() { return 0; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v --examples --tests --benches")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    assert!(log.contains(&source_fragment("examples/demo.cpp")));
    assert!(log.contains(&source_fragment("tests/smoke/main.cpp")));
    assert!(log.contains(&source_fragment("benches/micro.cpp")));
}

#[cargo_test]
fn cpp_named_example_test_and_bench_targets_can_be_selected() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/main.rs", "fn main() {}\n")
        .file("examples/showcase/main.cpp", "int main() { return 0; }\n")
        .file("tests/smoke.cpp", "int main() { return 0; }\n")
        .file("benches/micro/main.cpp", "int main() { return 0; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v --example showcase --test smoke --bench micro")
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    assert!(log.contains(&source_fragment("examples/showcase/main.cpp")));
    assert!(log.contains(&source_fragment("tests/smoke.cpp")));
    assert!(log.contains(&source_fragment("benches/micro/main.cpp")));
}

#[cargo_test]
fn z_compile_commands_writes_workspace_database() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <bar/answer.hpp>\nint main() { return dep_answer(); }\n",
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("bar/include/bar/answer.hpp", "int dep_answer();\n")
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    p.cargo("build -Zcompile-commands")
        .masquerade_as_nightly_cargo(&["compile-commands"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .env("CARGO_ENCODED_NATIVE_CXXFLAGS", "-std=c++20\u{1f}-Wextra")
        .run();

    let database_path = p.root().join("compile_commands.json");
    assert!(database_path.is_file());

    let database = std::fs::read_to_string(database_path).unwrap();
    let commands: serde_json::Value = serde_json::from_str(&database).unwrap();
    let entries = commands.as_array().unwrap();

    let main_entry = entries
        .iter()
        .find(|entry| {
            normalize_json_path(entry["file"].as_str().unwrap()).ends_with("src/main.cpp")
        })
        .unwrap();
    let bar_entry = entries
        .iter()
        .find(|entry| {
            normalize_json_path(entry["file"].as_str().unwrap()).ends_with("bar/src/lib.cpp")
        })
        .unwrap();

    assert_eq!(
        normalize_json_path(main_entry["directory"].as_str().unwrap()),
        normalize_json_path(&p.root().display().to_string())
    );
    assert!(
        main_entry["output"].as_str().unwrap().ends_with(".o")
            || main_entry["output"].as_str().unwrap().ends_with(".obj")
    );

    let main_args = main_entry["arguments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect::<Vec<_>>();
    let bar_include = normalize_json_path(&p.root().join("bar/include").display().to_string());
    assert!(main_args.iter().any(|arg| *arg == "-std=c++20"));
    assert!(
        main_args
            .iter()
            .any(|arg| normalize_json_path(arg) == bar_include
                || normalize_json_path(arg).ends_with("/bar/include"))
    );

    let bar_args = bar_entry["arguments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(bar_args.iter().any(|arg| *arg == "-Wextra"));
}

#[cargo_test]
fn z_compile_commands_is_written_for_fresh_native_builds() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    p.cargo("build").env("CXX", &tool).env("AR", &tool).run();

    let database_path = p.root().join("compile_commands.json");
    assert!(!database_path.exists());

    p.cargo("build -Zcompile-commands")
        .masquerade_as_nightly_cargo(&["compile-commands"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();

    let database = std::fs::read_to_string(database_path).unwrap();
    let commands: serde_json::Value = serde_json::from_str(&database).unwrap();
    assert!(commands.as_array().unwrap().iter().any(|entry| {
        normalize_json_path(entry["file"].as_str().unwrap()).ends_with("src/main.cpp")
    }));
}

#[cargo_test]
fn z_compile_commands_writes_workspace_entries_for_multiple_members() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [workspace]
                members = ["app", "util"]
                resolver = "2"
            "#,
        )
        .file(
            "app/Cargo.toml",
            r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                util = { path = "../util" }
            "#,
        )
        .file(
            "app/src/main.cpp",
            "#include <util/value.hpp>\nint main() { return util_value(); }\n",
        )
        .file(
            "util/Cargo.toml",
            r#"
                [package]
                name = "util"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("util/include/util/value.hpp", "int util_value();\n")
        .file("util/src/lib.cpp", "int util_value() { return 11; }\n")
        .build();

    p.cargo("build --workspace -Zcompile-commands")
        .masquerade_as_nightly_cargo(&["compile-commands"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();

    let database = std::fs::read_to_string(p.root().join("compile_commands.json")).unwrap();
    let commands: serde_json::Value = serde_json::from_str(&database).unwrap();
    let entries = commands.as_array().unwrap();

    let app_entry = entries
        .iter()
        .find(|entry| {
            normalize_json_path(entry["file"].as_str().unwrap()).ends_with("app/src/main.cpp")
        })
        .unwrap();
    let util_entry = entries
        .iter()
        .find(|entry| {
            normalize_json_path(entry["file"].as_str().unwrap()).ends_with("util/src/lib.cpp")
        })
        .unwrap();

    assert!(normalize_json_path(app_entry["directory"].as_str().unwrap()).ends_with("/app"));
    assert!(normalize_json_path(util_entry["directory"].as_str().unwrap()).ends_with("/util"));
}

#[cargo_test]
fn z_compile_commands_deduplicates_same_source_across_units() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <bar/answer.hpp>\nint main() { return dep_answer(); }\n",
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("bar/include/bar/answer.hpp", "int dep_answer();\n")
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    p.cargo("build -Zcompile-commands")
        .masquerade_as_nightly_cargo(&["compile-commands"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();

    let database = std::fs::read_to_string(p.root().join("compile_commands.json")).unwrap();
    let commands: serde_json::Value = serde_json::from_str(&database).unwrap();
    let entries = commands.as_array().unwrap();
    let unique_entries = entries
        .iter()
        .map(|entry| {
            (
                normalize_json_path(entry["directory"].as_str().unwrap()),
                normalize_json_path(entry["file"].as_str().unwrap()),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(unique_entries.len(), entries.len());
    assert!(entries.iter().any(|entry| {
        normalize_json_path(entry["file"].as_str().unwrap()).ends_with("src/main.cpp")
    }));
}

#[cargo_test]
fn cpp_unused_header_change_does_not_rebuild_native_targets() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("include/used.hpp", "int used_answer();\n")
        .file("include/unused.hpp", "int unused_answer();\n")
        .file(
            "src/lib.cpp",
            "#include <used.hpp>\nint used_answer() { return 42; }\n",
        )
        .file(
            "src/main.cpp",
            "#include <used.hpp>\nint main() { return used_answer(); }\n",
        )
        .build();

    let log_path = p.root().join("native-tool.log");
    let build = || {
        p.cargo("build -v")
            .env("CXX", &tool)
            .env("AR", &tool)
            .env("FAKE_NATIVE_TOOL_LOG", &log_path)
            .run();
    };

    build();
    let first_count = p.read_file("native-tool.log").lines().count();

    build();
    let second_count = p.read_file("native-tool.log").lines().count();
    assert_eq!(second_count, first_count);

    sleep_ms(1000);
    p.change_file(
        "include/unused.hpp",
        "int unused_answer();\nint still_unused();\n",
    );

    build();
    let third_count = p.read_file("native-tool.log").lines().count();
    assert_eq!(
        third_count, second_count,
        "changing an unused native header should not trigger recompilation"
    );
}

#[cargo_test]
fn cpp_prefers_host_windows_toolchain_family_when_env_is_unset() {
    let host = rustc_host();
    let Some((compiler_name, archiver_name)) = preferred_host_windows_tools(&host) else {
        return;
    };

    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let tool_dir = p.root().join("fake-toolchain");
    std::fs::create_dir_all(&tool_dir).unwrap();
    for tool_name in [
        "cl.exe",
        "clang-cl.exe",
        "g++.exe",
        "clang++.exe",
        "lib.exe",
        "llvm-lib.exe",
        "ar.exe",
        "llvm-ar.exe",
    ] {
        let source = tools::fake_native_tool_named(tool_name);
        let destination = tool_dir.join(tool_name);
        if !destination.exists() {
            std::fs::copy(source, destination).unwrap();
        }
    }

    let mut path_entries = vec![tool_dir.clone()];
    path_entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path_entries).unwrap();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env_remove("CXX")
        .env_remove("AR")
        .env("PATH", &path)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    assert!(
        log.lines().any(|line| line.starts_with(compiler_name)),
        "expected compiler `{compiler_name}` to be selected for host `{host}`, log was: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with(archiver_name)),
        "expected archiver `{archiver_name}` to be selected for host `{host}`, log was: {log}"
    );
}

fn preferred_host_windows_tools(host: &str) -> Option<(&'static str, &'static str)> {
    if host.contains("windows-msvc") {
        Some(("cl.exe", "lib.exe"))
    } else if host.contains("windows-gnullvm") {
        Some(("clang++.exe", "llvm-ar.exe"))
    } else if host.contains("windows-gnu") {
        Some(("g++.exe", "ar.exe"))
    } else {
        None
    }
}

fn host_non_crt_target_feature() -> Option<&'static str> {
    let host = rustc_host();

    if host.starts_with("x86_64") || host.starts_with("i686") {
        Some("sse2")
    } else if host.starts_with("aarch64") {
        Some("neon")
    } else {
        None
    }
}

#[cargo_test]
fn cpp_can_bootstrap_msvc_env_via_vcvarsall() {
    if !rustc_host().contains("windows-msvc") {
        return;
    }
    if [
        "cl.exe",
        "clang-cl.exe",
        "clang++.exe",
        "g++.exe",
        "lib.exe",
        "llvm-lib.exe",
    ]
    .into_iter()
    .any(path_contains_tool)
    {
        return;
    }

    let p = project()
        .file("Cargo.toml", &basic_bin_manifest("foo"))
        .file("src/lib.cpp", "int native_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let tool_dir = p.root().join("fake-msvc-toolchain");
    std::fs::create_dir_all(&tool_dir).unwrap();
    for tool_name in ["cl.exe", "lib.exe"] {
        let source = tools::fake_native_tool_named(tool_name);
        let destination = tool_dir.join(tool_name);
        if !destination.exists() {
            std::fs::copy(source, destination).unwrap();
        }
    }

    let vcvarsall = p.root().join("vcvarsall.bat");
    std::fs::write(
        &vcvarsall,
        format!(
            "@echo off\r\nset PATH={}\\;%PATH%\r\nset INCLUDE={}\\include\r\nset LIB={}\\lib\r\n",
            tool_dir.display(),
            p.root().display(),
            p.root().display(),
        ),
    )
    .unwrap();

    let log_path = p.root().join("native-tool.log");
    p.cargo("build -v")
        .env_remove("CXX")
        .env_remove("AR")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("CARGO_NATIVE_MSVC_VCVARSALL", &vcvarsall)
        .env("FAKE_NATIVE_TOOL_LOG", &log_path)
        .run();

    let log = p.read_file("native-tool.log");
    assert!(
        log.lines().any(|line| line.starts_with("cl.exe ")),
        "expected cl.exe to come from vcvarsall-initialized environment, log was: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("lib.exe ")),
        "expected lib.exe to come from vcvarsall-initialized environment, log was: {log}"
    );
}

fn path_contains_tool(tool: &str) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .any(|dir| dir.join(tool).is_file())
}

#[cargo_test]
fn build_script_artifact_dep_exposes_native_staticlib_metadata() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"
                resolver = "2"
                build = "build.rs"

                [build-dependencies]
                bar = { path = "bar", artifact = ["staticlib"] }
            "#,
        )
        .file("src/lib.rs", "")
        .file(
            "build.rs",
            r#"
                fn main() {
                    let file = std::path::PathBuf::from(
                        std::env::var("CARGO_STATICLIB_FILE_BAR_bar").expect("staticlib file"),
                    );
                    assert!(file.is_file(), "missing native staticlib: {}", file.display());

                    let include = std::path::PathBuf::from(
                        std::env::var("CARGO_STATICLIB_INCLUDE_BAR").expect("include root"),
                    );
                    assert!(include.is_dir(), "missing native include dir: {}", include.display());
                }
            "#,
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("bar/include/bar/answer.hpp", "int dep_answer();\n")
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    p.cargo("build -v -Z bindeps")
        .masquerade_as_nightly_cargo(&["bindeps"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();
}

#[cargo_test]
fn build_script_artifact_dep_exposes_native_cdylib_metadata() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"
                resolver = "2"
                build = "build.rs"

                [build-dependencies]
                bar = { path = "bar", artifact = ["cdylib"] }
            "#,
        )
        .file("src/lib.rs", "")
        .file(
            "build.rs",
            r#"
                fn main() {
                    let file = std::path::PathBuf::from(
                        std::env::var("CARGO_CDYLIB_FILE_BAR_bar").expect("cdylib file"),
                    );
                    assert!(file.is_file(), "missing native cdylib: {}", file.display());

                    let include = std::path::PathBuf::from(
                        std::env::var("CARGO_CDYLIB_INCLUDE_BAR").expect("include root"),
                    );
                    assert!(include.is_dir(), "missing native include dir: {}", include.display());
                }
            "#,
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"

                [lib]
                crate-type = ["cdylib"]
            "#,
        )
        .file("bar/include/bar/answer.hpp", "int dep_answer();\n")
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    p.cargo("build -v -Z bindeps")
        .masquerade_as_nightly_cargo(&["bindeps"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();
}

#[cargo_test]
fn build_script_direct_dep_exposes_native_metadata() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"
                build = "build.rs"

                [build-dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file("src/lib.rs", "")
        .file(
            "build.rs",
            r#"
                fn main() {
                    let include = std::path::PathBuf::from(
                        std::env::var("CARGO_DEP_BAR_INCLUDE").expect("dep include"),
                    );
                    assert!(include.is_dir(), "missing include dir: {}", include.display());

                    let staticlib = std::path::PathBuf::from(
                        std::env::var("CARGO_DEP_BAR_STATICLIB").expect("dep staticlib"),
                    );
                    assert!(staticlib.is_file(), "missing staticlib path: {}", staticlib.display());
                }
            "#,
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("bar/include/bar/answer.hpp", "int dep_answer();\n")
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    p.cargo("build -v -Z any-build-script-metadata")
        .masquerade_as_nightly_cargo(&["any-build-script-metadata"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();
}

#[cargo_test]
fn build_script_direct_dep_exposes_native_cdylib_metadata() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"
                build = "build.rs"

                [build-dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file("src/lib.rs", "")
        .file(
            "build.rs",
            r#"
                fn main() {
                    let include = std::path::PathBuf::from(
                        std::env::var("CARGO_DEP_BAR_INCLUDE").expect("dep include"),
                    );
                    assert!(include.is_dir(), "missing include dir: {}", include.display());

                    let cdylib = std::path::PathBuf::from(
                        std::env::var("CARGO_DEP_BAR_CDYLIB").expect("dep cdylib"),
                    );
                    assert!(cdylib.is_file(), "missing cdylib path: {}", cdylib.display());
                }
            "#,
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"

                [lib]
                crate-type = ["cdylib"]
            "#,
        )
        .file("bar/include/bar/answer.hpp", "int dep_answer();\n")
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    p.cargo("build -v -Z any-build-script-metadata")
        .masquerade_as_nightly_cargo(&["any-build-script-metadata"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();
}

#[cargo_test]
fn build_script_direct_dep_without_public_include_omits_include_metadata() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"
                build = "build.rs"

                [build-dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file("src/lib.rs", "")
        .file(
            "build.rs",
            r#"
                fn main() {
                    assert!(
                        std::env::var_os("CARGO_DEP_BAR_INCLUDE").is_none(),
                        "unexpected public include metadata"
                    );

                    let staticlib = std::path::PathBuf::from(
                        std::env::var("CARGO_DEP_BAR_STATICLIB").expect("dep staticlib"),
                    );
                    assert!(staticlib.is_file(), "missing staticlib path: {}", staticlib.display());
                }
            "#,
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    p.cargo("build -v -Z any-build-script-metadata")
        .masquerade_as_nightly_cargo(&["any-build-script-metadata"])
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();
}

#[cargo_test]
fn cpp_dep_uses_build_script_declared_include_metadata() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file(
            "src/main.cpp",
            "#include <bar/generated.hpp>\nint main() { return dep_answer(); }\n",
        )
        .file(
            "bar/Cargo.toml",
            r#"
                [package]
                name = "bar"
                version = "0.1.0"
                edition = "2024"
                build = "build.rs"
                links = "bar"
            "#,
        )
        .file(
            "bar/build.rs",
            r#"
                use std::env;
                use std::fs;
                use std::path::PathBuf;

                fn main() {
                    let include_root = PathBuf::from(env::var("OUT_DIR").unwrap()).join("include");
                    let include_dir = include_root.join("bar");
                    fs::create_dir_all(&include_dir).unwrap();
                    fs::write(include_dir.join("generated.hpp"), "int dep_answer();\n").unwrap();
                    println!("cargo::metadata=include={}", include_root.display());
                }
            "#,
        )
        .file("bar/src/lib.cpp", "int dep_answer() { return 7; }\n")
        .build();

    let log_path = p.root().join("native-tool.log");
    let build = || {
        p.cargo("build -v")
            .env("CXX", &tool)
            .env("AR", &tool)
            .env("FAKE_NATIVE_TOOL_LOG", &log_path)
            .run();
    };

    build();

    let generated_header = p
        .glob("target/debug/build/bar-*/out/include/bar/generated.hpp")
        .filter_map(Result::ok)
        .next()
        .expect("generated header");
    let include_root = generated_header
        .parent()
        .and_then(|path| path.parent())
        .expect("generated include root");

    let log = p.read_file("native-tool.log");
    assert!(
        log.contains(&source_fragment(&include_root.display().to_string())),
        "missing build-script declared include dir in compile line: {log}"
    );

    let first_count = log.lines().count();
    build();
    let second_count = p.read_file("native-tool.log").lines().count();
    assert_eq!(
        second_count, first_count,
        "native compile unexpectedly reran on a fresh build"
    );

    sleep_ms(1000);
    std::fs::write(&generated_header, "int dep_answer();\nint dep_again();\n").unwrap();

    build();
    let third_count = p.read_file("native-tool.log").lines().count();
    assert!(
        third_count > second_count,
        "changing a build-script declared native header should trigger recompilation"
    );
}

#[cargo_test]
fn cpp_metadata_reports_native_targets() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [lib]
                path = "src/lib.cpp"
                crate-type = ["staticlib"]
                native-include-dirs = ["native/private"]
                native-defines = ["META_DEFINE=1"]
                [[bin]]
                name = "foo"
                path = "src/main.cpp"
                native-link-search = ["native/libs"]
                native-link-libraries = ["meta_dep"]
                native-link-args = ["-Wl,--meta-link-arg"]
            "#,
        )
        .file("include/answer.hpp", "int helper_answer();\n")
        .file("native/private/detail/private.hpp", "inline int private_answer() { return 42; }\n")
        .file("native/libs/.keep", "")
        .file(
            "src/lib.cpp",
            "#include <answer.hpp>\n#include <detail/private.hpp>\nint helper_answer();\nint native_answer() { return private_answer() + helper_answer() - 42; }\n",
        )
        .file("src/lib/helper.cpp", "int helper_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let output = p.cargo("metadata -q --format-version 1").run();
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let targets = metadata["packages"][0]["targets"].as_array().unwrap();

    let native_lib = targets
        .iter()
        .find(|target| target["crate_types"] == serde_json::json!(["staticlib"]))
        .unwrap();
    assert_eq!(native_lib["kind"], serde_json::json!(["lib"]));
    assert_eq!(native_lib["doc"], serde_json::json!(false));
    assert_eq!(native_lib["doctest"], serde_json::json!(false));
    assert_eq!(native_lib["test"], serde_json::json!(false));
    assert_eq!(native_lib["native_language"], serde_json::json!("c++"));
    assert!(normalize_json_path(native_lib["src_path"].as_str().unwrap()).ends_with("src/lib.cpp"));
    assert!(
        normalize_json_path(native_lib["native_include_root"].as_str().unwrap())
            .ends_with("/include")
    );
    assert!(
        normalize_json_path(native_lib["native_sources_root"].as_str().unwrap())
            .ends_with("/src/lib")
    );
    assert_eq!(
        native_lib["native_defines"],
        serde_json::json!(["META_DEFINE=1"])
    );
    let native_include_dirs = native_lib["native_include_dirs"].as_array().unwrap();
    assert_eq!(native_include_dirs.len(), 1);
    assert!(
        normalize_json_path(native_include_dirs[0].as_str().unwrap()).ends_with("/native/private")
    );
    assert!(native_lib["native_link_search"].is_null());
    assert!(native_lib["native_link_libraries"].is_null());

    let native_bin = targets
        .iter()
        .find(|target| target["crate_types"] == serde_json::json!(["bin"]))
        .unwrap();
    assert_eq!(native_bin["kind"], serde_json::json!(["bin"]));
    assert_eq!(native_bin["doc"], serde_json::json!(false));
    assert_eq!(native_bin["doctest"], serde_json::json!(false));
    assert_eq!(native_bin["test"], serde_json::json!(false));
    assert_eq!(native_bin["native_language"], serde_json::json!("c++"));
    assert!(
        normalize_json_path(native_bin["src_path"].as_str().unwrap()).ends_with("src/main.cpp")
    );
    assert!(
        normalize_json_path(native_bin["native_include_root"].as_str().unwrap())
            .ends_with("/include")
    );
    assert!(native_bin["native_sources_root"].is_null());
    assert_eq!(
        native_bin["native_link_libraries"],
        serde_json::json!(["meta_dep"])
    );
    assert_eq!(
        native_bin["native_link_args"],
        serde_json::json!(["-Wl,--meta-link-arg"])
    );
    let native_link_search = native_bin["native_link_search"].as_array().unwrap();
    assert_eq!(native_link_search.len(), 1);
    assert!(normalize_json_path(native_link_search[0].as_str().unwrap()).ends_with("/native/libs"));
}

#[cargo_test]
fn cpp_unit_graph_reports_native_targets() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [lib]
                path = "src/lib.cpp"
                crate-type = ["staticlib"]
                native-include-dirs = ["native/private"]
                native-defines = ["UNIT_GRAPH_DEFINE=1"]

                [[bin]]
                name = "foo"
                path = "src/main.cpp"
                native-link-search = ["native/libs"]
                native-link-libraries = ["unit_graph_dep"]
                native-link-args = ["-Wl,--unit-graph-link-arg"]
            "#,
        )
        .file("include/answer.hpp", "int helper_answer();\n")
        .file("native/private/detail/private.hpp", "inline int private_answer() { return 42; }\n")
        .file("native/libs/.keep", "")
        .file(
            "src/lib.cpp",
            "#include <answer.hpp>\n#include <detail/private.hpp>\nint helper_answer();\nint native_answer() { return private_answer() + helper_answer() - 42; }\n",
        )
        .file("src/lib/helper.cpp", "int helper_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let output = p
        .cargo("build --unit-graph -Zunstable-options")
        .masquerade_as_nightly_cargo(&["unit-graph"])
        .run();
    let graph: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let units = graph["units"].as_array().unwrap();

    let native_lib = units
        .iter()
        .find(|unit| unit["target"]["crate_types"] == serde_json::json!(["staticlib"]))
        .unwrap();
    assert_eq!(native_lib["target"]["kind"], serde_json::json!(["lib"]));
    assert_eq!(
        native_lib["target"]["native_language"],
        serde_json::json!("c++")
    );
    assert!(
        normalize_json_path(native_lib["target"]["src_path"].as_str().unwrap())
            .ends_with("src/lib.cpp")
    );
    assert!(
        normalize_json_path(
            native_lib["target"]["native_include_root"]
                .as_str()
                .unwrap()
        )
        .ends_with("/include")
    );
    assert!(
        normalize_json_path(
            native_lib["target"]["native_sources_root"]
                .as_str()
                .unwrap()
        )
        .ends_with("/src/lib")
    );
    assert_eq!(
        native_lib["target"]["native_defines"],
        serde_json::json!(["UNIT_GRAPH_DEFINE=1"])
    );
    let native_include_dirs = native_lib["target"]["native_include_dirs"]
        .as_array()
        .unwrap();
    assert_eq!(native_include_dirs.len(), 1);
    assert!(
        normalize_json_path(native_include_dirs[0].as_str().unwrap()).ends_with("/native/private")
    );

    let native_bin = units
        .iter()
        .find(|unit| unit["target"]["crate_types"] == serde_json::json!(["bin"]))
        .unwrap();
    assert_eq!(native_bin["target"]["kind"], serde_json::json!(["bin"]));
    assert_eq!(
        native_bin["target"]["native_language"],
        serde_json::json!("c++")
    );
    assert!(
        normalize_json_path(native_bin["target"]["src_path"].as_str().unwrap())
            .ends_with("src/main.cpp")
    );
    assert!(
        normalize_json_path(
            native_bin["target"]["native_include_root"]
                .as_str()
                .unwrap()
        )
        .ends_with("/include")
    );
    assert!(native_bin["target"]["native_sources_root"].is_null());
    assert_eq!(
        native_bin["target"]["native_link_libraries"],
        serde_json::json!(["unit_graph_dep"])
    );
    assert_eq!(
        native_bin["target"]["native_link_args"],
        serde_json::json!(["-Wl,--unit-graph-link-arg"])
    );
    let native_link_search = native_bin["target"]["native_link_search"]
        .as_array()
        .unwrap();
    assert_eq!(native_link_search.len(), 1);
    assert!(normalize_json_path(native_link_search[0].as_str().unwrap()).ends_with("/native/libs"));
}

#[cargo_test]
fn cpp_metadata_reports_native_artifact_dependencies_in_resolve() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                staticdep = { path = "staticdep", artifact = ["staticlib"] }
                shareddep = { path = "shareddep", artifact = ["cdylib"] }
                bindep = { path = "bindep", artifact = ["bin:runner"] }
            "#,
        )
        .file("src/lib.rs", "")
        .file(
            "staticdep/Cargo.toml",
            r#"
                [package]
                name = "staticdep"
                version = "0.1.0"
                edition = "2024"
            "#,
        )
        .file(
            "staticdep/src/lib.cpp",
            "int static_answer() { return 1; }\n",
        )
        .file(
            "shareddep/Cargo.toml",
            r#"
                [package]
                name = "shareddep"
                version = "0.1.0"
                edition = "2024"

                [lib]
                crate-type = ["cdylib"]
            "#,
        )
        .file(
            "shareddep/src/lib.cpp",
            "int shared_answer() { return 2; }\n",
        )
        .file(
            "bindep/Cargo.toml",
            r#"
                [package]
                name = "bindep"
                version = "0.1.0"
                edition = "2024"

                [[bin]]
                name = "runner"
                path = "src/main.cpp"
            "#,
        )
        .file("bindep/src/main.cpp", "int main() { return 0; }\n")
        .build();

    let output = p
        .cargo("metadata -q --format-version 1 -Z bindeps")
        .masquerade_as_nightly_cargo(&["bindeps"])
        .run();
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let packages = metadata["packages"].as_array().unwrap();
    let root_pkg_id = packages
        .iter()
        .find(|pkg| pkg["name"] == serde_json::json!("foo"))
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let root_node = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"].as_str() == Some(root_pkg_id))
        .unwrap();
    let deps = root_node["deps"].as_array().unwrap();

    let static_dep = deps
        .iter()
        .find(|dep| dep["pkg"].as_str().unwrap().contains("/staticdep#0.1.0"))
        .unwrap();
    let static_dep_kinds = static_dep["dep_kinds"].as_array().unwrap();
    assert_eq!(static_dep_kinds.len(), 1);
    assert_eq!(
        static_dep_kinds[0]["artifact"],
        serde_json::json!("staticlib")
    );
    assert_eq!(static_dep_kinds[0]["kind"], serde_json::Value::Null);
    assert_eq!(
        static_dep_kinds[0]["extern_name"],
        serde_json::json!("staticdep")
    );
    assert!(static_dep_kinds[0]["bin_name"].is_null());
    assert!(static_dep_kinds[0]["compile_target"].is_null());

    let shared_dep = deps
        .iter()
        .find(|dep| dep["pkg"].as_str().unwrap().contains("/shareddep#0.1.0"))
        .unwrap();
    let shared_dep_kinds = shared_dep["dep_kinds"].as_array().unwrap();
    assert_eq!(shared_dep_kinds.len(), 1);
    assert_eq!(shared_dep_kinds[0]["artifact"], serde_json::json!("cdylib"));
    assert_eq!(shared_dep_kinds[0]["kind"], serde_json::Value::Null);
    assert_eq!(
        shared_dep_kinds[0]["extern_name"],
        serde_json::json!("shareddep")
    );
    assert!(shared_dep_kinds[0]["bin_name"].is_null());
    assert!(shared_dep_kinds[0]["compile_target"].is_null());

    let bin_dep = deps
        .iter()
        .find(|dep| dep["pkg"].as_str().unwrap().contains("/bindep#0.1.0"))
        .unwrap();
    assert_eq!(bin_dep["name"], serde_json::json!(""));
    let bin_dep_kinds = bin_dep["dep_kinds"].as_array().unwrap();
    assert_eq!(bin_dep_kinds.len(), 1);
    assert_eq!(bin_dep_kinds[0]["artifact"], serde_json::json!("bin"));
    assert_eq!(bin_dep_kinds[0]["kind"], serde_json::Value::Null);
    assert_eq!(bin_dep_kinds[0]["bin_name"], serde_json::json!("runner"));
    assert_eq!(bin_dep_kinds[0]["extern_name"], serde_json::json!("runner"));
    assert!(bin_dep_kinds[0]["compile_target"].is_null());
}

#[cargo_test]
fn cpp_compiler_artifact_messages_report_native_target_hints() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [lib]
                path = "src/lib.cpp"
                crate-type = ["staticlib"]
                native-include-dirs = ["native/private"]
                native-defines = ["JSON_DEFINE=1"]

                [[bin]]
                name = "foo"
                path = "src/main.cpp"
                native-link-search = ["native/libs"]
                native-link-libraries = ["json_dep"]
                native-link-args = ["-Wl,--json-link-arg"]
            "#,
        )
        .file("native/private/detail/private.hpp", "inline int private_answer() { return 42; }\n")
        .file("native/libs/.keep", "")
        .file(
            "src/lib.cpp",
            "#include <answer.hpp>\n#include <detail/private.hpp>\nint helper_answer();\nint native_answer() { return private_answer() + helper_answer() - 42; }\n",
        )
        .file("src/lib/helper.cpp", "int helper_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    let output = p
        .cargo("build --message-format json")
        .env("CXX", &tool)
        .env("AR", &tool)
        .run();

    let messages = String::from_utf8(output.stdout).unwrap();
    let artifacts = messages
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == serde_json::json!("compiler-artifact"))
        .collect::<Vec<_>>();

    let native_lib = artifacts
        .iter()
        .find(|artifact| artifact["target"]["crate_types"] == serde_json::json!(["staticlib"]))
        .unwrap();
    assert_eq!(
        native_lib["target"]["native_language"],
        serde_json::json!("c++")
    );
    assert_eq!(
        native_lib["target"]["native_defines"],
        serde_json::json!(["JSON_DEFINE=1"])
    );
    let native_include_dirs = native_lib["target"]["native_include_dirs"]
        .as_array()
        .unwrap();
    assert_eq!(native_include_dirs.len(), 1);
    assert!(
        normalize_json_path(native_include_dirs[0].as_str().unwrap()).ends_with("/native/private")
    );

    let native_bin = artifacts
        .iter()
        .find(|artifact| artifact["target"]["kind"] == serde_json::json!(["bin"]))
        .unwrap();
    assert_eq!(
        native_bin["target"]["native_language"],
        serde_json::json!("c++")
    );
    assert_eq!(
        native_bin["target"]["native_link_libraries"],
        serde_json::json!(["json_dep"])
    );
    assert_eq!(
        native_bin["target"]["native_link_args"],
        serde_json::json!(["-Wl,--json-link-arg"])
    );
    let native_link_search = native_bin["target"]["native_link_search"]
        .as_array()
        .unwrap();
    assert_eq!(native_link_search.len(), 1);
    assert!(normalize_json_path(native_link_search[0].as_str().unwrap()).ends_with("/native/libs"));
}

#[cargo_test]
fn cpp_sbom_reports_native_target_hints() {
    let tool = tools::fake_native_tool();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.1.0"
                edition = "2024"

                [lib]
                path = "src/lib.cpp"
                crate-type = ["staticlib"]
                native-include-dirs = ["native/private"]
                native-defines = ["SBOM_DEFINE=1"]

                [[bin]]
                name = "foo"
                path = "src/main.cpp"
                native-link-search = ["native/libs"]
                native-link-libraries = ["sbom_dep"]
                native-link-args = ["-Wl,--sbom-link-arg"]
            "#,
        )
        .file("include/answer.hpp", "int helper_answer();\n")
        .file("native/private/detail/private.hpp", "inline int private_answer() { return 42; }\n")
        .file("native/libs/.keep", "")
        .file(
            "src/lib.cpp",
            "#include <answer.hpp>\n#include <detail/private.hpp>\nint helper_answer();\nint native_answer() { return private_answer() + helper_answer() - 42; }\n",
        )
        .file("src/lib/helper.cpp", "int helper_answer() { return 42; }\n")
        .file(
            "src/main.cpp",
            "int native_answer();\nint main() { return native_answer(); }\n",
        )
        .build();

    p.cargo("build -Zsbom")
        .env("CARGO_BUILD_SBOM", "true")
        .env("CXX", &tool)
        .env("AR", &tool)
        .masquerade_as_nightly_cargo(&["sbom"])
        .run();

    let mut sbom_files = Vec::new();
    collect_sbom_files(&p.target_debug_dir(), &mut sbom_files);

    let sboms = sbom_files
        .iter()
        .map(|path| {
            let sbom: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let root_index = sbom["root"].as_u64().unwrap() as usize;
            sbom["crates"][root_index].clone()
        })
        .collect::<Vec<_>>();

    let native_lib_root = sboms
        .iter()
        .find(|crate_data| crate_data["crate_types"] == serde_json::json!(["staticlib"]))
        .unwrap_or_else(|| panic!("missing staticlib sbom root in {sboms:#?}"));

    assert_eq!(native_lib_root["kind"], serde_json::json!(["lib"]));
    assert_eq!(
        native_lib_root["crate_types"],
        serde_json::json!(["staticlib"])
    );
    assert_eq!(native_lib_root["native_language"], serde_json::json!("c++"));
    assert!(
        normalize_json_path(native_lib_root["native_include_root"].as_str().unwrap())
            .ends_with("/include")
    );
    assert!(
        normalize_json_path(native_lib_root["native_sources_root"].as_str().unwrap())
            .ends_with("/src/lib")
    );
    assert_eq!(
        native_lib_root["native_defines"],
        serde_json::json!(["SBOM_DEFINE=1"])
    );
    let native_include_dirs = native_lib_root["native_include_dirs"].as_array().unwrap();
    assert_eq!(native_include_dirs.len(), 1);
    assert!(
        normalize_json_path(native_include_dirs[0].as_str().unwrap()).ends_with("/native/private")
    );

    let native_bin_root = sboms
        .iter()
        .find(|crate_data| crate_data["crate_types"] == serde_json::json!(["bin"]))
        .unwrap_or_else(|| panic!("missing bin sbom root in {sboms:#?}"));

    assert_eq!(native_bin_root["kind"], serde_json::json!(["bin"]));
    assert_eq!(native_bin_root["crate_types"], serde_json::json!(["bin"]));
    assert_eq!(native_bin_root["native_language"], serde_json::json!("c++"));
    assert!(
        normalize_json_path(native_bin_root["native_include_root"].as_str().unwrap())
            .ends_with("/include")
    );
    assert!(native_bin_root["native_sources_root"].is_null());
    assert_eq!(
        native_bin_root["native_link_libraries"],
        serde_json::json!(["sbom_dep"])
    );
    assert_eq!(
        native_bin_root["native_link_args"],
        serde_json::json!(["-Wl,--sbom-link-arg"])
    );
    let native_link_search = native_bin_root["native_link_search"].as_array().unwrap();
    assert_eq!(native_link_search.len(), 1);
    assert!(normalize_json_path(native_link_search[0].as_str().unwrap()).ends_with("/native/libs"));
}
