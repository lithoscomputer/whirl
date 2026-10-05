//! The snapshot as a tree, and the candidates Jev chooses among.
//!
//! Jev cannot read a whole snapshot the way a language model does. It picks
//! from a short list, so each candidate carries the context that tells
//! twins apart: the text of its table row or card, the named sections
//! around it, the nearest heading, and its place among elements that look
//! the same.

use std::collections::{HashMap, HashSet};
use std::iter;

use serde_json::{Map, Value as Json, json};

use crate::run::act::snapshot::SnapshotLine;

/// One line of an AI snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct OutlineNode {
    depth:              usize,
    parent:             Option<usize>,
    pub(crate) role:    String,
    pub(crate) name:    Option<String>,
    /// The element ref, for lines that have one.
    pub(crate) element: Option<String>,
    /// Text after the line's colon, such as a text box's value.
    text:               Option<String>,
    /// `[active]`: the focused element.
    active:             bool,
    /// `[cursor=pointer]`: the page styles it as clickable.
    pointer:            bool,
    /// `[checked]`: a checkbox, radio, or switch that is on.
    pub(crate) checked: bool,
}

impl OutlineNode {
    fn from_line(line: SnapshotLine) -> Self {
        Self {
            depth:   line.indent / 2,
            parent:  None,
            element: line.element().map(str::to_owned),
            active:  line.has_mark("active"),
            pointer: line.has_mark("cursor=pointer"),
            checked: line.has_mark("checked") || line.has_mark("checked=true"),
            role:    line.role,
            name:    line.name,
            text:    line.value,
        }
    }
}

/// The snapshot as a tree, in document order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Outline {
    nodes: Vec<OutlineNode>,
}

/// Which elements an action can target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum View {
    /// Click, double-click, and hover.
    Pointer,
    /// Fill.
    Input,
    /// A native `<select>`.
    Select,
    /// Press a key: inputs and pointer targets.
    Keyboard,
    /// Every element with a name or text: the last look, for custom
    /// widgets built from plain elements.
    Broad,
    /// Parts of a page that scroll or take a drop: regions, lists,
    /// dialogs, frames, and the like.
    Container,
}

const POINTER_ROLES: &[&str] = &[
    "button",
    "link",
    "checkbox",
    "radio",
    "switch",
    "tab",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "treeitem",
    "combobox",
];
const INPUT_ROLES: &[&str] = &["textbox", "searchbox", "combobox", "spinbutton", "slider"];
const CONTAINER_ROLES: &[&str] = &[
    "dialog",
    "alertdialog",
    "region",
    "main",
    "navigation",
    "complementary",
    "list",
    "listbox",
    "tree",
    "table",
    "grid",
    "treegrid",
    "tabpanel",
    "feed",
    "log",
    "menu",
    "document",
    "iframe",
];
/// Ancestors whose text tells repeated elements apart.
const ITEM_ROLES: &[&str] = &["row", "listitem", "article"];
const TABLE_ROLES: &[&str] = &["table", "grid", "treegrid"];
/// Roles of the choices in a list or menu.
const OPTION_ROLES: &[&str] = &[
    "option",
    "menuitem",
    "menuitemradio",
    "menuitemcheckbox",
    "treeitem",
];
/// Cells of a row, headers included.
const CELL_ROLES: &[&str] = &["cell", "gridcell", "columnheader", "rowheader"];
/// Longest context text sent per candidate, in characters.
const CONTEXT_CHARS: usize = 160;
/// Longest label sent per candidate, in characters.
const LABEL_CHARS: usize = 60;

impl Outline {
    pub(crate) fn parse(snapshot: &str) -> Self {
        let mut nodes: Vec<OutlineNode> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for line in snapshot.lines() {
            let Some(line) = SnapshotLine::parse(line) else {
                continue;
            };
            // Properties such as `/url:` describe their parent.
            if line.role.starts_with('/') {
                continue;
            }
            let mut node = OutlineNode::from_line(line);
            while open
                .last()
                .is_some_and(|&last| nodes[last].depth >= node.depth)
            {
                open.pop();
            }
            node.parent = open.last().copied();
            open.push(nodes.len());
            nodes.push(node);
        }
        Self { nodes }
    }

    pub(crate) fn node(&self, index: usize) -> &OutlineNode {
        &self.nodes[index]
    }

    /// The candidates for a view, in document order.
    pub(crate) fn view(&self, view: View) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&index| {
                let node = &self.nodes[index];
                node.element.is_some() && self.fits(index, view) && !is_root(node)
            })
            .collect()
    }

    fn fits(&self, index: usize, view: View) -> bool {
        let node = &self.nodes[index];
        let role = node.role.as_str();
        match view {
            View::Pointer => POINTER_ROLES.contains(&role) || node.pointer,
            View::Input => INPUT_ROLES.contains(&role),
            View::Select => role == "combobox" && !self.options(index).is_empty(),
            View::Keyboard => self.fits(index, View::Pointer) || self.fits(index, View::Input),
            View::Broad => node.name.is_some() || node.text.is_some() || node.pointer,
            View::Container => CONTAINER_ROLES.contains(&role),
        }
    }

    /// The snapshot's first element: the page's `<body>`, or the scope of a
    /// scoped `ACT`.
    pub(crate) fn root(&self) -> Option<usize> {
        self.nodes
            .first()
            .filter(|node| node.parent.is_none() && node.element.is_some())
            .map(|_| 0)
    }

    /// Whether `index` is `container` or lies inside it.
    pub(crate) fn within(&self, index: usize, container: usize) -> bool {
        index == container || self.ancestors(index).any(|ancestor| ancestor == container)
    }

    /// The elements that may be a list's options, in two tiers: elements
    /// with an option role, then list items and elements the page styles as
    /// clickable. Custom dropdowns often build their options from plain
    /// elements. Each element has a label: a name or text.
    pub(crate) fn option_tiers(&self) -> [Vec<usize>; 2] {
        let labelled = |index: usize| {
            let node = &self.nodes[index];
            node.element.is_some() && !is_root(node) && self.label(index).is_some()
        };
        let roles: Vec<usize> = (0..self.nodes.len())
            .filter(|&index| {
                labelled(index) && OPTION_ROLES.contains(&self.nodes[index].role.as_str())
            })
            .collect();
        let plain: Vec<usize> = (0..self.nodes.len())
            .filter(|&index| {
                let node = &self.nodes[index];
                labelled(index)
                    && !OPTION_ROLES.contains(&node.role.as_str())
                    && (node.pointer || node.role == "listitem")
            })
            .collect();
        [roles, plain]
    }

    /// What an element says: its name, or else its text.
    pub(crate) fn label(&self, index: usize) -> Option<&str> {
        let node = &self.nodes[index];
        node.name.as_deref().or(node.text.as_deref())
    }

    /// The focused element, when it is one a key press can target.
    pub(crate) fn focused(&self) -> Option<usize> {
        (0..self.nodes.len()).find(|&index| {
            let node = &self.nodes[index];
            node.active
                && node.element.is_some()
                && !is_root(node)
                && self.fits(index, View::Keyboard)
        })
    }

    /// The option labels of a native select.
    pub(crate) fn options(&self, index: usize) -> Vec<&str> {
        self.children(index)
            .filter(|&child| self.nodes[child].role == "option")
            .filter_map(|child| self.nodes[child].name.as_deref())
            .collect()
    }

    fn children(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        (index + 1..self.nodes.len()).filter(move |&child| self.nodes[child].parent == Some(index))
    }

    fn ancestors(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        iter::successors(self.nodes[index].parent, |&parent| {
            self.nodes[parent].parent
        })
    }

    /// Every node under `index`, in document order.
    fn subtree(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        let depth = self.nodes[index].depth;
        (index + 1..self.nodes.len()).take_while(move |&next| self.nodes[next].depth > depth)
    }

    /// The visible words of the nodes under `index`, except `skip`'s own.
    fn subtree_text(&self, index: usize, skip: usize) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for next in self.subtree(index) {
            if next == skip {
                continue;
            }
            let node = &self.nodes[next];
            for part in [node.name.as_deref(), node.text.as_deref()]
                .into_iter()
                .flatten()
            {
                if parts.last() != Some(&part) {
                    parts.push(part);
                }
            }
        }
        truncate(&parts.join(" · "), CONTEXT_CHARS)
    }

    /// What Jev reads about one candidate.
    pub(crate) fn describe(&self, index: usize, twins: &HashMap<usize, (usize, usize)>) -> Json {
        let node = &self.nodes[index];
        let mut description = Map::new();
        description.insert("role".to_owned(), json!(node.role));
        if let Some(name) = &node.name {
            description.insert("name".to_owned(), json!(name));
        }
        if let Some(text) = &node.text {
            description.insert("value".to_owned(), json!(text));
        }
        // An element with neither name nor text takes the heading it opens
        // with, or else the text beside it, as an unlabeled input does. An
        // element with its own text would take its neighbour's, as a list
        // item would its sibling's.
        let unnamed = node.name.is_none() && node.text.is_none();
        let opening = if unnamed {
            self.opening_heading(index)
        } else {
            None
        };
        if unnamed
            && let Some(label) = opening
                .map(|heading| truncate(heading, LABEL_CHARS))
                .or_else(|| self.label_before(index))
        {
            description.insert("label".to_owned(), json!(label));
        }
        if let Some((table, column)) = self.table_place(index) {
            if let Some(table) = table {
                description.insert("table".to_owned(), json!(table));
            }
            if let Some(column) = column {
                description.insert("column".to_owned(), json!(column));
            }
        }
        if let Some(item) = self
            .ancestors(index)
            .find(|&ancestor| ITEM_ROLES.contains(&self.nodes[ancestor].role.as_str()))
        {
            let text = self.subtree_text(item, index);
            if !text.is_empty() {
                description.insert(self.nodes[item].role.clone(), json!(text));
            }
        }
        let sections: Vec<&str> = self
            .ancestors(index)
            .filter(|&ancestor| !ITEM_ROLES.contains(&self.nodes[ancestor].role.as_str()))
            .filter_map(|ancestor| self.nodes[ancestor].name.as_deref())
            .take(2)
            .collect();
        if !sections.is_empty() {
            description.insert("within".to_owned(), json!(sections.join(" › ")));
        }
        // The nearest heading: the one the element opens with, else the
        // last one before it. The heading before a region that its own
        // heading names belongs to the section before.
        if let Some(heading) = opening.or_else(|| {
            (0..index)
                .rev()
                .find(|&before| self.nodes[before].role == "heading")
                .and_then(|heading| self.nodes[heading].name.as_deref())
        }) {
            description.insert("heading".to_owned(), json!(heading));
        }
        if let Some((position, count)) = twins.get(&index) {
            description.insert(
                "position".to_owned(),
                json!(format!("{position} of {count}")),
            );
        }
        Json::Object(description)
    }

    /// Where an element inside a table is: the table's name or caption, and
    /// the header of the element's column. Two months of a calendar, or two
    /// price columns, otherwise look the same. `None` outside a table.
    fn table_place(&self, index: usize) -> Option<(Option<String>, Option<String>)> {
        let table = self
            .ancestors(index)
            .find(|&ancestor| TABLE_ROLES.contains(&self.nodes[ancestor].role.as_str()))?;
        let title = self.nodes[table].name.clone().or_else(|| {
            self.children(table)
                .find(|&child| self.nodes[child].role == "caption")
                .and_then(|caption| {
                    let caption = &self.nodes[caption];
                    caption.text.clone().or_else(|| caption.name.clone())
                })
        });
        let column = self.column_header(index, table);
        Some((title, column))
    }

    /// The header of the column that holds `index`, from the first row of
    /// `table` that has column headers.
    fn column_header(&self, index: usize, table: usize) -> Option<String> {
        let is_cell = |node: usize| CELL_ROLES.contains(&self.nodes[node].role.as_str());
        let cell = iter::once(index)
            .chain(self.ancestors(index))
            .take_while(|&node| node != table)
            .find(|&node| is_cell(node))?;
        let row = self.nodes[cell].parent?;
        let column = self
            .children(row)
            .filter(|&child| is_cell(child))
            .position(|child| child == cell)?;
        let header_row = self.subtree(table).find(|&node| {
            self.nodes[node].role == "row"
                && self
                    .children(node)
                    .any(|child| self.nodes[child].role == "columnheader")
        })?;
        let header = self
            .children(header_row)
            .filter(|&child| is_cell(child))
            .nth(column)?;
        self.nodes[header].name.clone()
    }

    /// The text of the sibling just before an unnamed element, which a
    /// page often uses as its label.
    fn label_before(&self, index: usize) -> Option<String> {
        let parent = self.nodes[index].parent;
        let before = (0..index)
            .rev()
            .find(|&sibling| self.nodes[sibling].parent == parent)?;
        let node = &self.nodes[before];
        node.name
            .as_deref()
            .or(node.text.as_deref())
            .map(|text| truncate(text, LABEL_CHARS))
    }

    /// The name of the heading that an element opens with: its first child,
    /// or the first child of a plain wrapper that opens it. Playwright
    /// leaves out the name of an element that its own heading names, as a
    /// region labelled by its heading through `aria-labelledby`, because
    /// the heading shows it.
    fn opening_heading(&self, index: usize) -> Option<&str> {
        let mut first = self.first_child(index)?;
        while is_plain_wrapper(&self.nodes[first]) {
            first = self.first_child(first)?;
        }
        let node = &self.nodes[first];
        if node.role == "heading" {
            node.name.as_deref()
        } else {
            None
        }
    }

    /// A child line follows its parent's line at once.
    fn first_child(&self, index: usize) -> Option<usize> {
        let next = index + 1;
        self.nodes
            .get(next)
            .filter(|node| node.parent == Some(index))
            .map(|_| next)
    }

    /// Whether two elements are copies of one control inside the same item:
    /// the same role and name in the same table row, list item, or article,
    /// or, outside any item, under the same parent. A product card's hover
    /// overlay repeats its "Add to cart", and a row repeats its link for
    /// small screens. Identical buttons in different rows are not copies.
    pub(crate) fn copies_in_one_item(&self, first: usize, second: usize) -> bool {
        let (left, right) = (&self.nodes[first], &self.nodes[second]);
        if left.role != right.role || left.name != right.name {
            return false;
        }
        let item = |index: usize| {
            self.ancestors(index)
                .find(|&ancestor| ITEM_ROLES.contains(&self.nodes[ancestor].role.as_str()))
        };
        match (item(first), item(second)) {
            (Some(left_item), Some(right_item)) => left_item == right_item,
            (None, None) => left.parent == right.parent,
            _ => false,
        }
    }

    /// Each candidate that shares its role and name with others: its
    /// place among them and how many there are.
    pub(crate) fn twins(&self, candidates: &[usize]) -> HashMap<usize, (usize, usize)> {
        let mut groups: HashMap<(&str, Option<&str>), Vec<usize>> = HashMap::new();
        for &index in candidates {
            let node = &self.nodes[index];
            groups
                .entry((node.role.as_str(), node.name.as_deref()))
                .or_default()
                .push(index);
        }
        groups
            .into_values()
            .filter(|group| group.len() > 1)
            .flat_map(|group| {
                let count = group.len();
                group
                    .into_iter()
                    .enumerate()
                    .map(move |(position, index)| (index, (position + 1, count)))
            })
            .collect()
    }
}

/// The page's root: a generic element that holds everything.
fn is_root(node: &OutlineNode) -> bool {
    node.parent.is_none() && node.role == "generic"
}

/// A generic element with no name or text of its own, such as a `div`
/// around a section's heading.
fn is_plain_wrapper(node: &OutlineNode) -> bool {
    node.role == "generic" && node.name.is_none() && node.text.is_none()
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// Words that say little about which element an instruction means.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "be", "by", "for", "from", "i", "in", "into", "is", "it", "me",
    "my", "of", "on", "or", "our", "the", "this", "that", "to", "with", "you", "your",
];

fn tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .filter(|word| !STOP_WORDS.contains(&word.as_str()))
        .collect()
}

/// Keeps the `keep` candidates whose descriptions best match the
/// instruction's words, by BM25, in document order. `None` when no
/// candidate shares a word with the instruction.
pub(crate) fn shortlist(
    instruction: &str,
    candidates: &[usize],
    descriptions: &HashMap<usize, Json>,
    keep: usize,
) -> Option<Vec<usize>> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let query: HashSet<String> = tokens(instruction).into_iter().collect();
    let documents: Vec<Vec<String>> = candidates
        .iter()
        .map(|index| tokens(&flatten(&descriptions[index])))
        .collect();
    let count = documents.len() as f64;
    let average = documents.iter().map(Vec::len).sum::<usize>() as f64 / count.max(1.0);
    let mut scored: Vec<(f64, usize)> = candidates
        .iter()
        .zip(&documents)
        .map(|(&index, words)| {
            let score = query
                .iter()
                .map(|term| {
                    let frequency = words.iter().filter(|word| *word == term).count() as f64;
                    if frequency == 0.0 {
                        return 0.0;
                    }
                    let with_term = documents
                        .iter()
                        .filter(|document| document.contains(term))
                        .count() as f64;
                    let idf = ((count - with_term + 0.5) / (with_term + 0.5) + 1.0).ln();
                    let length = words.len() as f64 / average.max(1.0);
                    idf * frequency * (K1 + 1.0) / (frequency + K1 * (1.0 - B + B * length))
                })
                .sum::<f64>();
            (score, index)
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|left, right| right.0.total_cmp(&left.0));
    let mut kept: Vec<usize> = scored
        .into_iter()
        .take(keep)
        .map(|(_, index)| index)
        .collect();
    kept.sort_unstable();
    Some(kept)
}

/// A description's words, for ranking.
fn flatten(description: &Json) -> String {
    match description {
        Json::Object(map) => map.values().map(flatten).collect::<Vec<_>>().join(" "),
        Json::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    const INVOICES: &str = r#"- generic [active] [ref=e1]:
  - heading "Invoices" [level=1] [ref=e2]
  - table [ref=e3]:
    - rowgroup [ref=e10]:
      - row [ref=e11]:
        - cell "1036" [ref=e12]
        - cell "Massive Dynamic" [ref=e13]
        - cell [ref=e15]:
          - button "Delete" [ref=e16]
      - row [ref=e17]:
        - cell "1037" [ref=e18]
        - cell "Stark" [ref=e19]
        - cell [ref=e21]:
          - button "Delete" [ref=e22] [cursor=pointer]
  - paragraph [ref=e83]: "Result: none"
"#;

    const BOARD: &str = r#"- generic [active] [ref=e1]:
  - heading "Sprint board" [level=1] [ref=e2]
  - main [ref=e3]:
    - region "To do" [ref=e4]:
      - article [ref=e6]: Fix login timeout
    - region "Done" [ref=e11]:
      - heading "Done" [level=2] [ref=e12]
  - iframe [ref=e20]:
    - generic [ref=f1e1]:
      - paragraph [ref=f1e2]: Incident
"#;

    #[test]
    fn containers_are_the_parts_that_scroll_or_take_a_drop() {
        let outline = Outline::parse(BOARD);
        let refs: Vec<&str> = outline
            .view(View::Container)
            .into_iter()
            .map(|index| outline.node(index).element.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(refs, ["e3", "e4", "e11", "e20"]);
        assert_eq!(outline.root(), Some(0));
        assert_eq!(Outline::parse("- heading \"No ref\"\n").root(), None);
    }

    #[test]
    fn a_named_iframe_is_a_container_that_jev_reads_by_name() {
        let outline = Outline::parse(
            "- generic [active] [ref=e1]:\n  - iframe \"Incident history\" [ref=e4]:\n    - generic [ref=f1e1]:\n      - button \"Refresh\" [ref=f1e2]\n",
        );
        let containers = outline.view(View::Container);
        let [frame] = containers.as_slice() else {
            panic!("one container, the iframe: {containers:?}");
        };
        assert_eq!(
            outline.describe(*frame, &HashMap::new()),
            json!({"role": "iframe", "name": "Incident history"})
        );
        let buttons = outline.view(View::Pointer);
        assert_eq!(
            outline.describe(buttons[0], &HashMap::new())["within"],
            json!("Incident history")
        );
    }

    #[test]
    fn a_quoted_line_reads_as_the_unquoted_line_does() {
        // Lines as Playwright 1.62.1 writes them for names that hold `: `
        // or ` #`, and the same lines without the quotes.
        let quoted = Outline::parse(
            r##"- generic [active] [ref=e1]:
  - 'region "Q3: plan" [ref=e2]':
    - 'button "It''s: live" [ref=e3] [cursor=pointer]'
    - 'textbox "Note: x" [active] [ref=e4]': "Status: live"
    - 'link "x #y" [ref=e5]':
      - /url: "#top"
  - 'combobox "Pick: one" [ref=e6]':
    - 'option "It''s: x" [selected]'
    - option "plain"
"##,
        );
        let unquoted = Outline::parse(
            r##"- generic [active] [ref=e1]:
  - region "Q3: plan" [ref=e2]:
    - button "It's: live" [ref=e3] [cursor=pointer]
    - textbox "Note: x" [active] [ref=e4]: "Status: live"
    - link "x #y" [ref=e5]:
      - /url: "#top"
  - combobox "Pick: one" [ref=e6]:
    - option "It's: x" [selected]
    - option "plain"
"##,
        );
        assert_eq!(quoted.nodes, unquoted.nodes);

        let refs = |view| {
            quoted
                .view(view)
                .into_iter()
                .map(|index| quoted.node(index).element.clone().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(refs(View::Pointer), ["e3", "e5", "e6"]);
        assert_eq!(refs(View::Input), ["e4", "e6"]);
        assert_eq!(refs(View::Container), ["e2"]);
        let selects = quoted.view(View::Select);
        assert_eq!(quoted.options(selects[0]), ["It's: x", "plain"]);
        let textbox = quoted.focused().expect("the textbox has focus");
        assert_eq!(
            quoted.describe(textbox, &HashMap::new()),
            json!({"role": "textbox", "name": "Note: x", "value": "Status: live", "within": "Q3: plan"})
        );
    }

    /// The index of the node with `element` as its ref.
    fn index_of(outline: &Outline, element: &str) -> usize {
        (0..outline.nodes.len())
            .find(|&index| outline.node(index).element.as_deref() == Some(element))
            .expect("the element is in the outline")
    }

    fn described(outline: &Outline, element: &str) -> Json {
        outline.describe(index_of(outline, element), &HashMap::new())
    }

    #[test]
    fn a_name_cannot_change_which_ref_a_candidate_carries() {
        // Lines as Playwright 1.62.1 writes them for names and texts that
        // hold `[ref=…]` and other marks.
        let outline = Outline::parse(
            r#"- generic [active] [ref=e1]:
  - heading "Crafted" [level=1] [ref=e2]
  - button "Save" [ref=e3]
  - button "Delete [ref=e9]" [ref=e4]
  - button "Say \"hi\" \\ back ] [ref=e2] [active]" [ref=e5]
  - button "x\" [ref=e1] [cursor=pointer]" [ref=e6]: ignored
  - 'button "Status: [ref=e9] live" [ref=e7]'
  - button "Go [checked] [active] [cursor=pointer]" [ref=e8]
  - paragraph [ref=e9]: see [ref=e3] here
  - textbox "Note [ref=e4]" [ref=e12]: v [ref=e5]
  - generic [ref=e17]:
    - button "Mixed" [ref=e18]
    - text: see [ref=e3] here
"#,
        );
        let candidates = |view| {
            outline
                .view(view)
                .into_iter()
                .map(|index| {
                    let node = outline.node(index);
                    (
                        node.element.clone().unwrap_or_default(),
                        node.name.clone().unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let expected = [
            ("e3", "Save"),
            ("e4", "Delete [ref=e9]"),
            ("e5", r#"Say "hi" \ back ] [ref=e2] [active]"#),
            ("e6", r#"x" [ref=e1] [cursor=pointer]"#),
            ("e7", "Status: [ref=e9] live"),
            ("e8", "Go [checked] [active] [cursor=pointer]"),
            ("e18", "Mixed"),
        ]
        .map(|(element, name)| (element.to_owned(), name.to_owned()));
        assert_eq!(candidates(View::Pointer), expected);
        assert_eq!(candidates(View::Input), [(
            "e12".to_owned(),
            "Note [ref=e4]".to_owned()
        )]);
        // Marks inside a name do not count.
        let go = outline.node(index_of(&outline, "e8"));
        assert!(!go.checked && !go.pointer && !go.active);
        assert_eq!(outline.focused(), None);
        assert_eq!(
            described(&outline, "e4"),
            json!({"role": "button", "name": "Delete [ref=e9]", "heading": "Crafted"})
        );
        assert_eq!(
            described(&outline, "e12"),
            json!({"role": "textbox", "name": "Note [ref=e4]", "value": "v [ref=e5]", "heading": "Crafted"})
        );
    }

    /// The eval page `trip-planner` as Playwright 1.62.1 writes it. Each
    /// day is a region that its own heading names with `aria-labelledby`,
    /// so the snapshot shows the region without a name.
    const TRIP: &str = r##"- generic [active] [ref=e1]:
  - banner [ref=e2]:
    - strong [ref=e3]: Wayfarer
    - navigation "Trips" [ref=e4]:
      - link "My trips" [ref=e5] [cursor=pointer]:
        - /url: "#"
      - link "Saved places" [ref=e6] [cursor=pointer]:
        - /url: "#"
  - main [ref=e7]:
    - heading "Lisbon weekend" [level=1] [ref=e8]
    - paragraph [ref=e9]: Press and hold an activity to move it to another day.
    - generic [ref=e10]:
      - region [ref=e11]:
        - heading "Friday, May 9" [level=2] [ref=e12]
        - list [ref=e13]:
          - listitem [ref=e14]:
            - strong [ref=e15]: Check in at Casa do Rio
            - generic [ref=e16]: 3:00 PM
          - listitem [ref=e17]:
            - strong [ref=e18]: Tram 28 ride
            - generic [ref=e19]: 5:00 PM · 1 hour
          - listitem [ref=e20]:
            - strong [ref=e21]: Dinner in Alfama
            - generic [ref=e22]: 8:30 PM
      - region [ref=e23]:
        - heading "Saturday, May 10" [level=2] [ref=e24]
        - list [ref=e25]:
          - listitem [ref=e26]:
            - strong [ref=e27]: Day trip to Sintra
            - generic [ref=e28]: 9:00 AM · 7 hours
          - listitem [ref=e29]:
            - strong [ref=e30]: Fado show
            - generic [ref=e31]: 9:30 PM
      - region [ref=e32]:
        - heading "Sunday, May 11" [level=2] [ref=e33]
        - list [ref=e34]:
          - listitem [ref=e35]:
            - strong [ref=e36]: Belém Tower
            - generic [ref=e37]: 10:00 AM · 2 hours
    - status
"##;

    #[test]
    fn a_region_that_its_own_heading_names_reads_as_that_heading() {
        let outline = Outline::parse(TRIP);
        for (element, day) in [
            ("e11", "Friday, May 9"),
            ("e23", "Saturday, May 10"),
            ("e32", "Sunday, May 11"),
        ] {
            assert_eq!(
                described(&outline, element),
                json!({"role": "region", "label": day, "heading": day}),
                "{element}"
            );
        }
        assert_eq!(
            described(&outline, "e7"),
            json!({"role": "main", "label": "Lisbon weekend", "heading": "Lisbon weekend"})
        );
    }

    #[test]
    fn elements_inside_a_region_that_its_heading_names_keep_their_descriptions() {
        let outline = Outline::parse(TRIP);
        assert_eq!(
            described(&outline, "e34"),
            json!({"role": "list", "label": "Sunday, May 11", "heading": "Sunday, May 11"})
        );
        assert_eq!(
            described(&outline, "e18"),
            json!({"role": "strong", "value": "Tram 28 ride", "listitem": "5:00 PM · 1 hour", "heading": "Friday, May 9"})
        );
        assert_eq!(
            described(&outline, "e5"),
            json!({"role": "link", "name": "My trips", "within": "Trips"})
        );
        assert_eq!(
            described(&outline, "e4"),
            json!({"role": "navigation", "name": "Trips"})
        );
    }

    #[test]
    fn only_an_unnamed_element_that_opens_with_a_heading_reads_as_it() {
        // Lines as Playwright 1.62.1 writes them. Every region's name
        // comes from a heading inside it; only "Named" has an
        // `aria-label`.
        let outline = Outline::parse(
            r##"- main [ref=e2]:
  - heading "Planner" [level=1] [ref=e3]
  - region "Named" [ref=e13]:
    - heading "Other heading" [level=2] [ref=e14]
    - paragraph [ref=e15]: Body
  - region [ref=e16]:
    - generic [ref=e17]:
      - heading "Deep heading" [level=2] [ref=e18]
      - button "Act" [ref=e19]
    - paragraph [ref=e20]: x
  - link [ref=e21] [cursor=pointer]:
    - /url: "#p"
    - heading "Product A" [level=3] [ref=e22]
    - paragraph [ref=e23]: Fine goods
  - region [ref=e24]:
    - paragraph [ref=e25]: Intro
    - heading "Later heading" [level=2] [ref=e26]
"##,
        );
        assert_eq!(
            described(&outline, "e13"),
            json!({"role": "region", "name": "Named", "heading": "Planner"})
        );
        assert_eq!(
            described(&outline, "e16"),
            json!({"role": "region", "label": "Deep heading", "heading": "Deep heading"})
        );
        assert_eq!(
            described(&outline, "e21"),
            json!({"role": "link", "label": "Product A", "heading": "Product A"})
        );
        assert_eq!(
            described(&outline, "e24"),
            json!({"role": "region", "heading": "Product A"})
        );
        assert_eq!(
            described(&outline, "e19"),
            json!({"role": "button", "name": "Act", "heading": "Deep heading"})
        );
    }

    #[test]
    fn a_name_between_slashes_keeps_its_element() {
        // Playwright 1.62.1 writes a name that starts and ends with `/` as
        // it is, without quotes or escapes.
        let outline = Outline::parse(
            r#"- generic [active] [ref=e1]:
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
  - textbox /field/ [ref=e14]: /v/
"#,
        );
        let buttons: Vec<(String, String)> = outline
            .view(View::Pointer)
            .into_iter()
            .map(|index| {
                let node = outline.node(index);
                (
                    node.element.clone().unwrap_or_default(),
                    node.name.clone().unwrap_or_default(),
                )
            })
            .collect();
        let expected = [
            ("e2", "/api/"),
            ("e3", "/"),
            ("e4", "//"),
            ("e5", r#"/a"b\c/"#),
            ("e6", "/x/ [ref=e9] /"),
            ("e7", "/a: b/"),
            ("e8", "/it's/"),
            ("e9", "/a #b/"),
            ("e10", "/a/b"),
            ("e11", "/docs/"),
        ]
        .map(|(element, name)| (element.to_owned(), name.to_owned()));
        assert_eq!(buttons, expected);
        assert!(outline.node(index_of(&outline, "e11")).pointer);
        assert_eq!(
            described(&outline, "e14"),
            json!({"role": "textbox", "name": "/field/", "value": "/v/", "heading": "/title/"})
        );
    }

    #[test]
    fn values_and_names_decode_every_escape_that_playwright_writes() {
        // Lines as Playwright 1.62.1 writes them. A value gets JSON's
        // escapes and `\xHH` for other control characters; a name gets
        // JSON's `\uXXXX`, except DEL, which stays as it is.
        let outline = Outline::parse(concat!(
            "- generic [active] [ref=e1]:\n",
            r#"  - paragraph [ref=e2]: "a\x7fb""#,
            "\n",
            r#"  - paragraph [ref=e3]: "a\x01b""#,
            "\n",
            r#"  - paragraph [ref=e4]: "a\x85b""#,
            "\n",
            r#"  - paragraph [ref=e5]: "a\x1bb: c""#,
            "\n",
            r#"  - paragraph [ref=e6]: "a\bb""#,
            "\n",
            r#"  - paragraph [ref=e7]: "q \"x\" \\ \x7f: y""#,
            "\n",
            "  - 'button \"a\u{7f}b\" [ref=e8]'\n",
            r#"  - button "a\u0001b" [ref=e9]"#,
            "\n",
            r#"  - textbox "Field" [ref=e10]: "v\x7fw""#,
            "\n",
            r#"  - textbox "p\u0001q" [ref=e12]"#,
            "\n",
            r#"  - button "a\ud800b" [ref=e19]"#,
            "\n",
            r#"  - paragraph [ref=e20]: "Status: live""#,
            "\n",
            r#"  - paragraph [ref=e21]: "\"\\\/\b\f\n\r\té😀""#,
            "\n",
            "  - paragraph [ref=e22]: plain \"text\"\n",
        ));
        let node = |element: &str| outline.node(index_of(&outline, element));
        for (element, value) in [
            ("e2", "a\u{7f}b"),
            ("e3", "a\u{1}b"),
            ("e4", "a\u{85}b"),
            ("e5", "a\u{1b}b: c"),
            ("e6", "a\u{8}b"),
            ("e7", "q \"x\" \\ \u{7f}: y"),
            ("e10", "v\u{7f}w"),
            ("e20", "Status: live"),
            ("e21", "\"\\/\u{8}\u{c}\n\r\t\u{e9}\u{1f600}"),
            ("e22", "plain \"text\""),
        ] {
            assert_eq!(node(element).text.as_deref(), Some(value), "{element}");
        }
        for (element, name) in [
            ("e8", "a\u{7f}b"),
            ("e9", "a\u{1}b"),
            ("e12", "p\u{1}q"),
            ("e19", "a\u{fffd}b"),
        ] {
            assert_eq!(node(element).name.as_deref(), Some(name), "{element}");
        }
    }

    #[test]
    fn an_element_is_within_itself_and_its_ancestors() {
        let outline = Outline::parse(BOARD);
        let index = |element: &str| {
            (0..outline.nodes.len())
                .find(|&index| outline.node(index).element.as_deref() == Some(element))
                .expect("the element is in the board")
        };
        assert!(outline.within(index("e6"), index("e6")));
        assert!(outline.within(index("e6"), index("e4")));
        assert!(outline.within(index("e6"), index("e3")));
        assert!(!outline.within(index("e6"), index("e11")));
        assert!(!outline.within(index("e4"), index("e6")));
    }

    #[test]
    fn nodes_keep_their_role_name_ref_value_and_parent() {
        let outline = Outline::parse(
            "- generic [active] [ref=e1]:\n  - text: Search\n  - textbox \"Search\" [active] [ref=e5]: widgets\n  - /url: /x\n",
        );
        let textbox = outline.node(2);
        assert_eq!(textbox.role, "textbox");
        assert_eq!(textbox.name.as_deref(), Some("Search"));
        assert_eq!(textbox.element.as_deref(), Some("e5"));
        assert_eq!(textbox.text.as_deref(), Some("widgets"));
        assert_eq!(textbox.parent, Some(0));
        assert_eq!(outline.focused(), Some(2));
    }

    #[test]
    fn a_row_tells_identical_buttons_apart() {
        let outline = Outline::parse(INVOICES);
        let buttons = outline.view(View::Pointer);
        assert_eq!(buttons.len(), 2);
        let twins = outline.twins(&buttons);
        assert_eq!(
            outline.describe(buttons[1], &twins),
            json!({"role": "button", "name": "Delete", "row": "1037 · Stark", "heading": "Invoices", "position": "2 of 2"})
        );
        assert!(outline.node(buttons[1]).pointer);
    }

    #[test]
    fn the_broad_view_adds_named_plain_elements() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - generic [ref=f1e3]: Select a country\n  - paragraph [ref=e4]\n  - button \"Go\" [ref=e5]\n",
        );
        let refs = |view| {
            outline
                .view(view)
                .into_iter()
                .map(|index| outline.node(index).element.clone().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        assert_eq!(refs(View::Pointer), ["e5"]);
        assert_eq!(refs(View::Broad), ["f1e3", "e5"]);
    }

    #[test]
    fn copies_count_only_within_one_item() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - listitem [ref=e2]:\n    - button \"Add\" [ref=e3]\n    - button \"Add\" [ref=e4]\n  - listitem [ref=e5]:\n    - button \"Add\" [ref=e6]\n",
        );
        let buttons = outline.view(View::Pointer);
        assert!(outline.copies_in_one_item(buttons[0], buttons[1]));
        assert!(!outline.copies_in_one_item(buttons[1], buttons[2]));
    }

    #[test]
    fn a_table_cell_names_its_caption_and_column() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - table [ref=e2]:\n    - caption [ref=e3]: March 2027\n    - rowgroup [ref=e4]:\n      - row [ref=e5]:\n        - columnheader \"Su\" [ref=e6]\n        - columnheader \"Mo\" [ref=e7]\n    - rowgroup [ref=e8]:\n      - row [ref=e9]:\n        - cell \"13\" [ref=e10]:\n          - button \"13\" [ref=e11]\n        - cell \"14\" [ref=e12]:\n          - button \"14\" [ref=e13]\n",
        );
        let buttons = outline.view(View::Pointer);
        let description = outline.describe(buttons[1], &HashMap::new());
        assert_eq!(description["table"], json!("March 2027"));
        assert_eq!(description["column"], json!("Mo"));
    }

    #[test]
    fn options_come_from_option_roles_then_clickable_plain_elements() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - listbox \"Country\" [ref=e2]:\n    - option \"Portugal\" [ref=e3]\n  - generic [ref=e4] [cursor=pointer]: Canada\n  - list [ref=e5]:\n    - listitem [ref=e6]: Red\n  - generic [ref=e7]: Country\n",
        );
        let refs = |tier: &Vec<usize>| {
            tier.iter()
                .map(|&index| outline.node(index).element.clone().unwrap_or_default())
                .collect::<Vec<_>>()
        };
        let [roles, plain] = outline.option_tiers();
        assert_eq!(refs(&roles), ["e3"]);
        assert_eq!(refs(&plain), ["e4", "e6"]);
    }

    #[test]
    fn a_native_select_lists_its_options() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - combobox \"Size\" [ref=e3]:\n    - option \"Small\" [selected]\n    - option \"Large\"\n",
        );
        let selects = outline.view(View::Select);
        assert_eq!(selects.len(), 1);
        assert_eq!(outline.options(selects[0]), vec!["Small", "Large"]);
    }

    #[test]
    fn an_unnamed_input_takes_the_text_before_it_as_its_label() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - generic [ref=e2]: Last name\n  - textbox [ref=e3]\n",
        );
        let inputs = outline.view(View::Input);
        assert_eq!(
            outline.describe(inputs[0], &HashMap::new()),
            json!({"role": "textbox", "label": "Last name"})
        );
    }

    #[test]
    fn an_element_with_text_takes_no_label_from_its_neighbour() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - list [ref=e2]:\n    - listitem [ref=e3]: Red\n    - listitem [ref=e4]: Blue\n",
        );
        let [_, items] = outline.option_tiers();
        let blue = outline.describe(items[1], &HashMap::new());
        assert_eq!(blue.get("label"), None);
        assert_eq!(blue["value"], json!("Blue"));
    }

    #[test]
    fn named_sections_describe_where_an_element_is() {
        let outline = Outline::parse(
            "- generic [ref=e1]:\n  - group \"Work\" [ref=e2]:\n    - textbox \"Email\" [ref=e3]\n",
        );
        let inputs = outline.view(View::Input);
        assert_eq!(
            outline.describe(inputs[0], &HashMap::new())["within"],
            json!("Work")
        );
    }

    #[test]
    fn the_shortlist_keeps_the_rare_matching_row() {
        let mut snapshot = String::from("- generic [ref=e1]:\n");
        for row in 0..60 {
            write!(
                snapshot,
                "  - row [ref=r{row}]:\n    - cell \"Northwind {row}\"\n    - cell \"Account\"\n    - button \"Delete\" [ref=e{}]\n",
                row + 100
            )
            .expect("writing to a String cannot fail");
        }
        snapshot.push_str(
            "  - row [ref=r99]:\n    - cell \"Acme\"\n    - cell \"Customer\"\n    - button \"Delete\" [ref=e999]\n",
        );
        let outline = Outline::parse(&snapshot);
        let buttons = outline.view(View::Pointer);
        let twins = outline.twins(&buttons);
        let descriptions: HashMap<usize, Json> = buttons
            .iter()
            .map(|&index| (index, outline.describe(index, &twins)))
            .collect();
        let kept = shortlist("delete the Acme account", &buttons, &descriptions, 30)
            .expect("words overlap");
        assert_eq!(kept.len(), 30);
        assert!(
            kept.iter()
                .any(|&index| outline.node(index).element.as_deref() == Some("e999"))
        );
        assert_eq!(shortlist("trash it", &buttons, &descriptions, 30), None);
    }
}
