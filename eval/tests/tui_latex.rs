//! LaTeX math → Unicode approximations.
//!
//! Kept in `/eval` per the test-location policy: behavioral rendering tests.

use pantheon_tui::richtext::render_latex;

#[test]
fn inline_superscript() {
    assert_eq!(render_latex("$x^2$"), "x²");
    assert_eq!(render_latex("$x^{10}$"), "x¹⁰");
}

#[test]
fn inline_subscript() {
    assert_eq!(render_latex("$a_n$"), "aₙ");
}

#[test]
fn greek_and_symbols() {
    let out = render_latex(r"$\alpha + \beta = \pi$, $\infty \rightarrow$");
    assert!(out.contains('α') && out.contains('β'), "greek: {out}");
    assert!(out.contains('π'), "pi: {out}");
    assert!(out.contains('∞') && out.contains('→'), "symbols: {out}");
}

#[test]
fn inline_frac_uses_fraction_slash() {
    assert_eq!(render_latex(r"$\frac{1}{2}$"), "1⁄2");
}

#[test]
fn display_frac_stacks() {
    let out = render_latex("$$\n\\frac{a+b}{c}\n$$");
    assert!(out.contains('─'), "fraction bar:\n{out}");
    assert!(out.contains("a+b") && out.contains('c'), "parts:\n{out}");
}

#[test]
fn operators() {
    let out = render_latex(r"$\sum_{i=1}^{n} i = \int_0^\infty x dx$, $\sqrt{2} \pm 1$");
    for ch in ['∑', '∫', '√', '±', '∞'] {
        assert!(out.contains(ch), "missing {ch}: {out}");
    }
}

#[test]
fn unknown_commands_pass_through() {
    let out = render_latex(r"$\foo{bar}$");
    assert!(out.contains(r"\foo"), "unknown kept: {out}");
}

#[test]
fn unmatched_dollar_passes_through() {
    // A single `$` with no partner is not math; it stays verbatim.
    assert_eq!(render_latex("price is $5"), "price is $5");
}

#[test]
fn prose_around_math_survives() {
    let out = render_latex("Einstein: $E=mc^2$ forever");
    assert!(out.starts_with("Einstein: "), "prefix: {out}");
    assert!(out.ends_with(" forever"), "suffix: {out}");
    assert!(out.contains("E=mc²"), "math: {out}");
}
