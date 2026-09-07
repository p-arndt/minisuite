// Embedded web UI assets (compiled in via include_str!). Pure std.

pub struct Asset {
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

const INDEX_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/index.html"));
const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/app.css"));
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/app.js"));
const FAVICON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui/favicon.svg"));

/// "/" and "/index.html" -> index; "/app.css","/app.js","/favicon.svg". None otherwise.
pub fn get(path: &str) -> Option<Asset> {
    let (content_type, s) = match path {
        "/" | "/index.html" => ("text/html; charset=utf-8", INDEX_HTML),
        "/app.css" => ("text/css; charset=utf-8", APP_CSS),
        "/app.js" => ("text/javascript; charset=utf-8", APP_JS),
        "/favicon.svg" => ("image/svg+xml", FAVICON),
        _ => return None,
    };
    Some(Asset {
        content_type,
        bytes: s.as_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_index_map_to_html() {
        assert_eq!(get("/").unwrap().content_type, "text/html; charset=utf-8");
        assert_eq!(
            get("/index.html").unwrap().content_type,
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn known_assets_have_expected_types() {
        assert_eq!(
            get("/app.css").unwrap().content_type,
            "text/css; charset=utf-8"
        );
        assert_eq!(
            get("/app.js").unwrap().content_type,
            "text/javascript; charset=utf-8"
        );
        assert_eq!(get("/favicon.svg").unwrap().content_type, "image/svg+xml");
    }

    #[test]
    fn unknown_path_is_none() {
        assert!(get("/nope").is_none());
        assert!(get("/../secret").is_none());
        assert!(get("").is_none());
    }

    #[test]
    fn assets_are_non_empty() {
        for p in ["/", "/index.html", "/app.css", "/app.js", "/favicon.svg"] {
            assert!(!get(p).unwrap().bytes.is_empty(), "{p} empty");
        }
    }
}
