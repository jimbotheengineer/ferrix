//! TSV clipboard interchange.
//!
//! Tab-separated values is the lingua franca of spreadsheet clipboards: Excel,
//! Google Sheets, LibreOffice, and Numbers all put TSV on the system clipboard
//! when you copy a range, and all accept it back. Supporting it means copy and
//! paste work *between* applications, not just within Ferrix.
//!
//! ## Quoting
//!
//! A cell containing a tab, newline, or double quote is wrapped in double
//! quotes with internal quotes doubled — the same convention as CSV, which is
//! what every spreadsheet expects. Cells without those characters are written
//! bare, so the common case stays byte-identical to what the user sees.
//!
//! ## Line endings
//!
//! Written with `\r\n`, which is what Excel produces and what Windows
//! clipboard consumers expect. Parsing accepts `\r\n`, `\n`, or `\r` so text
//! pasted from any platform works.

/// Serialize a rectangular block of display strings as TSV.
pub fn to_tsv(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        for (j, cell) in row.iter().enumerate() {
            if j > 0 {
                out.push('\t');
            }
            push_field(&mut out, cell);
        }
    }
    out
}

fn push_field(out: &mut String, s: &str) {
    let needs_quotes = s.contains('\t') || s.contains('\n') || s.contains('\r') || s.contains('"');
    if !needs_quotes {
        out.push_str(s);
        return;
    }
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
}

/// Parse TSV into a rectangular block.
///
/// Rows are padded to the width of the widest row, so a ragged paste still
/// produces a rectangle and callers can index without bounds checks.
pub fn from_tsv(text: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                // A doubled quote is a literal quote; a lone one ends the field.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if field.is_empty() => in_quotes = true,
            '\t' => row.push(std::mem::take(&mut field)),
            '\r' | '\n' => {
                // Consume the \n of a \r\n pair so it is not read as a second
                // line break.
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(ch),
        }
    }

    // Trailing field / row, unless the text ended exactly on a line break.
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }

    // Pad ragged rows so the result is a true rectangle.
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    for r in &mut rows {
        r.resize(width, String::new());
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn plain_block_roundtrips() {
        let b = block(&[&["a", "b"], &["c", "d"]]);
        let tsv = to_tsv(&b);
        assert_eq!(tsv, "a\tb\r\nc\td");
        assert_eq!(from_tsv(&tsv), b);
    }

    #[test]
    fn single_cell_has_no_separators() {
        assert_eq!(to_tsv(&block(&[&["42"]])), "42");
        assert_eq!(from_tsv("42"), block(&[&["42"]]));
    }

    #[test]
    fn empty_cells_survive() {
        let b = block(&[&["a", "", "c"]]);
        let tsv = to_tsv(&b);
        assert_eq!(tsv, "a\t\tc");
        assert_eq!(from_tsv(&tsv), b);
    }

    #[test]
    fn tabs_and_newlines_are_quoted() {
        // Without quoting these would silently become extra cells and rows —
        // the classic clipboard corruption bug.
        let b = block(&[&["has\ttab", "has\nnewline"]]);
        let tsv = to_tsv(&b);
        assert!(tsv.starts_with("\"has\ttab\"\t\"has\nnewline\""));
        assert_eq!(from_tsv(&tsv), b);
    }

    #[test]
    fn quotes_are_doubled() {
        let b = block(&[&["say \"hi\""]]);
        let tsv = to_tsv(&b);
        assert_eq!(tsv, "\"say \"\"hi\"\"\"");
        assert_eq!(from_tsv(&tsv), b);
    }

    #[test]
    fn accepts_unix_and_mac_line_endings() {
        // Pasting from a Linux or classic-Mac source must not merge rows.
        assert_eq!(from_tsv("a\tb\nc\td"), block(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(from_tsv("a\tb\rc\td"), block(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(
            from_tsv("a\tb\r\nc\td"),
            block(&[&["a", "b"], &["c", "d"]]),
            "\\r\\n must be one break, not two"
        );
    }

    #[test]
    fn trailing_newline_does_not_add_a_row() {
        assert_eq!(from_tsv("a\tb\r\n").len(), 1);
        assert_eq!(from_tsv("a\tb\n").len(), 1);
    }

    #[test]
    fn ragged_input_is_padded_to_a_rectangle() {
        let got = from_tsv("a\tb\tc\r\nd");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].len(), 3);
        assert_eq!(got[1], vec!["d", "", ""], "short row padded");
    }

    #[test]
    fn excel_style_paste_parses() {
        // Shape Excel actually puts on the clipboard: CRLF rows, bare fields,
        // quoted only where required.
        let excel = "Name\tQty\r\nWidget\t10\r\n\"Gadget, large\"\t5\r\n";
        let got = from_tsv(excel);
        assert_eq!(got.len(), 3);
        assert_eq!(got[2][0], "Gadget, large", "comma needs no quoting in TSV");
        assert_eq!(got[2][1], "5");
    }
}
