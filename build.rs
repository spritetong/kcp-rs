use build_helper2::*;

fn main() -> io::Result<()> {
    // Do not slow down rust-analyze.
    if cmake::is_under_rust_analyzer(true) {
        return Ok(());
    }

    let root = cargo::manifest::dir();
    let src_dir = root.join("src");
    let kcp_dir = root.join("kcp");
    let sources = [kcp_dir.join("ikcp.c")];

    ::cc::Build::new()
        .define(
            "IWORDS_BIG_ENDIAN",
            if target::endian() == Some(Endianness::Big) {
                "1"
            } else {
                "0"
            },
        )
        .define("IWORDS_MUST_ALIGN", "1")
        .files(&sources)
        .compile("kcp_sys");

    // Generate the FFI file.
    cmake::bindgen(
        src_dir.join("ffi.rs"),
        [kcp_dir.join("ikcp.h")],
        [&kcp_dir],
        ["(?i:ikcp)_.*"],
        ["__.*"],
        |x| Some(x.derive_default(false).derive_debug(false)),
    )?;

    // Watch changes of sources.
    for src in sources.iter() {
        rerun_if_changed(src);
    }

    Ok(())
}
