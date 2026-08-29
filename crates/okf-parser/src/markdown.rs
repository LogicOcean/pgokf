// SPDX-License-Identifier: AGPL-3.0-only
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Render Markdown into compact readable plain text.
///
/// Block boundaries (paragraphs, headings, list items, blockquotes, code
/// blocks, table rows) become single newlines, table cells are separated by
/// spaces so words never fuse across cells, and horizontal rules are kept as
/// a literal `---` line.
#[must_use]
pub fn plain_text(markdown: &str) -> String {
    let mut out = String::new();
    for event in Parser::new_ext(markdown, Options::all()) {
        match event {
            Event::Text(text) | Event::Code(text) => out.push_str(&text),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::Item
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::TableHead
                | TagEnd::TableRow,
            ) => push_newline(&mut out),
            Event::End(TagEnd::TableCell) => out.push(' '),
            Event::Rule => {
                push_newline(&mut out);
                out.push_str("---");
                push_newline(&mut out);
            }

            Event::Start(Tag::Item) if !out.ends_with('\n') && !out.is_empty() => {
                push_newline(&mut out);
            }
            _ => {}
        }
    }

    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::plain_text;

    #[test]
    fn plain_text_separates_table_cells_and_rows() {
        let markdown = "| Region | Status |\n| --- | --- |\n| east | up |\n| west | down |";

        let text = plain_text(markdown);

        assert_eq!(text, "Region Status\neast up\nwest down");
    }

    #[test]
    fn plain_text_preserves_code_fence_content() {
        let markdown = "Before\n\n```sql\nSELECT 1;\nSELECT 2;\n```\n\nAfter";

        let text = plain_text(markdown);

        assert_eq!(text, "Before\nSELECT 1;\nSELECT 2;\nAfter");
    }

    #[test]
    fn plain_text_drops_blockquote_markers_and_keeps_block_boundaries() {
        let markdown = "> quoted advice\n> continues here\n\nplain paragraph";

        let text = plain_text(markdown);

        assert_eq!(text, "quoted advice\ncontinues here\nplain paragraph");
    }

    #[test]
    fn plain_text_keeps_horizontal_rules_as_separator_lines() {
        let markdown = "above\n\n---\n\nbelow";

        let text = plain_text(markdown);

        assert_eq!(text, "above\n---\nbelow");
    }

    #[test]
    fn plain_text_returns_empty_string_for_empty_input() {
        assert_eq!(plain_text(""), "");
    }
}
