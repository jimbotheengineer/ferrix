//! Tests for the sparse cell-comment store.

use super::*;

fn c(row: u32, col: u32) -> CellRef {
    CellRef::new(row, col)
}

#[test]
fn empty_map_answers_nothing_and_probes_nothing() {
    let m = CommentMap::new();
    assert!(m.is_empty());
    assert_eq!(m.get(c(0, 0)), None);
    assert_eq!(m.row_comments(0), None);
    // The scale invariant on the paint path: a sheet with no comments must
    // not touch the map at all. If this ever becomes non-zero, every visible
    // cell of every uncommented sheet is paying for a feature it does not use.
    assert_eq!(
        m.probes(),
        0,
        "an empty comment map must cost zero map probes"
    );
}

#[test]
fn set_and_get_roundtrip() {
    let mut m = CommentMap::new();
    assert_eq!(m.set(c(3, 4), Comment::new("ana", "check this")), None);
    assert_eq!(m.len(), 1);
    let got = m.get(c(3, 4)).expect("comment is there");
    assert_eq!(got.author, "ana");
    assert_eq!(got.text, "check this");
}

#[test]
fn setting_the_same_cell_edits_rather_than_duplicates() {
    let mut m = CommentMap::new();
    m.set(c(1, 1), Comment::new("ana", "first"));
    let prev = m.set(c(1, 1), Comment::new("bo", "second"));
    assert_eq!(prev, Some(Comment::new("ana", "first")));
    assert_eq!(
        m.len(),
        1,
        "an edit must not leave two comments on one cell"
    );
    assert_eq!(m.get(c(1, 1)).unwrap().text, "second");
}

#[test]
fn remove_deletes_the_comment_and_the_row_entry() {
    let mut m = CommentMap::new();
    m.set(c(9, 2), Comment::new("ana", "note"));
    let gone = m.remove(c(9, 2));
    assert_eq!(gone, Some(Comment::new("ana", "note")));
    assert!(m.is_empty());
    assert_eq!(m.get(c(9, 2)), None);
    assert_eq!(m.row_comments(9), None, "an emptied row must not linger");
    assert_eq!(m.remove(c(9, 2)), None, "removing twice is not an error");
}

#[test]
fn restore_is_a_faithful_undo_in_both_directions() {
    let mut m = CommentMap::new();
    let prev = m.set(c(0, 0), Comment::new("ana", "v1"));
    m.restore(c(0, 0), prev);
    assert!(m.is_empty(), "undoing an insert must remove it entirely");

    m.set(c(0, 0), Comment::new("ana", "v1"));
    let prev = m.set(c(0, 0), Comment::new("ana", "v2"));
    m.restore(c(0, 0), prev);
    assert_eq!(m.get(c(0, 0)).unwrap().text, "v1");
}

#[test]
fn several_comments_share_one_row_in_column_order() {
    let mut m = CommentMap::new();
    m.set(c(5, 7), Comment::new("a", "seven"));
    m.set(c(5, 2), Comment::new("a", "two"));
    m.set(c(5, 4), Comment::new("a", "four"));
    let row = m.row_comments(5).expect("row 5 has comments");
    assert_eq!(
        row.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        vec![2, 4, 7],
        "a row's comments must stay sorted so lookup can binary search"
    );
    assert_eq!(m.get(c(5, 4)).unwrap().text, "four");
}

#[test]
fn iteration_is_in_row_then_column_order_for_reproducible_saves() {
    let mut m = CommentMap::new();
    for (r, c_) in [(4u32, 1u32), (0, 3), (4, 0), (0, 1)] {
        m.set(c(r, c_), Comment::new("a", format!("{r},{c_}")));
    }
    let seen: Vec<(u32, u32)> = m.iter().map(|(cell, _)| (cell.row, cell.col)).collect();
    assert_eq!(seen, vec![(0, 1), (0, 3), (4, 0), (4, 1)]);
}

#[test]
fn a_200m_row_sheet_with_three_comments_stores_exactly_three_entries() {
    // The scale claim, stated as the roadmap states it. Comments are sparse:
    // cost is O(comments), never O(rows). A per-cell field would need 200M
    // slots to say the same three things.
    let mut m = CommentMap::new();
    m.set(c(0, 0), Comment::new("ana", "first row"));
    m.set(c(99_999_999, 3), Comment::new("bo", "halfway"));
    m.set(c(199_999_999, 7), Comment::new("cy", "last row"));

    assert_eq!(m.len(), 3, "three comments must cost three entries");
    assert!(
        m.heap_bytes() < 2_000,
        "three comments over a 200M-row sheet cost {} bytes",
        m.heap_bytes()
    );
    // Deep row indices must survive the u32 keying intact.
    assert_eq!(m.get(c(199_999_999, 7)).unwrap().author, "cy");
    assert_eq!(m.get(c(199_999_998, 7)), None);
}

#[test]
fn a_cell_with_no_comment_costs_no_lookup_work_on_the_paint_path() {
    // Two claims, both about what a frame costs.
    //
    // 1. A sheet with NO comments must not probe the map once, however many
    //    cells are painted. `is_empty()` is the short-circuit the grid uses.
    let empty = CommentMap::new();
    for row in 0..40u32 {
        for col in 0..40u32 {
            if !empty.is_empty() {
                let _ = empty.get(c(row, col));
            }
        }
    }
    assert_eq!(
        empty.probes(),
        0,
        "1,600 painted cells on an uncommented sheet must cost zero probes"
    );

    // 2. On a sheet that DOES have comments, the cost is bounded by visible
    //    ROWS, not visible CELLS — the caller hoists `row_comments` out of its
    //    column loop. A per-cell `get()` would be 1,600 probes here.
    let mut m = CommentMap::new();
    m.set(c(3, 3), Comment::new("ana", "hi"));
    m.reset_probes();
    let mut found = 0;
    for row in 0..40u32 {
        let row_notes = m.row_comments(row);
        for col in 0..40u32 {
            if let Some(notes) = row_notes {
                if notes.binary_search_by_key(&col, |(c, _)| *c).is_ok() {
                    found += 1;
                }
            }
        }
    }
    assert_eq!(found, 1);
    assert_eq!(
        m.probes(),
        40,
        "the paint path must probe once per visible ROW, not once per cell"
    );
}

#[test]
fn remap_columns_moves_comments_without_colliding() {
    // A rotation: 0->1, 1->2, 2->0. Applied naively one at a time this
    // clobbers; the two-phase remap must land all three intact.
    let mut m = CommentMap::new();
    m.set(c(0, 0), Comment::new("a", "was col 0"));
    m.set(c(0, 1), Comment::new("a", "was col 1"));
    m.set(c(0, 2), Comment::new("a", "was col 2"));

    let map = std::collections::HashMap::from([(0u32, 1u32), (1, 2), (2, 0)]);
    m.remap_columns(&map);

    assert_eq!(
        m.len(),
        3,
        "a permutation must not lose or duplicate a note"
    );
    assert_eq!(m.get(c(0, 1)).unwrap().text, "was col 0");
    assert_eq!(m.get(c(0, 2)).unwrap().text, "was col 1");
    assert_eq!(m.get(c(0, 0)).unwrap().text, "was col 2");
}

#[test]
fn remap_leaves_untouched_columns_alone() {
    let mut m = CommentMap::new();
    m.set(c(2, 5), Comment::new("a", "stay"));
    m.set(c(2, 0), Comment::new("a", "move"));
    let map = std::collections::HashMap::from([(0u32, 3u32)]);
    m.remap_columns(&map);
    assert_eq!(m.get(c(2, 5)).unwrap().text, "stay");
    assert_eq!(m.get(c(2, 3)).unwrap().text, "move");
    assert_eq!(m.get(c(2, 0)), None);
}

#[test]
fn overlong_text_is_clamped_at_the_door() {
    // Refused here rather than mangled at export: a note the xlsx writer would
    // reject must never make it into the store.
    let mut m = CommentMap::new();
    let huge = "x".repeat(MAX_COMMENT_CHARS + 500);
    m.set(c(0, 0), Comment::new("a", huge));
    assert_eq!(
        m.get(c(0, 0)).unwrap().text.chars().count(),
        MAX_COMMENT_CHARS
    );
}

#[test]
fn clamping_respects_character_boundaries() {
    let mut m = CommentMap::new();
    // Multi-byte characters: a byte-wise cut here would panic or produce
    // invalid UTF-8.
    let huge: String = std::iter::repeat_n('é', MAX_COMMENT_CHARS + 10).collect();
    m.set(c(1, 1), Comment::new("a", huge));
    assert_eq!(
        m.get(c(1, 1)).unwrap().text.chars().count(),
        MAX_COMMENT_CHARS
    );
}

#[test]
fn equality_ignores_the_probe_counter() {
    let mut a = CommentMap::new();
    a.set(c(0, 0), Comment::new("x", "y"));
    let b = a.clone();
    let _ = a.get(c(0, 0));
    let _ = a.get(c(9, 9));
    assert_eq!(a, b, "reading a map must not change what it equals");
}
