use cfg_aliases::cfg_aliases;

fn main() {
    // The script doesn't depend on our code
    println!("cargo:rerun-if-changed=build.rs");

    // Declare the aliases below so recent Cargo versions do not warn about
    // every custom platform cfg used by this crate.
    for alias in [
        "android_platform",
        "wasm_platform",
        "macos_platform",
        "ios_platform",
        "windows_platform",
        "apple",
        "free_unix",
        "redox",
        "x11_platform",
        "wayland_platform",
        "orbital_platform",
    ] {
        println!("cargo:rustc-check-cfg=cfg({alias})");
    }

    // Setup cfg aliases
    cfg_aliases! {
        // Systems.
        android_platform: { target_os = "android" },
        wasm_platform: { all(target_family = "wasm", not(target_os = "emscripten")) },
        macos_platform: { target_os = "macos" },
        ios_platform: { target_os = "ios" },
        windows_platform: { target_os = "windows" },
        apple: { any(target_os = "ios", target_os = "macos") },
        free_unix: { all(unix, not(apple), not(android_platform), not(target_os = "emscripten")) },
        redox: { target_os = "redox" },

        // Native displays.
        x11_platform: { all(feature = "x11", free_unix, not(wasm), not(redox)) },
        wayland_platform: { all(feature = "wayland", free_unix, not(wasm), not(redox)) },
        orbital_platform: { redox },
    }
}
