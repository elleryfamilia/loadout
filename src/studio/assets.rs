//! Static assets (CSS + the htmx-shim JS) embedded in the binary via
//! `rust-embed` — no separate files to ship, no JS build step.

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "src/studio/assets"]
struct Assets;

/// Look up an embedded asset for a `/assets/<name>` request, returning its bytes
/// and content type. Path traversal is impossible: `rust-embed` only resolves
/// names that existed in the embedded folder at build time.
pub fn get(request_path: &str) -> Option<(Vec<u8>, &'static str)> {
    let name = request_path.trim_start_matches("/assets/");
    let file = Assets::get(name)?;
    Some((file.data.into_owned(), content_type(name)))
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("html") => "text/html; charset=utf-8",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_resolve() {
        let (css, ct) = get("/assets/studio.css").expect("studio.css must be embedded");
        assert!(ct.starts_with("text/css"));
        assert!(!css.is_empty());
        let (js, ct) = get("/assets/studio.js").expect("studio.js must be embedded");
        assert!(ct.starts_with("text/javascript"));
        assert!(!js.is_empty());
        // The self-hosted fonts are embedded and served as font/woff2.
        for name in ["inter", "alfa-slab-one"] {
            let (font, ct) = get(&format!("/assets/fonts/{name}.woff2"))
                .unwrap_or_else(|| panic!("{name}.woff2 must be embedded"));
            assert_eq!(ct, "font/woff2");
            assert!(!font.is_empty());
        }
        // Unknown / traversal names don't resolve.
        assert!(get("/assets/../config.toml").is_none());
        assert!(get("/assets/nope.css").is_none());
    }
}
