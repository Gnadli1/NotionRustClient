//! TeX → Unicode pretty-print for equations. Handles the common subset
//! (Greek, scripts, `\frac`, `\sqrt`, big operators, relations, arrows,
//! `\mathbb`, `\text`); anything else is None and the caller shows the source.
//! ponytail: no layout engine, so matrices/environments/`\\` always fall back.

/// The pretty form of `src`, or None if it uses something we can't render.
pub fn pretty_tex(src: &str) -> Option<String> {
    // bounds the recursion (one level per `{`); Notion equations are short
    if src.len() > 1000 {
        return None;
    }
    let mut p = P {
        cs: src.chars().collect(),
        i: 0,
    };
    p.seq(false)
}

struct P {
    cs: Vec<char>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<char> {
        self.cs.get(self.i).copied()
    }

    /// Output up to the end (or the closing `}` when `group`).
    fn seq(&mut self, group: bool) -> Option<String> {
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return (!group).then_some(out);
            };
            self.i += 1;
            match c {
                '}' => return group.then_some(out),
                '{' => out += &self.seq(true)?,
                '^' | '_' => out += &script(&self.arg()?, c == '^'),
                '\\' => out += &self.cmd()?,
                '&' => return None,
                '~' => out.push(' '),
                c => out.push(c),
            }
        }
    }

    /// One argument: a `{group}`, a command, or a single char.
    fn arg(&mut self) -> Option<String> {
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
        let c = self.peek()?;
        self.i += 1;
        match c {
            '{' => self.seq(true),
            '\\' => self.cmd(),
            '}' | '^' | '_' | '&' => None,
            c => Some(c.to_string()),
        }
    }

    /// A command; the `\` is already consumed.
    fn cmd(&mut self) -> Option<String> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        let name: String = self.cs[start..self.i].iter().collect();
        if name.is_empty() {
            let c = self.peek()?;
            self.i += 1;
            return match c {
                ',' | ':' | ';' | ' ' => Some(" ".into()),
                '!' => Some(String::new()),
                // `\\` ends a line; Notion's block equations show it as one
                '\\' => Some("\n".into()),
                '{' | '}' | '%' | '$' | '&' | '#' | '_' => Some(c.to_string()),
                '|' => Some("‖".into()),
                _ => None,
            };
        }
        Some(match name.as_str() {
            "frac" | "dfrac" | "tfrac" => {
                let (a, b) = (self.arg()?, self.arg()?);
                if a.chars().count() == 1 && b.chars().count() == 1 {
                    format!("{a}⁄{b}")
                } else {
                    format!("({a})/({b})")
                }
            }
            "sqrt" => {
                let root = if self.peek() == Some('[') {
                    self.i += 1;
                    let start = self.i;
                    while self.peek()? != ']' {
                        self.i += 1;
                    }
                    let n: String = self.cs[start..self.i].iter().collect();
                    self.i += 1;
                    n.chars().map(sup).collect::<Option<String>>()?
                } else {
                    String::new()
                };
                let x = self.arg()?;
                if x.chars().count() == 1 {
                    format!("{root}√{x}")
                } else {
                    format!("{root}√({x})")
                }
            }
            "left" | "right" => {
                // `\left.` is an invisible delimiter
                if self.peek() == Some('.') {
                    self.i += 1;
                }
                String::new()
            }
            "mathbb" => match self.arg()?.as_str() {
                "R" => "ℝ",
                "N" => "ℕ",
                "Z" => "ℤ",
                "Q" => "ℚ",
                "C" => "ℂ",
                _ => return None,
            }
            .into(),
            // some notes use the `\Z`/`\R` shorthand rather than \mathbb
            "R" => "ℝ".into(),
            "N" => "ℕ".into(),
            "Z" => "ℤ".into(),
            "Q" => "ℚ".into(),
            "C" => "ℂ".into(),
            "text" | "mathrm" | "textrm" | "operatorname" => self.arg()?,
            "quad" => " ".into(),
            "qquad" => "  ".into(),
            n => symbol(n)?.into(),
        })
    }
}

/// `s` as a super/subscript: Unicode if every char has one, else `^(s)`.
fn script(s: &str, up: bool) -> String {
    let map = if up { sup } else { sub };
    if let Some(u) = s.chars().map(map).collect::<Option<String>>() {
        return u;
    }
    let mark = if up { '^' } else { '_' };
    if s.chars().count() == 1 {
        format!("{mark}{s}")
    } else {
        format!("{mark}({s})")
    }
}

fn pair(c: char, from: &str, to: &str) -> Option<char> {
    from.chars()
        .position(|f| f == c)
        .and_then(|i| to.chars().nth(i))
}

fn sup(c: char) -> Option<char> {
    pair(
        c,
        "0123456789+-−=()abcdefghijklmnoprstuvwxyzT",
        "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁻⁼⁽⁾ᵃᵇᶜᵈᵉᶠᵍʰⁱʲᵏˡᵐⁿᵒᵖʳˢᵗᵘᵛʷˣʸᶻᵀ",
    )
}

fn sub(c: char) -> Option<char> {
    pair(
        c,
        "0123456789+-−=()aehijklmnoprstuvx",
        "₀₁₂₃₄₅₆₇₈₉₊₋₋₌₍₎ₐₑₕᵢⱼₖₗₘₙₒₚᵣₛₜᵤᵥₓ",
    )
}

fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
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
        "omicron" => "ο",
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
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "oint" => "∮",
        "lim" => "lim",
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "log" => "log",
        "ln" => "ln",
        "exp" => "exp",
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" => "∼",
        "propto" => "∝",
        "in" => "∈",
        "notin" => "∉",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "cup" => "∪",
        "cap" => "∩",
        "forall" => "∀",
        "exists" => "∃",
        "neg" => "¬",
        "land" => "∧",
        "lor" => "∨",
        "to" | "rightarrow" => "→",
        "leftarrow" => "←",
        "Rightarrow" => "⇒",
        "implies" | "Longrightarrow" => "⟹",
        "Leftarrow" => "⇐",
        "longleftarrow" => "⟵",
        "leftrightarrow" => "↔",
        "Leftrightarrow" => "⇔",
        "iff" | "Longleftrightarrow" => "⟺",
        "mapsto" => "↦",
        // Notion's editor sometimes spells `\in` as `\isin`
        "isin" => "∈",
        "ldots" => "…",
        "cdots" => "⋯",
        "circ" => "∘",
        "angle" => "∠",
        "degree" => "°",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders() {
        let cases = [
            ("x", "x"),
            ("a + b = c", "a + b = c"),
            (r"\alpha + \beta", "α + β"),
            (r"\Gamma \Delta \Omega", "Γ Δ Ω"),
            ("x^2", "x²"),
            ("e^{i\\pi}", "e^(iπ)"),
            ("x^{n+1}", "xⁿ⁺¹"),
            ("a_1", "a₁"),
            ("a_{ij}", "aᵢⱼ"),
            ("x_{\\alpha}", "x_α"),
            ("x_{yz}", "x_(yz)"),
            (r"\frac{1}{2}", "1⁄2"),
            (r"\frac{a+b}{2}", "(a+b)/(2)"),
            (r"\sqrt{x}", "√x"),
            (r"\sqrt{x+1}", "√(x+1)"),
            (r"\sqrt[3]{x}", "³√x"),
            (r"\sum_{i=1}^{n} i", "∑ᵢ₌₁ⁿ i"),
            (r"\int_0^\infty f", "∫₀^∞ f"),
            (r"\lim_{x \to 0}", "lim_(x → 0)"),
            (r"\prod \oint \partial \nabla", "∏ ∮ ∂ ∇"),
            (r"a \le b \ge c \ne d \approx e", "a ≤ b ≥ c ≠ d ≈ e"),
            (r"\pm \mp \times \div \cdot", "± ∓ × ÷ ·"),
            (r"x \in \mathbb{R}", "x ∈ ℝ"),
            (
                r"\mathbb{N} \subset \mathbb{Z} \subseteq \mathbb{Q} \cup \mathbb{C}",
                "ℕ ⊂ ℤ ⊆ ℚ ∪ ℂ",
            ),
            (r"\forall x \exists y", "∀ x ∃ y"),
            (r"A \Rightarrow B \Leftrightarrow C", "A ⇒ B ⇔ C"),
            (r"f \mapsto g \circ h", "f ↦ g ∘ h"),
            (r"\text{if } x", "if  x"),
            (r"\mathrm{d}x", "dx"),
            (r"\left( x \right)", "( x )"),
            (r"a\,b\;c\quad d", "a b c  d"),
            (r"{a}{b}", "ab"),
            (r"1, \ldots, n \cdots", "1, …, n ⋯"),
            (r"\neg p \land q \lor r", "¬ p ∧ q ∨ r"),
            (r"90^\circ \angle", "90^∘ ∠"),
            (r"E = mc^2", "E = mc²"),
            (r"\left. x \right|", " x |"),
            (r"T \implies Periodendauer", "T ⟹ Periodendauer"),
            (r"n \isin \Z", "n ∈ ℤ"),
            (r"a \\ b", "a \n b"),
            ("a\n\\\\ \\mapsto b", "a\n\n ↦ b"),
        ];
        for (src, want) in cases {
            assert_eq!(pretty_tex(src).as_deref(), Some(want), "{src}");
        }
    }

    #[test]
    fn falls_back() {
        for src in [
            r"\begin{matrix} a & b \end{matrix}",
            r"\unknown x",
            r"\mathbb{X}",
            "{a",
            "a}",
            "x^",
            r"\frac{a}",
            r"\sqrt[\alpha]{2}",
        ] {
            assert_eq!(pretty_tex(src), None, "{src}");
        }
        assert_eq!(pretty_tex(&"{".repeat(100_000)), None, "no stack overflow");
    }
}
