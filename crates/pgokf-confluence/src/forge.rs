// SPDX-License-Identifier: AGPL-3.0-only
//! Receive Confluence events that a Forge app delivers through Forge Remote.
//!
//! The Forge app in `integrations/confluence-forge` subscribes to page,
//! comment, and task events and has Forge post each one to this companion
//! with a Forge Invocation Token (FIT). [`fit`] verifies the token,
//! [`event`] decodes the delivery body, and [`receiver`] serves the endpoint.
pub mod event;
pub mod fit;
pub mod receiver;

/// `text` with every control character and line separator escaped (`\n`,
/// `\u{1b}`), so a key ID from a token nobody has verified yet, or a value
/// from a delivery body, can neither start a log line of its own nor drive
/// the terminal that shows the log.
pub(crate) fn printable(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
            shown.extend(character.escape_default());
        } else {
            shown.push(character);
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::printable;

    #[test]
    fn printable_escapes_what_could_break_or_drive_a_log_line() {
        // Arrange
        let text = "a\nb\r\tc\u{1b}[2K\u{7f}\u{85}\u{2028}d é ✓";

        // Act
        let shown = printable(text);

        // Assert
        assert_eq!(shown, "a\\nb\\r\\tc\\u{1b}[2K\\u{7f}\\u{85}\\u{2028}d é ✓");
    }
}
