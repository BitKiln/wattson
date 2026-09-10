//! Compiles the target-side C library and its test shim.
//!
//! The point is not to ship a Rust binding. It is that `cargo test` builds the C, links it,
//! and runs its output through the real decoder — so the instrumentation library is verified
//! by the same CI that verifies everything else, on a host, with no device attached.
//!
//! Two configurations are built:
//!
//! * `wattson_c` — the library as a firmware project would use it, plus the test shim.
//! * `wattson_c_disabled` — the same sources with `PP_ENABLED=0`, which must still compile and
//!   link to nothing. A macro that only compiles out on the author's machine is a macro that
//!   breaks someone's release build.

use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("target/rust has a parent");
    let include = root.join("include");
    let src = root.join("src").join("wattson.c");
    let tests = root.join("tests");
    let shim = tests.join("shim.c");
    let disabled = tests.join("disabled.c");

    for file in [&src, &shim, &disabled, &include.join("wattson.h")] {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        tests.join("wattson_config.h").display()
    );

    let mut build = cc::Build::new();
    build
        .file(&src)
        .file(&shim)
        .include(&include)
        .include(&tests)
        .define("PP_USE_CONFIG_HEADER", None)
        .warnings(true);
    // The library is held to the warning settings a firmware project would use. MSVC is
    // excluded: it has no C99 mode worth naming and flags conforming C that every embedded
    // toolchain accepts, so on Windows this build proves it compiles, and GCC and Clang in CI
    // prove it compiles cleanly.
    if build.get_compiler().is_like_gnu() || build.get_compiler().is_like_clang() {
        build
            .std("c99")
            .flag("-Werror")
            .flag("-Wextra")
            .flag("-Wpedantic");
    }
    build.compile("wattson_c");

    let mut off = cc::Build::new();
    if off.get_compiler().is_like_gnu() || off.get_compiler().is_like_clang() {
        off.std("c99");
    }
    off.file(&src)
        .file(&disabled)
        .include(&include)
        .define("PP_ENABLED", "0")
        .warnings(true);
    off.compile("wattson_c_disabled");

    // The examples, compiled but not linked. An example nobody builds is an example that stops
    // being true, and instrumentation advice that no longer compiles is worse than none.
    //
    // Each gets its own build because they deliberately define the same board functions -
    // `radio_send` instrumented two different ways is the whole point of having both - and
    // `cargo_metadata(false)` keeps them off the link line so those definitions never meet.
    //
    // freertos.c is not here: it needs FreeRTOS headers. It says so at the top of the file.
    let examples = root.join("examples");
    for name in ["uart.c", "gpio.c"] {
        let file = examples.join(name);
        println!("cargo:rerun-if-changed={}", file.display());
        let mut ex = cc::Build::new();
        ex.file(&file)
            .file(examples.join("example_hal.c"))
            .include(&include)
            .include(&examples)
            // These examples record from one context, which is the case wattson.h makes you
            // declare rather than assume. Firmware that instruments an ISR supplies critical
            // section hooks instead; the header refuses to guess.
            .define("PP_SINGLE_CONTEXT", None)
            .cargo_metadata(false)
            .warnings(true);
        if ex.get_compiler().is_like_gnu() || ex.get_compiler().is_like_clang() {
            ex.std("c99").flag("-Werror").flag("-Wextra");
        }
        ex.compile(&format!(
            "wattson_c_example_{}",
            name.trim_end_matches(".c")
        ));
    }
    println!(
        "cargo:rerun-if-changed={}",
        examples.join("example_hal.c").display()
    );
}
