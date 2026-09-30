//! Runs the grammar of SPEC section 17 against the parser. Both must accept
//! the same files, and find the same entries and steps in them.

use std::path::{Path, PathBuf};
use std::{fs, thread};

use pest::iterators::Pair;
use pest_vm::Vm;

use crate::lang::ast::{CheckStep, File};
use crate::lang::parse::{ParseError, parse_file};

const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

/// Parse errors from the rules of SPEC section 17.1, which the grammar
/// leaves out: the grammar accepts these files.
const RULES_OUTSIDE_THE_GRAMMAR: [&str; 16] = [
    "invalid option value",
    "duplicate or conflicting",
    "invalid regex in Unicode mode",
    "invalid JSONPath",
    "invalid XPath expression",
    "invalid response header name",
    "invalid request header name",
    "invalid or duplicate HTTP header name",
    "invalid date format",
    "unknown encoding label",
    "invalid bytes literal",
    "expected a percent from 0% to 100%",
    "expected an index after",
    "the step timeout is too long",
    "number out of range",
    "recursion limit exceeded",
];

/// One step of an entry, by its first line.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Action(usize),
    Page(usize),
    Check(usize),
}

struct Sample {
    name:   String,
    source: String,
}

fn spec_grammar() -> String {
    let spec = fs::read_to_string(Path::new(REPO).join("SPEC.md")).expect("SPEC.md is readable");
    let section = spec
        .split_once("\n## 17. Grammar\n")
        .expect("SPEC.md has section 17")
        .1;
    let block = section
        .split_once("\n```pest\n")
        .expect("section 17 has a pest block")
        .1;
    block
        .split_once("\n```\n")
        .expect("the pest block ends")
        .0
        .to_owned()
}

fn grammar() -> Vm {
    let grammar = spec_grammar();
    match pest_meta::parse_and_optimize(&grammar) {
        Ok((_, rules)) => Vm::new(rules),
        Err(errors) => {
            let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
            panic!(
                "the SPEC grammar is not valid pest:\n{}",
                messages.join("\n")
            );
        }
    }
}

fn parser_steps(file: &File) -> Vec<Vec<Step>> {
    file.entries
        .iter()
        .map(|entry| {
            let actions = entry.actions.iter().map(|action| action.line);
            let page = entry.page.iter().map(|page| page.line);
            let checks = entry.checks.iter().map(|check| match check {
                CheckStep::Assert(assert) => assert.line,
                CheckStep::Judge(judge) => judge.line,
                CheckStep::Capture(capture) => capture.line,
            });
            actions
                .map(|line| Step::Action(line as usize))
                .chain(page.map(|line| Step::Page(line as usize)))
                .chain(checks.map(|line| Step::Check(line as usize)))
                .collect()
        })
        .collect()
}

fn grammar_steps(file: Pair<'_, &str>) -> Vec<Vec<Step>> {
    file.into_inner()
        .filter(|pair| pair.as_rule().ends_with("_entry"))
        .map(|entry| {
            entry
                .into_inner()
                .filter_map(|pair| {
                    let line = pair.line_col().0;
                    match pair.as_rule() {
                        "mock_action" | "visit_action" | "http_action" | "browser_action" => {
                            Some(Step::Action(line))
                        }
                        "page_line" => Some(Step::Page(line)),
                        "assert_line" | "judge_line" | "capture_line" | "http_assert_line"
                        | "http_capture_line" => Some(Step::Check(line)),
                        _ => None,
                    }
                })
                .collect()
        })
        .collect()
}

fn outside_the_grammar(error: &ParseError) -> bool {
    RULES_OUTSIDE_THE_GRAMMAR
        .iter()
        .any(|message| error.message.contains(message))
}

/// Describes how the grammar and the parser disagree on a sample, if they
/// do.
fn mismatch(grammar: &Vm, sample: &Sample) -> Option<String> {
    let source = sample.source.as_str();
    let parsed = parse_file(Path::new(&sample.name), source);
    match (parsed, grammar.parse("file", source)) {
        (Ok(file), Ok(mut pairs)) => {
            let expected = parser_steps(&file);
            let actual = grammar_steps(pairs.next().expect("a file pair"));
            (expected != actual).then(|| {
                format!(
                    "{}: the entries differ\n  parser:  {expected:?}\n  grammar: {actual:?}",
                    sample.name
                )
            })
        }
        (Err(_), Err(_)) => None,
        (Ok(_), Err(error)) => Some(format!(
            "{}: only the parser accepts it\n{error}",
            sample.name
        )),
        (Err(error), Ok(_)) => (!outside_the_grammar(&error)).then(|| {
            format!(
                "{}: only the grammar accepts it\n{}",
                sample.name,
                error.render()
            )
        }),
    }
}

fn assert_no_mismatches(samples: Vec<Sample>) {
    // The grammar recurses once per level of nested JSON, which needs more
    // than a test thread's stack in a debug build.
    let mismatches = thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let grammar = grammar();
            samples
                .iter()
                .filter_map(|sample| mismatch(&grammar, sample))
                .collect::<Vec<String>>()
        })
        .expect("the grammar thread starts")
        .join()
        .expect("the grammar thread finishes");
    assert!(
        mismatches.is_empty(),
        "{} samples differ:\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

fn repo_path(path: &Path) -> String {
    path.strip_prefix(REPO)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Every file with one of `extensions` under `dir`, skipping build output.
fn files_under(dir: &Path, extensions: &[&str], found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("a readable directory entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if path.is_dir() {
            if !matches!(name, "target" | "node_modules") {
                files_under(&path, extensions, found);
            }
        } else if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extensions.contains(&extension))
        {
            found.push(path);
        }
    }
}

/// The `whirl` code blocks of a Markdown file.
fn markdown_samples(path: &Path, samples: &mut Vec<Sample>) {
    let text = fs::read_to_string(path).expect("a readable Markdown file");
    let mut lines = text.lines().enumerate();
    while let Some((index, line)) = lines.next() {
        let fence_len = line.len() - line.trim_start_matches('`').len();
        if fence_len < 3 || &line[fence_len..] != "whirl" {
            continue;
        }
        let fence = &line[..fence_len];
        let mut source = String::new();
        for (_, inner) in lines.by_ref() {
            if inner == fence {
                break;
            }
            source.push_str(inner);
            source.push('\n');
        }
        samples.push(Sample {
            name: format!("{}:{}", repo_path(path), index + 1),
            source,
        });
    }
}

/// The `.whirl` files and Markdown `whirl` blocks of the repository.
fn repository_samples() -> Vec<Sample> {
    let repo = Path::new(REPO);
    let mut paths = Vec::new();
    for dir in ["examples", "evals", "crates"] {
        files_under(&repo.join(dir), &["whirl", "md"], &mut paths);
    }
    paths.extend(["SPEC.md", "README.md"].map(|name| repo.join(name)));
    paths.sort();
    let mut samples = Vec::new();
    for path in paths {
        if path.extension().is_some_and(|extension| extension == "md") {
            markdown_samples(&path, &mut samples);
        } else {
            samples.push(Sample {
                name:   repo_path(&path),
                source: fs::read_to_string(&path).expect("a readable .whirl file"),
            });
        }
    }
    assert!(samples.len() > 150, "found only {} samples", samples.len());
    samples
}

#[test]
fn the_grammar_matches_the_parser_on_the_repository_samples() {
    assert_no_mismatches(repository_samples());
}

/// Tokens that sit at the edges of the grammar.
const TRICKY_TOKENS: [&str; 35] = [
    ">>",
    "to",
    "@5s",
    "@5",
    "none",
    "\"x\"",
    "x\"y\"",
    "{{x}}",
    "{{",
    "[1]",
    "{\"a\":1}",
    "{",
    "/x/",
    "/x/i",
    "button:*",
    "button:\"n\"",
    "nth:0",
    "nth:-1",
    "frame:f",
    "ai:\"x\"",
    "css:a",
    "label:",
    "text",
    "visible",
    "count",
    "not",
    "==",
    "matches",
    "json:$.a",
    "header:h",
    "file:a",
    "down",
    "#",
    "attr:x",
    "window:x",
];

/// Characters that sit at the edges of the grammar.
const TRICKY_CHARACTERS: [char; 12] = [
    '"', '\\', '{', '}', '#', ':', '\u{a0}', '\r', '/', '>', '[', '@',
];

/// Variants of a source, each with one line of it replaced by one of
/// `changes(line)`.
fn changed(source: &str, changes: impl Fn(&str) -> Vec<String>) -> Vec<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut variants = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        for text in changes(line) {
            if text != *line {
                let mut variant = lines.clone();
                variant[index] = &text;
                variants.push(variant.join("\n") + "\n");
            }
        }
    }
    variants
}

/// Changes that a person makes by mistake, or that move a token across a
/// boundary of the grammar.
fn line_changes(line: &str) -> Vec<String> {
    let tokens: Vec<&str> = line.split(' ').collect();
    let mut changes = vec![
        String::new(),
        format!("{line}\n{line}"),
        format!("{line} @5s"),
        format!("{line} >>"),
        format!("{line} # a comment"),
        format!("\n{line}"),
        line.replacen(' ', "", 1),
        line.replacen(' ', "\t", 1),
        line.replacen(' ', "  ", 1),
        line.replacen(' ', " >> ", 1),
        line.replacen(' ', " \"x\" ", 1),
        line.replacen('"', "", 2),
        line.chars().take(line.chars().count() / 2).collect(),
    ];
    if let Some((last, rest)) = tokens.split_last()
        && !rest.is_empty()
    {
        let rest = rest.join(" ");
        changes.push(format!("{rest} \"{last}\""));
        changes.push(format!("{rest} to {last}"));
        changes.push(format!("{rest} down"));
        changes.push(rest);
    }
    if let [first, second, rest @ ..] = tokens.as_slice() {
        let mut swapped = vec![*second, *first];
        swapped.extend_from_slice(rest);
        changes.push(swapped.join(" "));
    }
    changes
}

/// Each tricky token in place of one of the line's first tokens, or before
/// it.
fn token_changes(line: &str) -> Vec<String> {
    let tokens: Vec<&str> = line.split(' ').collect();
    let mut changes = Vec::new();
    for at in 1..tokens.len().min(5) {
        for token in TRICKY_TOKENS {
            let mut replaced = tokens.clone();
            replaced[at] = token;
            changes.push(replaced.join(" "));
            let mut inserted = tokens.clone();
            inserted.insert(at, token);
            changes.push(inserted.join(" "));
        }
    }
    changes
}

/// Each tricky character in place of a character next to a delimiter, or
/// before it.
fn character_changes(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let delimiter = |at: Option<usize>| {
        at.and_then(|at| chars.get(at))
            .is_some_and(|ch| " \"{}[]:#/@>\\".contains(*ch))
    };
    let mut changes = Vec::new();
    for at in 0..=chars.len() {
        if !delimiter(Some(at)) && !delimiter(at.checked_sub(1)) {
            continue;
        }
        for ch in TRICKY_CHARACTERS {
            if at < chars.len() {
                let mut replaced = chars.clone();
                replaced[at] = ch;
                changes.push(replaced.into_iter().collect());
            }
            let mut inserted = chars.clone();
            inserted.insert(at, ch);
            changes.push(inserted.into_iter().collect());
        }
    }
    changes
}

fn changed_samples(changes: fn(&str) -> Vec<String>) -> Vec<Sample> {
    repository_samples()
        .into_iter()
        .flat_map(|sample| {
            changed(&sample.source, changes)
                .into_iter()
                .enumerate()
                .map(move |(index, source)| Sample {
                    name: format!("{} (change {index})", sample.name),
                    source,
                })
        })
        .collect()
}

#[test]
fn the_grammar_matches_the_parser_on_changed_samples() {
    assert_no_mismatches(changed_samples(line_changes));
}

#[test]
#[ignore = "slow: runs about 480,000 changed samples; the nightly checks run it"]
fn the_grammar_matches_the_parser_on_many_small_changes() {
    let mut samples = changed_samples(token_changes);
    samples.extend(changed_samples(character_changes));
    assert_no_mismatches(samples);
}

#[test]
fn the_grammar_matches_the_parser_on_edge_cases() {
    let samples: Vec<Sample> = EDGE_CASES
        .iter()
        .map(|source| Sample {
            name:   format!("{source:?}"),
            source: (*source).to_owned(),
        })
        .chain([127, 128].into_iter().flat_map(|depth| {
            let arrays = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
            let objects = format!("{}1{}", "{\"a\": ".repeat(depth), "}".repeat(depth));
            [
                Sample {
                    name:   format!("a JSON literal nested {depth} deep"),
                    source: format!("VISIT /\nASSERT eval x == {arrays}\n"),
                },
                Sample {
                    name:   format!("a JSON body nested {depth} deep"),
                    source: format!("HTTP POST /x\n{objects}\n"),
                },
            ]
        }))
        .collect();
    assert_no_mismatches(samples);
}

/// Inputs near the edges of the grammar. Most start with `VISIT /` so that
/// the line after it is the one under test.
const EDGE_CASES: &[&str] = &[
    // Role prefixes and strict prefixes.
    "VISIT /\nCLICK button:\"Sign in\"\n",
    "VISIT /\nCLICK button:Submit >> css:x\n",
    "VISIT /\nCLICK button:~Sign\n",
    "VISIT /\nCLICK button~:Sign\n",
    "VISIT /\nCLICK label~:Email\n",
    "VISIT /\nCLICK button:~*\n",
    "VISIT /\nCLICK button:~*x\n",
    "VISIT /\nCLICK button:~\"*\"\n",
    "VISIT /\nCLICK button:~\"Sign in\"\n",
    "VISIT /\nCLICK button:\"~Sign\"\n",
    "VISIT /\nCLICK button:~~Sign\n",
    "VISIT /\nCLICK button:~\n",
    "VISIT /\nCLICK label:~\"First name\" >> text:~\n",
    "VISIT /\nCLICK text:~~x\n",
    "VISIT /\nCLICK css:~x\n",
    "VISIT /\nCLICK testid:~x\n",
    "VISIT /\nVISIT a:~\"x\"\n",
    "VISIT /\nVISIT a~\"x\"\n",
    "VISIT /\nCLICK dialog:* >> button:OK\n",
    "VISIT /\nCLICK button:\"*\"\n",
    "VISIT /\nCLICK button:*x\n",
    "VISIT /\nCLICK button:\n",
    "VISIT /\nCLICK button:a:b\n",
    "VISIT /\nCLICK alertdialog:* >> alert:*\n",
    "VISIT /\nCLICK buttons:x\n",
    "VISIT /\nCLICK role:button\n",
    "VISIT /\nCLICK generic:x\n",
    "VISIT /\nCLICK testid~:x\n",
    "VISIT /\nCLICK https://example.com\n",
    "VISIT /\nCLICK \"https://example.com\"\n",
    "VISIT /\nCLICK Note:\n",
    "VISIT /\nCLICK \">>\"\n",
    "VISIT /\nCLICK a >> >> b\n",
    "VISIT /\nFILL a >> x\n",
    "VISIT /\nFILL textbox:Email ada@example.com\n",
    "VISIT /\nASSERT button:visible visible\n",
    "VISIT /\nASSERT heading:\"Welcome back\" visible >> x\n",
    "VISIT /\nCAPTURE a: link:text text\n",
    "VISIT /\nSNAPSHOT a region:*\nsnapshot-mask: banner:* >> nth:0\n",
    "VISIT /\nSNAPSHOT a\nsnapshot-mask: ai:x\n",
    // Tokens that join, and values that start with `@`.
    "VISIT /\nCLICK css:a:\"b\"\n",
    "VISIT /\nASSERT response:r json:a:\"b\" == 1\n",
    "VISIT /\nCLICK label:\"a\"b\n",
    "VISIT /\nFILL x @a:\"b\"\n",
    "VISIT /\nFILL x \"@a\" @5s\n",
    "VISIT /\nCLICK css:@x\n",
    "HTTP GET /x\nX-Id: @x\n",
    "HTTP GET /x\nX-Id: \"@x\"\n",
    "[Options]\nuser-agent: \"@bot\"\n\nVISIT /\n",
    // Scopes, DRAG, and SCROLL read left to right.
    "VISIT /\nACT css:a css:b\n",
    "VISIT /\nACT css:form\n",
    "VISIT /\nACT \"css:form\"\n",
    "VISIT /\nACT https://x\n",
    "VISIT /\nJUDGE region:Cart \"the total is right\" @5s\n",
    "VISIT /\nEXTRACT e dialog:* \"the total\"\n",
    "VISIT /\nDRAG to to testid:done\n",
    "VISIT /\nSCROLL down >> x\n",
    "VISIT /\nSCROLL x >> down\n",
    "VISIT /\nSCROLL list:Filters down\n",
    "VISIT /\nPRESS textbox:* Enter\n",
    "VISIT /\nPRESS css:x\n",
    // Windows.
    "VISIT /\nCLICK x\nPOPUP pay\nWINDOW pay\nASSERT window:pay closed @30s\nWINDOW main\n",
    "VISIT /\nTAB main\n",
    "VISIT /\nASSERT tab:pay closed\n",
    "VISIT /\nASSERT window:\"pay\" closed\n",
    // Files and lines.
    "",
    "# a comment\n",
    "\u{feff}VISIT /\n",
    "VISIT /\r\nCLICK x\r\n",
    "VISIT /",
    "VISIT /\n# a comment",
    "VISIT\r/\n",
    "VISIT\u{a0}/\n",
    "VISIT\t/\n",
    "  VISIT /\n  # an indented comment\n\n",
    "VISIT /\n[Asserts]\nurl == x\n",
    "VISIT /\n[Captures]\na: url\n",
    "VISIT /\n[Unknown]\n",
    // Options.
    "[Options]\nbase:https://e.com\n\nVISIT /\n",
    "[Options]\nbase : x\nVISIT /\n",
    "[Options]\nbase: a b\nVISIT /\n",
    "[Options]\nbase: @5s\nVISIT /\n",
    "[Options]\nbase:\"a b\"\nVISIT /\n",
    "[Options]\nbase:\nVISIT /\n",
    "[Options]\nallow-hosts: a b c\nVISIT /\n",
    "[Options]\nallow-hosts:a\nVISIT /\n",
    "[Options]\nallow-hosts:\nVISIT /\n",
    "[Options]\n[Options]\nVISIT /\n",
    "[Options]\nbrowser: chrome\nVISIT /\n",
    "[Options]\nbrowser: {{b}}\nVISIT /\n",
    "[Options]\nunknown: x\nVISIT /\n",
    "[Options]x\nVISIT /\n",
    "[Options] # options\nVISIT /\n",
    "# first\n[Options]\n# second\nstep-timeout: 5s\nVISIT /\n",
    "VISIT /\n[Options]\n",
    "[Options]\nPAGE /\nVISIT /\n",
    "[Options]\nsnapshot-mask: css:a\nsnapshot-mask: css:b\nVISIT /\n",
    "[Options]\nsnapshot-mask: none\nsnapshot-mask: css:a\nVISIT /\n",
    "[Options]\nsnapshot-mask:none\nVISIT /\n",
    "[Options]\nsnapshot-mask: \"none\"\nVISIT /\n",
    "[Options]\nsnapshot-mask: ai:\"x\"\nVISIT /\n",
    "[Options]\nsnapshot-max-diff: @5s\nVISIT /\n",
    "[Options]\nsnapshot-max-diff:10%\nVISIT /\n",
    "[Options]\nsnapshot-max-diff: 101%\nVISIT /\n",
    "[Options]\nsnapshot-pixel-threshold: 1 2\nVISIT /\n",
    // Entries.
    "MOCK GET /a 204\nVISIT /\n",
    "MOCK GET /a 204\nCLICK Go\n",
    "HTTP GET /x\nASSERT status == 200\n",
    "HTTP GET /x\nCLICK x\n",
    "HTTP GET /x\nVISIT /\nCLICK x\nHTTP GET /y\nMOCK GET /z 200\nCLICK q\nASSERT url == x\n",
    "HTTP GET /x\nMOCK GET /z 200\nVISIT /\n",
    "MOCK GET /x 200\n",
    "MOCK GET /x 200\nASSERT url == x\nVISIT /\n",
    "MOCK GET /x 200\nPAGE /x\n",
    "HTTP GET /x\nMOCK GET /x 200\n",
    "VISIT /\nPAGE /\nPAGE /\n",
    "VISIT /\nASSERT url == x\nPAGE /\n",
    "VISIT /\nPAGE /\nCLICK x\nASSERT url == x\nCLICK y\n",
    "HTTP GET /x\nPAGE /\n",
    "HTTP GET /x\nJUDGE \"x\"\n",
    "ASSERT url == x\n",
    "CLICK x\n",
    // Tokens and values.
    "VISIT \"a\\u{41}\"\n",
    "VISIT \"a\\u{0000041}\"\n",
    "VISIT \"\\u{D800}\"\n",
    "VISIT \"\\u{110000}\"\n",
    "VISIT \"\\u{10FFFF}\"\n",
    "VISIT \"\\u{FFFF}\"\n",
    "VISIT \"\\u{}\"\n",
    "VISIT \"\\u{0}\"\n",
    "VISIT \"\\x\"\n",
    "VISIT \"\\{\"\n",
    "VISIT \"abc\n",
    "VISIT a\"b\"c\n",
    "VISIT {{x}}\n",
    "VISIT {{ x }}\n",
    "VISIT \"{{ x }}\"\n",
    "VISIT \\{{x}}\n",
    "VISIT {{env.X}}\n",
    "VISIT {{env.}}\n",
    "VISIT {{env}}\n",
    "VISIT {{setup.a}}\n",
    "VISIT {{a}b}}\n",
    "VISIT {{a}}}\n",
    "VISIT a{{b\n",
    "VISIT \"a{{b\" c}}\n",
    "VISIT a#b\n",
    "VISIT \"a#b\"\n",
    "VISIT a\\b\n",
    // Step timeouts.
    "VISIT @5s\n",
    "VISIT @5s @5s\n",
    "VISIT \"@5s\"\n",
    "VISIT / @5ms\n",
    "VISIT / @5m\n",
    "VISIT / @5s x\n",
    "VISIT / @010s\n",
    "VISIT / @5s # a comment\n",
    "VISIT / @5s\"x\"\n",
    // Locators.
    "VISIT /\nCLICK >>\n",
    "VISIT /\nCLICK >> >> x\n",
    "VISIT /\nCLICK a >>\n",
    "VISIT /\nCLICK css:a>>css:b\n",
    "VISIT /\nCLICK css:a >> css:b\n",
    "VISIT /\nCLICK button:nth:0\n",
    "VISIT /\nCLICK button:Submit >> css:x\n",
    "VISIT /\nCLICK button:* >> css:x\n",
    "VISIT /\nCLICK role:button\"x\"\n",
    "VISIT /\nCLICK role:{{x}}\n",
    "VISIT /\nCLICK button:~x\n",
    "VISIT /\nCLICK role:button.x\n",
    "VISIT /\nCLICK nth:0\n",
    "VISIT /\nCLICK css:a >> nth:0 >> nth:-1\n",
    "VISIT /\nCLICK css:a >> nth:x\n",
    "VISIT /\nCLICK css:a >> nth:01\n",
    "VISIT /\nCLICK css:a >> nth:\"0\"\n",
    "VISIT /\nCLICK frame:f\n",
    "VISIT /\nCLICK frame:f >> nth:0\n",
    "VISIT /\nCLICK frame:f >> css:a\n",
    "VISIT /\nCLICK css:a >> frame:f >> nth:1 >> frame:g >> css:b\n",
    "VISIT /\nCLICK ai:\"x\" >> nth:0\n",
    "VISIT /\nCLICK css:a >> ai:\"x\"\n",
    "VISIT /\nCLICK label:\n",
    "VISIT /\nCLICK label:\"\"\n",
    "VISIT /\nCLICK label:~x\n",
    "VISIT /\nCLICK testid~:x\n",
    "VISIT /\nCLICK text:a\"b c\"\n",
    "VISIT /\nCLICK \"text:a\"\n",
    "VISIT /\nCLICK @5s\n",
    "VISIT /\nCLICK x @5s\n",
    "VISIT /\nCLICK css:a css:b\n",
    "VISIT /\nCLICK button:to\n",
    // Actions.
    "VISIT /\nFILL x\n",
    "VISIT /\nFILL x y\n",
    "VISIT /\nFILL textbox:* x\n",
    "VISIT /\nFILL textbox:Email x\n",
    "VISIT /\nFILL textbox:Email x @5s\n",
    "VISIT /\nFILL x @5s\n",
    "VISIT /\nFILL a >> b v\n",
    "VISIT /\nFILL a >>\n",
    "VISIT /\nFILL frame:x v\n",
    "VISIT /\nFILL x y z\n",
    "VISIT /\nFILL @5s x\n",
    "VISIT /\nPRESS Enter\n",
    "VISIT /\nPRESS Enter @5s\n",
    "VISIT /\nPRESS textbox:* Enter\n",
    "VISIT /\nPRESS a b c\n",
    "VISIT /\nPRESS\n",
    "VISIT /\nPRESS @5s\n",
    "VISIT /\nDRAG a to b\n",
    "VISIT /\nDRAG to to b\n",
    "VISIT /\nDRAG a to to b\n",
    "VISIT /\nDRAG \"to\" to b\n",
    "VISIT /\nDRAG listitem:* to css:x\n",
    "VISIT /\nDRAG a to\n",
    "VISIT /\nDRAG a to @5s\n",
    "VISIT /\nDRAG a b\n",
    "VISIT /\nDRAG a >> b to c >> d @5s\n",
    "VISIT /\nSCROLL\n",
    "VISIT /\nSCROLL down\n",
    "VISIT /\nSCROLL x down\n",
    "VISIT /\nSCROLL \"down\"\n",
    "VISIT /\nSCROLL down x\n",
    "VISIT /\nSCROLL list:* down\n",
    "VISIT /\nSCROLL list:Filters down\n",
    "VISIT /\nSCROLL to 50%\n",
    "VISIT /\nSCROLL x to 50%\n",
    "VISIT /\nSCROLL x to 150%\n",
    "VISIT /\nSCROLL x to 50.%\n",
    "VISIT /\nSCROLL x to 33.5%\n",
    "VISIT /\nSCROLL to\n",
    "VISIT /\nSCROLL x to\n",
    "VISIT /\nSCROLL down @5s\n",
    "VISIT /\nSCROLL button:up >> css:x\n",
    "VISIT /\nSCROLL a down >> b\n",
    "VISIT /\nSCROLL x\n",
    "VISIT /\nSCROLL to 50% x\n",
    "VISIT /\nSCROLL x to 100.000000000000007%\n",
    "VISIT /\nUPLOAD x file:a\n",
    "VISIT /\nUPLOAD x \"file:a\"\n",
    "VISIT /\nUPLOAD file:a\n",
    "VISIT /\nUPLOAD x file:\n",
    "VISIT /\nUPLOAD x file:\"a b\"\n",
    "VISIT /\nUPLOAD button:* file:a\n",
    "VISIT /\nDROP x file:a b\n",
    "VISIT /\nSTORE local k v\n",
    "VISIT /\nSTORE local @5s v\n",
    "VISIT /\nSTORE local k\n",
    "VISIT /\nSTORE other k v\n",
    "VISIT /\nSTORE local k v w\n",
    "VISIT /\nPOPUP a-b\n",
    "VISIT /\nPOPUP 1a\n",
    "VISIT /\nPOPUP \"a\"\n",
    "VISIT /\nSCREENSHOT a b\n",
    "VISIT /\nWINDOW main @5s\n",
    "VISIT /\nRESPONSE r GET /x\n",
    "VISIT /\nRESPONSE r get /x\n",
    "VISIT /\nRESPONSE r GET\n",
    "VISIT /\nEVAL \"x\"\n",
    "VISIT /\nGOAL \"x\" @2m\n",
    "VISIT /\nSNAPSHOT a\nsnapshot-max-diff: 1\n",
    "VISIT /\nSNAPSHOT a\nsnapshot-max-diff: 1\nsnapshot-max-diff: 2\n",
    "VISIT /\nSNAPSHOT a css:x @5s\n",
    "VISIT /\nSNAPSHOT a x\n",
    "VISIT /\nSNAPSHOT a\nsnapshot-mask: none\n",
    "VISIT /\nSNAPSHOT a\nsnapshot-mask: none css:a\n",
    "VISIT /\nSNAPSHOT a\nsnapshot-mask: \"none\"\n",
    "VISIT /\nSNAPSHOT a\n\n# a comment\nsnapshot-mask: css:a\nCLICK x\n",
    "VISIT /\nSNAPSHOT a\nsnapshot-mask: css:a @5s\n",
    "VISIT /\nSNAPSHOT a\nfoo: x\n",
    "VISIT /\nSNAPSHOT a\nASSERT url == x\nsnapshot-mask: css:a\n",
    "VISIT /\nSNAPSHOT a\nPAGE /\nsnapshot-mask: css:a\n",
    "VISIT /\nSCREENSHOT a\nsnapshot-mask: css:a\n",
    "VISIT /\nACT \"x\"\n",
    "VISIT /\nACT css:form \"x\"\n",
    "VISIT /\nACT form \"x\"\n",
    "VISIT /\nACT css:form\n",
    "VISIT /\nACT css:a css:b\n",
    "VISIT /\nACT dialog:x y\n",
    "VISIT /\nACT\n",
    "VISIT /\nJUDGE \"x\"\n",
    "VISIT /\nJUDGE css:a \"x\" @5s\n",
    "VISIT /\nEXTRACT e \"x\"\n",
    "VISIT /\nEXTRACT e\n",
    "VISIT /\nEXTRACT e css:a \"x\"\n{\"type\": \"string\"}\n",
    "VISIT /\nEXTRACT e \"x\"\n\n# a comment\n{\n  \"type\": \"object\"\n}\nASSERT extract:e json:$.a == 1\n",
    "VISIT /\nEXTRACT e \"x\"\n{\"a\": \"{{x}}\"}\n",
    "VISIT /\nEXTRACT e \"x\"\n[1]\n",
    "VISIT /\nEXTRACT e \"x\"\n{\"a\": 1} # a comment\n",
    "VISIT /\nEXTRACT e \"x\"\n{\"a\": 1}  \nCLICK x\n",
    "VISIT /\nEXTRACT e \"x\"\n{\"a\": \"\\u{41}\"}\n",
    "VISIT /\nEXTRACT e css:a >> # a comment\n",
    "VISIT /\nCLICK a >> # a comment\n",
    // HTTP requests and mocks.
    "HTTP GET /x\nAccept:application/json\n",
    "HTTP GET /x\nAccept: a\n\nX-Other: b\n",
    "HTTP GET /x\nAccept: a\n# a note\nX-Other: b\n",
    "HTTP GET /x\nAccept: a # a note\n",
    "HTTP POST /x\nContent-Type: application/json\n\n{\"a\": 1}\n",
    "HTTP POST /x\n{\"a\": {{x}}}\n",
    "HTTP POST /x\n{\"a\": \"{{x}}\"}\n",
    "HTTP POST /x\n{\"a\": \"\\{{x}}\"}\n",
    "HTTP POST /x\n{\"a\": \"\\{x\"}\n",
    "HTTP POST /x\n{\"a\": {{ x }}}\n",
    "HTTP POST /x\n{{x}}\n",
    "HTTP POST /x\n[1, 2]\n",
    "HTTP POST /x\n[\n  1\n]\nASSERT status == 200\n",
    "MOCK GET /x 200\n[\"a\"]\nVISIT /\n",
    "HTTP POST /x\n{\"a\": 1} {\"b\": 2}\n",
    "HTTP POST /x\n{\"a\": 1}\n{\"b\": 2}\n",
    "HTTP POST /x\n{\"a\": 1}\nX: y\n",
    "HTTP POST /x\n{\n  \"a\": [1, 2],\n\n  \"b\": \"#\"\n}\nASSERT status == 200\n",
    "HTTP POST /x\n{\"a\": \"\\ud83d\\ude00\"}\n",
    "HTTP POST /x\n{\"a\": \"\\ud83d\"}\n",
    "HTTP POST /x\n{\"a\": \"\\ude00\"}\n",
    "HTTP POST /x\n{\"a\": 1e400}\n",
    "HTTP POST /x\n{\"a\": 01}\n",
    "HTTP POST /x\n{\"a\": \"tab\there\"}\n",
    "HTTP POST /x\n```\na,b\n```\n",
    "HTTP POST /x\n```  \na\n```\n",
    "HTTP POST /x\n  ```\n# not a comment\n\n{{x}}\n  ```  \nASSERT status == 200\n",
    "HTTP POST /x\n```\n{{ bad }}\n```\n",
    "HTTP POST /x\n```\na\n",
    "HTTP POST /x\n```\n```\n",
    "HTTP GET /x\nAccept: a\nAccept: b\n",
    "HTTP GET /x\nx-a: a\nX-A: b\n",
    "HTTP GET /x\nX: a b\n",
    "HTTP GET /x\nX:\n",
    "HTTP GET /x\nX: @5s\n",
    "HTTP GET /x @5s\nX: a\n",
    "HTTP GET /x\nX-a:b: c\n",
    "HTTP GET\n",
    "HTTP get /x\n",
    "MOCK GET /x 200\nVISIT /\n",
    "MOCK GET /x 200 @5s\nVISIT /\n",
    "MOCK GET /x 199\nVISIT /\n",
    "MOCK GET /x 600\nVISIT /\n",
    "MOCK GET /x failed\nX: y\nVISIT /\n",
    "MOCK GET /x failed\nVISIT /\n",
    "MOCK GET @5s 200\nVISIT /\n",
    "MOCK GET /x 200\nX: y\n{\"a\": 1}\nVISIT /\n",
    "VISIT /\nMOCK GET /x 200\nCLICK x\n",
    // PAGE.
    "VISIT /\nPAGE matches\n",
    "VISIT /\nPAGE \"matches\"\n",
    "VISIT /\nPAGE matches /x/ @5s\n",
    "VISIT /\nPAGE @5s\n",
    "VISIT /\nPAGE /x @5s\n",
    // ASSERT.
    "VISIT /\nASSERT url == x\n",
    "VISIT /\nASSERT url==x\n",
    "VISIT /\nASSERT css:a text==\"x\"\n",
    "VISIT /\nASSERT url == @5s\n",
    "VISIT /\nASSERT url == @5s @5s\n",
    "VISIT /\nASSERT url == x @5s @5s\n",
    "VISIT /\nASSERT window:a closed\n",
    "VISIT /\nASSERT tab:a open\n",
    "VISIT /\nASSERT tab:\"a\" closed\n",
    "VISIT /\nASSERT css:a visible\n",
    "VISIT /\nASSERT css:a visible == x\n",
    "VISIT /\nASSERT css:a not visible\n",
    "VISIT /\nASSERT button:* visible visible\n",
    "VISIT /\nASSERT button:\"visible\" visible\n",
    "VISIT /\nASSERT button:@5s visible\n",
    "VISIT /\nASSERT button:* @5s\n",
    "VISIT /\nASSERT button:* text text == x\n",
    "VISIT /\nASSERT css:a attr:aria-x == y\n",
    "VISIT /\nASSERT css:a attr:9x == y\n",
    "VISIT /\nASSERT css:a attr:\"x\" == y\n",
    "VISIT /\nASSERT x text == y\n",
    "VISIT /\nASSERT title == x\n",
    "VISIT /\nASSERT title:x visible\n",
    "VISIT /\nASSERT eval \"x\" == 1\n",
    "VISIT /\nASSERT eval @5s\n",
    "VISIT /\nASSERT response:r status == 200\n",
    "VISIT /\nASSERT response:r header:content-type == x\n",
    "VISIT /\nASSERT response:r header:\"a b\" == x\n",
    "VISIT /\nASSERT response:r header:{{h}} == x\n",
    "VISIT /\nASSERT response:\"r\" status == 200\n",
    "VISIT /\nASSERT request:r method == GET\n",
    "VISIT /\nASSERT request:r json:$.a == 1\n",
    "VISIT /\nASSERT request:r json:$[\"a\"] == 1\n",
    "VISIT /\nASSERT request:r json:\"$['a b']\" == 1\n",
    "VISIT /\nASSERT request:r json:$[ == 1\n",
    "VISIT /\nASSERT extract:e json:$.a == 1\n",
    "VISIT /\nASSERT url split\n",
    "VISIT /\nASSERT url split , nth 0 == x\n",
    "VISIT /\nASSERT url split , nth x == y\n",
    "VISIT /\nASSERT url regex /a(b)/ == b\n",
    "VISIT /\nASSERT url regex /a/x == b\n",
    "VISIT /\nASSERT url regex /a/ii == b\n",
    "VISIT /\nASSERT url replace a b replaceRegex /c/ d == x\n",
    "VISIT /\nASSERT url matches /a b#c/i\n",
    "VISIT /\nASSERT url matches /a\\/b/\n",
    "VISIT /\nASSERT url matches /a\n",
    "VISIT /\nASSERT url matches\n",
    "VISIT /\nASSERT url matches /\\-/\n",
    "VISIT /\nASSERT url matches /a/\"x\"\n",
    "VISIT /\nASSERT url not == x\n",
    "VISIT /\nASSERT url not not == x\n",
    "VISIT /\nASSERT url not\n",
    "VISIT /\nASSERT url exists\n",
    "VISIT /\nASSERT url isIpv4\n",
    "VISIT /\nASSERT url >= 1\n",
    "VISIT /\nASSERT url => 1\n",
    "VISIT /\nASSERT eval \"x\" == [1, 2]\n",
    "VISIT /\nASSERT eval \"x\" == [1, 2]x\n",
    "VISIT /\nASSERT eval \"x\" == [1,\n",
    "VISIT /\nASSERT eval \"x\" == {\"a\": {{v}}}\n",
    "VISIT /\nASSERT eval \"x\" == {{v}}\n",
    "VISIT /\nASSERT eval \"x\" == {x}\n",
    "VISIT /\nASSERT eval \"x\" == [1] @5s\n",
    "VISIT /\nASSERT eval \"x\" == [1]#c\n",
    "VISIT /\nASSERT eval \"x\" == [\"a # b\"]\n",
    "VISIT /\nASSERT eval \"x\" == hex,zz;\n",
    "VISIT /\nASSERT url == hex,zz;\n",
    "VISIT /\nASSERT url toDate \"%Q\" == x\n",
    "VISIT /\nASSERT url charsetDecode nope == x\n",
    // CAPTURE.
    "VISIT /\nCAPTURE a: url\n",
    "VISIT /\nCAPTURE a:url\n",
    "VISIT /\nCAPTURE a : url\n",
    "VISIT /\nCAPTURE a:\n",
    "VISIT /\nCAPTURE café: url\n",
    "VISIT /\nCAPTURE 1a: url\n",
    "VISIT /\nCAPTURE a: css:a text @5s\n",
    "VISIT /\nCAPTURE a: css:a visible\n",
    "VISIT /\nCAPTURE a: button:visible text\n",
    "VISIT /\nCAPTURE a: url @5s x\n",
    "VISIT /\nCAPTURE a:\"x\"\n",
    "VISIT /\nCAPTURE a:testid:x text\n",
    // Checks in an HTTP entry.
    "HTTP GET /x\nASSERT status == 200\nCAPTURE a: json:$.a\nASSERT header:x == y\n",
    "HTTP GET /x\nASSERT url == x\n",
    "HTTP GET /x\nASSERT window:a closed\n",
    "HTTP GET /x\nASSERT status\"x\" == 1\n",
    "HTTP GET /x\nCAPTURE a:status\n",
];
