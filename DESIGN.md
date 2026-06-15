---
name: TokenOS Design System
description: Visual specifications for the Token-Optimal Agent Execution Kernel desktop app.
colors:
  bg: "#020617"
  panel-fill: "#0F172A"
  panel-dark: "#090D1A"
  text: "#F8FAFC"
  muted: "#94A3B8"
  accent: "#10B981"
  good: "#34D399"
  warn: "#F59E0B"
  bad: "#EF4444"
typography:
  display:
    fontFamily: "Fira Code, monospace"
    fontSize: "20px"
    fontWeight: 700
  body:
    fontFamily: "Fira Sans, sans-serif"
    fontSize: "14px"
    fontWeight: 400
rounded:
  sm: "4px"
  md: "6px"
  lg: "8px"
spacing:
  sm: "6px"
  md: "8px"
  lg: "12px"
components:
  button-inactive:
    backgroundColor: "{colors.panel-fill}"
    textColor: "{colors.text}"
    rounded: "{rounded.md}"
    padding: "8px 12px"
  button-hover:
    backgroundColor: "#1E293B"
    textColor: "{colors.accent}"
    rounded: "{rounded.md}"
    padding: "8px 12px"
  card-container:
    backgroundColor: "{colors.panel-fill}"
    rounded: "{rounded.lg}"
    padding: "12px"
---

# Design System: TokenOS

## 1. Overview

**Creative North Star: "The Telemetry Terminal"**

TokenOS is designed to look like a high-precision telemetry instrument. The interface is optimized for developer operations: dark mode by default, high visual contrast, structured layout grids, and dense information display. The system rejects typical modern SaaS clichés like soft shadows, giant rounded corners, and pastel colors in favor of a crisp, technical look.

**Key Characteristics:**
- OLED-ready dark backgrounds
- Minimal, precise layout borders rather than soft ambient shadows
- Monospace typography for statistics, numbers, and hashes
- Bold, functional status coloring (emerald green for telemetry correctness)

## 2. Colors

The color palette is built for a terminal-like OLED layout, using deep slates and high-contrast text.

### Primary
- **Emerald Telemetry** (#10B981): The primary accent color, representing successful local executions and positive routing states.

### Neutral
- **Deep Void Background** (#020617): The base window and panel background.
- **Slate Panel Fill** (#0F172A): Used for active UI blocks, side nav, and container panels.
- **Midnight Shadow** (#090D1A): Used for inset panel grids, console output bg, and text area backgrounds.
- **High-contrast Ink** (#F8FAFC): Primary text, pure high-visibility white.
- **Slate Muted** (#94A3B8): Secondary text, labels, and helper descriptions.

### Named Rules
**The Telemetry Rarity Rule.** Neon accent colors (primary teal/emerald) must be used on less than 10% of any screen surface to draw immediate attention to key telemetry actions and status checks.

## 3. Typography

**Display Font:** Fira Code (monospace)
**Body Font:** Fira Sans (sans-serif)
**Label/Mono Font:** Fira Code (monospace)

### Hierarchy
- **Display** (bold, 24px, line-height: 1.2): Main view headers and application title.
- **Headline** (strong, 16px, line-height: 1.3): Section headers and dashboard panel titles.
- **Title** (medium, 14px, line-height: 1.3): Card labels and navigation items.
- **Body** (regular, 14px, line-height: 1.4): Dynamic descriptions, configuration items, and table values.
- **Label** (monospace, 11px / 12px, letter-spacing: normal): Numbers, paths, latency, costs, and hashes.

### Named Rules
**The Monospace-for-Metrics Rule.** Every numerical metric, token count, cost representation, and hash must be rendered in monospace font to align vertically and ensure high technical legibility.

## 4. Elevation

The application is flat and structured, using border lines and background tone shifts instead of soft drop shadows. Depth is conveyed strictly by contrast.

### Named Rules
**The Zero Shadow Rule.** Traditional ambient shadows are prohibited. Interactive states (like hover) are indicated with background highlights and border shifts, never with shadow changes.

## 5. Components

### Buttons
- **Shape:** Medium rounding (6px)
- **Inactive State:** Slate panel fill (#0F172A) with white text.
- **Hover / Focus State:** Slate-800 background (#1E293B) with emerald telemetry text (#10B981).
- **Active State:** Dark slate background (#111827) with active border.

### Cards / Containers
- **Corner Style:** Large rounding (8px)
- **Background:** Slate Panel Fill (#0F172A)
- **Border:** Thin 1px border (#1E293B) at rest, glowing on hover.
- **Internal Padding:** Large spacing (12px)

### Inputs / Fields
- **Style:** Deep black inset (#090D1A) with 1px slate border.
- **Focus:** EmeraldTelemetry border glow.
- **Error:** Red border stroke (#EF4444).

## 6. Do's and Don't's

### Do:
- **Do** render all costs, hashes, token counts, and latency values in monospace.
- **Do** use Emerald Telemetry (#10B981) for positive status and active buttons.
- **Do** keep card and button corners between 6px and 8px.

### Don't:
- **Don't** use emojis as icons; stick to clean SVG/text indicators.
- **Don't** add ambient shadows (e.g. box-shadow blurs greater than 0px).
- **Don't** use warm cream/beige backgrounds (maintain the dark void).
- **Don't** use uppercase tracking kickers above section titles.
