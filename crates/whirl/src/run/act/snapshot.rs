//! The page as `ACT` sees it (SPEC 7.4): a Playwright AI snapshot of the
//! selected tab, and the element refs it names.

use std::borrow::Cow;
use std::collections::HashMap;
use std::str::Chars;

use serde_json::{Value as Json, json};

/// An element ref from an AI snapshot: `e12`, or `f1e3` for an element
/// inside the first iframe. The shim resolves it with an `aria-ref=`
/// locator.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ElementRef(String);

impl ElementRef {
    /// Accepts `e<digits>` with an optional `f<digits>` frame prefix.
    pub(crate) fn try_new(text: &str) -> Option<Self> {
        let element = match text.strip_prefix('f') {
            Some(framed) => {
                let (frame, element) = framed.split_once('e')?;
                if !is_digits(frame) {
                    return None;
                }
                element
            }
            None => text.strip_prefix('e')?,
        };
        is_digits(element).then(|| Self(text.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// What the snapshot says about one element: its ARIA role and, when it
/// has one, its accessible name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotNode {
    role: String,
    name: Option<String>,
}

/// An element's ARIA role and accessible name: what the AI cache checks
/// before it replays a locator (SPEC 12.1).
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct Fingerprint {
    pub(crate) role: String,
    pub(crate) name: Option<String>,
}

impl Fingerprint {
    /// The fingerprint of the first entry of an element's AI snapshot,
    /// which is the element itself.
    pub(crate) fn of_snapshot(snapshot: &str) -> Option<Self> {
        let line = snapshot.lines().find_map(SnapshotLine::parse)?;
        Some(Self {
            role: line.role,
            name: line.name,
        })
    }
}

impl SnapshotNode {
    /// The element as a Whirl locator, such as `role:button "Sign in"`.
    /// It describes the element for reports; it is not guaranteed to be
    /// unique on the page.
    pub(crate) fn locator_text(&self) -> String {
        match &self.name {
            Some(name) => format!("role:{} {}", self.role, quote(name)),
            None => format!("role:{}", self.role),
        }
    }
}

/// A quoted Whirl value (SPEC 3.1).
pub(crate) fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// One AI snapshot of the selected tab.
#[derive(Clone, Debug)]
pub(crate) struct PageSnapshot {
    raw:   String,
    text:  String,
    nodes: HashMap<ElementRef, SnapshotNode>,
    /// In a snapshot of the whole page, the first ref, which names `<body>`.
    page:  Option<ElementRef>,
}

impl PageSnapshot {
    /// Indexes every `[ref=...]` line of a snapshot, and keeps the text the
    /// model reads.
    pub(crate) fn parse(snapshot: &str) -> Self {
        let nodes = snapshot.lines().filter_map(parse_line).collect();
        Self {
            raw: snapshot.to_owned(),
            text: condense(snapshot),
            nodes,
            page: None,
        }
    }

    /// Marks the snapshot as the whole page's, so its first ref, `<body>`,
    /// stands for the page (SPEC 7.4).
    pub(crate) fn of_page(mut self) -> Self {
        self.page = self
            .raw
            .lines()
            .find_map(parse_line)
            .map(|(element, _)| element);
        self
    }

    /// The snapshot as the shim took it, cursor hints included.
    pub(crate) fn raw(&self) -> &str {
        &self.raw
    }

    /// The snapshot as the model reads it (SPEC 7.4).
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The element a model answer names, when the snapshot has it.
    pub(crate) fn target(&self, raw_ref: &str) -> Option<Target> {
        let element = ElementRef::try_new(raw_ref)?;
        let node = self.nodes.get(&element)?.clone();
        let page = self.page.as_ref() == Some(&element);
        Some(Target {
            element,
            node,
            page,
        })
    }
}

/// The snapshot without the parts the model does not need: the `/url:` line
/// under every link and the `[cursor=pointer]` mark on every clickable element.
/// On a link-heavy page such as a Wikipedia article they are about a third of
/// the snapshot. The model also reads each line without its YAML quotes (see
/// [`EntryParts`]), so every element line has one form. Text in a name that
/// looks like a mark stays as it is.
fn condense(snapshot: &str) -> String {
    let mut text = String::with_capacity(snapshot.len());
    for line in snapshot.lines() {
        match EntryParts::split(line) {
            Some(parts) if parts.key == "/url" => continue,
            Some(parts) => {
                let key = KeyParts::split(&parts.key);
                text.push_str(parts.indent);
                text.push_str("- ");
                text.push_str(key.head);
                text.push_str(&key.marks.replace(" [cursor=pointer]", ""));
                text.push_str(parts.tail);
            }
            None => text.push_str(line),
        }
        text.push('\n');
    }
    text
}

/// One entry of an AI snapshot, read into its parts. Playwright writes each
/// entry as `- KEY`, as `- KEY:` before child lines, or as `- KEY: VALUE`.
/// The key holds the role, the name, and marks such as `[ref=e5]`, as in
/// `  - button "Sign in" [ref=e5] [cursor=pointer]: Go`. Text lines
/// (`- text: Go`) and properties (`- /url: /docs`) have the same form, with
/// `text` or `/url` as the role. [`PageSnapshot`] and Jev's outline read
/// every line with it, so they agree on each element's ref and name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotLine {
    /// The spaces before the `- `.
    pub(crate) indent: usize,
    pub(crate) role:   String,
    pub(crate) name:   Option<String>,
    /// The text after the key, such as a text box's value.
    pub(crate) value:  Option<String>,
    /// The marks after the name, without brackets, such as `ref=e5`.
    marks:             Vec<String>,
}

impl SnapshotLine {
    /// Reads one line. `None` for a line that is not an entry, or whose YAML
    /// quotes do not close.
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let parts = EntryParts::split(line)?;
        let key = KeyParts::split(&parts.key);
        Some(Self {
            indent: parts.indent.len(),
            role:   key.role.to_owned(),
            name:   key.name.and_then(read_name),
            value:  parts.value(),
            marks:  read_marks(key.marks),
        })
    }

    /// The element ref, from the line's `[ref=…]` mark.
    pub(crate) fn element(&self) -> Option<&str> {
        self.marks.iter().find_map(|mark| mark.strip_prefix("ref="))
    }

    /// Whether the line has a mark such as `active` or `cursor=pointer`.
    pub(crate) fn has_mark(&self, mark: &str) -> bool {
        self.marks.iter().any(|each| each == mark)
    }
}

/// A snapshot entry split where its key ends. `  - button "Go" [ref=e5]: x`
/// has the indent `  `, the key `button "Go" [ref=e5]`, and the tail `: x`.
struct EntryParts<'a> {
    indent: &'a str,
    /// The key without the YAML single quotes that Playwright puts around a
    /// key that holds `: `, ` #`, a brace, a backtick, or a control
    /// character. Inside the quotes, `''` stands for `'`.
    key:    Cow<'a, str>,
    /// What follows the key: nothing, `:` before child lines, or `: ` and a
    /// value. Playwright puts a value in double quotes, never in single.
    tail:   &'a str,
}

impl<'a> EntryParts<'a> {
    /// `None` for a line that is not an entry, or whose YAML quotes do not
    /// close.
    fn split(line: &'a str) -> Option<Self> {
        let entry = line.trim_start();
        let indent = &line[..line.len() - entry.len()];
        let body = entry.strip_prefix("- ")?;
        if let Some(quoted) = body.strip_prefix('\'') {
            let end = closing_quote(quoted)?;
            return Some(Self {
                indent,
                key: Cow::Owned(quoted[..end].replace("''", "'")),
                tail: &quoted[end + 1..],
            });
        }
        let end = plain_key_len(body);
        Some(Self {
            indent,
            key: Cow::Borrowed(&body[..end]),
            tail: &body[end..],
        })
    }

    /// The value after `: `, out of its double quotes when it has them.
    fn value(&self) -> Option<String> {
        let text = self.tail.strip_prefix(": ")?.trim();
        Some(match double_quoted(text) {
            Some((value, "")) => value,
            _ => text.to_owned(),
        })
    }
}

/// Where a YAML single-quoted text ends, after its opening quote: the first
/// quote that is not one of a doubled pair.
fn closing_quote(quoted: &str) -> Option<usize> {
    let mut quotes = quoted
        .match_indices('\'')
        .map(|(index, _)| index)
        .peekable();
    while let Some(index) = quotes.next() {
        if quotes.next_if_eq(&(index + 1)).is_none() {
            return Some(index);
        }
    }
    None
}

/// Where a key without YAML quotes ends: at the first `:` before a space or
/// the line's end. Playwright quotes a key that holds `: ` itself. The shim
/// writes an iframe's name in double quotes without YAML quotes, so the
/// search starts after a name in double quotes.
fn plain_key_len(body: &str) -> usize {
    let role_len = role_len(body);
    let after_name = body[role_len..]
        .strip_prefix(' ')
        .and_then(closing_double_quote)
        .map_or(role_len, |end| role_len + 1 + end + 1);
    body[after_name..]
        .match_indices(':')
        .map(|(index, _)| after_name + index)
        .find(|&colon| matches!(body.as_bytes().get(colon + 1), None | Some(b' ')))
        .unwrap_or(body.len())
}

fn role_len(key: &str) -> usize {
    key.find([' ', ':', '[']).unwrap_or(key.len())
}

/// A key split into its parts: `button "Sign in" [ref=e5]` has the role
/// `button`, the name `"Sign in"` as Playwright wrote it, and the marks
/// ` [ref=e5]`. Only the text after the name holds marks.
struct KeyParts<'k> {
    /// The role and the name.
    head:  &'k str,
    role:  &'k str,
    name:  Option<&'k str>,
    marks: &'k str,
}

impl<'k> KeyParts<'k> {
    /// Playwright's `renderAriaTree` writes a name with `JSON.stringify`,
    /// except a name that starts and ends with `/`, which it writes as it
    /// is, as in `button /api/`, with no quotes or escapes. Marks never hold
    /// a `/`, so such a name ends at the key's last `/`.
    fn split(key: &'k str) -> Self {
        let role_len = role_len(key);
        let name_len =
            key[role_len..]
                .strip_prefix(' ')
                .and_then(|written| match written.chars().next() {
                    Some('"') => closing_double_quote(written).map(|end| end + 1),
                    Some('/') => written.rfind('/').map(|end| end + 1),
                    _ => None,
                });
        let head_len = name_len.map_or(role_len, |len| role_len + 1 + len);
        Self {
            head:  &key[..head_len],
            role:  &key[..role_len],
            name:  name_len.map(|_| &key[role_len + 1..head_len]),
            marks: &key[head_len..],
        }
    }
}

/// A name's text: out of its double quotes, or, for a name between slashes,
/// as Playwright wrote it, slashes included.
fn read_name(written: &str) -> Option<String> {
    if written.starts_with('"') {
        double_quoted(written).map(|(name, _)| name)
    } else {
        Some(written.to_owned())
    }
}

/// The marks in text such as ` [ref=e5] [cursor=pointer]`, without their
/// brackets.
fn read_marks(mut text: &str) -> Vec<String> {
    let mut marks = Vec::new();
    while let Some((mark, rest)) = text
        .strip_prefix(" [")
        .and_then(|after| after.split_once(']'))
    {
        marks.push(mark.to_owned());
        text = rest;
    }
    marks
}

/// The double-quoted text at the start of `text`, decoded, and the rest of
/// `text`. Playwright writes a name with `JSON.stringify`, and a value with
/// the same escapes plus `\xHH` for the other control characters
/// (`yamlEscapeValueIfNeeded`). A lone surrogate reads as U+FFFD.
fn double_quoted(text: &str) -> Option<(String, &str)> {
    let end = closing_double_quote(text)?;
    Some((unescape(&text[1..end])?, &text[end + 1..]))
}

/// The index of the quote that closes the double-quoted text at the start
/// of `text`. A backslash escapes the character after it.
fn closing_double_quote(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, ch) in text.strip_prefix('"')?.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(index + 1),
            _ => {}
        }
    }
    None
}

/// The text of a double-quoted string without its quotes. `None` for an
/// escape that neither JSON nor Playwright writes.
fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let decoded = match chars.next()? {
            escaped @ ('"' | '\\' | '/') => escaped,
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'x' => char::from_u32(hex(&mut chars, 2)?)?,
            'u' => utf16_escape(&mut chars)?,
            _ => return None,
        };
        out.push(decoded);
    }
    Some(out)
}

/// The character of a `\uXXXX` escape, read after its `\u`, together with
/// the low surrogate that may follow a high one. A lone surrogate reads as
/// U+FFFD.
fn utf16_escape(chars: &mut Chars<'_>) -> Option<char> {
    let unit = hex(chars, 4)?;
    if !(0xD800..0xDC00).contains(&unit) {
        return Some(char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    let mut ahead = chars.clone();
    if ahead.next() == Some('\\')
        && ahead.next() == Some('u')
        && let Some(low) = hex(&mut ahead, 4)
        && (0xDC00..0xE000).contains(&low)
    {
        *chars = ahead;
        return char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00));
    }
    Some(char::REPLACEMENT_CHARACTER)
}

/// The value of the next `count` characters as hex digits.
fn hex(chars: &mut Chars<'_>, count: usize) -> Option<u32> {
    (0..count).try_fold(0, |value, _| Some(value * 16 + chars.next()?.to_digit(16)?))
}

/// Parses one snapshot line such as `  - button "Sign in" [ref=e5]`, with or
/// without Playwright's YAML quotes.
fn parse_line(line: &str) -> Option<(ElementRef, SnapshotNode)> {
    let entry = SnapshotLine::parse(line)?;
    let element = ElementRef::try_new(entry.element()?)?;
    Some((element, SnapshotNode {
        role: entry.role,
        name: entry.name,
    }))
}

/// An element the model chose. Only [`PageSnapshot::target`] builds one,
/// so a ref the page never showed cannot become an action target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Target {
    element: ElementRef,
    node:    SnapshotNode,
    /// True when the element is the page's `<body>`.
    page:    bool,
}

impl Target {
    /// The protocol locator JSON: one `ref` segment (protocol 4.1).
    pub(crate) fn locator_wire(&self) -> Json {
        json!([{"type": "ref", "ref": self.element.as_str()}])
    }

    pub(crate) fn locator_text(&self) -> String {
        self.node.locator_text()
    }

    /// The snapshot ref, such as `e12`.
    pub(crate) fn element_ref(&self) -> &str {
        self.element.as_str()
    }

    /// True when the element stands for the whole page.
    pub(crate) fn is_page(&self) -> bool {
        self.page
    }

    /// The element's role and accessible name (SPEC 12.1).
    pub(crate) fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            role: self.node.role.clone(),
            name: self.node.name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAPSHOT: &str = r#"- generic [active] [ref=e1]:
  - heading "Shop" [level=1] [ref=e2]
  - generic [ref=e3]:
    - text: Email
    - textbox "Email" [ref=e4]
  - button "Say \"hi\"" [ref=e5]
  - combobox "Size" [ref=e6]:
    - option "S" [selected]
  - iframe [ref=e7]:
    - button "Pay" [ref=f1e2]"#;

    #[test]
    fn indexes_every_ref_with_its_role_and_name() {
        let snapshot = PageSnapshot::parse(SNAPSHOT);
        let target = snapshot.target("e4").expect("e4 is in the snapshot");
        assert_eq!(target.locator_text(), r#"role:textbox "Email""#);
        let target = snapshot.target("e5").expect("e5 is in the snapshot");
        assert_eq!(target.locator_text(), r#"role:button "Say \"hi\"""#);
        let target = snapshot.target("e1").expect("e1 is in the snapshot");
        assert_eq!(target.locator_text(), "role:generic");
        let target = snapshot.target("f1e2").expect("framed refs are indexed");
        assert_eq!(target.locator_text(), r#"role:button "Pay""#);
    }

    #[test]
    fn an_iframe_the_shim_named_keeps_its_role_and_name() {
        let snapshot = PageSnapshot::parse(
            "- iframe \"Incident history\" [ref=e4]:\n  - paragraph [ref=f1e2]: Resolved\n",
        );
        let target = snapshot.target("e4").expect("e4 is in the snapshot");
        assert_eq!(target.locator_text(), r#"role:iframe "Incident history""#);
    }

    #[test]
    fn a_ref_the_snapshot_never_showed_is_not_a_target() {
        let snapshot = PageSnapshot::parse(SNAPSHOT);
        assert_eq!(snapshot.target("e99"), None);
        assert_eq!(snapshot.target("0-18372"), None);
        assert_eq!(snapshot.target("[ref=e4]"), None);
    }

    #[test]
    fn element_refs_accept_only_playwright_shapes() {
        for good in ["e1", "e123", "f1e2", "f10e300"] {
            assert!(ElementRef::try_new(good).is_some(), "{good}");
        }
        for bad in ["", "e", "f1", "fe2", "e1a", "x1", "f1x2", "E1"] {
            assert!(ElementRef::try_new(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_model_reads_the_snapshot_without_urls_and_cursor_marks() {
        let snapshot = PageSnapshot::parse(
            "- link \"Docs\" [ref=e3] [cursor=pointer]:\n  - /url: https://example.com/docs\n- button \"Go\" [ref=e4] [cursor=pointer]\n",
        );
        assert_eq!(
            snapshot.text(),
            "- link \"Docs\" [ref=e3]:\n- button \"Go\" [ref=e4]\n"
        );
        assert_eq!(
            snapshot.target("e3").map(|target| target.locator_text()),
            Some(r#"role:link "Docs""#.to_owned())
        );
    }

    /// Lines as Playwright 1.62.1 writes them for names that hold `: `,
    /// ` #`, a brace, or a backtick.
    const QUOTED: &str = r##"- generic [active] [ref=e1]:
  - 'button "Status: live" [ref=e2]'
  - 'button "It''s: here" [ref=e3]': Go
  - 'button "Both \"a\" and ''b'': x" [ref=e4]'
  - 'region "Q3: plan" [ref=e5]':
    - 'link "x #y" [ref=e6] [cursor=pointer]':
      - /url: "#top"
  - 'textbox "Note: x" [active] [ref=e7]': "Status: live"
  - 'button "{it''s}" [ref=e8]': x
  - 'button "tick `x`" [ref=e9]'
  - iframe [ref=e10]:
    - 'button "In: frame" [ref=f1e2]'"##;

    /// The same lines without the quotes.
    const UNQUOTED: &str = r##"- generic [active] [ref=e1]:
  - button "Status: live" [ref=e2]
  - button "It's: here" [ref=e3]: Go
  - button "Both \"a\" and 'b': x" [ref=e4]
  - region "Q3: plan" [ref=e5]:
    - link "x #y" [ref=e6] [cursor=pointer]:
      - /url: "#top"
  - textbox "Note: x" [active] [ref=e7]: "Status: live"
  - button "{it's}" [ref=e8]: x
  - button "tick `x`" [ref=e9]
  - iframe [ref=e10]:
    - button "In: frame" [ref=f1e2]"##;

    #[test]
    fn a_quoted_line_names_its_element_as_the_unquoted_line_does() {
        let snapshot = PageSnapshot::parse(QUOTED);
        for (element, locator) in [
            ("e2", r#"role:button "Status: live""#),
            ("e3", r#"role:button "It's: here""#),
            ("e4", r#"role:button "Both \"a\" and 'b': x""#),
            ("e5", r#"role:region "Q3: plan""#),
            ("e6", r#"role:link "x #y""#),
            ("e7", r#"role:textbox "Note: x""#),
            ("e8", r#"role:button "{it's}""#),
            ("e9", r#"role:button "tick `x`""#),
            ("f1e2", r#"role:button "In: frame""#),
        ] {
            let target = snapshot
                .target(element)
                .expect("the ref is in the snapshot");
            assert_eq!(target.locator_text(), locator, "{element}");
        }
        assert_eq!(snapshot.nodes, PageSnapshot::parse(UNQUOTED).nodes);
    }

    #[test]
    fn the_model_reads_quoted_lines_without_their_quotes() {
        let snapshot = PageSnapshot::parse(QUOTED);
        assert_eq!(snapshot.text(), PageSnapshot::parse(UNQUOTED).text());
        assert!(
            snapshot.text().contains(
                "\n  - region \"Q3: plan\" [ref=e5]:\n    - link \"x #y\" [ref=e6]:\n  - textbox"
            ),
            "{}",
            snapshot.text()
        );
        assert_eq!(snapshot.raw(), QUOTED);
    }

    #[test]
    fn only_a_single_quoted_entry_loses_its_quotes() {
        let text = |line: &str| PageSnapshot::parse(line).text().to_owned();
        for line in [
            r#"  - button "It's" [ref=e6]: b4"#,
            r#"  - button "'Lead" [ref=e9]: b7"#,
            r#"    - listitem [ref=e47]: "'q'""#,
            r#"    - text: "Note: x""#,
            "  - 'button \"no closing quote\" [ref=e2]",
        ] {
            assert_eq!(text(line), format!("{line}\n"));
        }
        assert_eq!(
            text("    - 'option \"It''s: x\" [selected]'"),
            "    - option \"It's: x\" [selected]\n"
        );
        assert_eq!(
            text("  - 'textbox \"Say ''hi'': now\" [ref=e2]': \"'b'\""),
            "  - textbox \"Say 'hi': now\" [ref=e2]: \"'b'\"\n"
        );
        // A line whose quotes do not close names no element.
        assert_eq!(
            PageSnapshot::parse("  - 'button \"no closing quote\" [ref=e2]").target("e2"),
            None
        );
    }

    /// Names and texts that hold `[ref=…]` and other marks, as Playwright
    /// 1.62.1 writes them. Chromium, Firefox, and WebKit give the same
    /// lines.
    const CRAFTED: &str = r#"- generic [active] [ref=e1]:
  - heading "Crafted" [level=1] [ref=e2]
  - button "Save" [ref=e3]
  - button "Delete [ref=e9]" [ref=e4]
  - button "Say \"hi\" \\ back ] [ref=e2] [active]" [ref=e5]
  - button "x\" [ref=e1] [cursor=pointer]" [ref=e6]: ignored
  - 'button "Status: [ref=e9] live" [ref=e7]'
  - button "Go [checked] [active] [cursor=pointer]" [ref=e8]
  - paragraph [ref=e9]: see [ref=e3] here
  - list [ref=e10]:
    - listitem [ref=e11]: item [ref=e2]
  - textbox "Note [ref=e4]" [ref=e12]: v [ref=e5]
  - generic [ref=e17]:
    - button "Mixed" [ref=e18]
    - text: see [ref=e3] here"#;

    #[test]
    fn a_name_cannot_change_which_ref_a_line_names() {
        let snapshot = PageSnapshot::parse(CRAFTED).of_page();
        for (element, locator) in [
            ("e1", "role:generic"),
            ("e2", r#"role:heading "Crafted""#),
            ("e3", r#"role:button "Save""#),
            ("e4", r#"role:button "Delete [ref=e9]""#),
            (
                "e5",
                r#"role:button "Say \"hi\" \\ back ] [ref=e2] [active]""#,
            ),
            ("e6", r#"role:button "x\" [ref=e1] [cursor=pointer]""#),
            ("e7", r#"role:button "Status: [ref=e9] live""#),
            (
                "e8",
                r#"role:button "Go [checked] [active] [cursor=pointer]""#,
            ),
            ("e9", "role:paragraph"),
            ("e10", "role:list"),
            ("e11", "role:listitem"),
            ("e12", r#"role:textbox "Note [ref=e4]""#),
            ("e17", "role:generic"),
            ("e18", r#"role:button "Mixed""#),
        ] {
            let target = snapshot
                .target(element)
                .expect("each ref is in the snapshot");
            assert_eq!(target.locator_text(), locator, "{element}");
            assert_eq!(target.is_page(), element == "e1", "{element}");
        }
        assert_eq!(snapshot.nodes.len(), 14);
    }

    #[test]
    fn the_model_reads_mark_text_in_a_name_as_it_is() {
        let snapshot = PageSnapshot::parse(CRAFTED);
        for line in [
            "\n  - button \"x\\\" [ref=e1] [cursor=pointer]\" [ref=e6]: ignored\n",
            "\n  - button \"Status: [ref=e9] live\" [ref=e7]\n",
            "\n  - button \"Go [checked] [active] [cursor=pointer]\" [ref=e8]\n",
            "\n    - text: see [ref=e3] here\n",
        ] {
            assert!(snapshot.text().contains(line), "{}", snapshot.text());
        }
    }

    /// Names that start and end with `/`. Playwright 1.62.1 writes them as
    /// they are, without quotes or escapes, and the slashes are part of the
    /// name: `getByRole("button", { name: "/api/", exact: true })` finds the
    /// first button. WebKit gives the same lines without `[cursor=pointer]`.
    const SLASHES: &str = r#"- generic [active] [ref=e1]:
  - button /api/ [ref=e2]
  - button / [ref=e3]
  - button // [ref=e4]
  - button /a"b\c/ [ref=e5]
  - button /x/ [ref=e9] / [ref=e6]
  - 'button /a: b/ [ref=e7]'
  - button /it's/ [ref=e8]
  - 'button /a #b/ [ref=e9]'
  - button "/a/b" [ref=e10]
  - link /docs/ [ref=e11] [cursor=pointer]:
    - /url: /docs
  - heading /title/ [level=2] [ref=e12]
  - paragraph [ref=e13]: /para/
  - textbox /field/ [ref=e14]: /v/"#;

    #[test]
    fn a_name_between_slashes_keeps_its_slashes() {
        let snapshot = PageSnapshot::parse(SLASHES);
        for (element, locator) in [
            ("e2", r#"role:button "/api/""#),
            ("e3", r#"role:button "/""#),
            ("e4", r#"role:button "//""#),
            ("e5", r#"role:button "/a\"b\\c/""#),
            ("e6", r#"role:button "/x/ [ref=e9] /""#),
            ("e7", r#"role:button "/a: b/""#),
            ("e8", r#"role:button "/it's/""#),
            ("e9", r#"role:button "/a #b/""#),
            ("e10", r#"role:button "/a/b""#),
            ("e11", r#"role:link "/docs/""#),
            ("e12", r#"role:heading "/title/""#),
            ("e13", "role:paragraph"),
            ("e14", r#"role:textbox "/field/""#),
        ] {
            let target = snapshot
                .target(element)
                .expect("each ref is in the snapshot");
            assert_eq!(target.locator_text(), locator, "{element}");
        }
        assert!(
            snapshot
                .text()
                .contains("\n  - link /docs/ [ref=e11]:\n  - heading"),
            "{}",
            snapshot.text()
        );
    }

    #[test]
    fn names_with_control_characters_decode() {
        // `JSON.stringify` escapes C0 controls and lone surrogates; DEL
        // stays as it is, so Playwright quotes the line.
        let snapshot = PageSnapshot::parse(concat!(
            "- generic [active] [ref=e1]:\n",
            "  - 'button \"a\u{7f}b\" [ref=e8]'\n",
            "  - button \"a\\u0001b\" [ref=e9]\n",
            "  - textbox \"p\\u0001q\" [ref=e12]\n",
            "  - button \"a\\ud800b\" [ref=e19]\n",
        ));
        for (element, locator) in [
            ("e8", "role:button \"a\u{7f}b\""),
            ("e9", "role:button \"a\u{1}b\""),
            ("e12", "role:textbox \"p\u{1}q\""),
            ("e19", "role:button \"a\u{fffd}b\""),
        ] {
            let target = snapshot
                .target(element)
                .expect("each ref is in the snapshot");
            assert_eq!(target.locator_text(), locator, "{element}");
        }
    }

    #[test]
    fn double_quoted_text_decodes_json_and_hex_escapes() {
        assert_eq!(
            double_quoted(r#""a\x7fb" [ref=e2]"#),
            Some(("a\u{7f}b".to_owned(), " [ref=e2]"))
        );
        assert_eq!(
            double_quoted(r#""\"\\\/\b\f\n\r\t\x85é""#),
            Some(("\"\\/\u{8}\u{c}\n\r\t\u{85}\u{e9}".to_owned(), ""))
        );
        assert_eq!(
            double_quoted(r#""😀 \udc00 \ud800x""#),
            Some(("\u{1f600} \u{fffd} \u{fffd}x".to_owned(), ""))
        );
        for text in [r#""\q""#, r#""\x7""#, r#""\u12""#, r#""open"#, "plain"] {
            assert_eq!(double_quoted(text), None, "{text}");
        }
    }

    #[test]
    fn locators_render_with_target_ref_segments() {
        let snapshot = PageSnapshot::parse(SNAPSHOT);
        let target = snapshot.target("f1e2").expect("present");
        assert_eq!(
            target.locator_wire(),
            json!([{"type": "ref", "ref": "f1e2"}])
        );
    }
}
