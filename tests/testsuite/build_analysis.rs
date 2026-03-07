//! Tests for `-Zbuild-analysis`.

use crate::prelude::*;
use crate::utils::tools;

use cargo_test_support::basic_manifest;
use cargo_test_support::compare::assert_e2e;
use cargo_test_support::paths::log_file;
use cargo_test_support::project;
use cargo_test_support::str;

#[cargo_test]
fn gated() {
    let p = project()
        .file("Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("src/lib.rs", "")
        .build();

    p.cargo("check")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .with_stderr_data(str![[r#"
[WARNING] ignoring 'build.analysis' config, pass `-Zbuild-analysis` to enable it
[CHECKING] foo v0.0.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();
}

#[cargo_test]
fn one_logfile_per_invocation() {
    let p = project()
        .file("Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("src/lib.rs", "")
        .build();

    // First invocation
    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .with_stderr_data(str![[r#"
[CHECKING] foo v0.0.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    let _ = get_log(0);

    // Second invocation
    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    let _ = get_log(1);
}

#[cargo_test]
fn log_msg_build_started() {
    let p = project()
        .file("Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("src/lib.rs", "")
        .build();

    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .run();

    assert_e2e().eq(
        &get_log(0),
        str![[r#"
[
  {
    "command": "{...}",
    "cwd": "[ROOT]/foo",
    "host": "[HOST_TARGET]",
    "jobs": "{...}",
    "num_cpus": "{...}",
    "profile": "dev",
    "reason": "build-started",
    "run_id": "[..]T[..]Z-[..]",
    "rustc_version": "1.[..]",
    "rustc_version_verbose": "{...}",
    "target_dir": "[ROOT]/foo/target",
    "timestamp": "[..]T[..]Z",
    "workspace_root": "[ROOT]/foo"
  },
  "{...}"
]
"#]]
        .is_json()
        .against_jsonlines(),
    );
}

#[cargo_test]
fn log_msg_timing_info() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.0"
                edition = "2015"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file("src/lib.rs", "")
        .file("bar/Cargo.toml", &basic_manifest("bar", "0.0.0"))
        .file("bar/src/lib.rs", "")
        .build();

    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .run();

    assert_e2e().eq(
        &get_log(0),
        str![[r#"
[
  "{...}",
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-started",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-rmeta-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z",
    "unblocked": [
      1
    ]
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-started",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-rmeta-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  }
]
"#]]
        .is_json()
        .against_jsonlines(),
    );
}

#[cargo_test(nightly, reason = "rustc --json=timings is unstable")]
fn log_msg_timing_info_section_timings() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.0"
                edition = "2015"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file("src/main.rs", "fn main() {}")
        .file("bar/Cargo.toml", &basic_manifest("bar", "0.0.0"))
        .file("bar/src/lib.rs", "")
        .build();

    p.cargo("check -Zbuild-analysis -Zsection-timings")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis", "section-timings"])
        .run();

    assert_e2e().eq(
        &get_log(0),
        str![[r#"
[
  "{...}",
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-started",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-section-started",
    "run_id": "[..]T[..]Z-[..]",
    "section": "codegen",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-rmeta-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-section-finished",
    "run_id": "[..]T[..]Z-[..]",
    "section": "codegen",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-section-started",
    "run_id": "[..]T[..]Z-[..]",
    "section": "link",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-section-finished",
    "run_id": "[..]T[..]Z-[..]",
    "section": "link",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 0,
    "reason": "unit-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z",
    "unblocked": [
      1
    ]
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-started",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-section-started",
    "run_id": "[..]T[..]Z-[..]",
    "section": "codegen",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-rmeta-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-section-finished",
    "run_id": "[..]T[..]Z-[..]",
    "section": "codegen",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-section-started",
    "run_id": "[..]T[..]Z-[..]",
    "section": "link",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-section-finished",
    "run_id": "[..]T[..]Z-[..]",
    "section": "link",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "index": 1,
    "reason": "unit-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  }
]
"#]]
        .is_json()
        .against_jsonlines(),
    );
}

#[cargo_test]
fn log_rebuild_reason_fresh_build() {
    let p = project()
        .file("Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("src/lib.rs", "")
        .build();

    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .with_stderr_data(str![[r#"
[CHECKING] foo v0.0.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    assert_e2e().eq(
        &get_log(0),
        str![[r#"
[
  "{...}",
  {
    "...": "{...}",
    "reason": "unit-graph-finished"
  },
  {
    "index": 0,
    "reason": "unit-fingerprint",
    "run_id": "[..]T[..]Z-[..]",
    "status": "new",
    "timestamp": "[..]T[..]Z"
  },
  {
    "...": "{...}",
    "reason": "unit-started"
  },
  "{...}"
]
"#]]
        .is_json()
        .against_jsonlines(),
    );
}

#[cargo_test]
fn log_rebuild_reason_file_changed() {
    // Test that changing a file logs the appropriate rebuild reason
    let p = project()
        .file("Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("src/lib.rs", "")
        .build();

    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .run();

    assert_e2e().eq(
        &get_log(0),
        str![[r#"
[
  "{...}",
  {
    "...": "{...}",
    "reason": "unit-graph-finished"
  },
  {
    "index": 0,
    "reason": "unit-fingerprint",
    "run_id": "[..]T[..]Z-[..]",
    "status": "new",
    "timestamp": "[..]T[..]Z"
  },
  {
    "...": "{...}",
    "reason": "unit-started"
  },
  "{...}"
]
"#]]
        .is_json()
        .against_jsonlines(),
    );

    // Change source file
    p.change_file("src/lib.rs", "//! comment");

    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .with_stderr_data(str![[r#"
[CHECKING] foo v0.0.0 ([ROOT]/foo)
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    // File changes SHOULD log rebuild-reason
    assert_e2e().eq(
        &get_log(1),
        str![[r#"
[
  "{...}",
  {
    "...": "{...}",
    "reason": "unit-graph-finished"
  },
  {
    "cause": {
      "dirty_reason": "fs-status-outdated",
      "fs_status": "stale-item",
      "reference": "[ROOT]/foo/target/debug/.fingerprint/foo-[HASH]/dep-lib-foo",
      "reference_mtime": "{...}",
      "stale": "[ROOT]/foo/src/lib.rs",
      "stale_item": "changed-file",
      "stale_mtime": "{...}"
    },
    "index": 0,
    "reason": "unit-fingerprint",
    "run_id": "[..]T[..]Z-[..]",
    "status": "dirty",
    "timestamp": "[..]T[..]Z"
  },
  {
    "...": "{...}",
    "reason": "unit-started"
  },
  "{...}"
]
"#]]
        .is_json()
        .against_jsonlines(),
    );
}

#[cargo_test]
fn log_rebuild_reason_no_rebuild() {
    let p = project()
        .file("Cargo.toml", &basic_manifest("foo", "0.0.0"))
        .file("src/lib.rs", "")
        .build();

    // First build
    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .run();

    assert_e2e().eq(
        &get_log(0),
        str![[r#"
[
  "{...}",
  {
    "...": "{...}",
    "reason": "unit-graph-finished"
  },
  {
    "index": 0,
    "reason": "unit-fingerprint",
    "run_id": "[..]T[..]Z-[..]",
    "status": "new",
    "timestamp": "[..]T[..]Z"
  },
  {
    "...": "{...}",
    "reason": "unit-started"
  },
  "{...}"
]
"#]]
        .is_json()
        .against_jsonlines(),
    );

    // Second build without changes
    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .with_stderr_data(str![[r#"
[FINISHED] `dev` profile [unoptimized + debuginfo] target(s) in [ELAPSED]s

"#]])
        .run();

    // Should NOT contain any rebuild-reason messages since nothing rebuilt
    assert_e2e().eq(
        &get_log(1),
        str![[r#"
[
  "{...}",
  {
    "...": "{...}",
    "reason": "unit-graph-finished"
  },
  {
    "index": 0,
    "reason": "unit-fingerprint",
    "run_id": "[..]T[..]Z-[..]",
    "status": "fresh",
    "timestamp": "[..]T[..]Z"
  }
]
"#]]
        .is_json()
        .against_jsonlines(),
    );
}

#[cargo_test]
fn log_msg_unit_graph() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.0"
                edition = "2015"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file("src/lib.rs", "")
        .file("build.rs", "fn main() {}")
        .file("bar/Cargo.toml", &basic_manifest("bar", "0.0.0"))
        .file("bar/src/lib.rs", "")
        .build();

    // `cargo doc` generates more units than `cargo check`
    // * check bar
    // * build foo build.rs
    // * run foo build.rs
    // * doc foo
    // * doc bar
    p.cargo("doc -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis", "section-timings"])
        .run();

    assert_e2e().eq(
        &get_log(0),
        str![[r#"
[
  "{...}",
  {
    "elapsed": "{...}",
    "reason": "unit-graph-started",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "index": 0,
    "mode": "check",
    "package_id": "path+[ROOTURL]/foo/bar#0.0.0",
    "platform": "[HOST_TARGET]",
    "reason": "unit-registered",
    "run_id": "[..]T[..]Z-[..]",
    "target": {
      "kind": "lib",
      "name": "bar"
    },
    "timestamp": "[..]T[..]Z"
  },
  {
    "index": 1,
    "mode": "doc",
    "package_id": "path+[ROOTURL]/foo/bar#0.0.0",
    "platform": "[HOST_TARGET]",
    "reason": "unit-registered",
    "run_id": "[..]T[..]Z-[..]",
    "target": {
      "kind": "lib",
      "name": "bar"
    },
    "timestamp": "[..]T[..]Z"
  },
  {
    "dependencies": [
      0,
      1,
      4
    ],
    "index": 2,
    "mode": "doc",
    "package_id": "path+[ROOTURL]/foo#0.0.0",
    "platform": "[HOST_TARGET]",
    "reason": "unit-registered",
    "requested": true,
    "run_id": "[..]T[..]Z-[..]",
    "target": {
      "kind": "lib",
      "name": "foo"
    },
    "timestamp": "[..]T[..]Z"
  },
  {
    "index": 3,
    "mode": "build",
    "package_id": "path+[ROOTURL]/foo#0.0.0",
    "platform": "[HOST_TARGET]",
    "reason": "unit-registered",
    "run_id": "[..]T[..]Z-[..]",
    "target": {
      "kind": "build-script",
      "name": "build-script-build"
    },
    "timestamp": "[..]T[..]Z"
  },
  {
    "dependencies": [
      3
    ],
    "index": 4,
    "mode": "run-custom-build",
    "package_id": "path+[ROOTURL]/foo#0.0.0",
    "platform": "[HOST_TARGET]",
    "reason": "unit-registered",
    "run_id": "[..]T[..]Z-[..]",
    "target": {
      "kind": "build-script",
      "name": "build-script-build"
    },
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "reason": "unit-graph-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  "{...}"
]
"#]]
        .is_json()
        .against_jsonlines(),
    );
}

#[cargo_test]
fn log_msg_resolution_events() {
    let p = project()
        .file(
            "Cargo.toml",
            r#"
                [package]
                name = "foo"
                version = "0.0.0"
                edition = "2015"

                [dependencies]
                bar = { path = "bar" }
            "#,
        )
        .file("src/lib.rs", "")
        .file("build.rs", "fn main() {}")
        .file("bar/Cargo.toml", &basic_manifest("bar", "0.0.0"))
        .file("bar/src/lib.rs", "")
        .build();

    p.cargo("doc -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .masquerade_as_nightly_cargo(&["build-analysis", "section-timings"])
        .run();

    assert_e2e().eq(
        &get_log(0),
        str![[r#"
[
  "{...}",
  {
    "elapsed": "{...}",
    "reason": "resolution-started",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  {
    "elapsed": "{...}",
    "reason": "resolution-finished",
    "run_id": "[..]T[..]Z-[..]",
    "timestamp": "[..]T[..]Z"
  },
  "{...}"
]
"#]]
        .is_json()
        .against_jsonlines(),
    );
}

#[cargo_test]
fn native_unit_registered_reports_target_hints() {
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
            native-defines = ["BUILD_ANALYSIS_DEFINE=1"]

            [[bin]]
            name = "foo"
            path = "src/main.cpp"
            native-link-search = ["native/libs"]
            native-link-libraries = ["analysis_dep"]
            native-link-args = ["-Wl,--analysis-link-arg"]
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

    p.cargo("check -Zbuild-analysis")
        .env("CARGO_BUILD_ANALYSIS_ENABLED", "true")
        .env("CXX", &tool)
        .env("AR", &tool)
        .masquerade_as_nightly_cargo(&["build-analysis"])
        .run();

    let log = std::fs::read_to_string(log_file(0)).unwrap();
    let registered = log
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| {
            message["reason"] == serde_json::json!("unit-registered")
                && message["package_id"]
                    .as_str()
                    .is_some_and(|package_id| package_id.contains("/foo#"))
                && message["target"]["crate_types"] == serde_json::json!(["staticlib"])
        })
        .unwrap();

    assert_eq!(registered["target"]["kind"], serde_json::json!("lib"));
    assert_eq!(
        registered["target"]["native_language"],
        serde_json::json!("c++")
    );
    let native_include_root = registered["target"]["native_include_root"]
        .as_str()
        .unwrap_or_else(|| panic!("missing native_include_root in {registered:#?}"));
    assert!(native_include_root.replace('\\', "/").ends_with("/include"));
    let native_sources_root = registered["target"]["native_sources_root"]
        .as_str()
        .unwrap_or_else(|| panic!("missing native_sources_root in {registered:#?}"));
    assert!(native_sources_root.replace('\\', "/").ends_with("/src/lib"));
    let native_include_dirs = registered["target"]["native_include_dirs"]
        .as_array()
        .unwrap_or_else(|| panic!("missing native_include_dirs in {registered:#?}"));
    assert_eq!(native_include_dirs.len(), 1);
    assert!(
        native_include_dirs[0]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("/native/private")
    );
    assert_eq!(
        registered["target"]["native_defines"],
        serde_json::json!(["BUILD_ANALYSIS_DEFINE=1"])
    );

    let registered_bin = log
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| {
            message["reason"] == serde_json::json!("unit-registered")
                && message["package_id"]
                    .as_str()
                    .is_some_and(|package_id| package_id.contains("/foo#"))
                && message["target"]["kind"] == serde_json::json!("bin")
        })
        .unwrap();
    assert_eq!(
        registered_bin["target"]["native_link_libraries"],
        serde_json::json!(["analysis_dep"])
    );
    assert_eq!(
        registered_bin["target"]["native_link_args"],
        serde_json::json!(["-Wl,--analysis-link-arg"])
    );
}

fn get_log(idx: usize) -> String {
    std::fs::read_to_string(log_file(idx)).unwrap()
}
