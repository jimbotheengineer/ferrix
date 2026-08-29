//! Excel number-format string parsing and rendering.
//!
//! A format code like `#,##0.00;[Red](#,##0.00);"—";@` is four sections
//! separated by `;`: positive, negative, zero, and text. Excel picks a section
//! by the value's sign, falling back in a specific order when fewer than four
//! are given. Each section is a sequence of literal runs and placeholders.
//!
//! # Why parse into a plan instead of interpreting the string per cell
//!
//! A 200M-row column has ONE format code. Interpreting the string per cell
//! would re-parse it 200 million times to produce the same plan. [`NumFmt`]
//! parses once; rendering walks a small pre-built structure. That is also why
//! the parse result is owned and cheap to clone: it lives beside the column,
//! not inside the cells.
//!
//! # What this deliberately does NOT do
//!
//! Excel's format language has corners that exist for 1990s compatibility and
//! that no import in practice depends on: fraction formats (`# ?/?`),
//! `[DBNum]` CJK numerals, and locale-id prefixes (`[$-409]`). Those parse into
//! [`Section::Passthrough`] so the value still renders as a plain number rather
//! than as a wrong number. Silently rendering `1/2` as `0.5` is a data
//! misrepresentation; rendering it as `0.5` while ADMITTING the code was not
//! modelled is merely a cosmetic gap.

use std::fmt::Write as _;

/// A parsed Excel format code, ready to render values.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct NumFmt {
    /// Sections in source order. Excel allows 1-4.
    sections: Vec<Section>,
}

/// One `;`-delimited section of a format code.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Section {
    /// Colour named by a `[Red]`-style token, if any.
    pub color: Option<FmtColor>,
    /// A `[>1000]`-style condition gating this section.
    pub condition: Option<Condition>,
    /// The literal/placeholder run list.
    parts: Vec<Part>,
    /// True when the code used tokens we do not model; render numerically.
    passthrough: bool,
    /// Whether this section carries an AM/PM token, which makes every hour
    /// token in it a 12-hour clock. Computed once at parse.
    twelve_hour: bool,
}

/// Colours Excel names directly in a format code.
///
/// Excel also allows `[Color 1]`..`[Color 56]` indices into a legacy palette.
/// Those map onto the nearest named colour rather than reproducing the full
/// 56-entry table, because the palette is theme-dependent in modern files and
/// an exact match is not achievable anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FmtColor {
    Black,
    Blue,
    Cyan,
    Green,
    Magenta,
    Red,
    White,
    Yellow,
}

impl FmtColor {
    /// RGB for painting. Chosen to stay legible on both themes rather than
    /// matching Excel's exact palette, which assumes a white background.
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            FmtColor::Black => (0x20, 0x20, 0x20),
            FmtColor::Blue => (0x2f, 0x6f, 0xd0),
            FmtColor::Cyan => (0x2a, 0x9d, 0xa8),
            FmtColor::Green => (0x2e, 0x9e, 0x4f),
            FmtColor::Magenta => (0xb5, 0x4c, 0xa8),
            FmtColor::Red => (0xd6, 0x38, 0x3c),
            FmtColor::White => (0xf0, 0xf0, 0xf0),
            FmtColor::Yellow => (0xc9, 0x9a, 0x1e),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        let lower = s.to_ascii_lowercase();
        Some(match lower.as_str() {
            "black" => FmtColor::Black,
            "blue" => FmtColor::Blue,
            "cyan" => FmtColor::Cyan,
            "green" => FmtColor::Green,
            "magenta" => FmtColor::Magenta,
            "red" => FmtColor::Red,
            "white" => FmtColor::White,
            "yellow" => FmtColor::Yellow,
            _ => {
                // `[Color 15]` — legacy palette index. Map the common ones and
                // fall back to Black rather than refusing the whole code.
                let idx: u8 = lower.strip_prefix("color")?.trim().parse().ok()?;
                match idx {
                    1 => FmtColor::Black,
                    2 => FmtColor::White,
                    3 => FmtColor::Red,
                    4 => FmtColor::Green,
                    5 => FmtColor::Blue,
                    6 => FmtColor::Yellow,
                    7 => FmtColor::Magenta,
                    8 => FmtColor::Cyan,
                    _ => FmtColor::Black,
                }
            }
        })
    }
}

/// A `[>=100]`-style gate on a section.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Condition {
    pub op: CmpOp,
    pub value: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Condition {
    fn holds(&self, v: f64) -> bool {
        match self.op {
            CmpOp::Lt => v < self.value,
            CmpOp::Le => v <= self.value,
            CmpOp::Gt => v > self.value,
            CmpOp::Ge => v >= self.value,
            CmpOp::Eq => v == self.value,
            CmpOp::Ne => v != self.value,
        }
    }
}

/// One element of a section.
#[derive(Clone, PartialEq, Debug)]
enum Part {
    /// Text emitted verbatim.
    Literal(String),
    /// The numeric body: digits, grouping, decimals, and any `%` scaling.
    Number(NumSpec),
    /// A date/time token run.
    DateTime(Vec<DtToken>),
    /// `@` — the raw text of a text cell.
    TextSlot,
    /// `*c` — repeat `c` to fill the column. Rendered as a single space, since
    /// the fill width is a paint-time property the core layer cannot know.
    Fill(char),
    /// `_c` — skip the width of `c`. Rendered as a single space.
    Skip,
}

/// The numeric portion of a section.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct NumSpec {
    /// Minimum integer digits, from `0` placeholders.
    int_zeros: usize,
    /// Minimum decimal digits, from `0` after the point.
    dec_zeros: usize,
    /// Maximum decimal digits, counting `0`, `#`, and `?`.
    dec_max: usize,
    /// Thousands separators requested by a `,` between digit placeholders.
    grouped: bool,
    /// Trailing commas scale by 1000 each: `#,##0,` shows thousands.
    scale_commas: u32,
    /// Percent tokens multiply by 100 (once each).
    percents: u32,
    /// Scientific notation, with the exponent's minimum digits.
    exponent: Option<usize>,
}

/// A date/time token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DtToken {
    Year2,
    Year4,
    Month1,
    Month2,
    MonthAbbr,
    MonthFull,
    Day1,
    Day2,
    DayAbbr,
    DayFull,
    Hour1,
    Hour2,
    Minute1,
    Minute2,
    Second1,
    Second2,
    /// `[h]`, `[m]`, `[s]` — elapsed time, not clamped to a clock.
    ElapsedHour,
    ElapsedMinute,
    ElapsedSecond,
    /// 12-hour marker; renders `AM`/`PM`.
    AmPm,
}

impl NumFmt {
    /// Parse a format code. Never fails: an unmodelled code becomes a
    /// passthrough section so the value still renders as a number.
    ///
    /// Failing loudly here would be wrong — the code arrives from a file the
    /// user did not write, and refusing to display a column because one style
    /// used a fraction format would be worse than showing plain numbers.
    pub fn parse(code: &str) -> Self {
        let mut sections = Vec::new();
        for raw in split_sections(code) {
            sections.push(parse_section(&raw));
        }
        if sections.is_empty() {
            sections.push(Section::default());
        }
        NumFmt { sections }
    }

    /// Does this code model anything beyond a plain number?
    pub fn is_trivial(&self) -> bool {
        self.sections.len() == 1 && self.sections[0].passthrough
    }

    /// Choose the section Excel would use for `v`.
    ///
    /// Excel's rules: with conditions present, the first matching condition
    /// wins and the last section is the "else". Without conditions, sections
    /// are positive/negative/zero/text, and a missing section falls back to
    /// the positive one — except that a two-section code uses section 0 for
    /// zero as well, which is why `>= 0.0` is the test rather than `> 0.0`.
    pub fn section_for(&self, v: f64) -> &Section {
        let conditional = self.sections.iter().any(|s| s.condition.is_some());
        if conditional {
            for s in &self.sections {
                match s.condition {
                    Some(c) if c.holds(v) => return s,
                    None => return s,
                    _ => {}
                }
            }
            return self.sections.last().expect("at least one section");
        }
        let n = self.sections.len();
        if v > 0.0 || (v == 0.0 && n < 3) {
            &self.sections[0]
        } else if v < 0.0 {
            self.sections.get(1).unwrap_or(&self.sections[0])
        } else {
            self.sections.get(2).unwrap_or(&self.sections[0])
        }
    }

    /// Render a number.
    pub fn render(&self, v: f64) -> String {
        let section = self.section_for(v);
        section.render_number(v, self.negative_needs_sign(v))
    }

    /// Render the text section (`@`) for a text cell, or return the input
    /// unchanged when the code has no text section.
    pub fn render_text(&self, s: &str) -> String {
        // Text uses the 4th section when present; otherwise text passes
        // through untouched. A numeric section must NOT be applied to text.
        let Some(sec) = self.sections.get(3) else {
            return s.to_string();
        };
        let mut out = String::new();
        for p in &sec.parts {
            match p {
                Part::Literal(l) => out.push_str(l),
                Part::TextSlot => out.push_str(s),
                Part::Fill(_) | Part::Skip => out.push(' '),
                _ => {}
            }
        }
        if out.is_empty() {
            s.to_string()
        } else {
            out
        }
    }

    /// Colour for a value, if its section names one.
    pub fn color_for(&self, v: f64) -> Option<FmtColor> {
        self.section_for(v).color
    }

    /// Whether the chosen section already spells the sign itself.
    ///
    /// A code like `0.00;(0.00)` puts negatives in parentheses and must NOT
    /// also emit a minus, or the user sees `(-5.00)`. But `0.00` alone has no
    /// negative section, so the fallback DOES need the sign.
    fn negative_needs_sign(&self, v: f64) -> bool {
        if v >= 0.0 {
            return false;
        }
        let conditional = self.sections.iter().any(|s| s.condition.is_some());
        if conditional {
            // A conditional code is explicit about what it prints; if the
            // matching section has no literal minus, Excel still shows one.
            // Only add a sign when the matching section actually renders a
            // number; a section that is pure text (an "else" like "neg")
            // must not be prefixed with a minus.
            let sec = self.section_for(v);
            return sec.renders_a_number() && !sec.has_literal_sign();
        }
        // Only a dedicated negative section suppresses the automatic sign.
        self.sections.len() < 2
    }
}

impl Section {
    /// Does this section actually format the value, or is it pure literal?
    fn renders_a_number(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, Part::Number(_) | Part::DateTime(_)))
    }

    fn has_literal_sign(&self) -> bool {
        self.parts.iter().any(|p| match p {
            Part::Literal(l) => l.contains('-') || l.contains('(') || l.contains('\u{2212}'),
            _ => false,
        })
    }

    /// Render `v` through this section's parts.
    fn render_number(&self, v: f64, need_sign: bool) -> String {
        if self.passthrough {
            return crate::value::format_number(v);
        }
        let mut out = String::new();
        if need_sign && v < 0.0 {
            out.push('-');
        }
        // The magnitude is what placeholders format; the sign is handled
        // above or spelled by the section's own literals.
        let mag = v.abs();
        for p in &self.parts {
            match p {
                Part::Literal(l) => out.push_str(l),
                Part::Number(spec) => out.push_str(&spec.render(mag)),
                Part::DateTime(toks) => {
                    // The 12-hour decision belongs to the whole section: an hour
                    // token in one part must know about an AM/PM token in another.
                    out.push_str(&render_datetime_ctx(v, toks, self.twelve_hour))
                }
                Part::TextSlot => {}
                Part::Fill(_) | Part::Skip => out.push(' '),
            }
        }
        out
    }
}

impl NumSpec {
    fn render(&self, v: f64) -> String {
        let mut value = v;
        for _ in 0..self.percents {
            value *= 100.0;
        }
        for _ in 0..self.scale_commas {
            value /= 1000.0;
        }

        if let Some(exp_digits) = self.exponent {
            return self.render_scientific(value, exp_digits);
        }

        // Round to the maximum decimal places, then trim back to the minimum:
        // `#.##` on 1.5 is "1.5", not "1.50", while `0.00` on 1.5 is "1.50".
        // Excel rounds half AWAY FROM ZERO; Rust's formatter rounds half to
        // even. Without this, 1.25 at one decimal renders "1.2" in Ferrix and
        // "1.3" in Excel -- a silent disagreement on a very common value.
        let s = format_half_up(value, self.dec_max);
        let (int_part, dec_part) = match s.split_once('.') {
            Some((i, d)) => (i.to_string(), d.to_string()),
            None => (s, String::new()),
        };

        let mut int_digits = int_part.trim_start_matches('-').to_string();
        while int_digits.len() < self.int_zeros {
            int_digits.insert(0, '0');
        }
        // `#,##0` with no integer digits still shows a lone zero; `#` alone
        // shows nothing for a zero integer part, matching Excel.
        // A `#` with no `0` shows nothing for a zero integer part -- but only
        // when the fraction is also empty, otherwise `#.##` on 0.5 would lose
        // its leading position entirely and render ".5".
        if int_digits == "0" && self.int_zeros == 0 {
            int_digits.clear();
        }
        if self.grouped {
            int_digits = group3(&int_digits);
        }

        let mut dec_trimmed = dec_part;
        while dec_trimmed.len() > self.dec_zeros && dec_trimmed.ends_with('0') {
            dec_trimmed.pop();
        }

        let mut out = int_digits;
        if !dec_trimmed.is_empty() {
            out.push('.');
            out.push_str(&dec_trimmed);
        }
        // `#` alone deliberately renders a zero as nothing (Excel does this),
        // so only supply a fallback zero when the code asked for a digit.
        if out.is_empty() && self.int_zeros > 0 {
            out.push('0');
        }
        out
    }

    fn render_scientific(&self, v: f64, exp_digits: usize) -> String {
        if v == 0.0 {
            let mut s = format!("{:.*}", self.dec_max, 0.0);
            let _ = write!(s, "E+{}", "0".repeat(exp_digits.max(1)));
            return s;
        }
        let exp = v.abs().log10().floor() as i32;
        let mantissa = v / 10f64.powi(exp);
        let mut s = format!("{:.*}", self.dec_max, mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        let _ = write!(s, "E{sign}{:0width$}", exp.abs(), width = exp_digits.max(1));
        s
    }
}

/// Split on `;` while respecting quoted literals, `[...]` blocks, and `\`
/// escapes — a `;` inside any of those is not a section break.
fn split_sections(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut in_bracket = false;
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '"' => {
                in_quote = !in_quote;
                cur.push(c);
            }
            '[' if !in_quote => {
                in_bracket = true;
                cur.push(c);
            }
            ']' if !in_quote => {
                in_bracket = false;
                cur.push(c);
            }
            ';' if !in_quote && !in_bracket => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn parse_section(src: &str) -> Section {
    let mut sec = Section::default();
    let mut parts: Vec<Part> = Vec::new();
    let mut literal = String::new();
    let mut spec = NumSpec::default();
    let mut saw_number = false;
    let mut in_decimals = false;
    let mut dt: Vec<DtToken> = Vec::new();
    let mut unmodelled = false;

    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;

    // A `m` means minute when it neighbours an hour or second token, and month
    // otherwise. Resolved in a second pass once all tokens are known.
    let mut minute_candidates: Vec<usize> = Vec::new();

    let flush_literal = |parts: &mut Vec<Part>, literal: &mut String| {
        if !literal.is_empty() {
            parts.push(Part::Literal(std::mem::take(literal)));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                // Escaped literal character.
                if i + 1 < chars.len() {
                    literal.push(chars[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    literal.push(chars[i]);
                    i += 1;
                }
                i += 1; // closing quote
                continue;
            }
            '[' => {
                let close = chars[i..].iter().position(|&x| x == ']').map(|p| p + i);
                let Some(close) = close else {
                    i += 1;
                    continue;
                };
                let inner: String = chars[i + 1..close].iter().collect();
                let t = inner.trim();
                if let Some(col) = FmtColor::parse(t) {
                    sec.color = Some(col);
                } else if let Some(cond) = parse_condition(t) {
                    sec.condition = Some(cond);
                } else {
                    // Elapsed-time tokens.
                    let lower = t.to_ascii_lowercase();
                    match lower.as_str() {
                        s if s.chars().all(|c| c == 'h') && !s.is_empty() => {
                            flush_literal(&mut parts, &mut literal);
                            dt.push(DtToken::ElapsedHour);
                            parts.push(Part::DateTime(vec![DtToken::ElapsedHour]));
                        }
                        s if s.chars().all(|c| c == 'm') && !s.is_empty() => {
                            flush_literal(&mut parts, &mut literal);
                            dt.push(DtToken::ElapsedMinute);
                            parts.push(Part::DateTime(vec![DtToken::ElapsedMinute]));
                        }
                        s if s.chars().all(|c| c == 's') && !s.is_empty() => {
                            flush_literal(&mut parts, &mut literal);
                            dt.push(DtToken::ElapsedSecond);
                            parts.push(Part::DateTime(vec![DtToken::ElapsedSecond]));
                        }
                        // `[$-409]` locale ids and `[$€-2]` currency blocks:
                        // the currency symbol between `$` and `-` is real
                        // content, the locale id is not.
                        s if s.starts_with('$') => {
                            let sym: String =
                                s[1..].split('-').next().unwrap_or("").trim().to_string();
                            if !sym.is_empty() {
                                literal.push_str(&sym);
                            }
                        }
                        _ => unmodelled = true,
                    }
                }
                i = close + 1;
                continue;
            }
            '0' | '#' | '?' => {
                if !saw_number {
                    // Mark the numeric body's position the first time a digit
                    // placeholder appears, so literals before and after it stay
                    // on the correct side. Splicing it in afterwards is what
                    // produced " units7" instead of "7 units".
                    flush_literal(&mut parts, &mut literal);
                    parts.push(Part::Number(NumSpec::default()));
                }
                saw_number = true;
                if in_decimals {
                    spec.dec_max += 1;
                    if c == '0' {
                        spec.dec_zeros += 1;
                    }
                } else if c == '0' {
                    spec.int_zeros += 1;
                }
                i += 1;
                continue;
            }
            '.' => {
                if saw_number || i + 1 < chars.len() {
                    in_decimals = true;
                    i += 1;
                    continue;
                }
                literal.push(c);
                i += 1;
                continue;
            }
            ',' => {
                // A comma among integer placeholders means grouping; trailing
                // commas after all digits scale by 1000 each.
                let rest_has_digit = chars[i + 1..].iter().any(|&x| matches!(x, '0' | '#' | '?'));
                if rest_has_digit {
                    spec.grouped = true;
                } else if saw_number {
                    spec.scale_commas += 1;
                } else {
                    literal.push(c);
                }
                i += 1;
                continue;
            }
            '%' => {
                spec.percents += 1;
                literal.push('%');
                i += 1;
                continue;
            }
            'E' | 'e' => {
                // `E+00` / `E-00` scientific notation.
                let next = chars.get(i + 1).copied();
                if matches!(next, Some('+') | Some('-')) {
                    let mut j = i + 2;
                    let mut digits = 0;
                    while j < chars.len() && matches!(chars[j], '0' | '#' | '?') {
                        digits += 1;
                        j += 1;
                    }
                    spec.exponent = Some(digits);
                    saw_number = true;
                    i = j;
                    continue;
                }
                literal.push(c);
                i += 1;
                continue;
            }
            '@' => {
                flush_literal(&mut parts, &mut literal);
                parts.push(Part::TextSlot);
                i += 1;
                continue;
            }
            '*' => {
                if i + 1 < chars.len() {
                    flush_literal(&mut parts, &mut literal);
                    parts.push(Part::Fill(chars[i + 1]));
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            '_' => {
                flush_literal(&mut parts, &mut literal);
                parts.push(Part::Skip);
                i += if i + 1 < chars.len() { 2 } else { 1 };
                continue;
            }
            '/' => {
                // A `/` between digit placeholders is a fraction format, which
                // this engine does not model. Everything else is a literal
                // (date separators reach here too).
                literal.push(c);
                i += 1;
                continue;
            }
            _ => {}
        }

        // Date/time token runs.
        let lower = c.to_ascii_lowercase();
        if matches!(lower, 'y' | 'm' | 'd' | 'h' | 's' | 'a') {
            let mut j = i;
            while j < chars.len() && chars[j].to_ascii_lowercase() == lower {
                j += 1;
            }
            let run = j - i;

            // `AM/PM` is a single token spelled across several characters.
            if lower == 'a' {
                let tail: String = chars[i..].iter().collect::<String>().to_ascii_uppercase();
                if tail.starts_with("AM/PM") {
                    flush_literal(&mut parts, &mut literal);
                    dt.push(DtToken::AmPm);
                    parts.push(Part::DateTime(vec![DtToken::AmPm]));
                    i += 5;
                    continue;
                }
                if tail.starts_with("A/P") {
                    flush_literal(&mut parts, &mut literal);
                    dt.push(DtToken::AmPm);
                    parts.push(Part::DateTime(vec![DtToken::AmPm]));
                    i += 3;
                    continue;
                }
                literal.push(c);
                i += 1;
                continue;
            }

            let tok = match (lower, run) {
                ('y', 1..=2) => DtToken::Year2,
                ('y', _) => DtToken::Year4,
                ('m', 1) => {
                    minute_candidates.push(dt.len());
                    DtToken::Month1
                }
                ('m', 2) => {
                    minute_candidates.push(dt.len());
                    DtToken::Month2
                }
                ('m', 3) => DtToken::MonthAbbr,
                ('m', _) => DtToken::MonthFull,
                ('d', 1) => DtToken::Day1,
                ('d', 2) => DtToken::Day2,
                ('d', 3) => DtToken::DayAbbr,
                ('d', _) => DtToken::DayFull,
                ('h', 1) => DtToken::Hour1,
                ('h', _) => DtToken::Hour2,
                ('s', 1) => DtToken::Second1,
                ('s', _) => DtToken::Second2,
                _ => unreachable!("token run guarded by the match above"),
            };
            // Push a date part in place so the separators between runs keep
            // their positions. Collecting all tokens and prepending them is
            // what rendered "yyyy-mm-dd" as "20230315--".
            flush_literal(&mut parts, &mut literal);
            dt.push(tok);
            parts.push(Part::DateTime(vec![tok]));
            i = j;
            continue;
        }

        literal.push(c);
        i += 1;
    }

    // Resolve `m` to minutes when adjacent to an hour or second token. Excel
    // decides this positionally, and getting it wrong turns a timestamp's
    // minutes into a month silently.
    for &idx in &minute_candidates {
        let prev_is_time = idx
            .checked_sub(1)
            .and_then(|p| dt.get(p))
            .is_some_and(|t| matches!(t, DtToken::Hour1 | DtToken::Hour2 | DtToken::ElapsedHour));
        let next_is_sec = dt
            .get(idx + 1)
            .is_some_and(|t| matches!(t, DtToken::Second1 | DtToken::Second2));
        if prev_is_time || next_is_sec {
            dt[idx] = match dt[idx] {
                DtToken::Month1 => DtToken::Minute1,
                DtToken::Month2 => DtToken::Minute2,
                other => other,
            };
        }
    }

    if !literal.is_empty() {
        parts.push(Part::Literal(std::mem::take(&mut literal)));
    }

    // A section is either date/time-shaped or number-shaped. Mixing both in
    // one section is not something Excel produces, and guessing would render
    // a serial date as a plain number or vice versa.
    let had_dates = !dt.is_empty();
    if had_dates {
        // Minute/month disambiguation rewrote `dt`; copy the resolved tokens
        // back onto the in-place parts, which are in the same order.
        let mut k = 0usize;
        for p in parts.iter_mut() {
            if let Part::DateTime(v) = p {
                if let Some(t) = dt.get(k) {
                    *v = vec![*t];
                }
                k += 1;
            }
        }
    } else if saw_number {
        // Fill in the placeholder dropped at the first digit, keeping its
        // position among the literals.
        for p in parts.iter_mut() {
            if matches!(p, Part::Number(_)) {
                *p = Part::Number(spec.clone());
                break;
            }
        }
    }

    sec.twelve_hour = dt.contains(&DtToken::AmPm);
    if parts.is_empty() || (unmodelled && !saw_number && !had_dates) {
        sec.passthrough = true;
    }
    sec.parts = parts;
    sec
}

fn parse_condition(t: &str) -> Option<Condition> {
    let (op, rest) = if let Some(r) = t.strip_prefix(">=") {
        (CmpOp::Ge, r)
    } else if let Some(r) = t.strip_prefix("<=") {
        (CmpOp::Le, r)
    } else if let Some(r) = t.strip_prefix("<>") {
        (CmpOp::Ne, r)
    } else if let Some(r) = t.strip_prefix('>') {
        (CmpOp::Gt, r)
    } else if let Some(r) = t.strip_prefix('<') {
        (CmpOp::Lt, r)
    } else {
        let r = t.strip_prefix('=')?;
        (CmpOp::Eq, r)
    };
    rest.trim()
        .parse::<f64>()
        .ok()
        .map(|value| Condition { op, value })
}

/// Format with half-away-from-zero rounding, as Excel does.
fn format_half_up(v: f64, places: usize) -> String {
    let factor = 10f64.powi(places as i32);
    let scaled = v * factor;
    // `round()` in Rust is already half-away-from-zero; the subtlety is doing
    // it on the scaled value rather than letting the formatter round.
    let rounded = if scaled.is_finite() {
        scaled.round() / factor
    } else {
        v
    };
    format!("{:.*}", places, rounded)
}

/// Insert thousands separators into a run of digits.
fn group3(digits: &str) -> String {
    if digits.len() <= 3 {
        return digits.to_string();
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let first = digits.len() % 3;
    if first > 0 {
        out.push_str(&digits[..first]);
    }
    for (n, chunk) in digits.as_bytes()[first..].chunks(3).enumerate() {
        if n > 0 || first > 0 {
            out.push(',');
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
    }
    out
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn render_datetime_ctx(serial: f64, toks: &[DtToken], twelve_hour: bool) -> String {
    if twelve_hour {
        let (_, _, _, h24, _, _, _) = crate::table::serial_parts(serial);
        let h12 = if h24 % 12 == 0 { 12 } else { h24 % 12 };
        return render_datetime_12h(serial, toks, h12);
    }
    let (y, mo, d, h, mi, s, weekday) = crate::table::serial_parts(serial);
    let mut out = String::new();
    for t in toks {
        match t {
            DtToken::Year2 => {
                let _ = write!(out, "{:02}", y % 100);
            }
            DtToken::Year4 => {
                let _ = write!(out, "{y:04}");
            }
            DtToken::Month1 => {
                let _ = write!(out, "{mo}");
            }
            DtToken::Month2 => {
                let _ = write!(out, "{mo:02}");
            }
            DtToken::MonthAbbr => out.push_str(&MONTHS[(mo as usize - 1).min(11)][..3]),
            DtToken::MonthFull => out.push_str(MONTHS[(mo as usize - 1).min(11)]),
            DtToken::Day1 => {
                let _ = write!(out, "{d}");
            }
            DtToken::Day2 => {
                let _ = write!(out, "{d:02}");
            }
            DtToken::DayAbbr => out.push_str(&DAYS[(weekday as usize).min(6)][..3]),
            DtToken::DayFull => out.push_str(DAYS[(weekday as usize).min(6)]),
            DtToken::Hour1 => {
                let _ = write!(out, "{h}");
            }
            DtToken::Hour2 => {
                let _ = write!(out, "{h:02}");
            }
            DtToken::Minute1 => {
                let _ = write!(out, "{mi}");
            }
            DtToken::Minute2 => {
                let _ = write!(out, "{mi:02}");
            }
            DtToken::Second1 => {
                let _ = write!(out, "{s}");
            }
            DtToken::Second2 => {
                let _ = write!(out, "{s:02}");
            }
            // Elapsed forms are NOT clamped to a clock: `[h]:mm` over a
            // 3-day span shows 72, not 0. That is the entire point of them.
            DtToken::ElapsedHour => {
                let _ = write!(out, "{}", (serial * 24.0).floor() as i64);
            }
            DtToken::ElapsedMinute => {
                let _ = write!(out, "{}", (serial * 1440.0).floor() as i64);
            }
            DtToken::ElapsedSecond => {
                let _ = write!(out, "{}", (serial * 86400.0).floor() as i64);
            }
            DtToken::AmPm => out.push_str(if h < 12 { "AM" } else { "PM" }),
        }
    }
    out
}

fn render_datetime_12h(serial: f64, toks: &[DtToken], h12: u32) -> String {
    let (y, mo, d, _h, mi, s, weekday) = crate::table::serial_parts(serial);
    let (_, _, _, h24, _, _, _) = crate::table::serial_parts(serial);
    let mut out = String::new();
    for t in toks {
        match t {
            DtToken::Hour1 => {
                let _ = write!(out, "{h12}");
            }
            DtToken::Hour2 => {
                let _ = write!(out, "{h12:02}");
            }
            DtToken::AmPm => out.push_str(if h24 < 12 { "AM" } else { "PM" }),
            DtToken::Year2 => {
                let _ = write!(out, "{:02}", y % 100);
            }
            DtToken::Year4 => {
                let _ = write!(out, "{y:04}");
            }
            DtToken::Month1 => {
                let _ = write!(out, "{mo}");
            }
            DtToken::Month2 => {
                let _ = write!(out, "{mo:02}");
            }
            DtToken::MonthAbbr => out.push_str(&MONTHS[(mo as usize - 1).min(11)][..3]),
            DtToken::MonthFull => out.push_str(MONTHS[(mo as usize - 1).min(11)]),
            DtToken::Day1 => {
                let _ = write!(out, "{d}");
            }
            DtToken::Day2 => {
                let _ = write!(out, "{d:02}");
            }
            DtToken::DayAbbr => out.push_str(&DAYS[(weekday as usize).min(6)][..3]),
            DtToken::DayFull => out.push_str(DAYS[(weekday as usize).min(6)]),
            DtToken::Minute1 => {
                let _ = write!(out, "{mi}");
            }
            DtToken::Minute2 => {
                let _ = write!(out, "{mi:02}");
            }
            DtToken::Second1 => {
                let _ = write!(out, "{s}");
            }
            DtToken::Second2 => {
                let _ = write!(out, "{s:02}");
            }
            DtToken::ElapsedHour => {
                let _ = write!(out, "{}", (serial * 24.0).floor() as i64);
            }
            DtToken::ElapsedMinute => {
                let _ = write!(out, "{}", (serial * 1440.0).floor() as i64);
            }
            DtToken::ElapsedSecond => {
                let _ = write!(out, "{}", (serial * 86400.0).floor() as i64);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
