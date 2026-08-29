//! The `.fxfmt` sidecar: persisted cell and column formatting.
//!
//! ## Why formatting gets its own file
//!
//! Formatting is not cell data. It survives independently of edits (you can
//! recolour a column you never typed in), it is orders of magnitude smaller,
//! and — critically — it does **not** need the base-file fingerprint that
//! `.fxedits` refuses to load without.
//!
//! That last point is the design decision worth stating. An edit says "cell
//! (5,2) now holds 42", which is meaningless if the base changed underneath
//! it, so `edits.rs` refuses stale sidecars. A format rule says "column 2 is
//! currency, and negatives are red" — a statement about a *column*, which
//! remains true and useful after the base is regenerated with a million more
//! rows. Tying formatting to a fingerprint would throw the user's work away
//! every time their data refreshed, which is precisely the workflow this
//! feature exists to serve.
//!
//! ## Size
//!
//! The file is O(rules), not O(rows) and not O(cells) — the same property the
//! in-memory [`SheetFormat`] has, for the same reason. Colouring a 200M-row
//! column writes about forty bytes.
//!
//! ## Layout
//!
//! ```text
//!   [magic   ] 8 bytes  "FXFMT001"
//!   [version ] u32
//!   [counts  ] columns u32, ranges u32, overrides u32
//!   [columns ] per column: col u32, number format, rule list
//!   [ranges  ] per range: 4x u32 bounds, optional number format, rule list
//!   [overrides] per cell: row u32, col u32, opt fill, opt text, opt format
//! ```
//!
//! Rule and colour encodings are described at [`write_rule`]. Every list is
//! length-prefixed and every optional field carries a presence byte, so a
//! reader never has to guess and a truncated file is detected rather than
//! misparsed.
//!
//! Maps are written in key order (`SheetFormat` uses `BTreeMap` for exactly
//! this reason), so saving twice produces identical bytes — the same
//! reproducibility `edits.rs` gets by sorting.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use ferrix_core::format::{FontFamily, Typography};
use ferrix_core::table::ConditionalRule;
use ferrix_core::{
    CellOverride, CellRef, CmpOp, ColumnFormat, DateStyle, ManualStyle, NumberFormat, RangeFormat,
    Rgb, SheetFormat, TableRange,
};

pub const FMT_MAGIC: &[u8; 8] = b"FXFMT001";
/// Bumped to 2 when typography joined `ManualStyle` and the `Manual` rule.
///
/// A v1 file has no typography bytes, so reading one with the v2 layout would
/// pull the following field's bytes into a font size and produce plausible
/// nonsense rather than an error. The version check at load rejects it
/// outright instead — a refused file is recoverable, a silently misread one
/// is not.
pub const FMT_VERSION: u32 = 2;

#[derive(Debug)]
pub enum FormatSidecarError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
    /// A tag byte no version of this format has ever written.
    UnknownTag(u8),
}

impl std::fmt::Display for FormatSidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatSidecarError::Io(e) => write!(f, "{e}"),
            FormatSidecarError::BadMagic => write!(f, "not a Ferrix formatting file"),
            FormatSidecarError::BadVersion(v) => {
                write!(f, "unsupported formatting version {v}")
            }
            FormatSidecarError::Truncated => write!(f, "formatting file is truncated"),
            FormatSidecarError::UnknownTag(t) => {
                write!(f, "unrecognised formatting tag {t}")
            }
        }
    }
}

impl std::error::Error for FormatSidecarError {}

impl From<io::Error> for FormatSidecarError {
    fn from(e: io::Error) -> Self {
        FormatSidecarError::Io(e)
    }
}

/// Sidecar path for a base file: `sales.ferrix` -> `sales.ferrix.fxfmt`.
pub fn format_path_for(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(".fxfmt");
    PathBuf::from(s)
}

// ==================================================================== write ==

fn put_u32<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_f64<W: Write>(w: &mut W, v: f64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn put_str<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    put_u32(w, s.len() as u32)?;
    w.write_all(s.as_bytes())
}

fn put_rgb<W: Write>(w: &mut W, c: Rgb) -> io::Result<()> {
    w.write_all(&[c.0, c.1, c.2])
}

/// An `Option<Rgb>` as a presence byte plus three colour bytes.
fn put_opt_rgb<W: Write>(w: &mut W, c: Option<Rgb>) -> io::Result<()> {
    match c {
        Some(c) => {
            w.write_all(&[1])?;
            put_rgb(w, c)
        }
        None => w.write_all(&[0]),
    }
}

/// A number format as a tag plus its parameters.
///
/// `Custom` writes its format string verbatim, which is what makes an
/// Excel-authored format Ferrix does not model survive a save/load cycle —
/// the same contract `NumberFormat::from_code` promises.
fn write_format<W: Write>(w: &mut W, f: &NumberFormat) -> io::Result<()> {
    match f {
        NumberFormat::General => w.write_all(&[0]),
        NumberFormat::Decimal { places } => w.write_all(&[1, *places]),
        NumberFormat::Thousands { places } => w.write_all(&[2, *places]),
        NumberFormat::Currency { symbol, places } => {
            w.write_all(&[3, *places])?;
            put_str(w, symbol)
        }
        NumberFormat::Percent { places } => w.write_all(&[4, *places]),
        NumberFormat::Date(d) => {
            let code = match d {
                DateStyle::Iso => 0u8,
                DateStyle::Us => 1,
                DateStyle::Euro => 2,
                DateStyle::IsoDateTime => 3,
                DateStyle::Time => 4,
            };
            w.write_all(&[5, code])
        }
        NumberFormat::Custom(s) => {
            w.write_all(&[6])?;
            put_str(w, s)
        }
    }
}

fn write_opt_format<W: Write>(w: &mut W, f: Option<&NumberFormat>) -> io::Result<()> {
    match f {
        Some(f) => {
            w.write_all(&[1])?;
            write_format(w, f)
        }
        None => w.write_all(&[0]),
    }
}

/// A conditional rule as a tag byte plus its fields.
///
/// Tags are append-only: a new rule kind takes the next free number and old
/// files keep parsing. A reader meeting a tag it does not know reports
/// [`FormatSidecarError::UnknownTag`] rather than skipping silently, because
/// silently dropping a user's rule is the failure mode that loses work.
fn write_rule<W: Write>(w: &mut W, r: &ConditionalRule) -> io::Result<()> {
    match r {
        ConditionalRule::Manual {
            fill,
            text,
            typography,
        } => {
            w.write_all(&[0])?;
            put_opt_rgb(w, *fill)?;
            put_opt_rgb(w, *text)?;
            put_typography(w, typography)
        }
        ConditionalRule::ColorScale2 { min, max } => {
            w.write_all(&[1])?;
            put_rgb(w, *min)?;
            put_rgb(w, *max)
        }
        ConditionalRule::ColorScale3 { min, mid, max } => {
            w.write_all(&[2])?;
            put_rgb(w, *min)?;
            put_rgb(w, *mid)?;
            put_rgb(w, *max)
        }
        ConditionalRule::DataBar { color } => {
            w.write_all(&[3])?;
            put_rgb(w, *color)
        }
        ConditionalRule::Threshold {
            op,
            value,
            fill,
            text,
        } => {
            w.write_all(&[4, op_code(*op)])?;
            put_f64(w, *value)?;
            put_rgb(w, *fill)?;
            put_rgb(w, *text)
        }
        ConditionalRule::Sign {
            negative,
            positive,
            zero,
        } => {
            w.write_all(&[5])?;
            put_opt_rgb(w, *negative)?;
            put_opt_rgb(w, *positive)?;
            put_opt_rgb(w, *zero)
        }
        ConditionalRule::TopBottom { top, n, fill, text } => {
            w.write_all(&[6, u8::from(*top)])?;
            put_u32(w, *n)?;
            put_rgb(w, *fill)?;
            put_rgb(w, *text)
        }
        ConditionalRule::TextContains { needle, fill, text } => {
            w.write_all(&[7])?;
            put_str(w, needle)?;
            put_rgb(w, *fill)?;
            put_rgb(w, *text)
        }
    }
}

fn write_rules<W: Write>(w: &mut W, rules: &[ConditionalRule]) -> io::Result<()> {
    put_u32(w, rules.len() as u32)?;
    for r in rules {
        write_rule(w, r)?;
    }
    Ok(())
}

const fn op_code(op: CmpOp) -> u8 {
    match op {
        CmpOp::Eq => 0,
        CmpOp::Ne => 1,
        CmpOp::Lt => 2,
        CmpOp::Le => 3,
        CmpOp::Gt => 4,
        CmpOp::Ge => 5,
    }
}

fn op_of(code: u8) -> Option<CmpOp> {
    Some(match code {
        0 => CmpOp::Eq,
        1 => CmpOp::Ne,
        2 => CmpOp::Lt,
        3 => CmpOp::Le,
        4 => CmpOp::Gt,
        5 => CmpOp::Ge,
        _ => return None,
    })
}

/// Write a sheet's formatting to `path` atomically.
///
/// Temp file plus rename, for the same reason `save_edits` does it: a crash
/// mid-save must not leave a half-written sidecar that the next open would
/// reject or, worse, partially apply.
pub fn save_format(path: &Path, fmt: &SheetFormat) -> Result<u64, FormatSidecarError> {
    let tmp = path.with_extension("fxfmt.tmp");
    {
        let f = File::create(&tmp)?;
        let mut w = BufWriter::new(f);

        w.write_all(FMT_MAGIC)?;
        put_u32(&mut w, FMT_VERSION)?;

        let cols: Vec<(u32, &ColumnFormat)> = fmt.columns().collect();
        let ovs: Vec<(CellRef, &CellOverride)> = fmt.overrides().collect();
        put_u32(&mut w, cols.len() as u32)?;
        put_u32(&mut w, fmt.ranges().len() as u32)?;
        put_u32(&mut w, ovs.len() as u32)?;

        for (col, cf) in cols {
            put_u32(&mut w, col)?;
            write_format(&mut w, &cf.format)?;
            write_rules(&mut w, &cf.rules)?;
        }
        for rf in fmt.ranges() {
            put_u32(&mut w, rf.range.first_row)?;
            put_u32(&mut w, rf.range.first_col)?;
            put_u32(&mut w, rf.range.last_row)?;
            put_u32(&mut w, rf.range.last_col)?;
            write_opt_format(&mut w, rf.format.as_ref())?;
            write_rules(&mut w, &rf.rules)?;
        }
        for (cell, ov) in ovs {
            put_u32(&mut w, cell.row)?;
            put_u32(&mut w, cell.col)?;
            put_opt_rgb(&mut w, ov.manual.fill)?;
            put_opt_rgb(&mut w, ov.manual.text)?;
            put_typography(&mut w, &ov.manual.typography)?;
            write_opt_format(&mut w, ov.format.as_ref())?;
        }
        w.flush()?;
    }
    let size = std::fs::metadata(&tmp)?.len();
    // Windows will not rename onto an existing file.
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
    Ok(size)
}

// ===================================================================== read ==

struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatSidecarError> {
        if self.p + n > self.d.len() {
            return Err(FormatSidecarError::Truncated);
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, FormatSidecarError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, FormatSidecarError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64, FormatSidecarError> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, FormatSidecarError> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn rgb(&mut self) -> Result<Rgb, FormatSidecarError> {
        let b = self.take(3)?;
        Ok(Rgb(b[0], b[1], b[2]))
    }
    fn opt_rgb(&mut self) -> Result<Option<Rgb>, FormatSidecarError> {
        match self.u8()? {
            0 => Ok(None),
            _ => Ok(Some(self.rgb()?)),
        }
    }
    /// Read the fixed 7-byte typography record written by `put_typography`.
    ///
    /// An unknown family byte degrades to `None` (inherit) rather than
    /// erroring, so a sidecar written by a newer build stays readable instead
    /// of taking the whole file down with it.
    fn typography(&mut self) -> Result<Typography, FormatSidecarError> {
        let family = match self.u8()? {
            1 => Some(FontFamily::Proportional),
            2 => Some(FontFamily::Monospace),
            _ => None,
        };
        let q = u16::from_le_bytes([self.u8()?, self.u8()?]);
        let size = if q == 0 { None } else { Some(q as f32 / 4.0) };
        let mut flag = || -> Result<Option<bool>, FormatSidecarError> {
            Ok(match self.u8()? {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            })
        };
        let bold = flag()?;
        let italic = flag()?;
        let underline = flag()?;
        let strikethrough = flag()?;
        Ok(Typography {
            family,
            size,
            bold,
            italic,
            underline,
            strikethrough,
        })
    }
    fn format(&mut self) -> Result<NumberFormat, FormatSidecarError> {
        Ok(match self.u8()? {
            0 => NumberFormat::General,
            1 => NumberFormat::Decimal { places: self.u8()? },
            2 => NumberFormat::Thousands { places: self.u8()? },
            3 => {
                let places = self.u8()?;
                NumberFormat::Currency {
                    symbol: self.string()?,
                    places,
                }
            }
            4 => NumberFormat::Percent { places: self.u8()? },
            5 => NumberFormat::Date(match self.u8()? {
                0 => DateStyle::Iso,
                1 => DateStyle::Us,
                2 => DateStyle::Euro,
                3 => DateStyle::IsoDateTime,
                4 => DateStyle::Time,
                t => return Err(FormatSidecarError::UnknownTag(t)),
            }),
            6 => NumberFormat::Custom(self.string()?),
            t => return Err(FormatSidecarError::UnknownTag(t)),
        })
    }
    fn opt_format(&mut self) -> Result<Option<NumberFormat>, FormatSidecarError> {
        match self.u8()? {
            0 => Ok(None),
            _ => Ok(Some(self.format()?)),
        }
    }
    fn rule(&mut self) -> Result<ConditionalRule, FormatSidecarError> {
        Ok(match self.u8()? {
            0 => ConditionalRule::Manual {
                fill: self.opt_rgb()?,
                text: self.opt_rgb()?,
                typography: self.typography()?,
            },
            1 => ConditionalRule::ColorScale2 {
                min: self.rgb()?,
                max: self.rgb()?,
            },
            2 => ConditionalRule::ColorScale3 {
                min: self.rgb()?,
                mid: self.rgb()?,
                max: self.rgb()?,
            },
            3 => ConditionalRule::DataBar { color: self.rgb()? },
            4 => {
                let code = self.u8()?;
                let op = op_of(code).ok_or(FormatSidecarError::UnknownTag(code))?;
                ConditionalRule::Threshold {
                    op,
                    value: self.f64()?,
                    fill: self.rgb()?,
                    text: self.rgb()?,
                }
            }
            5 => ConditionalRule::Sign {
                negative: self.opt_rgb()?,
                positive: self.opt_rgb()?,
                zero: self.opt_rgb()?,
            },
            6 => ConditionalRule::TopBottom {
                top: self.u8()? != 0,
                n: self.u32()?,
                fill: self.rgb()?,
                text: self.rgb()?,
            },
            7 => ConditionalRule::TextContains {
                needle: self.string()?,
                fill: self.rgb()?,
                text: self.rgb()?,
            },
            t => return Err(FormatSidecarError::UnknownTag(t)),
        })
    }
    fn rules(&mut self) -> Result<Vec<ConditionalRule>, FormatSidecarError> {
        let n = self.u32()? as usize;
        // A corrupt length must not make us preallocate gigabytes; the vector
        // grows as records are actually read instead.
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(self.rule()?);
        }
        Ok(out)
    }
}

/// Load a formatting sidecar.
///
/// Returns `Ok(None)` when the file does not exist — the common case for a
/// dataset nobody has formatted yet, and not an error.
pub fn load_format(path: &Path) -> Result<Option<SheetFormat>, FormatSidecarError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let mut c = Cursor { d: &buf, p: 0 };

    if c.take(8)? != FMT_MAGIC {
        return Err(FormatSidecarError::BadMagic);
    }
    let version = c.u32()?;
    if version != FMT_VERSION {
        return Err(FormatSidecarError::BadVersion(version));
    }

    let n_cols = c.u32()? as usize;
    let n_ranges = c.u32()? as usize;
    let n_ovs = c.u32()? as usize;

    let mut fmt = SheetFormat::new();
    for _ in 0..n_cols {
        let col = c.u32()?;
        let f = c.format()?;
        let rules = c.rules()?;
        let entry = fmt.column_mut(col);
        entry.format = f;
        entry.rules = rules;
    }
    for _ in 0..n_ranges {
        let range = TableRange::new(c.u32()?, c.u32()?, c.u32()?, c.u32()?);
        let format = c.opt_format()?;
        let rules = c.rules()?;
        fmt.push_range(RangeFormat {
            range,
            format,
            rules,
        });
    }
    for _ in 0..n_ovs {
        let cell = CellRef::new(c.u32()?, c.u32()?);
        let manual = ManualStyle {
            fill: c.opt_rgb()?,
            text: c.opt_rgb()?,
            typography: c.typography()?,
        };
        let format = c.opt_format()?;
        fmt.set_cell_override(cell, CellOverride { manual, format });
    }
    Ok(Some(fmt))
}

/// Write a [`Typography`] as a fixed 7-byte record.
///
/// Fixed width on purpose: a variable-length style would make every later
/// record's offset depend on this one, and the sidecar is read back by
/// seeking. Six optional fields plus a family byte is 7 bytes, and an unset
/// style is seven zeros.
fn put_typography<W: std::io::Write>(w: &mut W, t: &Typography) -> std::io::Result<()> {
    // Family: 0 = inherit, 1 = proportional, 2 = monospace.
    let fam = match t.family {
        None => 0u8,
        Some(FontFamily::Proportional) => 1,
        Some(FontFamily::Monospace) => 2,
    };
    w.write_all(&[fam])?;
    // Size in quarter-points, so 0 means inherit and 12.5pt survives exactly.
    let size = t.size.map(|p| (p * 4.0).round() as u16).unwrap_or(0);
    w.write_all(&size.to_le_bytes())?;
    for flag in [t.bold, t.italic, t.underline, t.strikethrough] {
        w.write_all(&[match flag {
            None => 0u8,
            Some(false) => 1,
            Some(true) => 2,
        }])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
