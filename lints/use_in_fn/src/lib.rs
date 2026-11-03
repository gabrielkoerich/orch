#![feature(rustc_private)]

extern crate rustc_ast;
extern crate rustc_errors;

use rustc_ast::{Block, ItemKind, StmtKind};
use rustc_errors::{Diag, DiagDecorator};
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};

dylint_linting::declare_early_lint! {
    /// ### What it does
    ///
    /// Flags a `use` item inside a function, method or closure body.
    ///
    /// ### Why is this bad?
    ///
    /// Imports belong at the top of the module, where a reader looks for them. Clippy's
    /// `items_after_statements` catches a `use` after a statement, but nothing catches one
    /// at the start of a body.
    ///
    /// ### Known problems
    ///
    /// A `use` that a macro from another crate expands to is not flagged.
    ///
    /// ### Example
    ///
    /// ```rust
    /// fn main() {
    ///     use std::fs;
    ///     let _ = fs::read("Cargo.toml");
    /// }
    /// ```
    ///
    /// Use instead:
    ///
    /// ```rust
    /// use std::fs;
    ///
    /// fn main() {
    ///     let _ = fs::read("Cargo.toml");
    /// }
    /// ```
    pub USE_IN_FN,
    Warn,
    "a `use` item inside a function, method or closure body"
}

impl EarlyLintPass for UseInFn {
    fn check_block(&mut self, cx: &EarlyContext<'_>, block: &Block) {
        for stmt in &block.stmts {
            if let StmtKind::Item(item) = &stmt.kind
                && let ItemKind::Use(_) = item.kind
                && !item.span.in_external_macro(cx.sess().source_map())
            {
                let decorate = DiagDecorator(|diag: &mut Diag<'_, ()>| {
                    diag.primary_message("`use` inside a function body");
                    diag.help("move it to the top of the module");
                });
                cx.emit_span_lint(USE_IN_FN, item.span, decorate);
            }
        }
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
