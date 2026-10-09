//! Hashes of source spans built from their tokens: comments, whitespace and the names a symbol is declared under do not count, and a line break counts only where it can end a statement.

use oxc_parser::{Kind, Token};
use oxc_span::Span;
use xxhash_rust::xxh3::Xxh3;

const NAME_MARK: &[u8] = b"\0";

const TOKEN_SEPARATOR: &[u8] = b"\x1f";

const SPAN_SEPARATOR: &[u8] = b"\x1e";

const LINE_BREAK: &[u8] = b"\n";

pub(crate) struct Fingerprint<'s> {
    source_text: &'s str,
    tokens: &'s [Token],
}

impl<'s> Fingerprint<'s> {
    pub(crate) fn new(source_text: &'s str, tokens: &'s [Token]) -> Self {
        Self {
            source_text,
            tokens,
        }
    }

    pub(crate) fn hash(&self, spans: &[Span], names: &[Span]) -> u64 {
        let mut hasher = Xxh3::new();

        for (index, span) in spans.iter().enumerate() {
            if index > 0 {
                hasher.update(SPAN_SEPARATOR);
            }

            let first = self
                .tokens
                .partition_point(|token| token.start() < span.start);
            let mut previous: Option<Kind> = None;

            for token in self.tokens[first..]
                .iter()
                .take_while(|token| token.end() <= span.end)
            {
                if token.is_on_new_line()
                    && previous.is_some_and(|previous| line_break_counts(previous, token.kind()))
                {
                    hasher.update(LINE_BREAK);
                }

                if names.contains(&token.span()) {
                    hasher.update(NAME_MARK);
                } else {
                    hasher.update(token.span().source_text(self.source_text).as_bytes());
                }

                hasher.update(TOKEN_SEPARATOR);
                previous = Some(token.kind());
            }
        }

        hasher.digest()
    }
}

fn line_break_counts(previous: Kind, next: Kind) -> bool {
    let ends_a_word = previous.is_identifier_or_keyword() || previous.is_update_operator();
    let continues_the_statement = matches!(
        next,
        Kind::RParen
            | Kind::RBrack
            | Kind::RCurly
            | Kind::Comma
            | Kind::Semicolon
            | Kind::Colon
            | Kind::Dot
            | Kind::QuestionDot
            | Kind::Question
            | Kind::Arrow
    ) || next.is_assignment_operator()
        || next.is_logical_operator()
        || (next.is_binary_operator() && next != Kind::Star);

    ends_a_word && !continues_the_statement
}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;
    use oxc_parser::{Parser, config::TokensParserConfig};
    use oxc_span::SourceType;

    use super::*;

    fn hash_masking(source_text: &str, name: Option<&str>) -> u64 {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source_text, SourceType::tsx())
            .with_config(TokensParserConfig)
            .parse();
        let names: Vec<Span> = name
            .map(|name| {
                let start = source_text.find(name).unwrap() as u32;

                Span::new(start, start + name.len() as u32)
            })
            .into_iter()
            .collect();

        assert!(!parsed.diagnostics.has_errors(), "{source_text}");

        Fingerprint::new(source_text, &parsed.tokens).hash(&[parsed.program.span], &names)
    }

    fn program_hash(source_text: &str) -> u64 {
        hash_masking(source_text, None)
    }

    #[test]
    fn comments_indentation_and_line_breaks_between_tokens_keep_the_hash() {
        let original = "function load(id) { return fetch(`/a/${id}`, { method: \"GET\" }); }";
        let reformatted = "function load(id) { // fetch one\n\n\t\treturn fetch(\n`/a/${id}`, /* verb */{\n method: \"GET\"\n },\n);\n}";
        let commented =
            "function load(id) { return fetch(`/a/${id}`,/* verb */{ method: \"GET\" }); }";

        assert_ne!(program_hash(original), program_hash(reformatted));
        assert_eq!(
            program_hash(original),
            program_hash(&reformatted.replace(",\n);", ");"))
        );
        assert_eq!(program_hash(original), program_hash(commented));
    }

    #[test]
    fn spaces_inside_literals_and_jsx_text_change_the_hash() {
        let pairs = [
            ("const a = \"x y\";", "const a = \"x  y\";"),
            ("const a = `x ${b} y`;", "const a = `x ${b}  y`;"),
            ("const a = /x y/;", "const a = /x  y/;"),
            ("const a = <p>x y</p>;", "const a = <p>x  y</p>;"),
        ];

        for (original, changed) in pairs {
            assert_ne!(program_hash(original), program_hash(changed), "{changed}");
        }
    }

    #[test]
    fn line_breaks_count_where_they_can_end_a_statement() {
        let pairs = [
            (
                "function f() { return\nvalue }",
                "function f() { return value }",
            ),
            ("let a = 1, b = 2; a\n++b", "let a = 1, b = 2; a++\nb"),
            (
                "class A { async\n*gen() {} }",
                "class A { async *gen() {} }",
            ),
            (
                "class A { get\nvalue() { return 1 } }",
                "class A { get value() { return 1 } }",
            ),
        ];

        for (original, changed) in pairs {
            assert_ne!(program_hash(original), program_hash(changed), "{changed}");
        }

        assert_eq!(
            program_hash("const total = price\n  + tax\n  - discount;"),
            program_hash("const total = price + tax - discount;")
        );
    }

    #[test]
    fn masked_names_hash_alike_and_code_changes_do_not() {
        assert_eq!(
            hash_masking(
                "function createChannel(id) { return id; }",
                Some("createChannel")
            ),
            hash_masking(
                "function openChannel(id) { return id; }",
                Some("openChannel")
            )
        );
        assert_ne!(
            hash_masking(
                "function createChannel(id) { return id; }",
                Some("createChannel")
            ),
            hash_masking(
                "function createChannel(id) { return id + 1; }",
                Some("createChannel")
            )
        );
    }
}
