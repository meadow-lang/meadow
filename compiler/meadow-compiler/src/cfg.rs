//! **Conditional compilation**: `@cfg(…)`.
//!
//! A declaration -- or a record field, a named constructor field, an effect
//! operation -- carrying `@cfg(condition)` is compiled only where the condition
//! holds, and is gone before anything else sees the module: it is not resolved,
//! not type-checked, and cannot be named. Several `@cfg`s on one declaration
//! must all hold.
//!
//! ```text
//! @cfg(os = "windows")
//! def separator = "\\"
//!
//! @cfg(not(os = "windows"))
//! def separator = "/"
//! ```
//!
//! A condition is what [`crate::Cfg`] knows:
//!
//! | condition | holds when |
//! |---|---|
//! | `os = "windows"` / `"linux"` / `"macos"` / `"android"` | the program is built for that system |
//! | `arch = "x86_64"` / `"aarch64"` | … that processor |
//! | `family = "unix"` / `"windows"`, or bare `unix` / `windows` | … that family of systems |
//! | `profile = "debug"` / `"release"`, or bare `debug` / `release` | the build profile |
//! | `backend = "vm"` / `"jit"` / `"aot"` / `"silo"` / `"cek"` | what will run it: Glade's interpreter, JIT or ahead-of-time code, Silo, or the CEK machine |
//! | `opt_level = "0"` / `"1"` / `"2"` | the optimisation level |
//! | bare `test` | `meadow test` is building it |
//! | any other name, or `name = "value"` | the build turned that flag on -- `--cfg fast`, `--cfg feature=gpu` |
//! | `feature = "name"` | …or the package is built with that one of its `[features]` |
//!
//! and `all(…)`, `any(…)` and `not(…)` combine them, as in Rust. A value a
//! built-in name cannot have -- `os = "linx"` -- is an error rather than a
//! condition that is quietly never true; a flag nobody set is simply off.

use crate::options::Options;
use meadow_ast as ast;
use meadow_diagnostics::Diagnostic;
use meadow_span::Span;

/// Remove from `module` everything whose `@cfg` does not hold under `opts`,
/// reporting conditions that cannot be read to `diags`.
pub fn strip(module: &mut ast::Module, opts: Options, filename: &str, diags: &mut Vec<Diagnostic>) {
    let mut report = |msg: String, span: Span| {
        diags.push(Diagnostic::new(
            msg,
            filename.to_string(),
            ("in this `@cfg`".to_string(), span),
            vec![],
        ))
    };
    module.decls.retain_mut(|d| {
        let ast::Decl::Attributed(attrs, inner) = &mut *d.value else {
            return true;
        };
        if !enabled(attrs, &opts, &mut report) {
            return false;
        }
        match &mut *inner.value {
            ast::Decl::Record(r) => fields(&mut r.fields, &opts, &mut report),
            ast::Decl::Effect(e) => fields(&mut e.ops, &opts, &mut report),
            ast::Decl::Data(data) => {
                for v in &mut data.variants {
                    if let ast::VariantFields::Named(fs) = &mut v.fields {
                        fields(fs, &opts, &mut report);
                    }
                }
            }
            _ => {}
        }
        true
    });
    // A declaration without attributes can still have fields that do.
    for d in &mut module.decls {
        match &mut *d.value {
            ast::Decl::Record(r) => fields(&mut r.fields, &opts, &mut report),
            ast::Decl::Effect(e) => fields(&mut e.ops, &opts, &mut report),
            ast::Decl::Data(data) => {
                for v in &mut data.variants {
                    if let ast::VariantFields::Named(fs) = &mut v.fields {
                        fields(fs, &opts, &mut report);
                    }
                }
            }
            _ => {}
        }
    }
}

fn fields(fields: &mut Vec<ast::Field>, opts: &Options, report: &mut impl FnMut(String, Span)) {
    fields.retain(|f| enabled(&f.attrs, opts, report));
}

/// Whether every `@cfg` among `attrs` holds. A condition that cannot be read is
/// reported and taken to hold, so the one error is the only one.
fn enabled(attrs: &[ast::Attr], opts: &Options, report: &mut impl FnMut(String, Span)) -> bool {
    attrs
        .iter()
        .filter(|a| &**a.name.value() == "cfg")
        .all(|a| match a.meta.as_slice() {
            [condition] => holds(condition, opts).unwrap_or_else(|(msg, span)| {
                report(msg, span);
                true
            }),
            _ => {
                report(
                    "`@cfg` takes one condition, as `@cfg(unix)` or `@cfg(all(unix, not(test)))`"
                        .to_string(),
                    a.name.span,
                );
                true
            }
        })
}

/// Whether `condition` holds under `opts`.
pub fn holds(condition: &ast::Meta, opts: &Options) -> Result<bool, (String, Span)> {
    let cfg = &opts.cfg;
    match condition {
        // `@cfg("…")` says nothing: a condition is a name, not a string.
        ast::Meta::Text(t) => Err((format!("`\"{}\"` is not a condition", t.value()), t.span)),
        ast::Meta::Word(name) => Ok(match &**name.value() {
            "unix" | "windows" => cfg.family() == &**name.value(),
            "debug" | "release" => cfg.profile == &**name.value(),
            "test" => cfg.test,
            flag => cfg.has_flag(flag),
        }),
        ast::Meta::Value(key, value) => {
            let (actual, allowed): (String, &[&str]) = match &**key.value() {
                "os" => (
                    cfg.os.to_string(),
                    &["windows", "linux", "macos", "android", "ios"],
                ),
                "arch" => (cfg.arch.to_string(), &["x86_64", "aarch64"]),
                "family" => (cfg.family().to_string(), &["unix", "windows"]),
                "profile" => (cfg.profile.to_string(), &["debug", "release"]),
                "backend" => (
                    cfg.backend.to_string(),
                    &["vm", "jit", "aot", "silo", "cek"],
                ),
                "opt_level" => (opts.opt.name()[1..].to_string(), &["0", "1", "2"]),
                flag => return Ok(cfg.has_flag(&format!("{flag}={}", value.value()))),
            };
            if !allowed.contains(&&**value.value()) {
                return Err((
                    format!(
                        "`{}` is never `\"{}\"`; it is one of {}",
                        key.value(),
                        value.value(),
                        allowed
                            .iter()
                            .map(|v| format!("`\"{v}\"`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    value.span,
                ));
            }
            Ok(actual == **value.value())
        }
        ast::Meta::List(name, args) => match (&**name.value(), args.as_slice()) {
            ("all", _) => args
                .iter()
                .try_fold(true, |acc, a| Ok(holds(a, opts)? && acc)),
            ("any", _) => args
                .iter()
                .try_fold(false, |acc, a| Ok(holds(a, opts)? || acc)),
            ("not", [one]) => Ok(!holds(one, opts)?),
            ("not", _) => Err(("`not` takes exactly one condition".to_string(), name.span)),
            (other, _) => Err((
                format!("there is no `{other}(…)`; conditions combine with `all`, `any` and `not`"),
                name.span,
            )),
        },
    }
}
