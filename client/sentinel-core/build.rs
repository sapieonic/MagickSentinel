//! Link directives for the SQLCipher build on Windows.
//!
//! `sqlcipher` builds `rusqlite/bundled-sqlcipher`, which compiles SQLCipher against
//! OpenSSL's libcrypto. On Windows that has to be a *static* libcrypto, because the
//! MSI packages no OpenSSL DLL and a dynamically linked binary installs cleanly and
//! then never starts. Static libcrypto in turn has undeclared dependencies on two
//! system import libraries, and nothing in the dependency graph emits them:
//!
//!   * `crypt32` -- the `Cert*` family, reached from OpenSSL's CAPI engine and its
//!     Windows certificate store backend (`e_capi.obj`, `winstore_store.obj`).
//!   * `user32`  -- `GetProcessWindowStation` and `GetUserObjectInformationW`, used by
//!     `OPENSSL_isservice` to decide whether it is running in a service, and
//!     `MessageBoxW`, used by `OPENSSL_showfatal`.
//!
//! Without them the link fails with eleven unresolved externals: eight `Cert*` and
//! the three above. That is exactly what the Windows CI job hit on the release build,
//! and it fails at *link* time, so no amount of type-checking on another platform
//! catches it.
//!
//! This belongs here rather than in `RUSTFLAGS` in the workflow. `client/installer/
//! build.ps1` is the path that produces the shipped MSI and it builds with this same
//! feature; a fix that lived only in CI would leave the actual release build broken,
//! which is the worse of the two failures because nothing would be watching for it.
//!
//! Note `OPENSSL_DIR` still has to point at a static build -- `libsqlite3-sys` reads
//! it directly on Windows and consults neither pkg-config nor vcpkg. That is set up
//! by `.github/actions/sqlcipher-openssl`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Features reach a build script as CARGO_FEATURE_<NAME>; the target is described
    // by CARGO_CFG_*, which is the target being built for rather than the host, so
    // cross-compiling from Linux is judged correctly.
    let sqlcipher = std::env::var_os("CARGO_FEATURE_SQLCIPHER").is_some();
    let windows = matches!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("windows"));
    if !sqlcipher || !windows {
        return;
    }

    // Deliberately not gated to the `msvc` environment. The mingw toolchain linking a
    // static libcrypto needs the same two libraries, and naming them costs nothing on
    // a target that does not.
    println!("cargo:rustc-link-lib=dylib=crypt32");
    println!("cargo:rustc-link-lib=dylib=user32");
}
