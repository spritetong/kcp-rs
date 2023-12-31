use leach::*;

fn main() -> io::Result<()> {
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
        .flag_if_supported("-Wno-unused-parameter")
        .files(&sources)
        .compile("kcp_sys");

    // Watch changes of sources.
    for src in sources.iter() {
        rerun_if_changed(src);
    }

    if cargo::features::enabled("gen-ffi") {
        // Generate the FFI file.
        cmake::Bindgen::default()
            .rs_file(src_dir.join("ffi.rs"))
            .allow_bad_code_styles()
            .headers([kcp_dir.join("ikcp.h"), kcp_dir.join("ikcp.c")])
            .includes([&kcp_dir])
            .allowlist([
                "IKCP_.*",
                "ikcp_create",
                "ikcp_release",
                "ikcp_setoutput",
                "ikcp_recv",
                "ikcp_send",
                "ikcp_update",
                "ikcp_check",
                "ikcp_input",
                "ikcp_flush",
                "ikcp_peeksize",
                "ikcp_setmtu",
                "ikcp_wndsize",
                "ikcp_waitsnd",
                "ikcp_nodelay",
                "ikcp_log",
                "ikcp_allocator",
                "ikcp_getconv",
            ])
            .blocklist(["__.*"])
            .derive(None::<&str>, ["Copy", "-Debug"])
            .generate(None)?;
    }

    Ok(())
}
