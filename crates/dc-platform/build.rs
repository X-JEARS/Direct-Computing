use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=native/x264_bridge.c");
    println!("cargo:rerun-if-env-changed=X264_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=X264_LIB_DIR");
    println!("cargo:rerun-if-env-changed=X264_STATIC");
    if env::var_os("CARGO_FEATURE_X264").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
    {
        return;
    }

    let mut build = cc::Build::new();
    build.file("native/x264_bridge.c");
    build.define("_CRT_SECURE_NO_WARNINGS", None);

    let explicit_include = env::var_os("X264_INCLUDE_DIR").map(PathBuf::from);
    let explicit_lib = env::var_os("X264_LIB_DIR").map(PathBuf::from);
    if let Some(include) = explicit_include {
        build.include(include);
    } else if let Ok(library) = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("x264")
    {
        for include in library.include_paths {
            build.include(include);
        }
        for path in library.link_paths {
            println!("cargo:rustc-link-search=native={}", path.display());
        }
    } else {
        panic!(
            "x264 feature requires libx264 headers: set X264_INCLUDE_DIR and X264_LIB_DIR, or install x264 plus pkg-config"
        );
    }
    if let Some(path) = explicit_lib {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    let link_kind = if env::var_os("X264_STATIC").is_some() {
        "static"
    } else {
        "dylib"
    };
    println!("cargo:rustc-link-lib={link_kind}=x264");
    build.compile("dc_x264_bridge");
}
