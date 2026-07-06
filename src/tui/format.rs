//! Pure formatting/layout helpers for the TUI (SPEC-UX B2/B3/B4): text
//! wrapping, diff-line classification for the confirmation modal, and
//! scroll-offset clamping. Kept free of any rendering/terminal dependency so
//! they're directly unit-testable (SPEC-UX B4).

/// Word-wraps `text` to `width` columns (at least 1). Existing newlines are
/// preserved as hard breaks; long words with no spaces are hard-split at
/// `width` so a single pathological token can't blow out the layout.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw_line.split(' ') {
            let mut remaining: String = word.to_string();
            loop {
                let candidate_len = if current.is_empty() {
                    remaining.chars().count()
                } else {
                    current.chars().count() + 1 + remaining.chars().count()
                };
                if candidate_len <= width {
                    if !current.is_empty() {
                        current.push(' ');
                    }
                    current.push_str(&remaining);
                    break;
                }
                if current.is_empty() {
                    // Word alone is wider than `width`: hard-split it.
                    let split_at = width.min(remaining.chars().count());
                    let head: String = remaining.chars().take(split_at).collect();
                    let tail: String = remaining.chars().skip(split_at).collect();
                    out.push(head);
                    if tail.is_empty() {
                        break;
                    }
                    remaining = tail;
                    continue;
                }
                out.push(std::mem::take(&mut current));
            }
        }
        out.push(current);
    }
    out
}

/// How one line of a unified diff should be colored in the confirmation
/// modal (SPEC-UX B3): add lines green, del lines red, everything else
/// (context, `@@` hunks, headers) the default color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Add,
    Del,
    Meta,
    Context,
}

/// Classifies one line of a unified diff produced by `similar`'s
/// `unified_diff()` (SPEC-UX B3). A pure function so the modal's coloring is
/// unit-testable without a terminal.
pub fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
        DiffLineKind::Meta
    } else if line.starts_with('+') {
        DiffLineKind::Add
    } else if line.starts_with('-') {
        DiffLineKind::Del
    } else {
        DiffLineKind::Context
    }
}

/// Splits a diff into `(kind, line)` pairs for the modal, in order.
pub fn diff_lines_for_modal(diff: &str) -> Vec<(DiffLineKind, &str)> {
    diff.lines().map(|l| (classify_diff_line(l), l)).collect()
}

/// Clamps a scroll offset (lines scrolled up from the bottom) so it never
/// exceeds the amount of content that can usefully be scrolled past
/// (SPEC-UX B2: PgUp/PgDn/mouse wheel/auto-scroll math).
pub fn clamp_scroll(total_lines: usize, viewport_height: usize, requested_offset: usize) -> usize {
    let max_offset = total_lines.saturating_sub(viewport_height);
    requested_offset.min(max_offset)
}

/// Whether the viewport is currently at the bottom (no lines scrolled up) --
/// used to decide whether auto-scroll should keep following new content
/// (SPEC-UX B2).
pub fn is_at_bottom(scroll_offset: usize) -> bool {
    scroll_offset == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_text_breaks_at_width() {
        let wrapped = wrap_text("the quick brown fox jumps", 10);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 10));
        assert_eq!(wrapped.join(" "), "the quick brown fox jumps");
    }

    #[test]
    fn wrap_text_preserves_existing_newlines() {
        let wrapped = wrap_text("line one\nline two", 80);
        assert_eq!(wrapped, vec!["line one", "line two"]);
    }

    #[test]
    fn wrap_text_hard_splits_a_word_wider_than_width() {
        let wrapped = wrap_text("aaaaaaaaaaaaaaaa", 5);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 5));
        assert_eq!(wrapped.concat(), "aaaaaaaaaaaaaaaa");
    }

    #[test]
    fn wrap_text_empty_line_preserved() {
        let wrapped = wrap_text("a\n\nb", 10);
        assert_eq!(wrapped, vec!["a", "", "b"]);
    }

    #[test]
    fn classify_diff_line_detects_add_del_meta_context() {
        assert_eq!(classify_diff_line("+new line"), DiffLineKind::Add);
        assert_eq!(classify_diff_line("-old line"), DiffLineKind::Del);
        assert_eq!(classify_diff_line("@@ -1,2 +1,2 @@"), DiffLineKind::Meta);
        assert_eq!(classify_diff_line("+++ b/file"), DiffLineKind::Meta);
        assert_eq!(classify_diff_line(" unchanged"), DiffLineKind::Context);
    }

    #[test]
    fn diff_lines_for_modal_preserves_order_and_count() {
        let diff = "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n";
        let lines = diff_lines_for_modal(diff);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[3].0, DiffLineKind::Del);
        assert_eq!(lines[4].0, DiffLineKind::Add);
    }

    #[test]
    fn clamp_scroll_never_exceeds_available_content() {
        assert_eq!(clamp_scroll(100, 20, 1000), 80);
        assert_eq!(clamp_scroll(10, 20, 5), 0);
        assert_eq!(clamp_scroll(100, 20, 30), 30);
    }

    #[test]
    fn is_at_bottom_true_only_at_zero_offset() {
        assert!(is_at_bottom(0));
        assert!(!is_at_bottom(1));
    }
}
