//! WebUI static-asset serving — the external front end.
//!
//! The front end is authored as `index.html` plus the `css/` and `js/` it
//! references, and it ships loose beside the executable (`<exe dir>/webui/`).
//! The proxy serves those files directly: no build-time inlining and no
//! embedded copy, so the page is a normal static site and an edit to a
//! stylesheet takes effect on the next reload.
//!
//! This used to *assemble* the split sources into one self-contained page at
//! build time (`build.rs`) and at run time as a fallback. That path is gone:
//! the binary no longer carries a compiled-in page. If the loose `webui/`
//! directory is absent, the proxy says so explicitly instead of half-serving a
//! page that cannot see its own styles.
//!
//! Std-only on purpose: nothing here needs a dependency, which keeps the
//! module usable from anywhere (the old `build.rs` was a separate crate and
//! could not link the library).

use std::path::{Path, PathBuf};

/// The value of `name="..."` inside a tag fragment.
fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)?.saturating_add(needle.len());
    let rest = tag.get(start..)?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_owned)
}

/// Why an asset could not be served.
#[derive(Debug)]
pub enum ServeError {
    /// The requested URL path is not inside the site directory.
    Escape(String),
    /// The file could not be read (missing, permission, …).
    Io { path: PathBuf, error: String },
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Escape(url) => write!(f, "`{url}` escapes the site directory"),
            Self::Io { path, error } => write!(f, "cannot read `{}`: {error}", path.display()),
        }
    }
}

/// Resolve a `/webui/...` URL path to a file under `root`.
///
/// `index.html` is the default when the path is empty or names the directory.
/// Anything that escapes `root` — a `..` segment, an absolute path — is
/// refused. `root` itself is trusted (it is the site directory next to the
/// executable), but the URL arrives from a browser and is untrusted.
pub fn asset_path(root: &Path, url_path: &str) -> Result<PathBuf, ServeError> {
    let rel = url_path
        .strip_prefix("/webui/")
        .unwrap_or(url_path)
        .trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if rel.contains("..") {
        return Err(ServeError::Escape(url_path.to_owned()));
    }
    Ok(root.join(rel))
}

/// Content type for a served asset, by extension.
pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Every file `index.html` references — kept as the source for asset
/// validation (the radius-token test) and for packaging checks.
pub fn referenced_files(root: &Path) -> Vec<PathBuf> {
    let Ok(index) = std::fs::read_to_string(root.join("index.html")) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    for marker in ["<link", "<script"] {
        let mut rest = index.as_str();
        while let Some(at) = rest.find(marker) {
            rest = &rest[at..];
            let end = rest.find('>').map_or(rest.len(), |i| i.saturating_add(1));
            let tag = &rest[..end];
            let name = if tag.starts_with("<link") {
                "href"
            } else {
                "src"
            };
            if let Some(v) = attr(tag, name) {
                if !v.contains("..") {
                    seen.insert(root.join(v));
                }
            }
            rest = rest.get(end..).unwrap_or("");
        }
    }
    seen.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_path_maps_urls_under_the_webui_root() {
        let root = Path::new("/srv/ccproxy/webui");
        assert_eq!(
            asset_path(root, "/webui/").unwrap(),
            root.join("index.html"),
            "bare directory serves index.html"
        );
        assert_eq!(
            asset_path(root, "/webui/index.html").unwrap(),
            root.join("index.html")
        );
        assert_eq!(
            asset_path(root, "/webui/css/tokens.css").unwrap(),
            root.join("css/tokens.css")
        );
        assert_eq!(
            asset_path(root, "/webui/js/stats.js").unwrap(),
            root.join("js/stats.js")
        );
    }

    #[test]
    fn asset_path_refuses_escapes() {
        let root = Path::new("/srv/ccproxy/webui");
        for evil in [
            "/webui/../ccproxy.json",
            "/webui/css/../../secret",
            "/../etc/passwd",
        ] {
            assert!(
                matches!(asset_path(root, evil), Err(ServeError::Escape(_))),
                "must refuse {evil}"
            );
        }
    }

    #[test]
    fn content_type_is_keyed_by_extension() {
        assert_eq!(
            content_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("tokens.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("stats.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("icon.svg")), "image/svg+xml");
        assert_eq!(
            content_type(Path::new("unknown.xyz")),
            "application/octet-stream"
        );
    }

    #[test]
    fn referenced_files_lists_every_asset() {
        let dir = std::env::temp_dir().join(format!("ccproxy-site-refs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("css")).unwrap();
        std::fs::create_dir_all(dir.join("js")).unwrap();
        std::fs::write(
            dir.join("index.html"),
            "<link rel=\"stylesheet\" href=\"css/a.css\"><script src=\"js/b.js\"></script>",
        )
        .unwrap();
        std::fs::write(dir.join("css/a.css"), "").unwrap();
        std::fs::write(dir.join("js/b.js"), "").unwrap();

        let files = referenced_files(&dir);
        assert!(files.iter().any(|p| p.ends_with("css/a.css")));
        assert!(files.iter().any(|p| p.ends_with("js/b.js")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_deb_asset_list_carries_every_file_the_panel_references() {
        // The deb lists assets one by one (cargo-deb resolves them relative to
        // the crate dir), so a stylesheet added to index.html and forgotten
        // here ships a package whose panel silently loses it — the deb install
        // rendered with no theme at all for exactly this reason. The tarball and
        // AppImage copy the directory, so only the deb needs the guard.
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/webui"));
        let crate_dir = root.parent().expect("webui lives in the crate dir");
        let manifest =
            std::fs::read_to_string(crate_dir.join("Cargo.toml")).expect("crate manifest");

        let mut missing = Vec::new();
        for file in referenced_files(root) {
            // The favicon is an inline `data:` URI, not a file on disk; only
            // real files can be packaged.
            if !file.is_file() {
                continue;
            }
            let Ok(rel) = file.strip_prefix(crate_dir) else {
                continue;
            };
            // cargo-deb wants forward slashes even on Windows.
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !manifest.contains(&rel) {
                missing.push(rel);
            }
        }
        assert!(
            missing.is_empty(),
            "the deb asset list omits referenced files: {missing:?}"
        );
    }

    #[test]
    fn corner_radii_come_only_from_the_token_scale() {
        // "Some buttons square, some round": the radii used to be ad-hoc
        // (16/14/8/13/2/999), so two controls on one row could disagree.
        // Every radius now comes from a named token in tokens.css; this pins
        // that the scale is declared, and that no stylesheet states a radius
        // as a bare value again.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("webui");
        let files = referenced_files(&dir);
        assert!(!files.is_empty(), "webui/index.html references no assets");
        let tokens = files
            .iter()
            .find(|p| p.ends_with("tokens.css"))
            .map(|p| std::fs::read_to_string(p).expect("tokens.css readable"))
            .expect("tokens.css is referenced by index.html");
        for token in [
            "--radius-panel",
            "--radius-card",
            "--radius-control",
            "--radius-pill",
        ] {
            assert!(tokens.contains(token), "tokens.css must declare {token}");
        }
        for path in &files {
            if path.ends_with("tokens.css") || path.extension().is_none_or(|e| e != "css") {
                continue;
            }
            let text = std::fs::read_to_string(path).expect("css readable");
            for (i, line) in text.lines().enumerate() {
                if !line.contains("border-radius") {
                    continue;
                }
                // `50%` is not a corner radius on the scale — it draws a circle
                // (the chip's status dot). Both spellings are fine; anything
                // else must name a token.
                let declared = line
                    .split("border-radius")
                    .skip(1)
                    .any(|rest| rest.contains("var(--radius-") || rest.contains("50%"));
                assert!(
                    declared,
                    "{}:{} uses a bare radius instead of a token: {}",
                    path.display(),
                    i.saturating_add(1),
                    line.trim()
                );
            }
        }
    }

    /// Every `var(--name)` occurrence on a line, paired with whether it spells
    /// a fallback (`var(--x, 0)`). A fallback means the author accepted the
    /// missing case, so only bare references have to resolve.
    fn var_references(line: &str) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut rest = line;
        while let Some(at) = rest.find("var(--") {
            rest = &rest[at + 4..];
            let end = rest.find(')').unwrap_or(rest.len());
            let body = &rest[..end];
            rest = &rest[end..];
            let (name, fallback) = match body.split_once(',') {
                Some((name, _)) => (name, true),
                None => (body, false),
            };
            // `var(--…)` in prose and other non-ASCII spellings are not
            // property names; a name is ASCII `[A-Za-z0-9-]`.
            let name = name.trim().trim_start_matches('-').trim();
            let valid =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            if valid {
                out.push((name.to_string(), fallback));
            }
        }
        out
    }

    /// Names declared on a line, in either spelling used here: a stylesheet's
    /// `--name: value` at the start of a line, or a script's
    /// `style="--name: 3"` written inline on an element. Anything followed by
    /// `)` is a reference, not a declaration.
    fn declared_names(line: &str) -> Vec<String> {
        let bytes = line.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i + 2 < bytes.len() {
            if bytes[i] == b'-' && bytes[i + 1] == b'-' {
                let mut j = i + 2;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
                    j += 1;
                }
                if j > i + 2 && j < bytes.len() && bytes[j] == b':' {
                    out.push(line[i + 2..j].to_string());
                }
                i = j.max(i + 2);
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn every_variable_reference_resolves_to_a_definition() {
        // Merging the mainline control kit into this branch left three
        // variables referenced but never declared — `--surface-high`,
        // `--surface-variant`, `--shadow-hover`. A `var()` on a missing name
        // computes to `unset`, so every self-drawn dropdown opened with a
        // transparent menu and no shadow, on all four themes, and no test
        // noticed: the radius gate only looked at radii.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("webui");
        let files = referenced_files(&dir);
        assert!(!files.is_empty(), "webui/index.html references no assets");

        let mut defined = std::collections::HashSet::new();
        for path in &files {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();
            if ext != "css" && ext != "js" {
                continue;
            }
            let text = std::fs::read_to_string(path).expect("asset readable");
            for line in strip_comments(&text).lines() {
                defined.extend(declared_names(line));
            }
        }
        assert!(
            defined.contains("primary"),
            "token definitions were not picked up: {defined:?}"
        );

        let mut missing: Vec<String> = Vec::new();
        for path in &files {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();
            if ext != "css" && ext != "js" {
                continue;
            }
            let text = std::fs::read_to_string(path).expect("asset readable");
            for (i, line) in text.lines().enumerate() {
                for (name, fallback) in var_references(line) {
                    if fallback || defined.contains(&name) {
                        continue;
                    }
                    missing.push(format!(
                        "{}:{} --{name}",
                        path.display(),
                        i.saturating_add(1)
                    ));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these variables are used but never declared, so they compute to unset: {missing:#?}"
        );
    }

    #[test]
    fn every_theme_card_can_survive_a_reload() {
        // The anti-flash block in <head> re-applies the saved theme before the
        // first paint, from its own hard-coded list — it has to, it runs before
        // settings.js. That second copy went stale: `jelly-dark` was missing,
        // so choosing it and reloading landed back on dark, and the picker
        // (which reads the attribute, not localStorage) then showed dark as
        // selected. Same class of bug as the missing `--surface-*` variables:
        // two lists that must agree, no test that they do.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("webui");
        let html = std::fs::read_to_string(root.join("index.html")).expect("index.html readable");

        let cards: Vec<String> = html
            .match_indices("data-theme-key=\"")
            .filter_map(|(at, pat)| {
                let rest = &html[at + pat.len()..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .collect();
        assert!(!cards.is_empty(), "no theme cards found in index.html");

        // Take the block around the THEMES literal rather than "the second
        // <script>": a nonce or an extra inline script would move that one.
        let literal = html.find("var THEMES = [").expect("no THEMES literal");
        let open = html[..literal]
            .rfind("<script")
            .expect("THEMES lives in a script");
        let preflash = &html[open..];
        let preflash = &preflash[..preflash.find("</script>").expect("unterminated script")];
        // 名单对不对只是一半：名单还得真的被用来落主题。只改名单、删掉
        // dataset.theme 那一行的话，刷新照样丢主题，而下面那条比对依旧绿。
        assert!(
            preflash.contains("document.documentElement.dataset.theme"),
            "the pre-paint block no longer applies the theme it looks up, so a reload loses it"
        );
        assert!(
            preflash.contains("THEMES.indexOf"),
            "the pre-paint block stopped consulting the THEMES list"
        );
        let list = &preflash[literal - open..];
        let list = &list[..list.find(']').expect("unterminated THEMES array")];
        let known: Vec<String> = list
            .match_indices('"')
            .filter_map(|(at, _)| {
                let rest = &list[at + 1..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .collect();

        // `dark` is the default: it needs no attribute, so it need not be listed.
        let missing: Vec<&String> = cards
            .iter()
            .filter(|c| c.as_str() != "dark" && !known.contains(c))
            .collect();
        assert!(
            missing.is_empty(),
            "these themes have a card but the pre-paint list omits them, so a reload loses them: {missing:?} (list = {known:?})"
        );
    }

    #[test]
    fn the_nav_slider_and_the_selected_item_are_wired_together() {
        // 选中底色现在由一块独立滑块（#nav-indicator）承担，导航项在 nav-slid
        // 之后转透明。这两半必须同时在位：元素被删掉、或者 router.js 不再给
        // nav 加 nav-slid，选中项就是全透明的——界面上"没有选中项"，但页面
        // 不报错，四视图冒烟也照样绿。这里把它们钉在一起。
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/webui"));
        let html = std::fs::read_to_string(dir.join("index.html")).expect("index.html readable");
        let css = std::fs::read_to_string(dir.join("css/layout.css")).expect("layout.css readable");
        let js = std::fs::read_to_string(dir.join("js/router.js")).expect("router.js readable");

        assert!(
            html.contains("id=\"nav-indicator\""),
            "index.html no longer carries #nav-indicator, so the selected pill has nothing to sit on"
        );
        // 规则本身要在位。`.nav-indicator.no-anim` 也含 ".nav-indicator" 这个
        // 子串，光用 contains 会把"主定位规则被删干净"判成通过。
        let rules = css_rules(&css);
        let indicator = rules
            .iter()
            .find(|(sel, _)| flat(sel) == ".nav-indicator")
            .map(|(_, body)| flat(body))
            .unwrap_or_default();
        assert!(
            indicator.contains("position:absolute"),
            "layout.css no longer lays .nav-indicator out as an absolute pill: {indicator}"
        );
        assert!(
            css.contains(".side-nav.nav-slid .nav-item.active"),
            "the nav-slid handoff rule is gone: the active item would stay opaque and hide the slider"
        );
        // 定位函数必须真的被调用。只断言 `function syncNavIndicator` 存在的话，
        // 把 route()/resize 里的调用全删掉，滑块会永远停在原地，测试照样绿。
        let route = function_body(&js, "route");
        assert!(
            route.contains("syncNavIndicator"),
            "route() stopped positioning the slider, so it would never move when the view changes"
        );
        let sync = function_body(&js, "syncNavIndicator");
        assert!(
            sync.contains("classList.add(\"nav-slid\""),
            "router.js no longer hands the active item over to nav-slid"
        );
        assert!(
            sync.contains("getBoundingClientRect") && sync.contains("translate3d"),
            "syncNavIndicator must measure with rects and write a translate3d offset, or the pill lands off the item"
        );
        // 兜底：nav-slid 没出现时选中项必须还有自己的实底，否则 JS 早期报错就等于
        // 导航没有选中态。这条断言守的是"降级路径还留着"。
        let fallback = rules
            .iter()
            .find(|(sel, _)| flat(sel) == ".nav-item.active")
            .map(|(_, body)| flat(body))
            .unwrap_or_default();
        assert!(
            fallback.contains("background:var(--primary-container)"),
            "the fallback fill on .nav-item.active was removed; a JS failure would leave no selected state: {fallback}"
        );
    }

    /// Drop `/* … */` blocks. Both parsers below read CSS as plain text, and the
    /// comments in this tree quote real declarations — `table { min-width:
    /// 780px }`, `--surface-head: …` — which would otherwise be counted as
    /// rules and as token definitions.
    fn strip_comments(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find("/*") {
            out.push_str(&rest[..at]);
            match rest[at + 2..].find("*/") {
                Some(end) => rest = &rest[at + 2 + end + 2..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// Whitespace-insensitive form, so reformatting is not a test failure.
    fn flat(text: &str) -> String {
        text.split_whitespace().collect()
    }

    /// Body of `function <name>(…)`, up to the first line that closes it in
    /// column zero. These files all close top-level functions that way, and it
    /// keeps "what the function does" assertions off the whole-file text.
    fn function_body<'a>(js: &'a str, name: &str) -> &'a str {
        let Some(at) = js.find(&format!("function {name}(")) else {
            return "";
        };
        let rest = &js[at..];
        let end = rest[1..].find("\n}").map_or(rest.len(), |e| e + 1);
        &rest[..end]
    }

    /// True when a fill hides whatever scrolls under it. A hex with an explicit
    /// low alpha, or `rgba(…, 0.07)`, does not: that was the 果冻暗 7%-white
    /// header regression.
    fn opaque_fill(value: &str) -> bool {
        let v = value
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_ascii_lowercase();
        if let Some(hex) = v.strip_prefix('#') {
            return match hex.len() {
                3 | 6 => true,
                4 => &hex[3..] == "f",
                8 => &hex[6..] == "ff",
                _ => false,
            };
        }
        if v.starts_with("rgba(") {
            return v
                .trim_end_matches(')')
                .rsplit(',')
                .next()
                .and_then(|a| a.trim().parse::<f64>().ok())
                .is_some_and(|a| a >= 1.0);
        }
        !v.is_empty() && v != "transparent" && !v.starts_with("hsla(")
    }

    /// Split a stylesheet into `(selector, body)` pairs, nested at-rules
    /// included: a header rule inside an `@media` still has to obey the rule.
    fn css_rules(text: &str) -> Vec<(String, String)> {
        let text = strip_comments(text);
        let mut out = Vec::new();
        let mut stack: Vec<(String, String)> = Vec::new();
        let mut head = String::new();
        for ch in text.chars() {
            match ch {
                '{' => {
                    stack.push((head.trim().to_string(), String::new()));
                    head.clear();
                }
                '}' => {
                    if let Some(rule) = stack.pop() {
                        out.push(rule);
                    }
                }
                _ => match stack.last_mut() {
                    Some((_, body)) => body.push(ch),
                    None => head.push(ch),
                },
            }
        }
        out
    }

    /// True when the last compound of any comma-separated selector part is a
    /// `th`: `.model-table thead th` and `th:first-child` both count, `td`
    /// does not.
    fn targets_th(selector: &str) -> bool {
        selector.split(',').any(|part| {
            let last = part.split_whitespace().last().unwrap_or("");
            let base = last.split([':', '[', '.', '#']).next().unwrap_or_default();
            base.eq_ignore_ascii_case("th")
        })
    }

    #[test]
    fn table_headers_are_filled_by_the_opaque_header_token() {
        // 用户报「模型分布的表头全透明」。两处原因都在这里查：表头取的是
        // `--surface`，而 `--surface` 就是卡片底色，深色与浅色两档里两者
        // 一模一样，看上去等于没有表头；果冻暗档还把它压成 7% 白。表头又是
        // sticky 的，半透明会让滚过的行从表头底下透上来。
        //
        // 于是定一条口径：th 的底色只能来自 `--surface-head`，而这个变量必须
        // 跟 `--surface` 出现在同一批文件里——少写一个主题，那个主题就会静默
        // 继承深色值（不报错、不白屏，只是换主题时表头颜色不对）。
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("webui");
        let files = referenced_files(&dir);
        assert!(!files.is_empty(), "webui/index.html references no assets");

        let mut missing_token: Vec<String> = Vec::new();
        let mut bad_fill: Vec<String> = Vec::new();
        let mut translucent: Vec<String> = Vec::new();
        let mut filled: Vec<String> = Vec::new();
        for path in &files {
            if path.extension().is_none_or(|e| e != "css") {
                continue;
            }
            let text = std::fs::read_to_string(path).expect("css readable");
            let declares_surface = text
                .lines()
                .any(|l| l.trim_start().starts_with("--surface:"));
            let declares_head = text
                .lines()
                .any(|l| l.trim_start().starts_with("--surface-head:"));
            if declares_surface && !declares_head {
                missing_token.push(path.display().to_string());
            }
            // 令牌本身也得是不透明的。果冻暗档曾经把它压成 7% 白，那一版下
            // 表头确实"有底色"，只是滚过的行全从底下透上来。
            if let Some(value) = text
                .lines()
                .find_map(|l| l.trim_start().strip_prefix("--surface-head:"))
            {
                if !opaque_fill(value) || value.contains("var(--surface)") {
                    translucent.push(format!(
                        "{} — --surface-head: {}",
                        path.display(),
                        value.trim()
                    ));
                }
            }
            for (selector, body) in css_rules(&text) {
                if !targets_th(&selector) {
                    continue;
                }
                for decl in body.split(';') {
                    let Some((prop, value)) = decl.split_once(':') else {
                        continue;
                    };
                    let prop = prop.trim().to_ascii_lowercase();
                    if prop != "background" && prop != "background-color" {
                        continue;
                    }
                    if value.contains("var(--surface-head)") {
                        filled.push(path.display().to_string());
                    } else {
                        bad_fill.push(format!(
                            "{} — {} {{ {prop}: {} }}",
                            path.display(),
                            selector,
                            value.trim()
                        ));
                    }
                }
            }
        }

        assert!(
            missing_token.is_empty(),
            "these files set --surface but never --surface-head, so their table headers inherit the dark fill: {missing_token:#?}"
        );
        assert!(
            bad_fill.is_empty(),
            "table headers must be filled by the opaque --surface-head token: {bad_fill:#?}"
        );
        assert!(
            translucent.is_empty(),
            "--surface-head must be opaque and must not just re-spell --surface, or the rows show through: {translucent:#?}"
        );
        // 每个真正画表头的样式表都得有一条 th 规则给出底色。只核对"出现的
        // background 对不对"是不够的：把给出底色的那一行整条删掉之后，上面的
        // 循环看到的是空集合，bad_fill 依旧为空 —— 修 bug 的那两行当年就是这样
        // 在没有测试红的情况下被删掉的。
        for sheet in ["detail.css", "overview.css"] {
            assert!(
                filled.iter().any(|f| f.ends_with(sheet)),
                "{sheet} no longer gives its header cells a --surface-head fill: {filled:?}"
            );
        }
    }

    #[test]
    fn log_lines_are_joined_without_text_separators() {
        // 行与行之间不能插分隔符。`.logpane` 是 `white-space: pre`、`.ln` 是
        // block，所以那个分隔符不是"换行"而是一个额外的文本节点；而 trimLogDom
        // 只按元素数裁剪（childElementCount / firstElementChild），文本节点永远
        // 删不掉。实测把日志追到 4.4 MB 时，2000 行的面板里堆了 37,240 个文本
        // 节点，每个都排成一个空行，scrollHeight 变成 39,241 行的高度，滚动时
        // 整块重排，主线程长任务到 11 秒 —— 日志页"上下滚动会卡死"就是这个。
        // 两处渲染（整段重画 + 增量追加）都必须空串拼接。
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/webui"));
        let js = std::fs::read_to_string(dir.join("js/logs.js")).expect("logs.js readable");
        let code = flat(&strip_comments(&js));

        assert!(
            !code.contains("logLineHtml).join(\"\\n\")"),
            "logs.js joins rendered lines with a newline again: trimLogDom only removes \
             elements, so those text nodes pile up and each renders as a blank row"
        );
        assert_eq!(
            code.matches("map(logLineHtml).join(\"\")").count(),
            2,
            "both log paints (full repaint and incremental append) must join with an empty \
             separator; found a different one"
        );
    }
}
