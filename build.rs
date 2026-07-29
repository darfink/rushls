fn main() {
    println!("cargo:rerun-if-changed=src/source/transport/srt/native.c");
    println!("cargo:rerun-if-changed=src/source/avformat/bitstream.c");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    let library = pkg_config::Config::new()
        .atleast_version("1.5.5")
        .statik(true)
        .cargo_metadata(false)
        .probe("srt")
        .expect("libSRT 1.5.5 or newer must be discoverable through pkg-config");

    let mut shim = cc::Build::new();
    shim.file("src/source/transport/srt/native.c")
        .warnings(true);
    for include in &library.include_paths {
        shim.include(include);
    }
    shim.compile("rushls_srt_native");

    // ffmpeg-sys-next intentionally binds avcodec.h but not the separate
    // bitstream-filter header. A tiny C boundary keeps that omitted ABI opaque
    // rather than reproducing AVBSFContext's layout in Rust.
    let avcodec = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libavcodec")
        .expect("libavcodec must be discoverable through pkg-config");
    let mut bitstream = cc::Build::new();
    bitstream
        .file("src/source/avformat/bitstream.c")
        .warnings(true);
    for include in &avcodec.include_paths {
        bitstream.include(include);
    }
    bitstream.compile("rushls_avformat_bitstream");

    for path in &library.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for framework in &library.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    for path in &library.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }

    // Only libSRT is embedded. Its crypto and C++ dependencies stay shared,
    // which avoids manufacturing a second OpenSSL copy beside FFmpeg.
    println!("cargo:rustc-link-lib=static=srt");
    for dependency in library.libs.iter().filter(|name| name.as_str() != "srt") {
        println!("cargo:rustc-link-lib=dylib={dependency}");
    }
}
