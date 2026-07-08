//! Sanitizing markdown → HTML rendering shared by studio and `load plan`.
//!
//! Threat model: the markdown is untrusted (model output, or a cloned repo's
//! guidance). Raw HTML is neutralized to text; link destinations are limited
//! to http/https/mailto/#fragment (checked case-insensitively after stripping
//! control/whitespace chars); images never fetch — they render as links when
//! their destination is safe, plain emphasis otherwise.

use pulldown_cmark::{html as md_html, Event, Options, Parser, Tag, TagEnd};

/// Render untrusted markdown to sanitized HTML.
pub fn render_markdown(md: &str) -> String {
    let body = strip_leading_comments(md);
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;

    // Stacks remembering what each open link/image was rewritten to, so the
    // matching end tag closes the same element.
    let mut link_stack: Vec<bool> = Vec::new(); // true = kept as <a>
    let mut image_stack: Vec<bool> = Vec::new(); // true = rewritten to <a>

    let parser = Parser::new_ext(body, opts).flat_map(move |ev| {
        let out: Vec<Event> = match ev {
            Event::Html(s) | Event::InlineHtml(s) => vec![Event::Text(s)],
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                if safe_url(&dest_url) {
                    link_stack.push(true);
                    vec![Event::Start(Tag::Link {
                        link_type,
                        dest_url,
                        title,
                        id,
                    })]
                } else {
                    link_stack.push(false);
                    vec![Event::Start(Tag::Emphasis)]
                }
            }
            Event::End(TagEnd::Link) => {
                if link_stack.pop().unwrap_or(true) {
                    vec![Event::End(TagEnd::Link)]
                } else {
                    vec![Event::End(TagEnd::Emphasis)]
                }
            }
            // Images never fetch: safe destination -> a plain link, else emphasis.
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                if safe_url(&dest_url) {
                    image_stack.push(true);
                    vec![Event::Start(Tag::Link {
                        link_type,
                        dest_url,
                        title,
                        id,
                    })]
                } else {
                    image_stack.push(false);
                    vec![Event::Start(Tag::Emphasis)]
                }
            }
            Event::End(TagEnd::Image) => {
                if image_stack.pop().unwrap_or(false) {
                    vec![Event::End(TagEnd::Link)]
                } else {
                    vec![Event::End(TagEnd::Emphasis)]
                }
            }
            other => vec![other],
        };
        out
    });

    let mut out = String::new();
    md_html::push_html(&mut out, parser);
    out
}

/// Whether a link destination is allowed: http(s), mailto, an intra-document
/// fragment, or a scheme-less relative reference. Checked case-insensitively
/// after stripping ASCII control and whitespace characters (defeats
/// `java\tscript:`-style smuggling).
fn safe_url(dest: &str) -> bool {
    let cleaned: String = dest
        .chars()
        .filter(|c| !c.is_ascii_control() && !c.is_whitespace())
        .collect();
    let lower = cleaned.to_ascii_lowercase();
    lower.starts_with('#')
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || !lower.contains(':')
}

/// Strip leading `<!-- … -->` comments (generated headers) before rendering.
pub fn strip_leading_comments(md: &str) -> &str {
    let mut t = md.trim_start();
    while let Some(rest) = t.strip_prefix("<!--") {
        match rest.find("-->") {
            Some(end) => t = rest[end + 3..].trim_start(),
            None => break,
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_html_is_neutralized() {
        let out = render_markdown("hello <script>alert(1)</script>");
        assert!(!out.contains("<script>alert"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn javascript_links_are_delinked() {
        for md in [
            "[x](javascript:alert(1))",
            "[x](JAVASCRIPT:alert(1))",
            "[x](java\tscript:alert(1))",
            "[x](data:text/html,hi)",
            "[x](vbscript:x)",
        ] {
            let out = render_markdown(md);
            assert!(!out.contains("href"), "should de-link: {md} -> {out}");
            assert!(out.contains('x'), "text kept: {md} -> {out}");
        }
    }

    #[test]
    fn safe_links_pass() {
        for md in [
            "[x](https://example.com)",
            "[x](http://example.com)",
            "[x](mailto:a@b.c)",
            "[x](#task-t1)",
            "[x](relative/path.md)",
        ] {
            let out = render_markdown(md);
            assert!(out.contains("<a href="), "should link: {md} -> {out}");
        }
    }

    #[test]
    fn images_never_render_as_img() {
        let out = render_markdown("![alt](https://example.com/a.png)");
        assert!(!out.contains("<img"), "{out}");
        assert!(
            out.contains("<a href=\"https://example.com/a.png\""),
            "{out}"
        );
        let out = render_markdown("![alt](javascript:x)");
        assert!(!out.contains("<img"), "{out}");
        assert!(!out.contains("href"), "{out}");
    }

    #[test]
    fn leading_generated_comments_are_stripped() {
        let out = render_markdown("<!-- loadout:generated x -->\n<!-- meta -->\n# Hi");
        assert!(out.contains("<h1>"));
        assert!(!out.contains("loadout:generated"));
    }

    #[test]
    fn tables_and_tasklists_still_work() {
        let out = render_markdown("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(out.contains("<table>"));
    }
}
