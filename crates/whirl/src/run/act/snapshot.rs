//! The page as `ACT` sees it (SPEC 7.4): a Playwright AI snapshot of the
//! selected tab, and the element refs it names.

use std::collections::HashMap;

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
/// the snapshot.
fn condense(snapshot: &str) -> String {
    let mut text = String::with_capacity(snapshot.len());
    for line in snapshot.lines() {
        if line.trim_start().starts_with("- /url:") {
            continue;
        }
        text.push_str(&line.replace(" [cursor=pointer]", ""));
        text.push('\n');
    }
    text
}

/// Parses one snapshot line such as `  - button "Sign in" [ref=e5]`.
fn parse_line(line: &str) -> Option<(ElementRef, SnapshotNode)> {
    let rest = line.trim_start().strip_prefix("- ")?;
    let ref_start = rest.find("[ref=")? + "[ref=".len();
    let ref_len = rest[ref_start..].find(']')?;
    let element = ElementRef::try_new(&rest[ref_start..ref_start + ref_len])?;
    let role_len = rest.find([' ', ':', '[']).unwrap_or(rest.len());
    let role = rest[..role_len].to_owned();
    let name = rest[role_len..]
        .strip_prefix(' ')
        .filter(|after_role| after_role.starts_with('"'))
        .and_then(quoted_name);
    Some((element, SnapshotNode { role, name }))
}

/// Reads the JSON-style quoted name at the start of `text`.
fn quoted_name(text: &str) -> Option<String> {
    let mut escaped = false;
    for (index, ch) in text.char_indices().skip(1) {
        match ch {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return serde_json::from_str(&text[..=index]).ok(),
            _ => {}
        }
    }
    None
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
