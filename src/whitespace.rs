//! Syntax-aware whitespace edits for source that has already passed through rustfmt.

use std::error::Error;
use std::fmt;
use std::ops::Range;

use serde::Deserialize;

pub use ra_ap_syntax::Edition;
use ra_ap_syntax::ast::{self, BinaryOp};
use ra_ap_syntax::{AstNode, SourceFile, SyntaxKind, SyntaxNode, SyntaxToken, TextRange};

/// Each switch controls only the boundaries requested by that rule.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RuleOptions {
    pub blank_lines_around_multiline: bool,
    pub blank_line_at_end_of_if: bool,
    pub blank_lines_between_trait_methods: bool,
    pub blank_lines_between_node_types: bool,
}

impl Default for RuleOptions {
    fn default() -> Self {
        Self {
            blank_lines_around_multiline: true,
            blank_line_at_end_of_if: true,
            blank_lines_between_trait_methods: true,
            blank_lines_between_node_types: true,
        }
    }
}

/// A byte offset and a one-based line and Unicode character column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub message: String,
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl Diagnostic {
    fn new(source: &str, offset: usize, message: impl Into<String>) -> Self {
        let prefix = &source[..offset];

        Self {
            message: message.into(),
            offset,
            line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
            column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}: {}", self.line, self.column, self.message)
    }
}

impl Error for Diagnostic {}

#[derive(Debug)]
struct Edit {
    range: Range<usize>,
    replacement: String,
}

struct Unit {
    node: SyntaxNode,
    code: Range<usize>,
    kind: UnitKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnitKind {
    Assignment,
    Macro,
    Syntax(SyntaxKind),
}

struct Pass<'src> {
    source: &'src str,
    options: RuleOptions,
    newline: &'static str,
    lists: Vec<SyntaxNode>,
    protected: Vec<Range<usize>>,
    edits: Vec<Edit>,
}

/// Format a source buffer without performing IO or changing any Rust tokens.
///
/// The edition is the source language edition, independently of rustfmt's style
/// edition. Invalid syntax and unsafe edits return a diagnostic, never partial output.
pub fn format(source: &str, edition: Edition, options: RuleOptions) -> Result<String, Diagnostic> {
    let parsed = SourceFile::parse(source, edition);

    if let Some(error) = parsed.errors().first() {
        return Err(Diagnostic::new(
            source,
            usize::from(error.range().start()),
            error.to_string(),
        ));
    }

    let root = parsed.tree().syntax().clone();

    let mut pass = Pass {
        source,
        options,
        newline: newline_convention(&root),
        lists: Vec::new(),
        protected: Vec::new(),
        edits: Vec::new(),
    };

    pass.collect(&root);
    pass.separate_units();

    let output = pass.apply()?;
    let reparsed = SourceFile::parse(&output, edition);

    if !reparsed.errors().is_empty() || tokens(&root) != tokens(reparsed.tree().syntax()) {
        return Err(Diagnostic::new(
            source,
            0,
            "whitespace edits changed Rust syntax",
        ));
    }

    Ok(output)
}

impl Pass<'_> {
    fn collect(&mut self, node: &SyntaxNode) {
        if is_skipped(node) || node.kind() == SyntaxKind::TOKEN_TREE {
            self.protected.push(byte_range(node.text_range()));

            return;
        }

        if node.kind() == SyntaxKind::ATTR {
            self.protected.push(byte_range(node.text_range()));

            return;
        }

        if is_list(node.kind()) {
            self.lists.push(node.clone());
        }

        if self.options.blank_line_at_end_of_if
            && let Some(branch) = ast::IfExpr::cast(node.clone())
            && branch.else_branch().is_some()
            && let Some(block) = branch.then_branch()
        {
            self.layout_branch(&block);
        }

        for element in node.children_with_tokens() {
            match element {
                ra_ap_syntax::NodeOrToken::Node(child) => self.collect(&child),

                ra_ap_syntax::NodeOrToken::Token(token)
                    if token.kind() != SyntaxKind::WHITESPACE =>
                {
                    self.protected.push(byte_range(token.text_range()));
                }

                ra_ap_syntax::NodeOrToken::Token(_) => {}
            }
        }
    }

    fn layout_branch(&mut self, block: &ast::BlockExpr) {
        let Some(list) = block.stmt_list() else {
            return;
        };

        if is_skipped(list.syntax()) {
            return;
        }

        let (Some(open), Some(close)) = (list.l_curly_token(), list.r_curly_token()) else {
            return;
        };

        let end = usize::from(close.text_range().start());

        let contents: Vec<_> = list
            .syntax()
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .filter(|token| {
                token.kind() != SyntaxKind::WHITESPACE
                    && token.text_range().start() >= open.text_range().end()
                    && token.text_range().end() <= close.text_range().start()
            })
            .collect();

        let (Some(first), Some(last)) = (contents.first(), contents.last()) else {
            return;
        };

        let content_start = first.text_range().start();
        let content_end = last.text_range().end();

        let multiline = list
            .syntax()
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .any(|token| {
                token.text_range().start() >= content_start
                    && token.text_range().end() <= content_end
                    && is_layout_newline(&token)
            });

        if !multiline {
            return;
        }

        let last = usize::from(last.text_range().end());
        let base = line_indent(self.source, end);

        self.request(last..end, format!("{}{}", self.newline.repeat(2), base));
    }

    fn separate_units(&mut self) {
        for list in self.lists.clone() {
            let units: Vec<_> = list
                .children()
                .filter_map(|node| Unit::new(node, self.source))
                .collect();

            let in_trait = list.kind() == SyntaxKind::ASSOC_ITEM_LIST
                && list
                    .parent()
                    .is_some_and(|parent| parent.kind() == SyntaxKind::TRAIT);

            for pair in units.windows(2) {
                let left = &pair[0];
                let right = &pair[1];

                let multiline = self.options.blank_lines_around_multiline
                    && (self.multiline(left) || self.multiline(right));

                let methods = self.options.blank_lines_between_trait_methods
                    && in_trait
                    && left.node.kind() == SyntaxKind::FN
                    && right.node.kind() == SyntaxKind::FN;

                let kinds = self.options.blank_lines_between_node_types && left.kind != right.kind;

                if multiline || methods || kinds {
                    self.separate(left, right);
                }
            }
        }
    }

    fn multiline(&self, unit: &Unit) -> bool {
        self.source[unit.code.clone()].contains('\n')
            || self.edits.iter().any(|edit| {
                unit.code.start <= edit.range.start
                    && edit.range.end <= unit.code.end
                    && edit.replacement.contains('\n')
            })
    }

    fn separate(&mut self, left: &Unit, right: &Unit) {
        // A leading comment can belong to the next node in the syntax tree,
        // even when it trails the preceding field on the same line.
        let end = right
            .node
            .descendants_with_tokens()
            .filter_map(|element| element.into_token())
            .find(|token| !matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT))
            .map_or(right.code.start, |token| {
                usize::from(token.text_range().start())
            });

        let mut start = left.code.end;
        let mut token = left.node.last_token().and_then(|token| token.next_token());

        // Keep same-line comments with the preceding unit. The first remaining
        // whitespace gap is outside any leading or standalone comment group.
        while let Some(ref current) = token {
            let range = byte_range(current.text_range());

            if range.start >= end {
                break;
            }

            if current.kind() == SyntaxKind::WHITESPACE {
                if current.text().contains('\n') {
                    break;
                }

            } else if current.kind() == SyntaxKind::COMMENT
                || is_field(left.node.kind()) && current.kind() == SyntaxKind::COMMA
            {
                start = range.end;
            } else {
                return;
            }

            token = current.next_token();
        }

        let gap_end = token.map_or(end, |token| {
            if token.kind() == SyntaxKind::WHITESPACE {
                usize::from(token.text_range().end()).min(end)
            } else {
                usize::from(token.text_range().start()).min(end)
            }
        });

        let gap = &self.source[start..gap_end];

        let indent = gap
            .rsplit_once('\n')
            .map_or_else(|| line_indent(self.source, end), |(_prefix, indent)| indent);

        self.request(
            start..gap_end,
            format!("{}{}", self.newline.repeat(2), indent),
        );
    }

    fn request(&mut self, range: Range<usize>, replacement: String) {
        if self.source[range.clone()] != replacement {
            self.edits.push(Edit { range, replacement });
        }
    }

    fn apply(mut self) -> Result<String, Diagnostic> {
        self.protected.sort_by_key(|range| range.start);

        self.edits.retain(|edit| {
            let idx = self
                .protected
                .partition_point(|range| range.end <= edit.range.start);

            !self
                .protected
                .get(idx)
                .is_some_and(|range| intersects(&edit.range, range))
        });

        self.edits
            .sort_by_key(|edit| (edit.range.start, edit.range.end));

        self.edits.dedup_by(|left, right| {
            left.range == right.range && left.replacement == right.replacement
        });

        for pair in self.edits.windows(2) {
            if pair[0].range.end > pair[1].range.start || pair[0].range.start == pair[1].range.start
            {
                return Err(Diagnostic::new(
                    self.source,
                    pair[1].range.start,
                    "overlapping whitespace edits",
                ));
            }
        }

        let mut output = self.source.to_owned();

        for edit in self.edits.into_iter().rev() {
            if !self.source[edit.range.clone()]
                .chars()
                .all(char::is_whitespace)
                || !edit.replacement.chars().all(char::is_whitespace)
            {
                return Err(Diagnostic::new(
                    self.source,
                    edit.range.start,
                    "unsafe whitespace edit",
                ));
            }

            output.replace_range(edit.range, &edit.replacement);
        }

        Ok(output)
    }
}

impl Unit {
    fn new(node: SyntaxNode, source: &str) -> Option<Self> {
        if !ast::Item::can_cast(node.kind())
            && !ast::Stmt::can_cast(node.kind())
            && !ast::Expr::can_cast(node.kind())
            && !is_field(node.kind())
            && node.kind() != SyntaxKind::MATCH_ARM
        {
            return None;
        }

        let code_node = ast::ExprStmt::cast(node.clone())
            .and_then(|statement| statement.expr())
            .map_or_else(|| node.clone(), |expr| expr.syntax().clone());

        let first = code_node.children_with_tokens().find(|element| {
            !matches!(
                element.kind(),
                SyntaxKind::ATTR | SyntaxKind::COMMENT | SyntaxKind::WHITESPACE
            )
        })?;

        let start = if is_field(node.kind()) {
            node.children_with_tokens()
                .find(|element| match element.kind() {
                    SyntaxKind::WHITESPACE => false,

                    SyntaxKind::COMMENT => {
                        let offset = usize::from(element.text_range().start());

                        source[..offset]
                            .rsplit('\n')
                            .next()
                            .is_some_and(|prefix| prefix.trim().is_empty())
                    }

                    _ => true,
                })?
                .text_range()
                .start()

        } else {
            first.text_range().start()
        };

        let code = usize::from(start)..usize::from(node.text_range().end());
        let kind = classify(&node);

        Some(Self { node, code, kind })
    }
}

fn classify(node: &SyntaxNode) -> UnitKind {
    if matches!(node.kind(), SyntaxKind::EXPR_STMT | SyntaxKind::PAREN_EXPR)
        && let Some(inner) = node
            .children()
            .find(|child| ast::Expr::can_cast(child.kind()))
    {
        return classify(&inner);
    }

    if ast::BinExpr::cast(node.clone())
        .and_then(|expr| expr.op_kind())
        .is_some_and(|operator| matches!(operator, BinaryOp::Assignment { .. }))
    {
        return UnitKind::Assignment;
    }

    if matches!(node.kind(), SyntaxKind::MACRO_EXPR | SyntaxKind::MACRO_CALL) {
        return UnitKind::Macro;
    }

    UnitKind::Syntax(node.kind())
}

fn is_list(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::SOURCE_FILE
            | SyntaxKind::ITEM_LIST
            | SyntaxKind::ASSOC_ITEM_LIST
            | SyntaxKind::EXTERN_ITEM_LIST
            | SyntaxKind::STMT_LIST
            | SyntaxKind::RECORD_FIELD_LIST
            | SyntaxKind::TUPLE_FIELD_LIST
            | SyntaxKind::MATCH_ARM_LIST
    )
}

fn is_field(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD)
}

fn is_skipped(node: &SyntaxNode) -> bool {
    node.children()
        .take_while(|child| child.kind() == SyntaxKind::ATTR)
        .filter_map(ast::Attr::cast)
        .any(|attr| {
            attr.meta()
                .and_then(|meta| meta.as_simple_path())
                .is_some_and(|path| {
                    let text: String = path
                        .syntax()
                        .descendants_with_tokens()
                        .filter_map(|element| element.into_token())
                        .filter(|token| {
                            !matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
                        })
                        .map(|token| token.text().to_owned())
                        .collect();

                    text == "rustfmt::skip"
                })
        })
}

fn byte_range(range: TextRange) -> Range<usize> {
    usize::from(range.start())..usize::from(range.end())
}

fn intersects(edit: &Range<usize>, protected: &Range<usize>) -> bool {
    edit.start < protected.end && protected.start < edit.end
        || edit.is_empty() && protected.start < edit.start && edit.start < protected.end
}

fn line_indent(source: &str, offset: usize) -> &str {
    let start = source[..offset].rfind('\n').map_or(0, |idx| idx + 1);
    let line = &source[start..offset];

    let length = line
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();

    &line[..length]
}

fn newline_convention(root: &SyntaxNode) -> &'static str {
    let newline = root
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .find(is_layout_newline);

    if newline.is_some_and(|token| token.text().contains("\r\n")) {
        return "\r\n";
    }

    "\n"
}

fn is_layout_newline(token: &SyntaxToken) -> bool {
    token.kind() == SyntaxKind::WHITESPACE
        && token.text().contains('\n')
        && !token.parent_ancestors().any(|ancestor| {
            matches!(ancestor.kind(), SyntaxKind::TOKEN_TREE | SyntaxKind::ATTR)
                || is_skipped(&ancestor)
        })
}

fn tokens(node: &SyntaxNode) -> Vec<(SyntaxKind, String)> {
    node.descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() != SyntaxKind::WHITESPACE)
        .map(|token: SyntaxToken| (token.kind(), token.text().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: RuleOptions = RuleOptions {
        blank_lines_around_multiline: false,
        blank_line_at_end_of_if: false,
        blank_lines_between_trait_methods: false,
        blank_lines_between_node_types: false,
    };

    const MULTILINE: RuleOptions = RuleOptions {
        blank_lines_around_multiline: true,
        ..NONE
    };

    const BRANCHES: RuleOptions = RuleOptions {
        blank_line_at_end_of_if: true,
        ..NONE
    };

    const METHODS: RuleOptions = RuleOptions {
        blank_lines_between_trait_methods: true,
        ..NONE
    };

    const KINDS: RuleOptions = RuleOptions {
        blank_lines_between_node_types: true,
        ..NONE
    };

    fn check(before: &str, after: &str, options: RuleOptions) {
        let output = format(before, Edition::Edition2024, options).unwrap();

        assert_eq!(output, after);

        assert_eq!(
            format(&output, Edition::Edition2024, options).unwrap(),
            output
        );

        assert_eq!(
            tokens(
                SourceFile::parse(before, Edition::Edition2024)
                    .tree()
                    .syntax()
            ),
            tokens(
                SourceFile::parse(&output, Edition::Edition2024)
                    .tree()
                    .syntax()
            )
        );
    }

    #[test]
    fn multiline_declarations_and_calls_are_complete_units() {
        check(
            r#"fn example() {
    let apple = "pear";
    let basket = Basket {
        fruit: apple,
        capacity: 12,
    };
    let label = "fruit";
}
"#,
            r#"fn example() {
    let apple = "pear";

    let basket = Basket {
        fruit: apple,
        capacity: 12,
    };

    let label = "fruit";
}
"#,
            MULTILINE,
        );

        check(
            r#"fn main() {
    let first = Thing {
        value: 1,
    };
    let second = Thing {
        value: 2,
    };
    result = build(
        first,
        second,
    );
    consume(
        result,
        second,
    );
    (
        first,
        second,
    )
}"#,
            r#"fn main() {
    let first = Thing {
        value: 1,
    };

    let second = Thing {
        value: 2,
    };

    result = build(
        first,
        second,
    );

    consume(
        result,
        second,
    );

    (
        first,
        second,
    )
}"#,
            MULTILINE,
        );
    }

    #[test]
    fn multiline_fields_are_separated_with_their_docs_attributes_and_commas() {
        let before = r#"pub struct Options {
    pub first: bool,
    pub second: bool,
    /// Override all four whitespace rules for this invocation.
    #[arg(long, value_parser = ["on", "off"])]
    pub whitespace: Option<String>, // Keep this comment with the field.
    /// Check formatting without changing files.
    #[arg(long)]
    pub check: bool,
    pub third: bool,
    pub fourth: bool,
}
"#;

        let after = r#"pub struct Options {
    pub first: bool,
    pub second: bool,

    /// Override all four whitespace rules for this invocation.
    #[arg(long, value_parser = ["on", "off"])]
    pub whitespace: Option<String>, // Keep this comment with the field.

    /// Check formatting without changing files.
    #[arg(long)]
    pub check: bool,

    pub third: bool,
    pub fourth: bool,
}
"#;

        check(before, after, MULTILINE);
        check(before, after, RuleOptions::default());
        check(before, before, NONE);
        check(before, before, KINDS);
    }

    #[test]
    fn multiline_field_types_cover_named_tuple_and_variant_fields() {
        let cases = [
            (
                "struct Named {\n    first: bool,\n    callback: fn(\n        First,\n        Second,\n    ),\n    last: bool,\n}\n",
                "struct Named {\n    first: bool,\n\n    callback: fn(\n        First,\n        Second,\n    ),\n\n    last: bool,\n}\n",
            ),
            (
                "struct Tuple(\n    bool,\n    /// A documented field.\n    #[cfg(ready)]\n    String,\n    bool,\n);\n",
                "struct Tuple(\n    bool,\n\n    /// A documented field.\n    #[cfg(ready)]\n    String,\n\n    bool,\n);\n",
            ),
            (
                "enum Choice {\n    Named {\n        first: bool,\n        #[cfg(ready)]\n        second: bool,\n        third: bool,\n    },\n}\n",
                "enum Choice {\n    Named {\n        first: bool,\n\n        #[cfg(ready)]\n        second: bool,\n\n        third: bool,\n    },\n}\n",
            ),
            (
                "struct Trailing {\n    /// First.\n    first: bool, // Keep with first.\n    second: bool,\n    third: bool,\n}\n",
                "struct Trailing {\n    /// First.\n    first: bool, // Keep with first.\n\n    second: bool,\n    third: bool,\n}\n",
            ),
        ];

        for (before, after) in cases {
            check(before, after, MULTILINE);
            check(before, before, NONE);
        }
    }

    #[test]
    fn field_padding_preserves_list_edges_skip_regions_and_expression_fields() {
        let sources = [
            "struct Single {\n    /// The only field.\n    #[cfg(ready)]\n    value: bool,\n}\n",
            "#[rustfmt::skip]\nstruct Skipped {\n    first: bool,\n    #[cfg(ready)]\n    second: bool,\n}\n",
            "struct SkippedField {\n    #[rustfmt::skip]\n    callback: fn( First,\n      Second ),\n}\n",
            "struct Plain {\n    first: bool,\n    second: bool,\n}\n",
            "fn main() {\n    let value = Value {\n        first: build(\n            argument,\n        ),\n        second: true,\n    };\n}\n",
        ];

        for source in sources {
            check(source, source, MULTILINE);
        }

        let before = "struct Fields {\r\n\tfirst: bool,\r\n\t/// Documented.\r\n\tsecond: bool,\r\n\tthird: bool,\r\n}\r\n";
        let after = "struct Fields {\r\n\tfirst: bool,\r\n\r\n\t/// Documented.\r\n\tsecond: bool,\r\n\r\n\tthird: bool,\r\n}\r\n";

        check(before, after, MULTILINE);
    }

    #[test]
    fn kinds_unwrap_statements_and_parentheses_and_group_assignments() {
        check(
            r#"fn example() {
    let apple = "pear";
    let foo = "bar";
    baz("");
    (baz(""));
    apple = "plum";
    apple += "x";
    basket.clear();
    basket.empty()
}
"#,
            r#"fn example() {
    let apple = "pear";
    let foo = "bar";

    baz("");
    (baz(""));

    apple = "plum";
    apple += "x";

    basket.clear();
    basket.empty()
}
"#,
            KINDS,
        );

        check(
            r#"use one::Thing;
use two::Other;
struct Empty;
fn main() {}
"#,
            r#"use one::Thing;
use two::Other;

struct Empty;

fn main() {}
"#,
            KINDS,
        );
    }

    #[test]
    fn multiline_match_arms_are_separated_without_padding_match_braces() {
        let before = r#"fn main() {
    match value {
        First => {
            first()
        }
        Second => second(),
        Third => third(),
        Fourth => {
            fourth()
        },
        _ => {
            last()
        },
    }
}
"#;

        let after = r#"fn main() {
    match value {
        First => {
            first()
        }

        Second => second(),
        Third => third(),

        Fourth => {
            fourth()
        },

        _ => {
            last()
        },
    }
}
"#;

        check(before, after, MULTILINE);
        check(before, after, RuleOptions::default());
        check(before, before, NONE);
        check(before, before, KINDS);
    }

    #[test]
    fn match_arm_spacing_preserves_comments_guards_and_attributes() {
        check(
            r#"fn main() {
    match value {
        First => first(), // Keep with first.
        // A guarded arm.
        #[cfg(ready)]
        Some(value)
            if ready => value, // Keep with value.
        // The fallback.
        _ => fallback(),
    }
}
"#,
            r#"fn main() {
    match value {
        First => first(), // Keep with first.

        // A guarded arm.
        #[cfg(ready)]
        Some(value)
            if ready => value, // Keep with value.

        // The fallback.
        _ => fallback(),
    }
}
"#,
            MULTILINE,
        );

        check(
            "fn main() {\r\n\tmatch value {\r\n\t\tFirst => {\r\n\t\t\tfirst()\r\n\t\t}\r\n\t\t_ => fallback(),\r\n\t}\r\n}\r\n",
            "fn main() {\r\n\tmatch value {\r\n\t\tFirst => {\r\n\t\t\tfirst()\r\n\t\t}\r\n\r\n\t\t_ => fallback(),\r\n\t}\r\n}\r\n",
            MULTILINE,
        );

        for source in [
            "fn main() {\n    match value {\n        First => first(),\n        _ => fallback(),\n    }\n}\n",
            "fn main() {\n    match value {\n        _ => {\n            fallback()\n        }\n    }\n}\n",
            "#[rustfmt::skip]\nfn main() {\n    match value {\n        First => {\n            first()\n        }\n        _ => fallback(),\n    }\n}\n",
        ] {
            check(source, source, RuleOptions::default());
        }
    }

    #[test]
    fn only_multiline_bodies_followed_by_else_are_padded() {
        let before = r#"fn example() {
    if ready {
        prepare();
        confirm();
    } else if waiting {
        pause();
        retry();
    } else {
        finish();
        close();
    }
}
"#;

        let after = r#"fn example() {
    if ready {
        prepare();
        confirm();

    } else if waiting {
        pause();
        retry();

    } else {
        finish();
        close();
    }
}
"#;

        check(before, after, BRANCHES);
        check(before, before, NONE);

        check(
            "fn main() {\n    if ready {\n        first();\n        second();\n    } else if waiting {\n        third();\n        fourth();\n    }\n}\n",
            "fn main() {\n    if ready {\n        first();\n        second();\n\n    } else if waiting {\n        third();\n        fourth();\n    }\n}\n",
            BRANCHES,
        );
    }

    #[test]
    fn compact_and_one_line_bodies_stay_unpadded() {
        let sources = [
            "fn main() {\n    let first = 0;\n    let value = if ready { 1 } else if waiting { 2 } else { 3 };\n    let last = 4;\n}\n",
            "fn main() {\n    let prefix_length = if parsed.command.is_some() { 2 } else { 1 };\n}\n",
            "fn main() {\n    let expected = if case[0] == \"rustfmt\" {\n        &case[1..]\n    } else {\n        &case\n    };\n}\n",
            "fn main() {\n    if ready { work(); }\n    if waiting {} else if ready { /* Wait. */ } else {}\n}\n",
            "fn main() {\n    if ready {\n        work(); // Keep this comment.\n    } else if waiting {\n        pause();\n    } else {\n        finish();\n    }\n}\n",
            "fn main() {\n    if ready {\n        // Nothing yet.\n    } else {\n        work();\n    }\n}\n",
            "fn main() {\n    if ready {\n\n        work();\n    } else {\n        finish();\n    }\n}\n",
            r##"fn main() {
    let value = if ready { r#"first
second"# } else { "" };
}
"##,
            "fn main() {\n    if ready { opaque! {\n        tokens\n    } } else { work(); }\n}\n",
        ];

        for source in sources {
            check(source, source, BRANCHES);
            check(source, source, RuleOptions::default());
        }
    }

    #[test]
    fn standalone_ifs_stay_unpadded_regardless_of_body_length() {
        let sources = [
            "fn main() {\n    if output.status.success() {\n        result.map_err(|error| at_path(Path::new(\"<stdin>\"), error))?;\n    }\n}\n",
            "fn main() {\n    if ready {\n        first();\n        second();\n    }\n}\n",
            "fn main() {\n    if ready {\n        if waiting {\n            first();\n            second();\n        }\n    }\n}\n",
            "fn main() {\n    if ready {\n\n        work();\n\n    }\n}\n",
        ];

        for source in sources {
            check(source, source, BRANCHES);
        }
    }

    #[test]
    fn trait_methods_keep_docs_and_attributes_and_rules_are_independent() {
        let before = r#"trait Store {
    fn open(&self);
    /// Close the store.
    #[must_use]
    fn close(&self) -> bool {
        true
    }
    type Item;
    const COUNT: usize;
    fn find(
        &self,
        key: Key,
    );
    fn save(&self);
}
"#;

        let after = r#"trait Store {
    fn open(&self);

    /// Close the store.
    #[must_use]
    fn close(&self) -> bool {
        true
    }
    type Item;
    const COUNT: usize;
    fn find(
        &self,
        key: Key,
    );

    fn save(&self);
}
"#;

        check(before, after, METHODS);
        check(before, before, NONE);

        check(
            r#"trait Store {
    fn open(&self);
    fn close(&self);
}
"#,
            r#"trait Store {
    fn open(&self);

    fn close(&self);
}
"#,
            METHODS,
        );

        check(
            r#"trait Store {
    fn open(&self);
    fn close(&self);
}
"#,
            r#"trait Store {
    fn open(&self);
    fn close(&self);
}
"#,
            RuleOptions {
                blank_lines_between_trait_methods: false,
                ..RuleOptions::default()
            },
        );
    }

    #[test]
    fn leading_trivia_does_not_make_code_multiline() {
        let source = r#"fn main() {
    // First line.
    // Second line.
    #[allow(unused)]
    let apple = 1;
    let pear = 2;
}
"#;

        check(source, source, MULTILINE);

        check(
            r#"fn main() {
    let apple = 1; // Keep with apple.
    // Use apple here.
    consume(apple);

    // An independent group.

    let pear = 2;
}
"#,
            r#"fn main() {
    let apple = 1; // Keep with apple.

    // Use apple here.
    consume(apple);

    // An independent group.

    let pear = 2;
}
"#,
            KINDS,
        );
    }

    #[test]
    fn literals_macros_and_skip_regions_remain_byte_identical() {
        let source = r###"#![allow(unused)]
#[rustfmt::skip]
fn skipped(){if true { 1 } else { 2 };let value=0;consume(value);}
fn main() {
    let text = r#"αβ
	if ready { 1 } else { 2 }

"#;
    opaque! { if ready { 1 } else { 2 }; let value=0; consume(value); }
    opaque! { strange Rust tokens => { } }
    let bytes = b"\n\x00";
    let byte = b' ';
    let character = 'λ';
}
"###;

        let output = format(source, Edition::Edition2024, RuleOptions::default()).unwrap();

        check(source, &output, RuleOptions::default());

        assert!(output.contains("\n\nfn main()"));
        assert!(output.contains("    opaque! { strange Rust tokens => { } }\n\n    let bytes"));
        assert!(output.contains("    let bytes = b\"\\n\\x00\";\n    let byte"));

        let skipped =
            "#![rustfmt::skip]\nfn main(){if true { 1 } else { 2 };let value=0;consume(value);}";

        check(skipped, skipped, RuleOptions::default());
    }

    #[test]
    fn newline_indentation_file_edges_and_disabled_rules_are_preserved() {
        check(
            "fn main() {\r\n\tlet café = 0;\r\n\tlet value = if ready {\r\n\t\t// Preserve the value.\r\n\t\tcafé\r\n\t} else {\r\n\t\t2\r\n\t};\r\n\tconsume(value);\r\n}",
            "fn main() {\r\n\tlet café = 0;\r\n\r\n\tlet value = if ready {\r\n\t\t// Preserve the value.\r\n\t\tcafé\r\n\r\n\t} else {\r\n\t\t2\r\n\t};\r\n\r\n\tconsume(value);\r\n}",
            RuleOptions::default(),
        );

        let source = r#"
fn main() {
    let value = 1;


    consume(value);
}

"#;

        check(source, source, NONE);

        check(
            source,
            r#"
fn main() {
    let value = 1;

    consume(value);
}

"#,
            KINDS,
        );
    }

    #[test]
    fn nested_lists_cover_modules_impls_closures_match_arms_and_blocks() {
        check(
            r#"mod nested {
    use super::Thing;
    const COUNT: usize = {
        first();
        2
    };
    static ITEMS: [usize; 2] = [
        1,
        2,
    ];
    impl Thing {
        const VALUE: usize = 1;
        fn run() {
            let closure = || {
                let value = 1;
                consume(value);
            };
            match value {
                Some(value) => {
                    let copy = value;
                    consume(copy);
                }
                None => other(),
            }
            unsafe {
                let value = 1;
                consume(value);
            }
            async {
                let value = 1;
                consume(value);
            };
        }
    }
}
"#,
            r#"mod nested {
    use super::Thing;

    const COUNT: usize = {
        first();

        2
    };

    static ITEMS: [usize; 2] = [
        1,
        2,
    ];

    impl Thing {
        const VALUE: usize = 1;

        fn run() {
            let closure = || {
                let value = 1;

                consume(value);
            };

            match value {
                Some(value) => {
                    let copy = value;

                    consume(copy);
                }

                None => other(),
            }

            unsafe {
                let value = 1;

                consume(value);
            }

            async {
                let value = 1;

                consume(value);
            };
        }
    }
}
"#,
            RuleOptions::default(),
        );
    }

    #[test]
    fn control_flow_and_remaining_expression_kinds_stay_distinct() {
        check(
            r#"fn main() {
    if ready {}
    if waiting {}
    match value {}
    for value in values {}
    while ready {}
    loop {}
    {}
    return;
    break;
    continue;
    1 + 2;
    3 - 4;
    5 == 6;
    [1, 2];
    (1, 2);
    object.field;
    7
}
"#,
            r#"fn main() {
    if ready {}
    if waiting {}

    match value {}

    for value in values {}

    while ready {}

    loop {}

    {}

    return;

    break;

    continue;

    1 + 2;
    3 - 4;
    5 == 6;

    [1, 2];

    (1, 2);

    object.field;

    7
}
"#,
            KINDS,
        );
    }

    #[test]
    fn nested_chains_pad_only_their_own_multiline_bodies() {
        check(
            "fn main() {\n    if ready {\n        if waiting {\n            first();\n            second();\n        } else {\n            finish();\n        }\n    } else {\n        other();\n    }\n}\n",
            "fn main() {\n    if ready {\n        if waiting {\n            first();\n            second();\n\n        } else {\n            finish();\n        }\n\n    } else {\n        other();\n    }\n}\n",
            BRANCHES,
        );

        check(
            "fn main() {\n    if ready { 1 } else if waiting {\n        prepare();\n        2\n    } else { 3 };\n    if ready {\n        if waiting { 2 } else { 3 }\n    }\n}\n",
            "fn main() {\n    if ready { 1 } else if waiting {\n        prepare();\n        2\n\n    } else { 3 };\n    if ready {\n        if waiting { 2 } else { 3 }\n    }\n}\n",
            BRANCHES,
        );
    }

    #[test]
    fn opaque_contents_are_protected_including_their_whitespace() {
        let before = "#[rustfmt::skip]\nmod untouched { fn main(){if ready { value }; let x=1; call(x);} }\nmacro_rules! untouched { () => { if ready { value } else { other } } }\nfn main() {\n    opaque! {\n\t\tif ready { value } else { other }\n\n\n    };\n    let value = if ready { 1 } else { 2 };\n    #[rustfmt::skip]\n    let skipped = ||{ if ready { value }; call();};\n}\n";
        let after = "#[rustfmt::skip]\nmod untouched { fn main(){if ready { value }; let x=1; call(x);} }\n\nmacro_rules! untouched { () => { if ready { value } else { other } } }\n\nfn main() {\n    opaque! {\n\t\tif ready { value } else { other }\n\n\n    };\n\n    let value = if ready { 1 } else { 2 };\n    #[rustfmt::skip]\n    let skipped = ||{ if ready { value }; call();};\n}\n";

        check(before, after, RuleOptions::default());
    }

    #[test]
    fn comments_and_attributes_at_boundaries_keep_their_owners() {
        check(
            r#"fn main() {
    let value = 1; /* Keep with value. */
    /* Consume the value. */
    #[allow(unused)]
    consume(value);
    // A separate group.

    let other = 2;
}
"#,
            r#"fn main() {
    let value = 1; /* Keep with value. */

    /* Consume the value. */
    #[allow(unused)]
    consume(value);

    // A separate group.

    let other = 2;
}
"#,
            KINDS,
        );

        check(
            r#"fn main() {
    if ready {
        work();
        /* The last content. */
    } else if waiting {
        prepare();
        work(); // Keep this comment.


    } else {}
}
"#,
            r#"fn main() {
    if ready {
        work();
        /* The last content. */

    } else if waiting {
        prepare();
        work(); // Keep this comment.

    } else {}
}
"#,
            BRANCHES,
        );
    }

    #[test]
    fn method_separation_is_not_disabled_by_either_general_rule() {
        let before = r#"trait Store {
    fn open(&self);
    /// Close.
    fn close(&self);
}
"#;

        let after = r#"trait Store {
    fn open(&self);

    /// Close.
    fn close(&self);
}
"#;

        for options in [
            RuleOptions {
                blank_lines_around_multiline: false,
                ..RuleOptions::default()
            },
            RuleOptions {
                blank_lines_between_node_types: false,
                ..RuleOptions::default()
            },
            METHODS,
        ] {
            check(before, after, options);
        }
    }

    #[test]
    fn layout_ignores_newlines_and_indentation_inside_literals_and_macros() {
        check(
            "fn main() {\n    let text = r#\"α\r\n\tcharacters\"#;\n    let value = if ready { 1 } else { 2 };\n}\n",
            "fn main() {\n    let text = r#\"α\r\n\tcharacters\"#;\n\n    let value = if ready { 1 } else { 2 };\n}\n",
            RuleOptions::default(),
        );

        check(
            "macro_rules! opaque { () => {\r\n\tignored\r\n} }\nfn main() {\n  if ready { value };\n}\n",
            "macro_rules! opaque { () => {\r\n\tignored\r\n} }\n\nfn main() {\n  if ready { value };\n}\n",
            RuleOptions::default(),
        );
    }

    #[test]
    fn expression_attributes_do_not_make_single_line_calls_multiline() {
        let source = r#"fn main() {
    #[cfg(
        feature = "calls",
    )]
    consume(value);
    consume(other);
}
"#;

        check(source, source, MULTILINE);
    }

    #[test]
    fn invalid_source_reports_a_location_without_returning_partial_edits() {
        let source = "fn valid() {}\nfn broken( {";
        let error = format(source, Edition::Edition2024, RuleOptions::default()).unwrap_err();

        assert_eq!(error.line, 2);
        assert!(error.column > 0);
        assert!(error.offset >= source.find("fn broken").unwrap());
        assert!(error.to_string().starts_with("2:"));
        assert!(format("async fn ready() {}", Edition::Edition2015, NONE).is_err());
        assert!(format("async fn ready() {}", Edition::Edition2018, NONE).is_ok());
    }
}
