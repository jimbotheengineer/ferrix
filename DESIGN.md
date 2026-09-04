---
version: alpha
name: Ferrix
description: A modern, high-density spreadsheet workbench for datasets that exceed conventional spreadsheet limits; precise, fast, restrained, and unmistakably human-designed.
colors:
  primary: "#4D9DFF"
  dark-bg: "#121418"
  dark-panel: "#171A1F"
  dark-header: "#1D2127"
  dark-grid-line: "#262B33"
  dark-text: "#D6DBE3"
  dark-text-muted: "#909AA8"
  dark-accent: "#4D9DFF"
  dark-accent-soft: "#1E334D"
  dark-number: "#8FD0A8"
  dark-error: "#FF7B72"
  dark-row-alt: "#15181D"
  dark-match-edge: "#F0C050"
  dark-table-band: "#1A202A"
  dark-invalid: "#E5484F"
  dark-comment: "#F2B13C"
  dark-padding: "#0C0D10"
  light-bg: "#FBFCFD"
  light-panel: "#EFF1F5"
  light-header: "#E3E7EE"
  light-grid-line: "#D0D6E0"
  light-text: "#1B1F26"
  light-text-muted: "#5B6574"
  light-accent: "#1460C8"
  light-accent-soft: "#CDDFF8"
  light-number: "#0D6E3C"
  light-error: "#BA1F1A"
  light-row-alt: "#F1F3F7"
  light-match-edge: "#8A5D08"
  light-table-band: "#E6EBF3"
  light-invalid: "#CC2824"
  light-comment: "#B87A00"
  light-padding: "#E8EAEF"
  white: "#FFFFFF"
  black: "#000000"
typography:
  app-title:
    fontFamily: Inter
    fontSize: 0.9375rem
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: "0.04em"
  ui-body:
    fontFamily: Inter
    fontSize: 0.8125rem
    fontWeight: 400
    lineHeight: 1.35
    letterSpacing: "0em"
  ui-label:
    fontFamily: Inter
    fontSize: 0.75rem
    fontWeight: 500
    lineHeight: 1.25
    letterSpacing: "0em"
  grid-cell:
    fontFamily: Inter
    fontSize: 0.78125rem
    fontWeight: 400
    lineHeight: 1.25
    letterSpacing: "0em"
  data-mono:
    fontFamily: JetBrains Mono
    fontSize: 0.75rem
    fontWeight: 400
    lineHeight: 1.3
    letterSpacing: "0em"
  formula:
    fontFamily: JetBrains Mono
    fontSize: 0.8125rem
    fontWeight: 400
    lineHeight: 1.35
    letterSpacing: "0em"
  dialog-title:
    fontFamily: Inter
    fontSize: 1rem
    fontWeight: 650
    lineHeight: 1.3
    letterSpacing: "-0.01em"
rounded:
  none: 0px
  xs: 2px
  sm: 4px
  md: 6px
  lg: 8px
spacing:
  xxs: 2px
  xs: 4px
  sm: 6px
  md: 8px
  lg: 12px
  xl: 16px
  xxl: 24px
components:
  app-shell-dark:
    backgroundColor: "{colors.dark-bg}"
    textColor: "{colors.dark-text}"
    typography: "{typography.ui-body}"
    rounded: "{rounded.none}"
  app-shell-light:
    backgroundColor: "{colors.light-bg}"
    textColor: "{colors.light-text}"
    typography: "{typography.ui-body}"
    rounded: "{rounded.none}"
  toolbar-dark:
    backgroundColor: "{colors.dark-panel}"
    textColor: "{colors.dark-text}"
    typography: "{typography.ui-label}"
    rounded: "{rounded.none}"
    padding: 8px
  toolbar-light:
    backgroundColor: "{colors.light-panel}"
    textColor: "{colors.light-text}"
    typography: "{typography.ui-label}"
    rounded: "{rounded.none}"
    padding: 8px
  control-dark:
    backgroundColor: "{colors.dark-header}"
    textColor: "{colors.dark-text}"
    typography: "{typography.ui-label}"
    rounded: "{rounded.xs}"
    padding: 6px
  control-light:
    backgroundColor: "{colors.light-header}"
    textColor: "{colors.light-text}"
    typography: "{typography.ui-label}"
    rounded: "{rounded.xs}"
    padding: 6px
  control-dark-hover:
    backgroundColor: "{colors.dark-accent-soft}"
    textColor: "{colors.dark-text}"
    typography: "{typography.ui-label}"
    rounded: "{rounded.xs}"
    padding: 6px
  control-light-hover:
    backgroundColor: "{colors.light-accent-soft}"
    textColor: "{colors.light-text}"
    typography: "{typography.ui-label}"
    rounded: "{rounded.xs}"
    padding: 6px
  destructive-dark:
    backgroundColor: "{colors.dark-invalid}"
    textColor: "{colors.black}"
    typography: "{typography.ui-label}"
    rounded: "{rounded.xs}"
    padding: 8px
  destructive-light:
    backgroundColor: "{colors.light-invalid}"
    textColor: "{colors.white}"
    typography: "{typography.ui-label}"
    rounded: "{rounded.xs}"
    padding: 8px
  grid-boundary-dark:
    backgroundColor: "{colors.dark-grid-line}"
  grid-boundary-light:
    backgroundColor: "{colors.light-grid-line}"
  selection-edge-dark:
    backgroundColor: "{colors.dark-accent}"
  selection-edge-light:
    backgroundColor: "{colors.light-accent}"
  numeric-ink-dark:
    textColor: "{colors.dark-number}"
  numeric-ink-light:
    textColor: "{colors.light-number}"
  error-ink-dark:
    textColor: "{colors.dark-error}"
  error-ink-light:
    textColor: "{colors.light-error}"
  alternate-row-dark:
    backgroundColor: "{colors.dark-row-alt}"
  alternate-row-light:
    backgroundColor: "{colors.light-row-alt}"
  current-match-edge-dark:
    backgroundColor: "{colors.dark-match-edge}"
  current-match-edge-light:
    backgroundColor: "{colors.light-match-edge}"
  table-band-dark:
    backgroundColor: "{colors.dark-table-band}"
  table-band-light:
    backgroundColor: "{colors.light-table-band}"
  comment-marker-dark:
    backgroundColor: "{colors.dark-comment}"
  comment-marker-light:
    backgroundColor: "{colors.light-comment}"
  beyond-data-dark:
    backgroundColor: "{colors.dark-padding}"
  beyond-data-light:
    backgroundColor: "{colors.light-padding}"
  formula-bar-dark:
    backgroundColor: "{colors.dark-header}"
    textColor: "{colors.dark-text}"
    typography: "{typography.formula}"
    rounded: "{rounded.none}"
    padding: 8px
  formula-bar-light:
    backgroundColor: "{colors.light-header}"
    textColor: "{colors.light-text}"
    typography: "{typography.formula}"
    rounded: "{rounded.none}"
    padding: 8px
  cell-dark:
    backgroundColor: "{colors.dark-bg}"
    textColor: "{colors.dark-text}"
    typography: "{typography.grid-cell}"
    rounded: "{rounded.none}"
    height: 22px
  cell-light:
    backgroundColor: "{colors.light-bg}"
    textColor: "{colors.light-text}"
    typography: "{typography.grid-cell}"
    rounded: "{rounded.none}"
    height: 22px
  status-dark:
    backgroundColor: "{colors.dark-header}"
    textColor: "{colors.dark-text-muted}"
    typography: "{typography.data-mono}"
    rounded: "{rounded.none}"
    padding: 4px
  status-light:
    backgroundColor: "{colors.light-header}"
    textColor: "{colors.light-text-muted}"
    typography: "{typography.data-mono}"
    rounded: "{rounded.none}"
    padding: 4px
---

## Overview

Ferrix is a desktop spreadsheet workbench for datasets that exceed conventional spreadsheet limits. Its interface must communicate three things immediately: **this is a spreadsheet**, **the data is trustworthy**, and **the application remains responsive at extreme scale**.

The visual direction is **modern industrial utility**: crisp hierarchy, compact controls, restrained color, strong alignment, and deliberate detail. It should feel designed by people who use spreadsheets and data tools all day—not generated from a fashionable app template.

### Product promise

> Open, inspect, transform, calculate, chart, and export data at scales where conventional spreadsheets stop—without surrendering familiar spreadsheet interaction or hiding what the software is doing.

### Design principles

1. **The grid is the product.** Chrome exists to support the worksheet, not compete with it.
2. **Fast must also feel calm.** Interaction is immediate, layout is stable, and long operations report progress without blocking unrelated work.
3. **Never trade truth for polish.** Limits, truncation, partial results, stale sidecars, memory constraints, and destructive consequences are explicit.
4. **Power is discoverable, not diluted.** Familiar shortcuts and direct manipulation remain first-class; the command palette and ribbon improve access rather than replacing them.
5. **Density is intentional.** Ferrix is a professional data tool. It should fit meaningful information on screen while preserving legibility and reliable targets.
6. **Smoothness may not remove capability.** Progressive disclosure can move infrequent controls out of the way, but no core function may become vague, inaccessible, or mouse-only.
7. **Storage scale is not visual scale.** Rendering and UI state are bounded by the viewport or by explicit limits, never by the row count.

### Audience

- Analysts and researchers working with multi-million-row CSV, XLSX, or Parquet data.
- Engineers inspecting exports, telemetry, logs, and generated datasets.
- Spreadsheet experts who expect keyboard fluency, formulas, names, tables, pivots, validation, and print controls.
- Users on constrained machines who need the application to explain memory and background work honestly.

### Personality

Ferrix is **precise, capable, restrained, candid, and fast**. It is not playful, chatty, futuristic, luxurious, or “magical.” Product copy uses concrete verbs and quantities.

### Explicit non-goals: avoiding the “AI app” look

Do not use:

- Blue-purple gradients, aurora backgrounds, neon glows, or luminous borders.
- Glassmorphism, blurred translucent cards, or floating layers without functional hierarchy.
- Oversized rounded cards, pill-shaped controls everywhere, or excessive empty space.
- Sparkle icons, robot imagery, “magic” terminology, chat bubbles, or prompt-like primary input.
- Dashboard card mosaics where a table, panel, or dialog is more direct.
- Generic hero copy inside the desktop application.
- Motion added only to make the interface appear sophisticated.
- Icon-only actions when the symbol is not universally understood.

Ferrix may use modern rendering, typography, and transitions; modernity comes from precision and coherence, not decoration.

### Current implementation baseline

The existing egui interface establishes useful invariants that this document retains:

- Dark and light semantic themes.
- A two-row ribbon with Home, File, Format, Formula, Data, and View tabs.
- A resizable formula bar with a name box.
- A viewport-virtualized, directly painted grid.
- Optional search/replace and filter bars.
- Bottom sheet tabs and a separate status bar.
- Explicit loading/export progress with cancellation.
- Per-sheet zoom and UI state persistence.

This specification guides refinement and future work. It does not require rewriting working controls merely to imitate a web design system.

## Colors

### Color model

Color tokens are semantic. UI code must request the active theme value through `Theme`; feature code must not embed ad hoc RGB constants. Dark and light palettes are tuned independently rather than mechanically inverted.

The blue accent means **selection, focus, active navigation, or a primary neutral action**. It does not mean “AI.” Use it sparingly enough that active state remains obvious.

### Structural surfaces

| Role | Dark | Light | Usage |
|---|---:|---:|---|
| Canvas | `#121418` | `#FBFCFD` | Primary grid and application background |
| Panel | `#171A1F` | `#EFF1F5` | Ribbon and elevated utility regions |
| Header | `#1D2127` | `#E3E7EE` | Formula, search, status, row/column headers |
| Grid line | `#262B33` | `#D0D6E0` | Cell boundaries and quiet separators |
| Alternate row | `#15181D` | `#F1F3F7` | Subtle scan assistance |
| Table band | `#1A202A` | `#E6EBF3` | Structured-table banding, distinct from sheet striping |
| Beyond-data padding | `#0C0D10` | `#E8EAEF` | Visibly distinguishes non-data rows |

Structural surfaces remain flat. Use a one-pixel boundary or a small value shift instead of drop shadows wherever possible.

### Ink and semantic color

| Meaning | Dark | Light | Rule |
|---|---:|---:|---|
| Primary text | `#D6DBE3` | `#1B1F26` | Labels, cell text, dialog copy |
| Muted text | `#909AA8` | `#5B6574` | Metadata, hints, status |
| Accent | `#4D9DFF` | `#1460C8` | Focus, selection edge, active controls |
| Number | `#8FD0A8` | `#0D6E3C` | Numeric semantic ink where type coloring is enabled |
| Error | `#FF7B72` | `#BA1F1A` | Formula errors and failed operations |
| Invalid marker | `#E5484F` | `#CC2824` | Validation corner marker and high-priority danger |
| Comment marker | `#F2B13C` | `#B87A00` | Comments and non-error attention |
| Match edge | `#F0C050` | `#8A5D08` | Current search match |

Never rely on color alone. Error, warning, validation, match, filtered, and selected states require shape, icon, text, border, position, or count in addition to hue.

### Overlay rules

Selection range fills, search fills, and current-match fills are translucent layers in the egui implementation. They must preserve underlying row banding, table banding, conditional formatting, and readable semantic text.

Layer order for a cell, back to front:

1. Base canvas or alternate-row surface.
2. Table banding.
3. Manual and conditional formatting.
4. Search-match fill.
5. Multi-cell selection fill.
6. Text and data-bar content.
7. Current-cell, match, validation, comment, and formula-reference edges/markers.

The current match is distinguished primarily by its amber edge, not by a bright fill that reduces text contrast.

### Contrast requirements

- Normal text target: WCAG AA, at least 4.5:1 where feasible.
- Dense 12.5 px grid text: never below 4.0:1 on any surface on which it is actually painted; 4.5:1 remains the preferred target.
- Focus, reference, and non-text boundaries: at least 3:1 against adjacent surfaces.
- Muted text must never be used for required instructions, destructive consequences, truncated-result warnings, or active cell content.
- Theme tests must composite translucent overlays before evaluating contrast.

## Typography

### Typeface strategy

Use a neutral humanist or neo-grotesque sans serif for interface text and a technical monospace for formulas, addresses, timings, counts, and diagnostic values.

Preferred families:

- UI: **Inter**, falling back to the platform sans serif and then egui proportional.
- Data/formula: **JetBrains Mono**, falling back to the platform monospace and then egui monospace.

Bundling fonts is optional until text shaping, package size, and platform rendering are validated. Consistent metrics and crisp rendering matter more than a branded font.

### Hierarchy

Ferrix uses a compact desktop hierarchy rather than web-page headings:

- Brand: 15 px, bold, modest tracking.
- Dialog title: 16 px, semibold.
- Primary UI and menus: 13 px.
- Grid cells: 12.5–13 px at 100% zoom.
- Secondary labels and status: 11.5–12 px.
- Formula bar: 13 px monospace.

Avoid all-caps except the short `FERRIX` wordmark and high-severity, intentionally explicit warnings such as `INCOMPLETE`.

### Data typography

- Keep tabular numbers aligned by cell alignment; use tabular figures where the font supports them.
- Formula source, A1 addresses, dimensions, durations, row counts, memory quantities, and FPS are monospace.
- Do not use monospace for entire dialogs or command labels; it adds visual noise and harms reading speed.
- Never truncate a numeric quantity in a way that changes its meaning. Abbreviations may supplement, not replace, an exact value in a tooltip or detail surface.

## Layout

### Application anatomy

The default workspace is a vertical stack:

1. **Ribbon header** — brand, tabs, command overflow, resource and dataset summary.
2. **Ribbon controls** — commands for the active tab; wraps at narrow widths.
3. **Formula region** — name box, `fx`, formula editor, optional result; vertically resizable.
4. **Context region** — search/replace, import progress, or other temporary horizontal tools.
5. **Grid viewport** — consumes all remaining space.
6. **Status bar** — operation message on the left; state, validity, filters, traces, and render metrics on the right.
7. **Sheet tabs** — workbook navigation, add, rename, reorder, and delete.

The grid always receives the remaining flexible area. No toolbar, panel, or empty state may accidentally reduce it to zero; resizable vertical surfaces require hard maximums.

### Baseline dimensions at 100% zoom

| Element | Target |
|---|---:|
| Grid row | 22 px |
| Column header | 26 px |
| Default column | 108 px |
| Row header / name box | 88 px |
| Scrollbar | 12 px visual width |
| Formula/search input | 22–24 px |
| Standard compact control | 26–28 px |
| Dialog control | 30–32 px |
| Panel inset | 8 px |
| Inline gap | 6 px |
| Major group gap | 12 px |

A 22 px grid row is an information-density choice, not the default target for every interactive control. Tiny cell targets are acceptable because selection also supports keyboard navigation and whole-cell hit areas. Standalone controls should generally provide at least 28 px height; high-frequency ribbon controls may be compact if labels remain legible.

### Grid ownership

The directly painted grid owns:

- Row and column headers.
- Selection and fill handles.
- Cell contents and decoration.
- Frozen/split panes.
- Search, validation, comment, trace, table, subtotal, and formula-reference overlays.
- Scrollbars and hit testing derived from the same geometry used to paint.

Do not overlay detached web-style cards on top of data. Tools needing persistent width should become a purposeful docked panel; short-lived actions belong in anchored popovers, dialogs, or context bars.

### Ribbon organization

- **Home:** open, save, undo, redo, bold, italic, merge, chart, find.
- **File:** file lifecycle, import/export, page setup, print.
- **Format:** typography, number formatting, alignment, borders, conditional formatting.
- **Formula:** functions, names, calculate, trace, goal seek.
- **Data:** sort, filter, tables, validation, deduplicate, consolidate, pivot, protection.
- **View:** zoom, theme, formulas, empty rows, panes, visibility.

Commands are defined once in the registry and consumed by ribbon, overflow menus, and command palette. A command must not acquire different names, shortcuts, availability rules, or consequences across those surfaces.

Ribbon controls may wrap when the window narrows. They may not clip, overlap, shrink labels into ambiguity, or force horizontal scrolling. If density becomes excessive, organize commands into labelled groups before inventing additional tabs.

### Window-size behavior

- Recommended minimum workspace: 1024 × 640 logical pixels.
- Below the recommended width, ribbon groups wrap and optional telemetry yields before core controls.
- Formula result text truncates before the formula editor loses a usable width.
- Resource statistics may collapse into one labelled disclosure, but progress and cancel remain visible.
- Dialogs clamp to the viewport and allow their body to scroll while titles and actions remain visible.
- At very small sizes, preserve keyboard access and the grid even if secondary status telemetry is hidden.

### Zoom

Worksheet zoom ranges from 25% to 400%, persisted per workbook path and sheet. Zoom changes grid metrics, not ribbon or dialog chrome. At low zoom, preserve minimum one-pixel grid lines and selection edges. At high zoom, do not inflate scrollbars or application chrome.

## Elevation & Depth

Ferrix is predominantly flat.

Use elevation only to establish interaction order:

1. Base grid and fixed panels.
2. Menus, anchored popovers, autocomplete, and validation dropdowns.
3. Modeless tool windows such as charts or builders.
4. Modal decisions and recovery prompts.

Preferred depth cues:

- One-pixel boundaries.
- Small surface-value changes.
- A restrained shadow on floating windows only.
- Dimming behind a true modal.

Do not use elevation to turn every toolbar group, statistic, or empty state into a card.

## Shapes

### Geometry

Ferrix uses small radii and strong alignment:

- Cells, headers, tab strips, status bars, and full-width panels: square.
- Standard controls: 2–4 px radius.
- Dialogs and popovers: 6–8 px radius.
- Focus rings follow the control geometry.
- Pills are reserved for compact categorical values with a real label/value role; they are not a default button shape.

### Borders and separators

- Grid boundaries: 1 px.
- Selected cell: 2 px accent edge at normal scale.
- Search current match and formula reference: 1.5–2 px semantic edge.
- Panel separators: 1 px structural line.
- Disabled controls retain their boundary and label structure; opacity reduction must not make them disappear.

### Icons

- Prefer a coherent, simple line-icon family suitable for 14–16 px rendering.
- Pair unfamiliar icons with text in the ribbon and dialogs.
- Icon-only controls require a tooltip, accessible name, focus state, and a conventional symbol.
- Emoji may remain temporarily where they are already used, but they are not the long-term icon system: platform-dependent emoji color and geometry undermine visual consistency.
- Never use decorative sparkles or “magic wand” imagery for ordinary computation.

## Components

### Grid cell

States to support explicitly:

- Default, alternate row, table band, and beyond-data padding.
- Hovered header or resize boundary.
- Active cell and selected range.
- Editing in place.
- Formula result and formula-source view.
- Search match and current search match.
- Valid, invalid, warning, comment, and protected.
- Conditional format, data bar, sparkline, and merged region.
- Frozen or split boundary adjacency.
- Filtered, sorted, hidden, subtotal, and pivot-derived rows.
- Unavailable because a long destructive rewrite temporarily owns the source file.

The active cell must remain identifiable when conditional formatting and search highlighting are both present. Selection never erases the meaning of invalid, error, or comment markers.

### Row and column headers

- Headers are quieter than selected data but visibly separate from the grid.
- Clicking selects the full row or column; dragging extends selection.
- Resize and reorder affordances use the cursor plus a visible insertion or guide line.
- Sort and filter state appears in or adjacent to the relevant header and is never represented by color alone.
- Original row numbers remain visible in filtered search results. Never renumber a filtered 200-million-row source into a misleading 1…N sequence.

### Formula bar and name box

- The name box matches the row-header width and accepts A1 references or defined names.
- The formula editor uses monospace and shares one persistent input identity between single-line and multiline forms so caret and focus survive resizing.
- Enter commits; Escape cancels; Alt+Enter inserts a line break in multiline mode.
- The bar resizes from one to twelve text rows. Double-clicking the resize edge toggles between one and four rows.
- Formula preview values use number or error color, but parse details remain textual.
- Formula references use a stable cycle of distinct outline colors in dark and light themes; selection blue is excluded from that cycle.

### Ribbon controls

- Use concise verb-first labels: `Open`, `Save`, `Export CSV`, `Freeze rows`.
- Display shortcut and rationale in hover text; keep the visible row compact.
- Disabled commands remain visible and explain why they are disabled.
- Toggle controls visually distinguish off, hover, focus, and on.
- Adjacent controls with a shared task may form a group separated by spacing or one vertical rule; do not box every group into a card.

### Command palette

- Opens without discarding an in-progress cell edit.
- Keyboard owns navigation while open; the opening chord must not propagate to the grid.
- Results rank by textual relevance, then recent use, then stable registry order.
- Each result shows command title, category, shortcut, and disabled reason where relevant.
- The palette is a command accelerator, not a chatbot and not a place for natural-language promises the application cannot deterministically fulfill.

### Search and replace

- Search is live as text changes.
- Enter advances; Shift+Enter reverses; F3 remains available.
- Case, whole-cell, regex, and filter toggles have text tooltips and visible selected states.
- Regex syntax errors are distinct from zero matches.
- Results show current position, exact total, elapsed time, and truncation state.
- A capped filtered result must use high-severity text: it visually looks complete but is not.
- Replace distinguishes displayed values from formula source.
- Replace All is one undo step where technically feasible and reports examined/replaced counts.
- Cancel states whether applied replacements remain.

### Status bar

Left side: the latest actionable operation message.

Right side, in descending importance:

1. Errors, incomplete or invalid data, and blocked states.
2. Filtered/visible row counts and trace truncation.
3. Selection aggregates where available.
4. Background operation state.
5. Performance diagnostics such as FPS, frame time, and painted-cell count.

Diagnostics can be user-toggleable for release builds, but honesty signals and operation state cannot be hidden with them.

Status messages must identify the object and outcome: `A12 committed · 4 recalculated (83 µs)` is preferable to `Done`.

### Sheet tabs

- Tabs sit beneath the status bar and preserve conventional workbook placement.
- Active sheet is indicated by shape/edge and text weight, not color alone.
- Support click to switch, `+` to add, double-click or context action to rename, drag to reorder, and context action to delete.
- Deleting the last sheet is impossible; duplicate names are refused case-insensitively.
- Dirty workbook state is visible at the document or window level, not repeated noisily on every tab.

### Dialogs and builders

Use a predictable structure:

1. Concise title.
2. One sentence of consequence or scope when needed.
3. Labelled controls aligned to a grid.
4. Inline validation next to the field.
5. Optional preview or exact scope summary.
6. Right-aligned actions: secondary first, primary last.

Destructive actions use explicit object names: `Delete “Q4 Sales”`, not `Yes`. Safe cancellation is the default on Escape and outside-click; outside-click must not confirm.

For import, pivot, chart, validation, protection, page setup, goal seek, and other complex flows, preserve the user's current grid context wherever possible. A builder should show the source selection and destination before execution.

### Empty states

Empty states are operational, not promotional.

- Cold start: `Open a file` as the primary action, with recent files and supported formats available nearby.
- New empty sheet: show a usable grid immediately; do not replace it with a large onboarding card.
- Empty filter: `No rows match—filter mode is hiding every row`, with a direct way to clear or edit the filter.
- Empty panel: explain what selection or prerequisite enables it.

### Loading and progress

- File loading, conversion, export, print, compact, and large replace operations report a named operation, determinate progress when knowable, quantities, and cancellation.
- The layout does not jump when a progress indicator appears.
- A cancellable operation must say whether cancellation rolls back, leaves partial edits, or leaves a prior file intact.
- Background reads should not block editing if snapshot semantics make that safe.
- Operations rewriting the active backing file may block conflicting input, with an explicit reason.
- Spinners without explanatory text are insufficient beyond a brief sub-second delay.

### Notifications

Prefer persistent inline or status feedback for actions whose outcome affects data. Use transient toasts only for reversible, low-risk acknowledgements and always provide enough time to read them. Never put a data-loss warning solely in a toast.

## Do's and Don'ts

### Do

- Keep the grid dominant and viewport work bounded.
- Preserve established spreadsheet keyboard behavior.
- Show exact counts, units, destinations, and affected ranges.
- Explain why a control is disabled.
- Keep source data, cache, overlays, and exported output conceptually distinct in copy.
- Use progressive disclosure for advanced options while keeping them reachable from keyboard and registry.
- Let users cancel long work and state cancellation semantics precisely.
- Keep dark and light themes independently tuned and tested.
- Test interactions through painted geometry, not only by calling handlers.
- Prefer a flat panel, separator, or aligned row over a decorative card.
- Use familiar terminology from spreadsheets and data tooling.
- Protect unsaved work, prior exports, and valid preference files with atomic behavior.

### Don't

- Do not hide incomplete, sampled, capped, approximate, or stale results.
- Do not freeze the UI for a task that can safely use a worker and snapshot.
- Do not animate large grid regions or tie animation cost to row count.
- Do not remove shortcuts or direct manipulation to create a “cleaner” interface.
- Do not use selection blue for unrelated branding or decoration.
- Do not make every action an icon-only button.
- Do not use modal dialogs for routine navigation or harmless acknowledgements.
- Do not silently guess import formats, overwrite targets, resolve malformed patterns, or rebind broken sheet references.
- Do not render millions of widgets or primitives when the viewport can only reveal hundreds.
- Do not claim completion until the resulting file, sidecar, export, or preference has actually been verified.
- Do not imitate AI chat products, crypto dashboards, or generic SaaS landing pages.

### Interaction acceptance checklist

Every new feature must answer:

- Can it be reached by mouse and keyboard?
- Is its scope visible before execution?
- Does it preserve or explicitly replace undo history?
- What happens on Escape, close, failure, and cancellation?
- Does it remain correct at 0 rows, 1 row, 200 million rows, and capped results?
- Does it work with mapped and in-memory sheets?
- Does it work under both themes, selection, search highlight, and conditional formatting?
- Does it expose enough progress to distinguish slow work from a hang?
- Does it avoid allocations proportional to dataset size on the UI thread?
- Is the result phrased in concrete, testable language?

### Accessibility acceptance checklist

- All controls have accessible names and meaningful focus order.
- Every mouse action has a keyboard path unless the interaction is inherently spatial, in which case an alternate command exists.
- Focus remains visible in both themes and over all control states.
- Color is never the sole carrier of state.
- Text and non-text contrast meet the targets in this document after overlays are composited.
- Zoom and OS scaling do not clip controls or make dialogs unreachable.
- Reduced-motion preference removes nonessential animation.
- Error messages identify the field, object, or range and provide a correction path.
- Disabled controls expose their disabled reason.
- Screen-reader structure is evaluated as egui accessibility support evolves; missing framework support is tracked explicitly rather than assumed.

### Performance and smoothness acceptance checklist

- The grid targets 60 fps during ordinary navigation on supported hardware.
- Pointer, keyboard, focus, and selection feedback appears in the next frame.
- UI work scales with visible cells, active decorations, or explicit result limits—not total rows.
- Long I/O and computation leave a responsive cancel path.
- Progress updates are rate-limited enough not to dominate the work they describe.
- No operation causes avoidable full-layout jitter.
- Loading indicators appear only after a short threshold where possible, preventing flashes for microsecond opens.
- Background operations repaint only as often as useful progress changes.
- Telemetry measures real frame output and painted cells.
- Smoothness optimizations may not suppress errors, lower numerical integrity, silently sample data, or change operation semantics.

### Implementation guidance for egui

- Continue threading `Theme` values through rendering; no feature-level color constants.
- Derive hit testing, editing placement, markers, and overlays from the same grid geometry used for paint.
- Keep grid cells painter primitives rather than per-cell widgets.
- Keep scroll position in row/column space with sufficient numeric precision for extreme extents.
- Use stable widget IDs for controls whose visual shape changes, especially the formula editor.
- Persist user choices by stable slugs or human-recognizable workbook/sheet keys, never ephemeral enum indices or runtime IDs.
- Keep command metadata in the registry and dispatch through one path.
- Treat every displayed total, capped count, and progress quantity as a correctness assertion.
- Add theme and geometry tests alongside visual-state additions.
- Prefer semantic structs and tests over comments that merely request visual consistency.

### Definition of done for UI work

A Ferrix UI change is complete when:

1. Its normal, hover, focus, active, disabled, loading, empty, success, warning, and failure states that apply are implemented.
2. Keyboard, cancellation, and undo semantics are defined and exercised.
3. Dark and light themes are legible under real overlay combinations.
4. Painted geometry and hit geometry agree at supported zooms.
5. Dataset-scale behavior remains bounded and honest.
6. Persisted settings survive restart when persistence is part of the feature.
7. Destructive or external writes are verified and cannot silently replace good data with partial data.
8. User-facing language names scope, outcome, and limitations precisely.
9. The feature looks like part of Ferrix—not a transplanted web card, AI assistant, or unrelated design system.
