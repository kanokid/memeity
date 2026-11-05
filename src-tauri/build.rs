//! Bakes memeity's Google OAuth client into the binary, so users just sign in.
//!
//! Source, in priority order:
//! 1. env vars `MEMEITY_GOOGLE_CLIENT_ID` / `MEMEITY_GOOGLE_CLIENT_SECRET` (release builds, CI)
//! 2. `oauth/google-client.json`: the JSON Google Cloud Console downloads for a
//!    "Desktop app" client (gitignored; see oauth/HOW-TO.md)
//!
//! With neither, the app falls back to its in-app "bring your own client" setup.
//!
//! Desktop-app client secrets can't be kept secret from a determined user (Google's
//! docs say as much); we keep them out of the repo to avoid casual quota abuse.

fn main() {
    println!("cargo:rerun-if-env-changed=MEMEITY_GOOGLE_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=MEMEITY_GOOGLE_CLIENT_SECRET");
    // A directory (not the possibly-missing file) so cargo doesn't rebuild every time.
    println!("cargo:rerun-if-changed=oauth");

    let from_env = std::env::var("MEMEITY_GOOGLE_CLIENT_ID")
        .ok()
        .zip(std::env::var("MEMEITY_GOOGLE_CLIENT_SECRET").ok());
    let from_file = || {
        let bytes = std::fs::read("oauth/google-client.json").ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("oauth/google-client.json isn't valid JSON: {e}"));
        let installed = json.get("installed").unwrap_or_else(|| {
            panic!("oauth/google-client.json isn't a \"Desktop app\" OAuth client (no \"installed\" key)")
        });
        let field = |k: &str| installed.get(k)?.as_str().map(str::to_owned);
        Some((field("client_id")?, field("client_secret")?))
    };

    if let Some((id, secret)) = from_env.or_else(from_file) {
        println!("cargo:rustc-env=MEMEITY_GOOGLE_CLIENT_ID={id}");
        println!("cargo:rustc-env=MEMEITY_GOOGLE_CLIENT_SECRET={secret}");
    } else {
        println!("cargo:warning=No Google OAuth client embedded; the app will ask for one at runtime. See src-tauri/oauth/HOW-TO.md");
    }

    tauri_build::build()
}
