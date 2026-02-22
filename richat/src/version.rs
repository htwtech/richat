use {richat_shared::version::Version, std::env};

/// Version fields can be overridden via environment variables at build time:
///   RICHAT_DISPLAY_PACKAGE  — package name (default: cargo pkg name)
///   RICHAT_DISPLAY_VERSION  — version string (default: cargo pkg version)
///   RICHAT_DISPLAY_HOSTNAME — "true"/"false" to show/hide hostname (default: true)
pub const VERSION: Version = Version {
    package: match option_env!("RICHAT_DISPLAY_PACKAGE") {
        Some(v) => v,
        None => env!("CARGO_PKG_NAME"),
    },
    version: match option_env!("RICHAT_DISPLAY_VERSION") {
        Some(v) => v,
        None => env!("CARGO_PKG_VERSION"),
    },
    proto: env!("YELLOWSTONE_GRPC_PROTO_VERSION"),
    proto_richat: env!("RICHAT_PROTO_VERSION"),
    solana: env!("SOLANA_SDK_VERSION"),
    git: env!("GIT_VERSION"),
    rustc: env!("VERGEN_RUSTC_SEMVER"),
    buildts: env!("VERGEN_BUILD_TIMESTAMP"),
};
