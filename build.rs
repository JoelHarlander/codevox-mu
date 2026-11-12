fn main() {
    println!("cargo:rerun-if-env-changed=SPOTIFY_CLIENT_ID");
    let raw = std::env::var("SPOTIFY_CLIENT_ID").unwrap_or_default();
    let trimmed = raw.trim();
    // Always set the rustc env. An empty value makes option_env blank, which
    // the resolver skips in favour of the built-in client id. Leaving the
    // variable unset would let rustc inherit a rejected value instead.
    let value =
        if trimmed.is_empty() || trimmed.chars().any(|char| char.is_control() || char == '=') {
            ""
        } else {
            trimmed
        };
    println!("cargo:rustc-env=SPOTIFY_CLIENT_ID={value}");
}
