//! **Reading the LIR back from LLVM text**: the inverse of the printer, for
//! the LIR's own forms. The back end writes its fixed functions -- sharing,
//! erasing, a field's read, the ways in from the runtime -- as LLVM text,
//! the clearest way to write a fixed function, and reads them back here as
//! LIR; and a test reads every function of real programs back, and checks
//! that it prints as it was written.

use crate::{BinOp, Cast, Decl, FCmp, Func, GTy, Global, ICmp, Init, Inst, Item, Ty, Val};

/// What went wrong, and on which line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: String,
    pub why: &'static str,
}

fn bad<T>(line: &str, why: &'static str) -> Result<T, Error> {
    Err(Error {
        line: line.to_string(),
        why,
    })
}

/// The functions defined in `text`, in order: every `define ... { ... }` in
/// it. What is outside them is left.
pub fn funcs(text: &str) -> Result<Vec<Func>, Error> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if !line.starts_with("define ") {
            continue;
        }
        let mut f = header(line)?;
        let mut first = true;
        for l in lines.by_ref() {
            if l == "}" {
                break;
            }
            if first {
                first = false;
                if l == "entry:" {
                    continue;
                }
                f.unlabelled = true;
            }
            if let Some(label) = l.strip_suffix(':')
                && !label.starts_with(' ')
            {
                f.body.push(Inst::Label(label.to_string()));
                continue;
            }
            let Some(text) = l.strip_prefix("  ") else {
                return bad(l, "an instruction not indented by two spaces");
            };
            f.body.push(inst(text)?);
        }
        out.push(f);
    }
    Ok(out)
}

/// Everything in `text`, a module or a part of one, as items: functions,
/// declarations, globals, and what only LLVM reads (comments, metadata,
/// blank lines) as notes. A function's definition is followed by a blank
/// line, which is its own.
pub fn items(text: &str) -> Result<Vec<Item>, Error> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].strip_suffix('\n').unwrap_or(lines[i]);
        if line.starts_with("define ") {
            let start = i;
            while i < lines.len() && lines[i].trim_end_matches('\n') != "}" {
                i += 1;
            }
            if i == lines.len() {
                return bad(line, "a definition not closed");
            }
            i += 1;
            if i >= lines.len() || lines[i] != "\n" {
                return bad(line, "a definition not followed by a blank line");
            }
            i += 1;
            let one: String = lines[start..i].concat();
            out.push(Item::Func(func(&one)?));
            continue;
        }
        if line.starts_with("declare ") {
            out.push(Item::Decl(decl(line)?));
        } else if line.starts_with('@') && line.contains(" = ") {
            out.push(Item::Global(global(line)?));
        } else {
            out.push(Item::Note(lines[i].to_string()));
        }
        i += 1;
    }
    Ok(out)
}

/// `declare [linkage] [cc] ret @name(types) [attrs]`.
pub fn decl(line: &str) -> Result<Decl, Error> {
    let rest = line.strip_prefix("declare ").ok_or(Error {
        line: line.into(),
        why: "not a declaration",
    })?;
    let open = rest.find('(').ok_or(Error {
        line: line.into(),
        why: "a declaration without parameters",
    })?;
    let close = rest.rfind(')').ok_or(Error {
        line: line.into(),
        why: "a declaration's parameters unclosed",
    })?;
    let words: Vec<&str> = rest[..open].split(' ').collect();
    let (name, words) = words.split_last().ok_or(Error {
        line: line.into(),
        why: "a declaration without a name",
    })?;
    let (ret, words) = words.split_last().ok_or(Error {
        line: line.into(),
        why: "a declaration without a type",
    })?;
    let mut linkage = Vec::new();
    let mut cc = None;
    for w in words {
        match *w {
            "ghccc" | "tailcc" => cc = Some(w.to_string()),
            _ => linkage.push(*w),
        }
    }
    let inside = &rest[open + 1..close];
    let params = if inside.is_empty() {
        Vec::new()
    } else {
        inside
            .split(", ")
            .map(|t| ty(line, t))
            .collect::<Result<_, _>>()?
    };
    Ok(Decl {
        name: name.to_string(),
        linkage: linkage.join(" "),
        cc,
        ret: ty(line, ret)?,
        params,
        attrs: rest[close + 1..].trim().to_string(),
    })
}

/// `@name = [linkage] constant|global type [init]`.
pub fn global(line: &str) -> Result<Global, Error> {
    let (name, rest) = line.split_once(" = ").ok_or(Error {
        line: line.into(),
        why: "not a global",
    })?;
    let mut linkage = Vec::new();
    let mut rest = rest;
    let constant = loop {
        let (w, r) = rest.split_once(' ').ok_or(Error {
            line: line.into(),
            why: "a global of no type",
        })?;
        rest = r;
        match w {
            "constant" => break true,
            "global" => break false,
            _ => linkage.push(w),
        }
    };
    let (gty, rest) = if let Some(r) = rest.strip_prefix('[') {
        let (inside, r) = r.split_once(']').ok_or(Error {
            line: line.into(),
            why: "an array type unclosed",
        })?;
        let (n, elem) = inside.split_once(" x ").ok_or(Error {
            line: line.into(),
            why: "an array type",
        })?;
        let n = n.parse().map_err(|_| Error {
            line: line.into(),
            why: "an array's length",
        })?;
        (GTy::array(n, ty(line, elem)?), r.trim_start())
    } else {
        let (t, r) = rest.split_once(' ').unwrap_or((rest, ""));
        (GTy::scalar(ty(line, t)?), r)
    };
    let init = if rest.is_empty() {
        None
    } else {
        Some(init(line, rest)?)
    };
    Ok(Global {
        name: name.to_string(),
        linkage: linkage.join(" "),
        constant,
        ty: gty,
        init,
    })
}

fn init(line: &str, s: &str) -> Result<Init, Error> {
    Ok(match s {
        "zeroinitializer" => Init::Zero,
        "null" => Init::Null,
        _ if s.starts_with("c\"") => {
            let inner = s[2..].strip_suffix('"').ok_or(Error {
                line: line.into(),
                why: "text unclosed",
            })?;
            let mut bytes = Vec::new();
            let raw = inner.as_bytes();
            let mut k = 0;
            while k < raw.len() {
                if raw[k] == b'\\' {
                    let hex = std::str::from_utf8(&raw[k + 1..k + 3]).unwrap_or("");
                    bytes.push(u8::from_str_radix(hex, 16).map_err(|_| Error {
                        line: line.into(),
                        why: "an escape in text",
                    })?);
                    k += 3;
                } else {
                    bytes.push(raw[k]);
                    k += 1;
                }
            }
            Init::Bytes(bytes)
        }
        _ if s.starts_with('[') => {
            let inner = s[1..].strip_suffix(']').ok_or(Error {
                line: line.into(),
                why: "an array unclosed",
            })?;
            let mut xs = Vec::new();
            for e in inner.split(", ") {
                let (_, v) = e.split_once(' ').ok_or(Error {
                    line: line.into(),
                    why: "an element without a type",
                })?;
                xs.push(init(line, v)?);
            }
            Init::Elems(xs)
        }
        _ if s.starts_with('@') => Init::Ref(s.to_string()),
        _ => Init::Int(s.parse().map_err(|_| Error {
            line: line.into(),
            why: "an initial value the LIR has not",
        })?),
    })
}

/// The one function defined in `text`.
pub fn func(text: &str) -> Result<Func, Error> {
    let mut fs = funcs(text)?;
    match fs.len() {
        1 => Ok(fs.remove(0)),
        _ => bad(text.lines().next().unwrap_or(""), "not one function"),
    }
}

/// A function's first line: `define [linkage] [cc] ret @name(params) [attrs] {`.
fn header(line: &str) -> Result<Func, Error> {
    let rest = &line["define ".len()..];
    let Some(rest) = rest.strip_suffix(" {") else {
        return bad(line, "a definition's line ends ` {`");
    };
    let open = rest.find('(').ok_or(Error {
        line: line.into(),
        why: "no parameters",
    })?;
    let close = rest.rfind(')').ok_or(Error {
        line: line.into(),
        why: "no parameters",
    })?;
    let before: Vec<&str> = rest[..open].split(' ').collect();
    let attrs = rest[close + 1..].trim().to_string();
    let (name, before) = before.split_last().ok_or(Error {
        line: line.into(),
        why: "no name",
    })?;
    let (ret, before) = before.split_last().ok_or(Error {
        line: line.into(),
        why: "no type",
    })?;
    let ret = Ty::named(ret).ok_or(Error {
        line: line.into(),
        why: "an answer of a type the LIR has not",
    })?;
    let mut linkage = Vec::new();
    let mut cc = None;
    for w in before {
        match *w {
            "ghccc" | "tailcc" => cc = Some(w.to_string()),
            _ => linkage.push(*w),
        }
    }
    let mut params = Vec::new();
    let inside = &rest[open + 1..close];
    if !inside.is_empty() {
        for p in inside.split(", ") {
            let (t, v) = p.split_once(' ').ok_or(Error {
                line: line.into(),
                why: "a parameter without a name",
            })?;
            let t = Ty::named(t).ok_or(Error {
                line: line.into(),
                why: "a parameter of a type the LIR has not",
            })?;
            params.push((t, v.to_string()));
        }
    }
    Ok(Func {
        name: name.to_string(),
        linkage: linkage.join(" "),
        cc,
        ret: Some(ret),
        params,
        attrs,
        ..Func::default()
    })
}

fn ty(line: &str, t: &str) -> Result<Ty, Error> {
    Ty::named(t).ok_or(Error {
        line: line.into(),
        why: "a type the LIR has not",
    })
}

/// `a, b` as its two operands.
fn two(line: &str, s: &str) -> Result<(Val, Val), Error> {
    match s.split_once(", ") {
        Some((a, b)) => Ok((a.to_string(), b.to_string())),
        None => bad(line, "two operands expected"),
    }
}

/// `%name` as a label's name.
fn label(line: &str, s: &str) -> Result<String, Error> {
    match s.strip_prefix("label %") {
        Some(l) => Ok(l.to_string()),
        None => bad(line, "a label expected"),
    }
}

/// One instruction, written as the printer writes it.
pub fn inst(line: &str) -> Result<Inst, Error> {
    if line == "unreachable" {
        return Ok(Inst::Unreachable);
    }
    if let Some(rest) = line.strip_prefix("br ") {
        if let Some(to) = rest.strip_prefix("label %") {
            return Ok(Inst::Br(to.to_string()));
        }
        let Some(rest) = rest.strip_prefix("i1 ") else {
            return bad(line, "a branch on something not an `i1`");
        };
        let (rest, unlikely) = match rest.rsplit_once(", !prof !") {
            Some((r, n)) => (
                r,
                Some(n.parse().map_err(|_| Error {
                    line: line.into(),
                    why: "branch weights not a number",
                })?),
            ),
            None => (rest, None),
        };
        let parts: Vec<&str> = rest.split(", ").collect();
        let [cond, yes, no] = parts.as_slice() else {
            return bad(line, "a branch of other than two ways");
        };
        return Ok(Inst::CondBr {
            cond: cond.to_string(),
            yes: label(line, yes)?,
            no: label(line, no)?,
            unlikely,
        });
    }
    if let Some(rest) = line.strip_prefix("ret ") {
        if rest == "void" {
            return Ok(Inst::Ret {
                ty: Ty::Void,
                v: None,
            });
        }
        let (t, v) = rest.split_once(' ').ok_or(Error {
            line: line.into(),
            why: "a return without a value",
        })?;
        return Ok(Inst::Ret {
            ty: ty(line, t)?,
            v: Some(v.to_string()),
        });
    }
    if let Some(rest) = line.strip_prefix("switch ") {
        let (t, rest) = rest.split_once(' ').ok_or(Error {
            line: line.into(),
            why: "a switch",
        })?;
        let t = ty(line, t)?;
        let (head, cases) = rest.split_once(" [").ok_or(Error {
            line: line.into(),
            why: "a switch without cases",
        })?;
        let (v, default) = two(line, head)?;
        let cases = cases.strip_suffix(" ]").ok_or(Error {
            line: line.into(),
            why: "a switch's cases unclosed",
        })?;
        let mut out = Vec::new();
        let words: Vec<&str> = cases.split_whitespace().collect();
        for c in words.chunks(4) {
            let [_, k, _, to] = c else {
                return bad(line, "a switch's case");
            };
            let k = k.trim_end_matches(',').parse().map_err(|_| Error {
                line: line.into(),
                why: "a case not a number",
            })?;
            out.push((k, to.trim_start_matches('%').to_string()));
        }
        return Ok(Inst::Switch {
            ty: t,
            v,
            default: label(line, &default)?,
            cases: out,
        });
    }
    if let Some(rest) = line.strip_prefix("store ") {
        let (rest, volatile) = match rest.strip_prefix("volatile ") {
            Some(r) => (r, true),
            None => (rest, false),
        };
        let (t, rest) = rest.split_once(' ').ok_or(Error {
            line: line.into(),
            why: "a store",
        })?;
        let (v, p) = two(line, rest)?;
        let p = p.strip_prefix("ptr ").ok_or(Error {
            line: line.into(),
            why: "a store to no pointer",
        })?;
        return Ok(Inst::Store {
            ty: ty(line, t)?,
            v,
            ptr: p.to_string(),
            volatile,
        });
    }
    // What answers into a register, or a call that answers nothing.
    let (dst, rhs) = match line.split_once(" = ") {
        Some((d, r)) if d.starts_with('%') => (Some(d.to_string()), r),
        _ => (None, line),
    };
    if rhs.starts_with("call ") || rhs.starts_with("tail call ") {
        return call(line, dst, rhs);
    }
    let Some(dst) = dst else {
        return bad(line, "an instruction the LIR has not");
    };
    let (op, rest) = rhs.split_once(' ').ok_or(Error {
        line: line.into(),
        why: "an instruction without operands",
    })?;
    match op {
        "icmp" | "fcmp" => {
            let (c, rest) = rest.split_once(' ').ok_or(Error {
                line: line.into(),
                why: "a comparison",
            })?;
            let (t, rest) = rest.split_once(' ').ok_or(Error {
                line: line.into(),
                why: "a comparison",
            })?;
            let (a, b) = two(line, rest)?;
            let t = ty(line, t)?;
            if op == "icmp" {
                let cond = ICmp::named(c).ok_or(Error {
                    line: line.into(),
                    why: "an integer comparison the LIR has not",
                })?;
                Ok(Inst::ICmp {
                    dst,
                    cond,
                    ty: t,
                    a,
                    b,
                })
            } else {
                let cond = FCmp::named(c).ok_or(Error {
                    line: line.into(),
                    why: "a floating comparison the LIR has not",
                })?;
                Ok(Inst::FCmp {
                    dst,
                    cond,
                    ty: t,
                    a,
                    b,
                })
            }
        }
        "load" => {
            let (rest, volatile) = match rest.strip_prefix("volatile ") {
                Some(r) => (r, true),
                None => (rest, false),
            };
            let (t, p) = two(line, rest)?;
            let p = p.strip_prefix("ptr ").ok_or(Error {
                line: line.into(),
                why: "a load from no pointer",
            })?;
            Ok(Inst::Load {
                dst,
                ty: ty(line, &t)?,
                ptr: p.to_string(),
                volatile,
            })
        }
        "getelementptr" => {
            let parts: Vec<&str> = rest.split(", ").collect();
            let [elem, base, index] = parts.as_slice() else {
                return bad(line, "an address of other than one index");
            };
            Ok(Inst::Gep {
                dst,
                elem: ty(line, elem)?,
                base: base
                    .strip_prefix("ptr ")
                    .ok_or(Error {
                        line: line.into(),
                        why: "an address from no pointer",
                    })?
                    .to_string(),
                index: index
                    .strip_prefix("i64 ")
                    .ok_or(Error {
                        line: line.into(),
                        why: "an index not an `i64`",
                    })?
                    .to_string(),
            })
        }
        "alloca" => {
            let n = rest
                .strip_prefix('[')
                .and_then(|r| r.strip_suffix(" x i64]"))
                .and_then(|n| n.parse().ok())
                .ok_or(Error {
                    line: line.into(),
                    why: "an alloca of other than words",
                })?;
            Ok(Inst::Alloca { dst, words: n })
        }
        "select" => {
            let parts: Vec<&str> = rest.split(", ").collect();
            let [c, a, b] = parts.as_slice() else {
                return bad(line, "a select");
            };
            let c = c.strip_prefix("i1 ").ok_or(Error {
                line: line.into(),
                why: "a select on no `i1`",
            })?;
            let (ta, a) = a.split_once(' ').ok_or(Error {
                line: line.into(),
                why: "a select's operand",
            })?;
            let (_, b) = b.split_once(' ').ok_or(Error {
                line: line.into(),
                why: "a select's operand",
            })?;
            Ok(Inst::Select {
                dst,
                cond: c.to_string(),
                ty: ty(line, ta)?,
                a: a.to_string(),
                b: b.to_string(),
            })
        }
        "phi" => {
            let (t, rest) = rest.split_once(' ').ok_or(Error {
                line: line.into(),
                why: "a phi",
            })?;
            let mut incoming = Vec::new();
            for arm in rest.split("], ") {
                let arm = arm.trim_start_matches("[ ").trim_end_matches(" ]").trim();
                let (v, from) = two(line, arm)?;
                incoming.push((v, from.trim_start_matches('%').to_string()));
            }
            Ok(Inst::Phi {
                dst,
                ty: ty(line, t)?,
                incoming,
            })
        }
        _ => {
            if let Some(op) = BinOp::named(op) {
                let (t, rest) = rest.split_once(' ').ok_or(Error {
                    line: line.into(),
                    why: "an operation",
                })?;
                let (a, b) = two(line, rest)?;
                return Ok(Inst::Bin {
                    dst,
                    op,
                    ty: ty(line, t)?,
                    a,
                    b,
                });
            }
            if let Some(c) = Cast::named(op) {
                let (from, rest) = rest.split_once(' ').ok_or(Error {
                    line: line.into(),
                    why: "a cast",
                })?;
                let (v, to) = rest.split_once(" to ").ok_or(Error {
                    line: line.into(),
                    why: "a cast to nothing",
                })?;
                return Ok(Inst::Cast {
                    dst,
                    op: c,
                    from: ty(line, from)?,
                    v: v.to_string(),
                    to: ty(line, to)?,
                });
            }
            bad(line, "an instruction the LIR has not")
        }
    }
}

/// `[tail] call [cc] ret callee(args)`.
fn call(line: &str, dst: Option<Val>, rhs: &str) -> Result<Inst, Error> {
    let (tail, rest) = match rhs.strip_prefix("tail call ") {
        Some(r) => (true, r),
        None => (false, &rhs["call ".len()..]),
    };
    let (cc, rest) = match rest.split_once(' ') {
        Some((c @ ("ghccc" | "tailcc"), r)) => (Some(c.to_string()), r),
        _ => (None, rest),
    };
    let (ret, rest) = rest.split_once(' ').ok_or(Error {
        line: line.into(),
        why: "a call",
    })?;
    let open = rest.find('(').ok_or(Error {
        line: line.into(),
        why: "a call without arguments",
    })?;
    let inside = rest[open + 1..].strip_suffix(')').ok_or(Error {
        line: line.into(),
        why: "a call's arguments unclosed",
    })?;
    let mut args = Vec::new();
    if !inside.is_empty() {
        for a in inside.split(", ") {
            let (t, v) = a.split_once(' ').ok_or(Error {
                line: line.into(),
                why: "an argument without a type",
            })?;
            args.push((ty(line, t)?, v.to_string()));
        }
    }
    Ok(Inst::Call {
        dst,
        tail,
        cc,
        ret: ty(line, ret)?,
        callee: rest[..open].to_string(),
        args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every function of every `.ll` file under `MEADOW_LIR_CORPUS`, read and
    /// printed again, is the text it was: the printer and this agree on
    /// everything the back end writes. Without the variable, a few by hand.
    #[test]
    fn what_is_read_prints_as_it_was_written() {
        let text = "\
define hidden ghccc i64 @mw.L.f.1(i64 %ctx, i64 %a0) noinline {
entry:
  %t1 = alloca [4 x i64]
  %t2 = icmp eq i64 %a0, 0
  br i1 %t2, label %b1, label %b2, !prof !1
b1:
  %t3 = tail call ghccc i64 @mw.L.g.1(i64 %ctx, i64 %a0)
  ret i64 %t3
b2:
  %t4 = phi i64 [ %a0, %entry ], [ 0, %b1 ]
  switch i64 %t4, label %b3 [ i64 4, label %b4 i64 5, label %b5 ]
b3:
  call void @meadow_fail(ptr @mw.s.1, i64 3)
  unreachable
}
";
        let f = func(text).unwrap();
        assert_eq!(format!("{f}"), format!("{text}\n"));
        let Ok(dir) = std::env::var("MEADOW_LIR_CORPUS") else {
            return;
        };
        let mut files = Vec::new();
        walk(std::path::Path::new(&dir), &mut files);
        let mut n = 0;
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let mut at = 0;
            while let Some(start) = text[at..].find("\ndefine ") {
                let start = at + start + 1;
                let end = start + text[start..].find("\n}\n").expect("closed") + 3;
                let one = &text[start..end];
                let f = func(one).unwrap_or_else(|e| panic!("{}: {e:?}", file.display()));
                assert_eq!(f.to_string(), format!("{one}\n"), "in {}", file.display());
                n += 1;
                at = end;
            }
        }
        eprintln!("{n} functions read and printed back");
        // And every module whole: its declarations and globals too.
        let mut m = 0;
        let mut files = Vec::new();
        walk(std::path::Path::new(&dir), &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let read = items(&text).unwrap_or_else(|e| panic!("{}: {e:?}", file.display()));
            let printed: String = read.iter().map(|i| i.to_string()).collect();
            assert_eq!(printed, text, "in {}", file.display());
            m += 1;
        }
        eprintln!("{m} modules read and printed back");
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "ll") {
                out.push(p);
            }
        }
    }
}
