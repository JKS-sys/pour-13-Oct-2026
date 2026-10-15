//! Pour's colours: one hue per kind of thing, so a glance tells a formula from an
//! app, a version from a path, a flag from an argument. No pink or magenta anywhere
//! (no hue between 280° and 350°); every colour reads at 4.5:1 or better on a dark
//! terminal and stays legible on a light one. Colour follows the terminal: off in
//! pipes and with NO_COLOR, on in CI logs (GitHub Actions, GitLab, Buildkite…),
//! which render ANSI colour.
//!
//! Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved. See LICENSE.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hue {
    Gold,
    Amber,
    Orange,
    Red,
    Lime,
    Green,
    Teal,
    Cyan,
    Sky,
    Blue,
    Indigo,
    Violet,
    Slate,
}

impl Hue {
    /// (truecolor RGB, xterm-256 fallback)
    pub fn rgb(self) -> ((u8, u8, u8), u8) {
        match self {
            Hue::Gold => ((232, 181, 58), 179),
            Hue::Amber => ((245, 158, 11), 214),
            Hue::Orange => ((251, 146, 60), 209),
            Hue::Red => ((248, 113, 113), 203),
            Hue::Lime => ((163, 230, 53), 149),
            Hue::Green => ((74, 222, 128), 78),
            Hue::Teal => ((45, 212, 191), 43),
            Hue::Cyan => ((34, 211, 238), 45),
            Hue::Sky => ((56, 189, 248), 39),
            Hue::Blue => ((96, 165, 250), 75),
            Hue::Indigo => ((129, 140, 248), 105),
            Hue::Violet => ((167, 139, 250), 141),
            Hue::Slate => ((148, 163, 184), 247),
        }
    }

    /// Hue angle in degrees (for the no-pink audit).
    #[cfg(test)]
    pub fn degrees(self) -> f64 {
        let ((r, g, b), _) = self.rgb();
        let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        if d == 0.0 {
            return 0.0;
        }
        let h = if max == r {
            60.0 * (((g - b) / d) % 6.0)
        } else if max == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        if h < 0.0 {
            h + 360.0
        } else {
            h
        }
    }
}

#[cfg(test)]
pub const ALL: [Hue; 13] = [
    Hue::Gold,
    Hue::Amber,
    Hue::Orange,
    Hue::Red,
    Hue::Lime,
    Hue::Green,
    Hue::Teal,
    Hue::Cyan,
    Hue::Sky,
    Hue::Blue,
    Hue::Indigo,
    Hue::Violet,
    Hue::Slate,
];

/// Brackets and nesting levels, outermost first.
pub const DEPTH: [Hue; 6] = [
    Hue::Gold,
    Hue::Cyan,
    Hue::Lime,
    Hue::Violet,
    Hue::Orange,
    Hue::Sky,
];

/// Colours for CI logs that are not terminals: GitHub Actions, GitLab CI,
/// Buildkite, Azure Pipelines, CircleCI and FORCE_COLOR all render ANSI.
pub fn init() {
    let env = |k: &str| std::env::var_os(k).is_some();
    if env("NO_COLOR") {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
        return;
    }
    if env("FORCE_COLOR")
        || env("GITHUB_ACTIONS")
        || env("GITLAB_CI")
        || env("BUILDKITE")
        || env("TF_BUILD")
        || env("CIRCLECI")
        || std::env::var("CLICOLOR_FORCE").is_ok_and(|v| v != "0")
    {
        console::set_colors_enabled(true);
        console::set_colors_enabled_stderr(true);
    }
}

fn truecolor() -> bool {
    matches!(
        std::env::var("COLORTERM").unwrap_or_default().as_str(),
        "truecolor" | "24bit"
    ) || std::env::var_os("GITHUB_ACTIONS").is_some()
        || std::env::var("TERM_PROGRAM").is_ok_and(|t| t != "Apple_Terminal")
}

fn sgr(h: Hue, bold: bool) -> String {
    let ((r, g, b), n) = h.rgb();
    let b0 = if bold { "1;" } else { "" };
    if truecolor() {
        format!("\x1b[{b0}38;2;{r};{g};{b}m")
    } else {
        format!("\x1b[{b0}38;5;{n}m")
    }
}

/// `text` in hue `h` (plain when colours are off).
pub fn c(h: Hue, text: impl AsRef<str>) -> String {
    let t = text.as_ref();
    if !console::colors_enabled() || t.is_empty() {
        return t.to_string();
    }
    format!("{}{t}\x1b[0m", sgr(h, false))
}

/// Bold and coloured.
pub fn b(h: Hue, text: impl AsRef<str>) -> String {
    let t = text.as_ref();
    if !console::colors_enabled() || t.is_empty() {
        return t.to_string();
    }
    format!("{}{t}\x1b[0m", sgr(h, true))
}

// ---- one hue per kind of thing -------------------------------------------------

pub fn formula(n: &str) -> String {
    b(Hue::Teal, n)
}
pub fn cask(n: &str) -> String {
    b(Hue::Gold, n)
}
pub fn tool(n: &str) -> String {
    b(Hue::Lime, n)
}
pub fn version(v: &str) -> String {
    c(Hue::Cyan, v)
}
pub fn old_version(v: &str) -> String {
    c(Hue::Red, v)
}
pub fn new_version(v: &str) -> String {
    c(Hue::Green, v)
}
pub fn number(n: impl std::fmt::Display) -> String {
    c(Hue::Cyan, n.to_string())
}
pub fn path(p: &str) -> String {
    c(Hue::Sky, p)
}
pub fn url(u: &str) -> String {
    c(Hue::Blue, u)
}
pub fn tap(t: &str) -> String {
    c(Hue::Violet, t)
}
pub fn dim(t: &str) -> String {
    c(Hue::Slate, t)
}
pub fn brew(t: &str) -> String {
    c(Hue::Amber, t)
}

/// Section headings each get their own hue, in a fixed order.
pub fn heading(label: &str, i: usize) -> String {
    let h = [
        Hue::Gold,
        Hue::Teal,
        Hue::Sky,
        Hue::Lime,
        Hue::Violet,
        Hue::Orange,
        Hue::Blue,
        Hue::Green,
        Hue::Indigo,
        Hue::Amber,
    ][i % 10];
    format!("{} {}", b(h, "==>"), b(h, label))
}

/// A command line in colour: `pour` gold, the subcommand sky, flags violet,
/// package names teal (formulae) or gold (after --cask), placeholders slate.
pub fn cmd(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut cask_next = false;
    for (i, w) in line.split(' ').enumerate() {
        let s = if i == 0 {
            b(Hue::Gold, w)
        } else if w.starts_with('-') {
            cask_next = w == "--cask";
            c(Hue::Violet, w)
        } else if w.starts_with('<') || w.ends_with('…') {
            c(Hue::Slate, w)
        } else if i == 1 {
            c(Hue::Sky, w)
        } else if cask_next {
            cask(w)
        } else {
            c(Hue::Teal, w)
        };
        out.push(s);
    }
    out.join(" ")
}

/// Pretty JSON with keys blue, strings green, numbers cyan, booleans orange,
/// null red and brackets coloured by depth. Plain `to_string_pretty` when colours are off.
pub fn json(v: &Value) -> String {
    if !console::colors_enabled() {
        return serde_json::to_string_pretty(v).unwrap_or_default();
    }
    let mut out = String::new();
    write_json(v, 0, &mut out);
    out
}

fn bracket(ch: &str, depth: usize) -> String {
    b(DEPTH[depth % DEPTH.len()], ch)
}

fn write_json(v: &Value, depth: usize, out: &mut String) {
    let pad = |d: usize| "  ".repeat(d);
    match v {
        Value::Null => out.push_str(&c(Hue::Red, "null")),
        Value::Bool(x) => out.push_str(&c(Hue::Orange, x.to_string())),
        Value::Number(n) => out.push_str(&c(Hue::Cyan, n.to_string())),
        Value::String(s) => {
            out.push_str(&c(Hue::Green, serde_json::to_string(s).unwrap_or_default()))
        }
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str(&bracket("[]", depth));
                return;
            }
            out.push_str(&bracket("[", depth));
            out.push('\n');
            for (i, x) in a.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                write_json(x, depth + 1, out);
                if i + 1 < a.len() {
                    out.push_str(&dim(","));
                }
                out.push('\n');
            }
            out.push_str(&pad(depth));
            out.push_str(&bracket("]", depth));
        }
        Value::Object(o) => {
            if o.is_empty() {
                out.push_str(&bracket("{}", depth));
                return;
            }
            out.push_str(&bracket("{", depth));
            out.push('\n');
            for (i, (k, x)) in o.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                out.push_str(&c(Hue::Blue, serde_json::to_string(k).unwrap_or_default()));
                out.push_str(&dim(": "));
                write_json(x, depth + 1, out);
                if i + 1 < o.len() {
                    out.push_str(&dim(","));
                }
                out.push('\n');
            }
            out.push_str(&pad(depth));
            out.push_str(&bracket("}", depth));
        }
    }
}

/// Print JSON: coloured on a terminal or in CI, plain in pipes.
pub fn print_json(v: &Value) {
    println!("{}", json(v));
}

/// A small bar chart cell: `██████░░░░` in `h`, `frac` from 0 to 1.
pub fn bar(frac: f64, width: usize, h: Hue) -> String {
    let f = frac.clamp(0.0, 1.0);
    let full = (f * width as f64).round() as usize;
    format!(
        "{}{}",
        c(h, "█".repeat(full)),
        dim(&"░".repeat(width.saturating_sub(full)))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pink_anywhere() {
        for h in ALL {
            let d = h.degrees();
            assert!(
                !(280.0..=350.0).contains(&d),
                "{h:?} is {d:.0}° — pink/magenta is not allowed"
            );
        }
    }

    #[test]
    fn plain_when_off_and_depth_coloured_when_on() {
        // one test: the colour switch is global, parallel tests must not race on it
        console::set_colors_enabled(false);
        assert_eq!(cmd("pour install --cask peek"), "pour install --cask peek");
        assert_eq!(
            json(&serde_json::json!({"a": [1, true]})),
            "{\n  \"a\": [\n    1,\n    true\n  ]\n}"
        );
        console::set_colors_enabled(true);
        let s = json(&serde_json::json!({"a": {"b": [1]}}));
        console::set_colors_enabled(false);
        // three nesting levels → three different bracket colours
        let n = DEPTH[..3]
            .iter()
            .filter(|h| s.contains(&sgr(**h, true)))
            .count();
        assert_eq!(n, 3);
    }
}
