//! The snapshot as a tree, and the candidates Jev chooses among.
//!
//! Jev cannot read a whole snapshot the way a language model does. It picks
//! from a short list, so each candidate carries the context that tells
//! twins apart: the text of its table row or card, the named sections
//! around it, the nearest heading, and its place among elements that look
//! the same. This follows Stagehand's Jev candidate views
//! (browserbase/stagehand#2952).

use std::collections::{HashMap, HashSet};
use std::iter;

use serde_json::{Map, Value as Json, json};

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
/// Ancestors whose text tells repeated elements apart.
const ITEM_ROLES: &[&str] = &["row", "listitem", "article"];
const TABLE_ROLES: &[&str] = &["table", "grid", "treegrid"];
/// Cells of a row, headers included.
const CELL_ROLES: &[&str] = &["cell", "gridcell", "columnheader", "rowheader"];
/// Longest context text sent per candidate, in characters.
const CONTEXT_CHARS: usize = 160;

impl Outline {
    pub(crate) fn parse(snapshot: &str) -> Self {
        let mut nodes: Vec<OutlineNode> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for line in snapshot.lines() {
            let indent = line.len() - line.trim_start().len();
            let Some(content) = line.trim_start().strip_prefix("- ") else {
                continue;
            };
            // Properties such as `/url:` describe their parent.
            if content.starts_with('/') {
                continue;
            }
            let mut node = parse_node(content);
            node.depth = indent / 2;
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
        }
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
        if node.name.is_none()
            && let Some(before) = self.label_before(index)
        {
            description.insert("label".to_owned(), json!(before));
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
        if let Some(heading) = (0..index)
            .rev()
            .find(|&before| self.nodes[before].role == "heading")
            .and_then(|heading| self.nodes[heading].name.as_deref())
        {
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
            .map(|text| truncate(text, 60))
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

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// Parses a line's content such as `button "Sign in" [ref=e5]` or
/// `textbox "Search" [ref=e5]: widgets`.
fn parse_node(content: &str) -> OutlineNode {
    let mut node = OutlineNode::default();
    let role_len = content.find([' ', ':', '[']).unwrap_or(content.len());
    content[..role_len].clone_into(&mut node.role);
    let mut rest = &content[role_len..];
    if let Some(after) = rest.strip_prefix(' ')
        && after.starts_with('"')
        && let Some((name, left)) = json_string(after)
    {
        node.name = Some(name);
        rest = left;
    }
    while let Some(after) = rest.strip_prefix(" [") {
        let Some(end) = after.find(']') else {
            break;
        };
        match &after[..end] {
            "active" => node.active = true,
            "cursor=pointer" => node.pointer = true,
            "checked" | "checked=true" => node.checked = true,
            attr => {
                if let Some(element) = attr.strip_prefix("ref=") {
                    node.element = Some(element.to_owned());
                }
            }
        }
        rest = &after[end + 1..];
    }
    if let Some(text) = rest.strip_prefix(": ") {
        let text = text.trim();
        node.text = Some(match json_string(text) {
            Some((unquoted, "")) => unquoted,
            _ => text.to_owned(),
        });
    }
    node
}

/// Reads the JSON string at the start of `text`; returns it and the rest.
fn json_string(text: &str) -> Option<(String, &str)> {
    let mut escaped = false;
    for (index, ch) in text.char_indices().skip(1) {
        match ch {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => {
                let value = serde_json::from_str(&text[..=index]).ok()?;
                return Some((value, &text[index + 1..]));
            }
            _ => {}
        }
    }
    None
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
