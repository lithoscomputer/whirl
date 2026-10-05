//! XPath 1.0 over HTML and XML for the `xpath:` filter of Whirl checks
//! (SPEC 9.5).
//!
//! This is the one Whirl crate with `unsafe` code (ADR
//! `evaluate-checks-in-rust` §1.6). It calls libxml2 through the `libxml`
//! crate's bindings, because that crate's safe API cannot read the type of
//! an XPath result or keep libxml2's errors off stderr. Every libxml2 object
//! lives inside one call of [`evaluate`] or [`validate`], so calls on
//! different threads share nothing.

#![allow(
    unsafe_code,
    reason = "calls libxml2's C API (ADR evaluate-checks-in-rust §1.6)"
)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_int, c_void};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};

use libxml::bindings as sys;

/// How [`evaluate`] parses its input (SPEC 9.5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Markup {
    Html,
    Xml,
}

/// The result of an XPath 1.0 expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// The number of selected nodes.
    NodeSet(usize),
    Boolean(bool),
    Number(f64),
    String(String),
}

/// Why an expression gave no result.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    /// The input is not well-formed XML; carries libxml2's first error.
    #[error("the input is not well-formed XML: {0}")]
    Xml(String),
    /// libxml2 built no HTML document, as for an empty input.
    #[error("the input holds no HTML document")]
    Html,
    /// The expression is invalid or cannot be evaluated; carries
    /// libxml2's message, such as `Invalid expression`.
    #[error("{0}")]
    Expression(String),
    /// The input is larger than libxml2's C API accepts.
    #[error("the input is too large")]
    TooLarge,
    /// The expression gave a result type outside XPath 1.0.
    #[error("the expression gives an unsupported result type")]
    Unsupported,
    #[error("libxml2 ran out of memory")]
    OutOfMemory,
}

/// Parses `input` as HTML or XML and evaluates `expression` on it. The
/// namespaces declared on the root element are available by their
/// prefixes, and the default namespace as `_`, as in Hurl.
pub fn evaluate(input: &str, markup: Markup, expression: &str) -> Result<Output, Error> {
    let expression = c_string(expression)?;
    let document = Document::parse(input, markup)?;
    let context = Context::new(Some(&document))?;
    context.register_root_namespaces(&document);
    context.evaluate(&expression)
}

/// Checks an expression before a run. Compiling finds syntax errors. A trial
/// evaluation on an empty document finds unknown functions and variables and
/// wrong argument counts, which libxml2 reports only when it evaluates. A
/// namespace prefix passes, because it depends on the real document.
pub fn validate(expression: &str) -> Result<(), Error> {
    let expression = c_string(expression)?;
    Context::new(None)?.compile(&expression)?;
    let probe = Document::parse("<r/>", Markup::Xml)?;
    let context = Context::new(Some(&probe))?;
    match context.evaluate(&expression) {
        Err(Error::Expression(_)) if context.errors.code() == Some(UNDEFINED_PREFIX) => Ok(()),
        result => result.map(drop),
    }
}

/// libxml2's code for an unregistered namespace prefix.
const UNDEFINED_PREFIX: c_int = sys::xmlParserErrors_XML_XPATH_UNDEF_PREFIX_ERROR.cast_signed();

/// Whirl decodes every input to UTF-8 first, so libxml2 must ignore any
/// declared encoding.
const UTF8: &CStr = c"UTF-8";

const HTML_OPTIONS: c_int = (sys::htmlParserOption_HTML_PARSE_RECOVER
    | sys::htmlParserOption_HTML_PARSE_NOERROR
    | sys::htmlParserOption_HTML_PARSE_NOWARNING
    | sys::htmlParserOption_HTML_PARSE_NONET
    | sys::htmlParserOption_HTML_PARSE_IGNORE_ENC)
    .cast_signed();

/// Strict XML with no network access and no external entities. Errors go
/// to the handler, which keeps the first one for the message.
const XML_OPTIONS: c_int = (sys::xmlParserOption_XML_PARSE_NONET
    | sys::xmlParserOption_XML_PARSE_NO_XXE
    | sys::xmlParserOption_XML_PARSE_IGNORE_ENC)
    .cast_signed();

fn c_string(expression: &str) -> Result<CString, Error> {
    CString::new(expression)
        .map_err(|_| Error::Expression("the expression holds a NUL character".to_owned()))
}

/// The first error that libxml2 reported to [`record_error`].
#[derive(Default)]
struct ErrorSlot(RefCell<Option<(c_int, String)>>);

impl ErrorSlot {
    /// The pointer that libxml2 passes back to [`record_error`].
    fn as_user_data(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast()
    }

    fn code(&self) -> Option<c_int> {
        self.0.borrow().as_ref().map(|(code, _)| *code)
    }

    fn message(&self) -> Option<String> {
        self.0.borrow().as_ref().map(|(_, message)| message.clone())
    }
}

/// Keeps the first error that libxml2 reports, instead of the default
/// handler that writes it to stderr.
unsafe extern "C" fn record_error(data: *mut c_void, error: *const sys::xmlError) {
    // SAFETY: libxml2 passes back the `as_user_data` pointer of an
    // `ErrorSlot` that outlives the object it was registered on, and an
    // error that is valid for the duration of this call.
    let (slot, error) = unsafe { (&*data.cast::<ErrorSlot>(), error.as_ref()) };
    let Some(error) = error else {
        return;
    };
    if error.level < sys::xmlErrorLevel_XML_ERR_ERROR {
        return;
    }
    let message = if error.message.is_null() {
        String::new()
    } else {
        // SAFETY: a non-null message is a NUL-terminated string owned by
        // `error`.
        unsafe { CStr::from_ptr(error.message) }
            .to_string_lossy()
            .trim()
            .to_owned()
    };
    let Ok(mut first) = slot.0.try_borrow_mut() else {
        return;
    };
    first.get_or_insert((error.code, message));
}

/// A parsed document.
struct Document {
    raw: NonNull<sys::xmlDoc>,
}

impl Document {
    fn parse(input: &str, markup: Markup) -> Result<Self, Error> {
        let size = c_int::try_from(input.len()).map_err(|_| Error::TooLarge)?;
        let errors = ErrorSlot::default();
        let parser = Parser::new(markup)?;
        // SAFETY: `parser` is a live parser context, and `errors` outlives
        // it.
        unsafe {
            sys::xmlCtxtSetErrorHandler(
                parser.raw.as_ptr(),
                Some(record_error),
                errors.as_user_data(),
            );
        }
        let buffer = input.as_ptr().cast();
        let raw = match markup {
            // SAFETY: `buffer` holds `size` bytes, and the encoding name is
            // NUL-terminated. The returned document is owned by the caller.
            Markup::Html => unsafe {
                sys::htmlCtxtReadMemory(
                    parser.raw.as_ptr(),
                    buffer,
                    size,
                    ptr::null(),
                    UTF8.as_ptr(),
                    HTML_OPTIONS,
                )
            },
            // SAFETY: as for HTML.
            Markup::Xml => unsafe {
                sys::xmlCtxtReadMemory(
                    parser.raw.as_ptr(),
                    buffer,
                    size,
                    ptr::null(),
                    UTF8.as_ptr(),
                    XML_OPTIONS,
                )
            },
        };
        drop(parser);
        NonNull::new(raw)
            .map(|raw| Self { raw })
            .ok_or_else(|| match markup {
                Markup::Html => Error::Html,
                Markup::Xml => Error::Xml(
                    errors
                        .message()
                        .unwrap_or_else(|| "libxml2 gave no reason".to_owned()),
                ),
            })
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        // SAFETY: the document is owned here and freed once.
        unsafe { sys::xmlFreeDoc(self.raw.as_ptr()) };
    }
}

/// A parser context, freed when dropped.
struct Parser {
    raw:    NonNull<sys::xmlParserCtxt>,
    markup: Markup,
}

impl Parser {
    fn new(markup: Markup) -> Result<Self, Error> {
        let raw = match markup {
            // SAFETY: allocates a new parser context owned by the caller.
            Markup::Html => unsafe { sys::htmlNewParserCtxt() },
            // SAFETY: as for HTML.
            Markup::Xml => unsafe { sys::xmlNewParserCtxt() },
        };
        NonNull::new(raw)
            .map(|raw| Self { raw, markup })
            .ok_or(Error::OutOfMemory)
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        match self.markup {
            // SAFETY: the context is owned here and freed once. A document
            // it built stays valid.
            Markup::Html => unsafe { sys::htmlFreeParserCtxt(self.raw.as_ptr()) },
            // SAFETY: as for HTML.
            Markup::Xml => unsafe { sys::xmlFreeParserCtxt(self.raw.as_ptr()) },
        }
    }
}

/// An XPath context on a document, or on no document for compiling.
struct Context<'doc> {
    raw:       NonNull<sys::xmlXPathContext>,
    /// Boxed so its address stays fixed while libxml2 holds it.
    errors:    Box<ErrorSlot>,
    _document: PhantomData<&'doc Document>,
}

impl<'doc> Context<'doc> {
    fn new(document: Option<&'doc Document>) -> Result<Self, Error> {
        let doc = document.map_or(ptr::null_mut(), |document| document.raw.as_ptr());
        // SAFETY: `doc` is null or a live document that outlives the
        // context through `'doc`.
        let raw =
            NonNull::new(unsafe { sys::xmlXPathNewContext(doc) }).ok_or(Error::OutOfMemory)?;
        let errors = Box::<ErrorSlot>::default();
        // SAFETY: `raw` is a live context, and `errors` is freed only after
        // it.
        unsafe {
            sys::xmlXPathSetErrorHandler(raw.as_ptr(), Some(record_error), errors.as_user_data());
        }
        Ok(Self {
            raw,
            errors,
            _document: PhantomData,
        })
    }

    /// Registers the namespaces declared on the root element, with the
    /// default namespace as `_`.
    fn register_root_namespaces(&self, document: &Document) {
        // SAFETY: the document is live; the root is null or one of its
        // nodes.
        let root = unsafe { sys::xmlDocGetRootElement(document.raw.as_ptr()).as_ref() };
        let Some(root) = root else {
            return;
        };
        let mut next = root.nsDef;
        // SAFETY: `nsDef` is null or the head of the root's list of
        // namespace declarations, which lives as long as the document.
        while let Some(namespace) = unsafe { next.as_ref() } {
            let prefix = if namespace.prefix.is_null() {
                c"_".as_ptr().cast()
            } else {
                namespace.prefix
            };
            // SAFETY: both strings are NUL-terminated; libxml2 copies them.
            // A failed registration leaves the prefix undefined, which the
            // expression then reports.
            unsafe { sys::xmlXPathRegisterNs(self.raw.as_ptr(), prefix, namespace.href) };
            next = namespace.next;
        }
    }

    fn compile(&self, expression: &CStr) -> Result<(), Error> {
        // SAFETY: the context is live and the expression is NUL-terminated.
        let compiled =
            unsafe { sys::xmlXPathCtxtCompile(self.raw.as_ptr(), expression.as_ptr().cast()) };
        if compiled.is_null() {
            return Err(self.error());
        }
        // SAFETY: the compiled expression is owned here and freed once.
        unsafe { sys::xmlXPathFreeCompExpr(compiled) };
        Ok(())
    }

    fn evaluate(&self, expression: &CStr) -> Result<Output, Error> {
        // SAFETY: the context and its document are live, and the expression
        // is NUL-terminated. The result is owned by the caller.
        let raw = unsafe { sys::xmlXPathEval(expression.as_ptr().cast(), self.raw.as_ptr()) };
        let Some(raw) = NonNull::new(raw) else {
            return Err(self.error());
        };
        XpathObject(raw).output()
    }

    fn error(&self) -> Error {
        Error::Expression(
            self.errors
                .message()
                .unwrap_or_else(|| "the expression failed".to_owned()),
        )
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: the context is owned here and freed once, before
        // `errors`.
        unsafe { sys::xmlXPathFreeContext(self.raw.as_ptr()) };
    }
}

/// An XPath result, freed when dropped.
struct XpathObject(NonNull<sys::xmlXPathObject>);

impl XpathObject {
    fn output(&self) -> Result<Output, Error> {
        // SAFETY: the object is live until `self` drops.
        let object = unsafe { self.0.as_ref() };
        Ok(match object.type_ {
            sys::xmlXPathObjectType_XPATH_NODESET => {
                // SAFETY: `nodesetval` is null for an empty set, or a node
                // set owned by the object.
                let nodes = unsafe { object.nodesetval.as_ref() }.map_or(0, |set| set.nodeNr);
                Output::NodeSet(usize::try_from(nodes).unwrap_or(0))
            }
            sys::xmlXPathObjectType_XPATH_BOOLEAN => Output::Boolean(object.boolval != 0),
            sys::xmlXPathObjectType_XPATH_NUMBER => Output::Number(object.floatval),
            sys::xmlXPathObjectType_XPATH_STRING => Output::String(if object.stringval.is_null() {
                String::new()
            } else {
                // SAFETY: a non-null string value is a NUL-terminated UTF-8
                // string owned by the object.
                unsafe { CStr::from_ptr(object.stringval.cast()) }
                    .to_string_lossy()
                    .into_owned()
            }),
            _ => return Err(Error::Unsupported),
        })
    }
}

impl Drop for XpathObject {
    fn drop(&mut self) {
        // SAFETY: the object is owned here and freed once.
        unsafe { sys::xmlXPathFreeObject(self.0.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    const PAGE: &str = "<!doctype html><html><head><meta charset=windows-1252>\
        <title>Shop</title></head><body><h1>Caf\u{e9}</h1>\
        <ul><li>One</li><li>Two</li><li class=last>Three</li></ul></body></html>";

    const FEED: &str = "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\
        <feed xmlns=\"http://www.w3.org/2005/Atom\" xmlns:media=\"http://search.yahoo.com/mrss/\">\
        <title>Caf\u{e9} news</title><entry><media:title>One</media:title></entry></feed>";

    fn html(expression: &str) -> Result<Output, Error> {
        evaluate(PAGE, Markup::Html, expression)
    }

    #[test]
    fn reads_each_result_type_from_html() {
        assert_eq!(html("//li"), Ok(Output::NodeSet(3)));
        assert_eq!(html("//table"), Ok(Output::NodeSet(0)));
        assert_eq!(html("count(//li)"), Ok(Output::Number(3.0)));
        assert_eq!(html("boolean(//h1)"), Ok(Output::Boolean(true)));
        assert_eq!(
            html("string(//li[@class='last'])"),
            Ok(Output::String("Three".to_owned()))
        );
    }

    #[test]
    fn keeps_utf8_text_whatever_the_declared_encoding() {
        assert_eq!(
            html("string(//h1)"),
            Ok(Output::String("Caf\u{e9}".to_owned()))
        );
        assert_eq!(
            evaluate(FEED, Markup::Xml, "string(//_:title)"),
            Ok(Output::String("Caf\u{e9} news".to_owned()))
        );
    }

    #[test]
    fn registers_the_root_namespaces() {
        assert_eq!(
            evaluate(FEED, Markup::Xml, "//_:entry"),
            Ok(Output::NodeSet(1))
        );
        assert_eq!(
            evaluate(FEED, Markup::Xml, "string(//media:title)"),
            Ok(Output::String("One".to_owned()))
        );
        // Without a prefix, a name matches only elements in no namespace.
        assert_eq!(
            evaluate(FEED, Markup::Xml, "//entry"),
            Ok(Output::NodeSet(0))
        );
    }

    #[test]
    fn reports_malformed_xml_with_libxml2s_reason() {
        let Err(Error::Xml(reason)) = evaluate("<a><b></a>", Markup::Xml, "/a") else {
            panic!("malformed XML must fail");
        };
        assert!(reason.contains("mismatch"), "{reason}");
    }

    #[test]
    fn does_not_load_external_entities() {
        let xml = "<!DOCTYPE r [<!ENTITY x SYSTEM \"file:///etc/hosts\">]><r>&x;</r>";
        assert_eq!(
            evaluate(xml, Markup::Xml, "string(/r)"),
            Ok(Output::String(String::new()))
        );
    }

    #[test]
    fn reports_expression_errors() {
        assert_eq!(
            html("//li["),
            Err(Error::Expression("Invalid expression".to_owned()))
        );
        let Err(Error::Expression(message)) = html("foo()") else {
            panic!("an unknown function must fail");
        };
        assert!(message.contains("foo"), "{message}");
    }

    #[test]
    fn validates_expressions_before_a_run() {
        assert_eq!(validate("//li[@class='last']"), Ok(()));
        assert_eq!(validate("count(//_:entry)"), Ok(()));
        assert_eq!(validate("string(//media:title)"), Ok(()));
        for invalid in ["//li[", "count(", "foo()", "$x", "count('a')", "a\0b"] {
            assert!(validate(invalid).is_err(), "{invalid:?} must be invalid");
        }
    }

    #[test]
    fn parses_an_empty_input() {
        assert!(matches!(
            evaluate("", Markup::Html, "count(//p)"),
            Ok(_) | Err(Error::Html)
        ));
        assert!(matches!(evaluate("", Markup::Xml, "/"), Err(Error::Xml(_))));
    }

    #[test]
    fn evaluates_on_many_threads_at_once() {
        thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| (0..50).map(|_| html("count(//li)")).collect::<Vec<_>>()))
                .collect();
            for handle in handles {
                let results = handle.join().expect("no thread panics");
                assert!(
                    results
                        .iter()
                        .all(|result| *result == Ok(Output::Number(3.0)))
                );
            }
        });
    }
}
