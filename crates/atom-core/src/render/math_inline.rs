//! Inline math (`$…$`, `\(…\)`) rendered as readable Unicode text.
//!
//! The Kitty math engine (atom-tui's math.rs) owns *display* math:
//! ratatex 0.1.0 segments a message into prose and `$$…$$` regions and
//! rasterizes the latter into row-block images. It has no inline-math
//! segment at all, and its row×column image model could not sit inside
//! a wrapped text line anyway. So inline `$…$` reaches the plain
//! markdown path as raw LaTeX (as pulldown-cmark `InlineMath` events
//! in render::markdown). This module closes that gap: paired
//! delimiters and converts the LaTeX to terminal-safe Unicode (Greek
//! letters, super/subscripts, operators), styled with italics.
//!
//! Delimiter pairing follows pandoc's safety rules so prose that
//! merely contains dollar signs renders untouched: currency amounts
//! ("$5 vs $10") never pair because a closer directly followed by an
//! alphanumeric character is rejected, and spans with adjacent
//! whitespace or line breaks are not math.

use super::colors::{ansi_fg, COLOR_FOREGROUND};

const ITALIC: &str = "\x1b[3m";
const RESET: &str = "\x1b[0m";

/// Renders `src` (the dollar-delimiters already stripped) as styled
/// Unicode for embedding in an ANSI-styled markdown line.
pub fn styled(src: &str) -> String {
    let body = to_unicode(src);
    if body.is_empty() {
        return String::new();
    }
    format!("{}{}{}{}", ansi_fg(COLOR_FOREGROUND), ITALIC, body, RESET)
}

/// Detects an inline `$…$` span whose opening `$` sits at `open` (the
/// caller asserts `chars[open] == '$'`). Returns the index just past
/// the closing `$` plus the rendered body, or `None` when the pairing
/// rules say this is prose and the `$` should pass through literally.
pub fn inline_math_span(chars: &[char], open: usize) -> Option<(usize, String)> {
    // `$$` is display math — never claimed inline; neither is `$$…$$`'s
    // second `$` reached via fallback (the char after a close is checked
    // below), and an opener immediately followed by whitespace, a newline,
    // or nothing is prose.
    match chars.get(open + 1) {
        None | Some('$') => return None,
        Some(c) if c.is_whitespace() => return None,
        _ => {}
    }
    let mut close = open + 1;
    while close < chars.len() {
        match chars[close] {
            '\n' => return None, // inline spans a single line
            '$' => break,
            _ => close += 1,
        }
    }
    // No closer on the line, empty span, or whitespace against either
    // delimiter: all prose.
    if close >= chars.len() || close == open + 1 || chars[close - 1].is_whitespace() {
        return None;
    }
    // A closer directly followed by an alphanumeric character (or another
    // `$`) indicates paired currency amounts and the like — "$5 vs $10",
    // "$$x$$" — not math.
    match chars.get(close + 1) {
        Some(c) if c.is_alphanumeric() || *c == '$' => return None,
        _ => {}
    }
    let src: String = chars[open + 1..close].iter().collect();
    Some((close + 1, styled(&src)))
}

/// Converts a TeX fragment (no outer delimiters) to terminal-friendly
/// Unicode. Unknown commands degrade gracefully: the backslash is
/// dropped and the name kept.
pub fn to_unicode(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        i = parse_node(&chars, i, &mut out);
    }
    // TeX treats runs of whitespace as one space; drop leading/trailing.
    let mut collapsed = String::with_capacity(out.len());
    let mut pending_space = true;
    for c in out.chars() {
        if c == ' ' {
            if !pending_space {
                collapsed.push(' ');
                pending_space = true;
            }
        } else {
            collapsed.push(c);
            pending_space = c == '\u{2009}';
        }
    }
    collapsed.trim_end().to_string()
}

/// Renders one TeX atom starting at `i` into `out`, returning the index
/// just past it. Whitespace collapses; braced groups are transparent.
fn parse_node(chars: &[char], i: usize, out: &mut String) -> usize {
    match chars[i] {
        ' ' | '\t' => out.push(' '),
        '~' => out.push(' '), // TeX non-breaking space
        '{' => {
            if let Some(close) = matching_brace(chars, i) {
                let mut j = i + 1;
                while j < close {
                    j = parse_node(chars, j, out);
                }
                return close + 1;
            }
            out.push('{');
        }
        '}' => out.push('}'),
        '^' | '_' => {
            let (end, raw, conv) = read_arg(chars, i + 1);
            push_script(&raw, &conv, chars[i] == '^', out);
            return end;
        }
        '\'' => {
            let n = chars[i..].iter().take_while(|&&c| c == '\'').count();
            for _ in 0..n {
                out.push('′');
            }
            return i + n;
        }
        '\\' => return parse_command(chars, i + 1, out),
        '-' => out.push('−'),
        _ => out.push(chars[i]),
    }
    i + 1
}

/// Parses a command whose backslash was consumed; `i` points at the
/// name. Returns the index just past the whole command (including any
/// arguments it consumed).
fn parse_command(chars: &[char], i: usize, out: &mut String) -> usize {
    if i >= chars.len() {
        return i;
    }
    let (name, end) = if chars[i].is_ascii_alphabetic() {
        let n = chars[i..]
            .iter()
            .take_while(|&&c| c.is_ascii_alphabetic())
            .count();
        (chars[i..i + n].iter().collect::<String>(), i + n)
    } else {
        (chars[i].to_string(), i + 1)
    };

    match name.as_str() {
        // Escaped punctuation keeps its literal form.
        "$" | "{" | "}" | "%" | "&" | "#" | "_" => out.push_str(&name),
        // Spacing knobs. Thin space for `\,` and `\:`, nothing for `\!`,
        // per TeX's spacing correction classes.
        "," | ":" => out.push('\u{2009}'),
        ";" | " " => out.push(' '),
        "!" => {}
        "\\" => out.push(' '),
        "quad" => out.push('\u{2003}'),
        "qquad" => out.push_str("\u{2003}\u{2003}"),

        "frac" | "dfrac" | "tfrac" | "cfrac" => {
            let (e1, _, a) = read_arg(chars, end);
            let (e2, _, b) = read_arg(chars, e1);
            push_fraction(&a, &b, out);
            return e2;
        }
        "binom" => {
            let (e1, _, a) = read_arg(chars, end);
            let (e2, _, b) = read_arg(chars, e1);
            out.push('(');
            out.push_str(a.trim());
            out.push(' ');
            out.push_str(b.trim());
            out.push(')');
            return e2;
        }
        "over" | "atop" => out.push('/'),
        "sqrt" => {
            let mut j = end;
            if chars.get(j) == Some(&'[') {
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                j += 1;
            }
            let (e, _, a) = read_arg(chars, j);
            let a = a.trim();
            out.push('√');
            if a.chars().count() > 1 {
                out.push('(');
                out.push_str(a);
                out.push(')');
            } else {
                out.push_str(a);
            }
            return e;
        }

        // Upright text and font switches: keep the argument verbatim.
        "text"
        | "mathrm"
        | "textrm"
        | "textbf"
        | "textit"
        | "texttt"
        | "mbox"
        | "operatorname"
        | "operatornamewithlimits"
        | "mathbf"
        | "boldsymbol"
        | "bm"
        | "mathbb"
        | "mathcal"
        | "mathscr"
        | "mathit"
        | "mathsf"
        | "mathtt"
        | "mathfrak" => {
            let (e, _, a) = read_arg(chars, end);
            out.push_str(a.trim());
            separate_from_word(chars, e, out);
            return e;
        }

        "hat" | "widehat" => return accent_arg(chars, end, '\u{0302}', out),
        "tilde" | "widetilde" => return accent_arg(chars, end, '\u{0303}', out),
        "bar" | "overline" => return accent_arg(chars, end, '\u{0304}', out),
        "dot" => return accent_arg(chars, end, '\u{0307}', out),
        "ddot" => return accent_arg(chars, end, '\u{0308}', out),
        "check" => return accent_arg(chars, end, '\u{030C}', out),
        "breve" => return accent_arg(chars, end, '\u{0306}', out),
        "vec" | "overrightarrow" => return accent_arg(chars, end, '\u{20D7}', out),

        // Layout and sizing commands a terminal cannot honor: drop.
        "left" | "right" | "middle" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr"
        | "Bigl" | "Bigr" | "biggl" | "biggr" | "Biggl" | "Biggr" | "limits" | "nolimits"
        | "displaystyle" | "textstyle" | "scriptstyle" | "scriptscriptstyle" => {}

        "begin" | "end" => {
            let (e, _, _) = read_arg(chars, end);
            return e;
        }

        // Operator names stay upright and visually separated from what
        // follows ("sin x", not "sinx").
        "sin" | "cos" | "tan" | "cot" | "sec" | "csc" | "arcsin" | "arccos" | "arctan" | "sinh"
        | "cosh" | "tanh" | "coth" | "log" | "ln" | "lg" | "exp" | "lim" | "limsup" | "liminf"
        | "min" | "max" | "sup" | "inf" | "det" | "dim" | "ker" | "arg" | "gcd" | "deg" | "hom"
        | "Pr" | "im" | "re" => {
            out.push_str(&name);
            separate_from_word(chars, end, out);
        }

        _ => match symbol(&name) {
            Some(sym) => out.push_str(sym),
            // Unknown command: drop the backslash, keep the name.
            None => out.push_str(&name),
        },
    }
    end
}

/// Ensures a rendered upright word is followed by a space when body text
/// (a letter, digit, or another command) comes next, so `\log x` reads
/// "log x" rather than "logx".
fn separate_from_word(chars: &[char], i: usize, out: &mut String) {
    if let Some(c) = chars.get(i) {
        if c.is_alphanumeric() || *c == '\\' {
            out.push(' ');
        }
    }
}

fn accent_arg(chars: &[char], i: usize, mark: char, out: &mut String) -> usize {
    let (end, _, a) = read_arg(chars, i);
    let a = a.trim();
    out.push_str(a);
    out.push(mark);
    end
}

/// Reads one TeX argument at `i`: a braced group, a single character, or
/// an escape/command. Returns (index past it, raw source, converted text).
fn read_arg(chars: &[char], i: usize) -> (usize, String, String) {
    if i >= chars.len() {
        return (i, String::new(), String::new());
    }
    if chars[i] == '{' {
        if let Some(close) = matching_brace(chars, i) {
            let raw: String = chars[i + 1..close].iter().collect();
            let mut buf = String::new();
            let mut j = i + 1;
            while j < close {
                j = parse_node(chars, j, &mut buf);
            }
            return (close + 1, raw, buf);
        }
    }
    if chars[i] == '\\' {
        let start = i;
        let mut buf = String::new();
        let end = parse_command(chars, i + 1, &mut buf);
        return (end, chars[start..end].iter().collect(), buf);
    }
    let mut buf = String::new();
    parse_node(chars, i, &mut buf);
    (i + 1, chars[i].to_string(), buf)
}

fn matching_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Emits `a/b`, parenthesizing sides that would otherwise read
/// ambiguously ("x/(log x)", but "n(n+1)(2n+1)/6").
fn push_fraction(a: &str, b: &str, out: &mut String) {
    let a = a.trim();
    let b = b.trim();
    let ambiguous = |s: &str| s.contains(' ') || s.contains('/');
    if ambiguous(a) {
        out.push('(');
        out.push_str(a);
        out.push(')');
    } else {
        out.push_str(a);
    }
    out.push('/');
    if ambiguous(b) {
        out.push('(');
        out.push_str(b);
        out.push(')');
    } else {
        out.push_str(b);
    }
}

/// Renders a super/subscript argument. Single ASCII characters with a
/// Unicode super/subscript counterpart become one (x², p⁻ˢ, xᵢ₊₁);
/// anything else falls back to a readable caret/underscore form
/// (a^(b₁)) rather than dropping the structure. Single-character
/// fallbacks skip the parentheses (int_C → ∫_C).
fn push_script(raw: &str, conv: &str, is_sup: bool, out: &mut String) {
    if !raw.contains('\\') && !raw.is_empty() {
        let mut mapped = String::new();
        let mut ok = true;
        for c in raw.chars() {
            match if is_sup { sup_char(c) } else { sub_char(c) } {
                Some(m) => mapped.push(m),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            out.push_str(&mapped);
            return;
        }
    }
    let conv = conv.trim();
    if conv.chars().count() <= 1 {
        out.push(if is_sup { '^' } else { '_' });
        out.push_str(conv);
    } else {
        out.push_str(if is_sup { "^(" } else { "_(" });
        out.push_str(conv);
        out.push(')');
    }
}

fn sup_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' | '−' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        _ => return None,
    })
}

fn sub_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' | '−' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// Command names that map to a single Unicode symbol.
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "varepsilon" => "ϵ",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "nabla" => "∇",
        "partial" => "∂",
        "infty" => "∞",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "emptyset" | "varnothing" => "∅",
        "cdot" => "·",
        "cdots" => "⋯",
        "ldots" | "dots" | "dotsc" | "dotso" => "…",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "times" => "×",
        "div" => "÷",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "odot" => "⊙",
        "pm" => "±",
        "mp" => "∓",
        "le" | "leq" | "leqslant" => "≤",
        "ge" | "geq" | "geqslant" => "≥",
        "ne" | "neq" => "≠",
        "equiv" => "≡",
        "approx" => "≈",
        "simeq" => "≃",
        "cong" => "≅",
        "sim" => "∼",
        "propto" => "∝",
        "asymp" => "≍",
        "ll" => "≪",
        "gg" => "≫",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "in" => "∈",
        "ni" => "∋",
        "notin" => "∉",
        "cup" => "∪",
        "cap" => "∩",
        "sqcup" => "⊔",
        "sqcap" => "⊓",
        "setminus" => "∖",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "neg" | "lnot" => "¬",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "iiiint" => "⨌",
        "oint" => "∮",
        "oiint" => "∯",
        "angle" => "∠",
        "perp" => "⊥",
        "parallel" => "∥",
        "mid" => "∣",
        "prime" => "′",
        "ell" => "ℓ",
        "hbar" => "ℏ",
        "imath" => "ı",
        "jmath" => "ȷ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "aleph" => "ℵ",
        "wp" => "℘",
        "bot" => "⊥",
        "top" => "⊤",
        "to" => "→",
        "rightarrow" => "→",
        "longrightarrow" => "⟶",
        "leftarrow" | "gets" => "←",
        "longleftarrow" => "⟵",
        "Rightarrow" | "implies" => "⇒",
        "Longrightarrow" => "⟹",
        "Leftarrow" | "impliedby" => "⇐",
        "leftrightarrow" => "↔",
        "Leftrightarrow" | "iff" => "⇔",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "Uparrow" => "⇑",
        "Downarrow" => "⇓",
        "mapsto" => "↦",
        "hookrightarrow" => "↪",
        "hookleftarrow" => "↩",
        "rightharpoonup" => "⇀",
        "leftharpoondown" => "↽",
        "langle" => "⟨",
        "rangle" => "⟩",
        "vert" | "lvert" | "rvert" => "∣",
        "Vert" | "lVert" | "rVert" => "‖",
        "|" => "‖",
        "\\" => " ",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{inline_math_span, to_unicode};

    fn rendered(src: &str) -> String {
        to_unicode(src)
    }

    #[test]
    fn greek_letters_render() {
        assert_eq!(rendered(r"\pi(x)"), "π(x)");
        assert_eq!(rendered(r"s = \sigma + it"), "s = σ + it");
        assert_eq!(rendered(r"\nabla \cdot \mathbf{F}"), "∇ · F");
        assert_eq!(rendered(r"\zeta(s)"), "ζ(s)");
    }

    #[test]
    fn super_and_subscripts_render() {
        assert_eq!(rendered(r"x^2 + y^2"), "x² + y²");
        assert_eq!(rendered(r"p^{-s}"), "p⁻ˢ");
        assert_eq!(rendered(r"p^{-2s}"), "p⁻²ˢ");
        assert_eq!(rendered(r"x_1"), "x₁");
        assert_eq!(rendered(r"n_{k+1}"), "nₖ₊₁");
        // Unmappable scripts fall back to a readable form.
        assert_eq!(rendered(r"a^{b+C}"), "a^(b+C)");
        // Single unmappable character keeps the bare marker form.
        assert_eq!(rendered(r"\int_C F"), "∫_C F");
    }

    #[test]
    fn fractions_and_roots_render() {
        assert_eq!(rendered(r"\frac{x}{6}"), "x/6");
        assert_eq!(rendered(r"\frac{x}{\log x}"), "x/(log x)");
        assert_eq!(rendered(r"\sqrt{2}"), "√2");
        assert_eq!(rendered(r"\sqrt{x+1}"), "√(x+1)");
        assert_eq!(rendered(r"\frac{n(n+1)(2n+1)}{6}"), "n(n+1)(2n+1)/6");
    }

    #[test]
    fn operators_and_words_render() {
        assert_eq!(rendered(r"7 + 8 = 15"), "7 + 8 = 15");
        assert_eq!(
            rendered(r"1 + p^{-s} + p^{-2s} + \cdots"),
            "1 + p⁻ˢ + p⁻²ˢ + ⋯"
        );
        assert_eq!(rendered(r"\log x"), "log x");
        assert_eq!(rendered(r"\sin(x)"), "sin(x)");
        assert_eq!(
            rendered(r"W = \int_C \mathbf{F} \cdot d\mathbf{r}"),
            "W = ∫_C F · dr"
        );
        assert_eq!(rendered(r"\hat{\mathbf{n}}\,dA"), "n̂\u{2009}dA");
        assert_eq!(rendered(r"\text{Re}(s) > 1"), "Re(s) > 1");
    }

    #[test]
    fn span_rules_follow_pandoc() {
        let math: Vec<char> = "at $x^2$ now".chars().collect();
        assert!(inline_math_span(&math, 3).is_some());
        // Currency amounts do not pair: closer followed by a digit.
        let money: Vec<char> = "$5 vs $10 total".chars().collect();
        assert!(inline_math_span(&money, 0).is_none());
        // Display math and stray delimiters pass through.
        let display: Vec<char> = "$$x^2$$".chars().collect();
        assert!(inline_math_span(&display, 0).is_none());
        let spaced: Vec<char> = "a $ x $ b".chars().collect();
        assert!(inline_math_span(&spaced, 2).is_none());
        let open: Vec<char> = "cost is $5".chars().collect();
        assert!(inline_math_span(&open, 8).is_none());
        // Real math next to currency still pairs.
        let mixed: Vec<char> = "$5 and $x^2$ reward".chars().collect();
        assert!(inline_math_span(&mixed, 9).is_some());
    }

    #[test]
    fn markdown_path_converts_inline_math() {
        let out = crate::render::markdown::render_markdown(
            "Count them: $\\pi(x)$ is the number of primes up to $x$.",
            80,
        );
        assert!(out.contains("π(x)"), "got {out:?}");
        assert!(out.contains("primes up to"), "got {out:?}");
        assert!(!out.contains("\\pi"), "got {out:?}");
        // Currency is preserved verbatim.
        let out = crate::render::markdown::render_markdown("it costs $5 vs $10", 80);
        assert!(out.contains("$5"), "got {out:?}");
        assert!(out.contains("$10"), "got {out:?}");
    }
}
