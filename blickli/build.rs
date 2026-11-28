use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=swift/coreai_bridge.swift");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let obj_path = out_dir.join("coreai_bridge.o");
    let lib_path = out_dir.join("libcoreai_bridge.a");
    let module_cache = out_dir.join("swift-module-cache");
    fs::create_dir_all(&module_cache).expect("failed to create swift module cache dir");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "aarch64".to_owned());
    let swift_arch = match arch.as_str() {
        "aarch64" => "arm64",
        other => other,
    };
    let target = format!("{swift_arch}-apple-macosx27.0");

    let status = Command::new("swiftc")
        .env("CLANG_MODULE_CACHE_PATH", &module_cache)
        .env("TMPDIR", &out_dir)
        .args([
            "-O",
            "-parse-as-library",
            "-emit-object",
            "-target",
            &target,
            "-module-cache-path",
        ])
        .arg(&module_cache)
        .args(["swift/coreai_bridge.swift", "-o"])
        .arg(&obj_path)
        .status()
        .expect("failed to invoke swiftc");
    assert!(status.success(), "swiftc failed compiling coreai_bridge.swift");

    let status = Command::new("ar")
        .args(["rcs"])
        .arg(&lib_path)
        .arg(&obj_path)
        .status()
        .expect("failed to invoke ar");
    assert!(status.success(), "ar failed archiving libcoreai_bridge.a");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=coreai_bridge");
    println!("cargo:rustc-link-lib=framework=CoreAI");
    println!("cargo:rustc-link-lib=framework=Foundation");

    // Link macOS system Swift runtime libraries from the SDK / dyld shared cache.
    if let Ok(sdk_out) = Command::new("xcrun").arg("--show-sdk-path").output() {
        if sdk_out.status.success() {
            let sdk = String::from_utf8_lossy(&sdk_out.stdout).trim().to_owned();
            println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
            println!("cargo:rustc-link-search=framework={sdk}/System/Library/SubFrameworks");
        }
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-link-lib=dylib=swiftCore");
    println!("cargo:rustc-link-lib=dylib=swift_Concurrency");
    println!("cargo:rustc-link-lib=dylib=swiftFoundation");
    println!("cargo:rustc-link-lib=dylib=swiftDispatch");
    println!("cargo:rustc-link-lib=dylib=swiftDarwin");
    println!("cargo:rustc-link-lib=dylib=swiftObjectiveC");
}
