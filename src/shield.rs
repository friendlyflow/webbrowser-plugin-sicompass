//! A web page's words can never become a tag in a row.
//!
//! A page comes from a stranger, and `plugin.json` says `"markup": true`, so the
//! app reads tags out of every row this plugin sends. A page that writes
//! `<img src="/home/user/secret.png">` as visible text must get those characters
//! shown, not a picture. The SDK's walker escapes everything it copies from a page
//! (since 0.9.12), and the tests here hold it to that on every path through it.

use sicompass_sdk::ffon::{FfonElement, FormMap, html_to_ffon_with_forms};
use sicompass_sdk::tags;

/// Undo `tags::escape`. Text that was never escaped comes through untouched, so
/// it is safe on anything: a row's raw slice, or what the user typed.
pub(crate) fn unescape(s: &str) -> String {
    tags::unescape(s)
}

/// A URL written so it can sit inside `<link>` and be followed as it stands. The
/// app reads a `<link>` raw, so a `\<` there would be followed with its backslash.
/// A URL has its own way to write a bracket, which every browser turns it into
/// anyway. A `#fragment` is matched against a row's `<id>` instead, which holds
/// the text form.
pub(crate) fn link_value(url: &str) -> String {
    if url.starts_with('#') {
        return tags::escape(url);
    }
    url.replace('<', "%3C").replace('>', "%3E")
}

/// A web page as rows and the form map that goes with them. The only way a page
/// becomes rows.
pub(crate) fn html_to_rows(html: &str, base_url: &str) -> (Vec<FfonElement>, FormMap) {
    html_to_ffon_with_forms(html, base_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sicompass_sdk::markup;
    use sicompass_sdk::tags::TargetKind;

    /// Text that would be live if it reached a row as written.
    const HOSTILE: [&str; 6] = [
        "<b>x</b>",
        r#"<img src="/etc/passwd">"#,
        "<style>li { color: red }</style>",
        "<input>y</input>",
        "<button>go</button>",
        r"ends in a backslash\",
    ];

    /// The only tags the walker writes. Anything else unescaped in a row came
    /// from the page.
    const WALKERS_TAGS: [&str; 13] = [
        "<id>",
        "</id>",
        "<link>",
        "</link>",
        "<input>",
        "</input>",
        "<button>",
        "</button>",
        "<checkbox>",
        "<checkbox checked>",
        "<radio>",
        "<checked>",
        "</checked>",
    ];

    fn html_text(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn html_attr(s: &str) -> String {
        html_text(s).replace('"', "&quot;")
    }

    /// Every path text can take through the walker, as (name, document body).
    /// Text and attribute value are `p` already written for the place it goes.
    fn pages(p: &str) -> Vec<(&'static str, String)> {
        let t = html_text(p);
        let a = html_attr(p);
        vec![
            ("p", format!("<p>{t}</p>")),
            ("p with a span", format!("<p>a <span>{t}</span> b</p>")),
            ("div", format!("<div>{t}</div>")),
            ("custom element", format!("<x-y>{t}</x-y>")),
            ("h1", format!("<h1>{t}</h1>")),
            ("ul", format!("<ul><li>{t}</li></ul>")),
            ("ol", format!("<ol><li>{t}</li></ol>")),
            (
                "td",
                format!("<table><tr><td>{t}</td><td>b</td></tr></table>"),
            ),
            ("th", format!("<table><tr><th>{t}</th></tr></table>")),
            (
                "td script",
                format!("<table><tr><td><script>{p}</script></td></tr></table>"),
            ),
            (
                "td style",
                format!("<table><tr><td><style>{p}</style></td></tr></table>"),
            ),
            ("dt", format!("<dl><dt>{t}</dt><dd>d</dd></dl>")),
            ("dd", format!("<dl><dt>t</dt><dd>{t}</dd></dl>")),
            ("pre", format!("<pre>{t}</pre>")),
            ("code", format!("<code>{t}</code>")),
            ("nav", format!("<nav><ul><li>{t}</li></ul></nav>")),
            ("img alt", format!(r#"<img src="a.png" alt="{a}">"#)),
            (
                "a text",
                format!(r#"<a href="https://a.invalid/x">{t}</a>"#),
            ),
            (
                "a text in a p",
                format!(r#"<p>see <a href="https://a.invalid/x">{t}</a></p>"#),
            ),
            ("a href", format!(r#"<a href="{a}">link</a>"#)),
            (
                "a absolute href",
                format!(r#"<a href="https://a.invalid/?q={a}">link</a>"#),
            ),
            ("a fragment href", format!(r##"<a href="#{a}">link</a>"##)),
            ("a href in a p", format!(r#"<p><a href="{a}">link</a></p>"#)),
            ("div id", format!(r#"<div id="{a}"><p>text</p></div>"#)),
            ("h2 id", format!(r#"<h2 id="{a}">title</h2>"#)),
            ("ul id", format!(r#"<ul id="{a}"><li>item</li></ul>"#)),
            ("dl id", format!(r#"<dl id="{a}"><dt>t</dt></dl>"#)),
            ("nav id", format!(r#"<nav id="{a}"><p>text</p></nav>"#)),
            (
                "form id",
                format!(r#"<form id="{a}"><input name="q"></form>"#),
            ),
            ("input value", format!(r#"<input name="q" value="{a}">"#)),
            ("input placeholder", format!(r#"<input placeholder="{a}">"#)),
            ("input aria-label", format!(r#"<input aria-label="{a}">"#)),
            ("input name", format!(r#"<form><input name="{a}"></form>"#)),
            ("input id", format!(r#"<form><input id="{a}"></form>"#)),
            (
                "checkbox",
                format!(r#"<form><input type="checkbox" name="{a}"></form>"#),
            ),
            (
                "radio",
                format!(r#"<form><input type="radio" name="{a}" value="{a}"></form>"#),
            ),
            (
                "submit value",
                format!(r#"<form><input type="submit" value="{a}"></form>"#),
            ),
            (
                "button text",
                format!(r#"<form><button>{t}</button></form>"#),
            ),
            (
                "button id",
                format!(r#"<form><button id="{a}" name="{a}">go</button></form>"#),
            ),
            ("textarea", format!("<form><textarea>{t}</textarea></form>")),
            (
                "textarea name",
                format!(r#"<form><textarea name="{a}"></textarea></form>"#),
            ),
            (
                "select name",
                format!(r#"<form><select name="{a}"><option>o</option></select></form>"#),
            ),
            (
                "select aria-label",
                format!(r#"<form><select aria-label="{a}"><option>o</option></select></form>"#),
            ),
            (
                "option text",
                format!(r#"<form><select name="s"><option>{t}</option></select></form>"#),
            ),
            (
                "selected option",
                format!(r#"<form><select name="s"><option selected>{t}</option></select></form>"#),
            ),
            (
                "option value",
                format!(r#"<form><select name="s"><option value="{a}"></option></select></form>"#),
            ),
        ]
    }

    fn document(body: &str) -> String {
        format!("<!DOCTYPE html><html><head><title>t</title></head><body>{body}</body></html>")
    }

    fn all_rows(elements: &[FfonElement], out: &mut Vec<String>) {
        for element in elements {
            match element {
                FfonElement::Str(s) => out.push(s.clone()),
                FfonElement::Obj(o) => {
                    out.push(o.key.clone());
                    all_rows(&o.children, out);
                }
            }
        }
    }

    /// Every unescaped `<` in `row` opens a tag the walker writes, and there is
    /// no target in it but a genuine link or a form's submit button.
    fn assert_only_the_walkers_tags(row: &str, context: &str) {
        for (at, _) in row.match_indices('<') {
            if row[..at].ends_with('\\') {
                continue;
            }
            let end = row[at..]
                .find('>')
                .unwrap_or_else(|| panic!("{context}: open bracket in {row}"));
            let tag = &row[at..=at + end];
            assert!(
                WALKERS_TAGS.contains(&tag),
                "{context}: {tag} is the page's, in {row}"
            );
        }

        assert!(!markup::has_markup(row), "{context}: markup in {row}");
        assert!(markup::style_sheet(row).is_none(), "{context}: {row}");
        let mut targets = tags::targets(row);
        targets.extend(markup::targets(row));
        for target in targets {
            match target.kind {
                TargetKind::Button => assert!(
                    target.value.starts_with("submit:form_"),
                    "{context}: button {} in {row}",
                    target.value
                ),
                TargetKind::Link => assert!(
                    !target.value.contains(['<', '>']) || target.value.starts_with('#'),
                    "{context}: link {} in {row}",
                    target.value
                ),
                other => panic!("{context}: {other:?} {} in {row}", target.value),
            }
        }
    }

    #[test]
    fn no_path_through_the_walker_lets_a_page_write_a_tag() {
        for text in HOSTILE {
            for (place, body) in pages(text) {
                let context = format!("{text} as {place}");
                let (elements, _) = html_to_rows(&document(&body), "https://a.invalid/page");
                let mut rows = Vec::new();
                all_rows(&elements, &mut rows);
                assert!(!rows.is_empty(), "{context}: nothing came through");
                for row in &rows {
                    assert_only_the_walkers_tags(row, &context);
                }
            }
        }
    }

    /// Where a row shows text, `strip_display` gives the original bytes.
    /// `known_sdk_limit_a_row_with_an_id_or_button_...` below is the exception.
    #[test]
    fn the_text_reads_as_the_page_wrote_it() {
        const SHOWN: [&str; 32] = [
            "p",
            "p with a span",
            "custom element",
            "h1",
            "ul",
            "ol",
            "td",
            "th",
            "dt",
            "dd",
            "pre",
            "code",
            "nav",
            "img alt",
            "a text",
            "a text in a p",
            "input value",
            "input placeholder",
            "input aria-label",
            "input name",
            "input id",
            "checkbox",
            "radio",
            "submit value",
            "button text",
            "textarea",
            "textarea name",
            "select name",
            "select aria-label",
            "option text",
            "selected option",
            "option value",
        ];
        for text in HOSTILE {
            let tag_shaped = matches!(text, "<input>y</input>" | "<button>go</button>");
            for (place, body) in pages(text) {
                if !SHOWN.contains(&place)
                    || tag_shaped && matches!(place, "submit value" | "button text")
                {
                    continue;
                }
                let (elements, _) = html_to_rows(&document(&body), "https://a.invalid/page");
                let mut rows = Vec::new();
                all_rows(&elements, &mut rows);
                let shown: Vec<String> = rows.iter().map(|r| tags::strip_display(r)).collect();
                assert!(
                    shown.iter().any(|s| s.contains(text)),
                    "{text} as {place}: {shown:?} from {rows:?}"
                );
            }
        }
    }

    /// A link is followed raw, so its value holds no backslash of ours, in the
    /// three forms a walker's `href` can arrive in.
    /// Used to lose its text on a second pass over a row with an `<id>` or a button.
    #[test]
    fn a_row_with_an_id_or_button_reads_as_the_page_wrote_it() {
        for (body, shown) in [
            (
                r#"<div id="a"><p>&lt;input&gt;y&lt;/input&gt;</p></div>"#,
                "<input>y</input>",
            ),
            (
                r#"<div id="a"><p>&lt;button&gt;go&lt;/button&gt;</p></div>"#,
                "<button>go</button>",
            ),
            (
                r#"<form><button>&lt;input&gt;y&lt;/input&gt;</button></form>"#,
                "<input>y</input>",
            ),
        ] {
            let (elements, _) = html_to_rows(&document(body), "https://a.invalid/");
            let mut rows = Vec::new();
            all_rows(&elements, &mut rows);
            let row = rows.last().unwrap();
            assert_only_the_walkers_tags(row, body);
            assert_eq!(tags::strip_display(row), shown, "{row}");
        }
    }

    /// Used to read as an escaped closing tag, so the app saw no field and no link.
    #[test]
    fn a_value_ending_in_a_backslash_is_still_a_field_and_a_link() {
        let (elements, _) = html_to_rows(
            &document(r#"<input name="q" value="back\">"#),
            "https://a.invalid/",
        );
        let FfonElement::Str(row) = &elements[0] else {
            panic!("a field");
        };
        assert_eq!(row, r"q: <input>back\</input>");
        assert!(tags::has_input(row));
        assert_eq!(tags::extract_input(row).as_deref(), Some(r"back\"));
        assert_eq!(tags::strip_display(row), r"q: back\");

        let (elements, _) = html_to_rows(
            &document(r#"<a href="https://a.invalid/back\">x</a>"#),
            "https://a.invalid/",
        );
        let FfonElement::Obj(row) = &elements[0] else {
            panic!("a link");
        };
        assert!(tags::has_link(&row.key), "{}", row.key);
        assert!(tags::extract_link(&row.key).is_some(), "{}", row.key);
    }

    #[test]
    fn a_link_with_brackets_is_percent_encoded() {
        for (href, value) in [
            // Absolute: the walker copies it.
            (
                "https://a.invalid/?q=&lt;b&gt;",
                "https://a.invalid/?q=%3Cb%3E",
            ),
            // Relative: `Url::join` percent-encodes the stand-ins.
            ("q/&lt;b&gt;?x=&lt;", "https://a.invalid/q/%3Cb%3E?x=%3C"),
            // A fragment names a row's `<id>`, so it keeps the id's form.
            ("#&lt;b&gt;", r"#\<b\>"),
        ] {
            let body = format!(r#"<a href="{href}">x</a>"#);
            let (elements, _) = html_to_rows(&document(&body), "https://a.invalid/");
            let FfonElement::Obj(row) = &elements[0] else {
                panic!("a link");
            };
            assert_eq!(tags::extract_link(&row.key).as_deref(), Some(value));
            assert!(tags::has_link(&row.key));
        }
    }

    #[test]
    fn an_ampersand_in_a_query_survives() {
        let (elements, _) = html_to_rows(
            &document(r#"<a href="p?a=1&amp;b=2">x &amp; y &lt; z</a>"#),
            "https://a.invalid/",
        );
        let FfonElement::Obj(row) = &elements[0] else {
            panic!("a link");
        };
        assert_eq!(
            tags::extract_link(&row.key).as_deref(),
            Some("https://a.invalid/p?a=1&b=2")
        );
        assert_eq!(
            row.key,
            r"x & y \< z <link>https://a.invalid/p?a=1&b=2</link>"
        );
    }

    /// Markup that reads differently the second time it is parsed, which is what
    /// writing a page out and handing it on risks. These are the shapes of the
    /// mutation XSS classics, aimed at the walker's unescaped table cell.
    #[test]
    fn a_page_that_mutates_when_parsed_again_writes_no_tag() {
        let payload = r#"<img src="/etc/passwd">"#;
        for body in [
            format!("<table><tr><td><noscript>{payload}</noscript></td></tr></table>"),
            "<table><tr><td><noscript>&amp;lt;img src=x&amp;gt;</noscript></td></tr></table>"
                .to_owned(),
            format!("<table><tr><td><svg><style>{payload}</style></svg></td></tr></table>"),
            format!("<table><tr><td><svg><title>{payload}</title></svg></td></tr></table>"),
            format!(
                "<table><tr><td><math><mtext><style>{payload}</style></mtext></math></td></tr></table>"
            ),
            format!(
                "<table><tr><td><form><math><mtext></form><form><mglyph><style></math>{payload}</style></form></td></tr></table>"
            ),
            format!(
                "<table><tr><td><svg></p><style><a id=\"</style>{payload}\">x</a></svg></td></tr></table>"
            ),
            format!("<table><tr><td><xmp>{payload}</xmp></td></tr></table>"),
            format!("<table><tr><td><iframe>{payload}</iframe></td></tr></table>"),
            format!("<table><tr><td><noembed>{payload}</noembed></td></tr></table>"),
            format!("<table><tr><td><noframes>{payload}</noframes></td></tr></table>"),
            format!("<table><tr><td><title>{payload}</title></td></tr></table>"),
            format!("<table><tr><td><textarea>{payload}</textarea></td></tr></table>"),
            format!("<table><tr><td><template>{payload}</template></td></tr></table>"),
            format!("<table><tr><td><!-- -->{payload}<!-- --></td></tr></table>"),
            format!("<table><tr><td><plaintext>{payload}"),
            format!("<!-- --!>{payload}<!-- -->"),
            format!(r#"<!-- x --><p title="--><img src=x>">{payload}</p>"#),
            format!("<select><option>{payload}</option></select>"),
            format!("<listing>{payload}</listing>"),
        ] {
            let (elements, _) = html_to_rows(&document(&body), "https://a.invalid/");
            let mut rows = Vec::new();
            all_rows(&elements, &mut rows);
            for row in &rows {
                assert_only_the_walkers_tags(row, &body);
            }
        }
    }

    /// A form's selector and its key are plain, as they were: the selector goes
    /// to Chrome, and the key is matched against a path the app built.
    #[test]
    fn the_form_map_holds_plain_brackets() {
        use sicompass_sdk::ffon::FormNodeKind;
        let body = r#"<form><input name="a&lt;b" type="radio" value="c&gt;d">
            <input placeholder="e&lt;f" name="g">
            <select name="h"><option value="i&lt;j">k&lt;l</option></select>
            <input type="submit" value="m&lt;n"></form>"#;
        let (_, forms) = html_to_rows(&document(body), "https://a.invalid/");
        let radio = &forms["form_1/a<b/a<b"];
        assert_eq!(radio.css_selector, r#"[name="a<b"]"#);
        assert_eq!(
            radio.kind,
            FormNodeKind::RadioOption {
                group: "a<b".to_owned(),
                value: "c>d".to_owned()
            }
        );
        assert!(forms.contains_key("form_1/e<f"));
        let option = &forms["form_1/h/k<l"];
        assert_eq!(
            option.css_selector,
            r#"select[name="h"] option[value="i<j"]"#
        );
        assert!(forms.contains_key("form_1/m<n"));
    }

    #[test]
    fn unescape_undoes_escape_and_leaves_anything_else() {
        for text in HOSTILE.into_iter().chain([r"\<", r"\\<", r"a\>b", ""]) {
            assert_eq!(unescape(&tags::escape(text)), text);
        }
        assert_eq!(unescape(r"C:\dir\file"), r"C:\dir\file");
    }
}
