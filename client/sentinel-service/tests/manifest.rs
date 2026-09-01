//! The service MUST NOT touch audio (spec 6.1 and 4).
//!
//! A Windows service runs in session 0 and cannot reach a user's audio session, so
//! any capture code here would be dead — and a SYSTEM process that links WASAPI is a
//! question at the bank's security review that has no good answer. The cheapest way
//! to keep that true over time is to assert it on the dependency graph, before
//! someone adds `sentinel-capture` "just for the tier enum".

#[test]
fn no_audio_dependencies() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("the crate's own manifest is readable");

    // Strip comments: the manifest explains *why* these are absent, and the
    // explanation naturally names them.
    let code: String = manifest
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    for forbidden in ["sentinel-capture", "cpal", "audiopus", "opus", "hound"] {
        assert!(
            !code.contains(forbidden),
            "sentinel-service must not depend on {forbidden}: a SYSTEM service in \
             session 0 cannot reach a user's audio session, which is why the client \
             is two processes"
        );
    }
}

#[test]
fn the_windows_feature_set_does_not_include_audio_apis() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("the crate's own manifest is readable");
    assert!(
        !manifest.contains("Win32_Media_Audio"),
        "enabling the WASAPI bindings in the service would make audio code compile here"
    );
}
