use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Render Markdown into compact readable plain text.
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
                | TagEnd::CodeBlock,
            ) => push_newline(&mut out),
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
