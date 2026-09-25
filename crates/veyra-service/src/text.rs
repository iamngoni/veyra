//! Tiny text helper shared by routes and assistant tools that must clip
//! recorded strings (rationales, reasons, comments) to a bounded length
//! before they reach a response.

/// Clips `text` to `max` characters, marking a cut with `…`. Character
/// boundaries are respected so multi-byte text is never split mid-codepoint.
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_respects_character_boundaries() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("ééééé", 3), "ééé…");
        assert_eq!(clip("", 0), "");
        assert_eq!(clip("abc", 0), "…");
    }
}
