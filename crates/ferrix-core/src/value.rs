//! The cell value type.
//!
//! Size is load-bearing: at 10M rows every extra byte is 10 MB of RAM and a
//! worse cache-miss rate while scrolling. `Value` is kept to 16 bytes by
//! interning strings down to a 4-byte id rather than inlining a `String`
//! (which alone would be 24 bytes plus a heap allocation per cell).

use crate::arena::StrId;

/// Error kinds mirroring Excel's, so formula results round-trip on export.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ErrorKind {
    /// #DIV/0!
    DivZero,
    /// #VALUE!
    Value,
    /// #REF!
    Ref,
    /// #NAME?
    Name,
    /// #NUM!
    Num,
    /// #N/A
    NotAvailable,
    /// #NULL!
    Null,
    /// Circular reference detected by the dependency graph.
    Circular,
    /// #SPILL! — a dynamic-array formula's result could not spill because a
    /// cell in its target rectangle is occupied (#27 P2). The blocking cell's
    /// address is NOT carried in the value (that would break the 16-byte
    /// budget); it is recorded beside the data in the spill region store, and
    /// recovered by host cell for the hover/error message.
    Spill,
}

impl ErrorKind {
    /// Stable on-disk code. These values are written into saved files, so they
    /// must never be renumbered — only appended to.
    pub const fn to_code(self) -> u8 {
        match self {
            ErrorKind::DivZero => 0,
            ErrorKind::Value => 1,
            ErrorKind::Ref => 2,
            ErrorKind::Name => 3,
            ErrorKind::Num => 4,
            ErrorKind::NotAvailable => 5,
            ErrorKind::Null => 6,
            ErrorKind::Circular => 7,
            ErrorKind::Spill => 8,
        }
    }

    /// Inverse of `to_code`. An unknown code degrades to `Circular` rather
    /// than panicking, so a file from a newer version stays readable.
    pub const fn from_code(b: u8) -> Self {
        match b {
            0 => ErrorKind::DivZero,
            1 => ErrorKind::Value,
            2 => ErrorKind::Ref,
            3 => ErrorKind::Name,
            4 => ErrorKind::Num,
            5 => ErrorKind::NotAvailable,
            6 => ErrorKind::Null,
            8 => ErrorKind::Spill,
            _ => ErrorKind::Circular,
        }
    }

    /// The canonical spreadsheet spelling, used for display and .xlsx export.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorKind::DivZero => "#DIV/0!",
            ErrorKind::Value => "#VALUE!",
            ErrorKind::Ref => "#REF!",
            ErrorKind::Name => "#NAME?",
            ErrorKind::Num => "#NUM!",
            ErrorKind::NotAvailable => "#N/A",
            ErrorKind::Null => "#NULL!",
            ErrorKind::Circular => "#CIRC!",
            ErrorKind::Spill => "#SPILL!",
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single cell's value.
///
/// `Number` is f64 to match IEEE-754 spreadsheet semantics exactly.
/// `Text` holds an arena id, not the bytes.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Value {
    #[default]
    Empty,
    Number(f64),
    Bool(bool),
    Text(StrId),
    Error(ErrorKind),
}

impl Value {
    #[inline]
    pub fn is_empty(&self) -> bool {
        matches!(self, Value::Empty)
    }

    #[inline]
    pub fn is_error(&self) -> bool {
        matches!(self, Value::Error(_))
    }

    /// Numeric coercion following spreadsheet rules: bools are 1/0, empty is 0,
    /// text and errors do not coerce.
    #[inline]
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Empty => Some(0.0),
            Value::Text(_) | Value::Error(_) => None,
        }
    }

    /// Truthiness for IF() and friends: non-zero numbers are true.
    #[inline]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => Some(*n != 0.0),
            Value::Empty => Some(false),
            Value::Text(_) | Value::Error(_) => None,
        }
    }

    /// The error carried by this value, if any. Lets evaluators propagate
    /// the *first* error encountered rather than inventing a new one.
    #[inline]
    pub fn error(&self) -> Option<ErrorKind> {
        match self {
            Value::Error(e) => Some(*e),
            _ => None,
        }
    }

    /// A compact type tag, used to store values column-wise.
    #[inline]
    pub fn tag(&self) -> ValueTag {
        match self {
            Value::Empty => ValueTag::Empty,
            Value::Number(_) => ValueTag::Number,
            Value::Bool(_) => ValueTag::Bool,
            Value::Text(_) => ValueTag::Text,
            Value::Error(_) => ValueTag::Error,
        }
    }
}

/// One-byte discriminant for the columnar store.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum ValueTag {
    #[default]
    Empty = 0,
    Number = 1,
    Bool = 2,
    Text = 3,
    Error = 4,
}

/// Format a number the way a spreadsheet does: no trailing `.0`, no scientific
/// notation until the magnitude genuinely needs it.
pub fn format_number(n: f64) -> String {
    if n.is_nan() {
        return "#NUM!".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "∞".into() } else { "-∞".into() };
    }
    if n == n.trunc() && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    let a = n.abs();
    if a != 0.0 && (a < 1e-4 || a >= 1e11) {
        let s = format!("{n:E}");
        return s.replace('E', "E+").replace("E+-", "E-");
    }
    // Trim to 10 significant-ish decimals then strip trailing zeros.
    let mut s = format!("{n:.10}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_stays_16_bytes() {
        // Guardrail: this is the whole reason strings are interned.
        assert!(
            std::mem::size_of::<Value>() <= 16,
            "Value grew to {} bytes; at 10M rows that is {} MB per column",
            std::mem::size_of::<Value>(),
            std::mem::size_of::<Value>() * 10_000_000 / 1_048_576
        );
    }

    #[test]
    fn numeric_coercion_rules() {
        assert_eq!(Value::Number(3.5).as_number(), Some(3.5));
        assert_eq!(Value::Bool(true).as_number(), Some(1.0));
        assert_eq!(Value::Bool(false).as_number(), Some(0.0));
        assert_eq!(Value::Empty.as_number(), Some(0.0));
        assert_eq!(Value::Text(StrId(0)).as_number(), None);
        assert_eq!(Value::Error(ErrorKind::DivZero).as_number(), None);
    }

    #[test]
    fn bool_coercion_rules() {
        assert_eq!(Value::Number(0.0).as_bool(), Some(false));
        assert_eq!(Value::Number(-1.0).as_bool(), Some(true));
        assert_eq!(Value::Empty.as_bool(), Some(false));
        assert_eq!(Value::Error(ErrorKind::Num).as_bool(), None);
    }

    #[test]
    fn error_display_matches_excel() {
        assert_eq!(ErrorKind::DivZero.to_string(), "#DIV/0!");
        assert_eq!(ErrorKind::NotAvailable.to_string(), "#N/A");
        assert_eq!(ErrorKind::Name.to_string(), "#NAME?");
        // #27 P2: a blocked spill surfaces as Excel's #SPILL!.
        assert_eq!(ErrorKind::Spill.to_string(), "#SPILL!");
    }

    #[test]
    fn spill_error_code_roundtrips_and_is_appended() {
        // The on-disk code must be a NEW value (8), never a renumber of an
        // existing one, or old files would decode to the wrong error.
        assert_eq!(ErrorKind::Spill.to_code(), 8);
        assert_eq!(ErrorKind::from_code(8), ErrorKind::Spill);
        // Every existing code keeps its meaning.
        for k in [
            ErrorKind::DivZero,
            ErrorKind::Value,
            ErrorKind::Ref,
            ErrorKind::Name,
            ErrorKind::Num,
            ErrorKind::NotAvailable,
            ErrorKind::Null,
            ErrorKind::Circular,
            ErrorKind::Spill,
        ] {
            assert_eq!(ErrorKind::from_code(k.to_code()), k);
        }
    }

    #[test]
    fn number_formatting() {
        assert_eq!(format_number(42.0), "42");
        assert_eq!(format_number(-7.0), "-7");
        assert_eq!(format_number(3.5), "3.5");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(1.0 / 3.0), "0.3333333333");
    }
}
