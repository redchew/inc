//! High level language analysis and transformations.
use {
    crate::{
        compiler::state::State,
        core::{Expr::*, Literal::*, *},
    },
    std::{clone::Clone, collections::HashMap},
};

/// Perform all language transformations and analysis on the syntax tree
///
/// A syntax tree is renamed into unique references, lambdas lifted to top level
/// and then program broken down into simpler ANF expressions and then tail
/// calls are annotated with a marker.
pub fn analyze(s: &mut State, prog: Vec<Syntax>) -> Vec<Core> {
    prog.into_iter()
        .map(|e| rename(&HashMap::new(), &Ident::empty(), 0, e))
        .flat_map(lift)
        .map(|e| inline(s, e))
        .map(anf)
        .map(tco)
        .collect()
}

/** Rename all references to unique names.

Unique **identifiers** for each variable in a program is a prerequisite for any
program analysis. Each [String] in the source program is replaced with a fully
qualified, globally unique [Ident] and the type change from [Expr]<[String]> to
[Expr]<[Ident]> conveys the basic idea.

* Top level definitions `(define pi 3.14)` can map to the identifiers literally as `pi`
* Named closures and functions are namespaced with the function name `f::x` and `f::y`
* Function **arguments** are named like local variables.
* Unnamed bindings are indexed like`{let 0}::a`

This is a fairly tricky to get right and being able to reuse a well tested
existing implementation would be great. See [RFC 2603], its [discussion] and
[tracking issue] to learn how rustc does this. See tests for more info

[RFC 2603]: https://github.com/rust-lang/rfcs/blob/master/text/2603-rust-symbol-name-mangling-v0.md
[discussion]: https://github.com/rust-lang/rfcs/pull/2603
[tracking issue]: https://github.com/rust-lang/rust/issues/60705
 **/
fn rename(env: &HashMap<&str, Ident>, base: &Ident, index: u8, prog: Syntax) -> Core {
    match prog {
        // If an identifier is defined already, refer to it, otherwise create a
        // new one in the top level environment since its unbound.
        Identifier(s) => {
            env.get(s.as_str()).map_or(Ident::expr(s), |n| Expr::Identifier(n.clone()))
        }
        Let { bindings, body } => {
            let base = base.extend(format!("{{let {}}}", index));

            // Collect all the names about to be bound for evaluating body
            let mut all = env.clone();
            for (name, _val) in bindings.iter() {
                all.insert(name.as_str(), base.extend(name));
            }

            // A sub expression in let binding is evaluated with the complete
            // environment including the one being defined only if the subexpresison
            // captures the closure with another let or lambda, otherwise evaluate with
            // only the rest of the bindings.
            Let {
                bindings: bindings
                    .iter()
                    .map(|(current, value)| {
                        // Collect all the names excluding the one being defined now
                        let mut rest = env.clone();
                        for (name, _) in bindings.iter() {
                            if name != current {
                                rest.insert(name.as_str(), base.extend(name));
                            }
                        }

                        let value = match value {
                            Let { .. } => rename(&all, &base, index + 1, value.clone()),
                            Lambda(c) => {
                                let base = base.extend(current);
                                rename(&all, &base, index + 1, Lambda(c.clone()))
                            }
                            _ => rename(&rest, &base, index + 1, value.clone()),
                        };

                        let ident = all.get(current.as_str()).unwrap().clone();

                        (ident, value)
                    })
                    .collect(),

                body: body.into_iter().map(|b| rename(&all, &base, index + 1, b)).collect(),
            }
        }

        List(list) => List(list.into_iter().map(|l| rename(env, base, index, l)).collect()),

        Cond { pred, then, alt } => Cond {
            pred: box rename(env, base, index, *pred),
            then: box rename(env, base, index, *then),
            alt: alt.map(|u| box rename(env, base, index, *u)),
        },

        Lambda(Closure { formals, free, body, tail }) => {
            let mut env = env.clone();
            for arg in formals.iter() {
                env.insert(arg, base.extend(arg));
            }

            Lambda(Closure {
                formals: formals.iter().map(|arg| base.extend(arg)).collect(),
                free: free.into_iter().map(|arg| base.extend(arg)).collect(),
                body: body.into_iter().map(|b| rename(&env, base, 0, b)).collect(),
                tail,
            })
        }

        Define { name, val } => {
            Define { name: base.extend(&name), val: box rename(env, &base.extend(&name), 0, *val) }
        }

        Vector(list) => Vector(list.into_iter().map(|l| rename(env, base, index, l)).collect()),

        // All literals and constants evaluate to itself
        Literal(v) => Literal(v),
    }
}

/// Lift all lambdas to top level
///
/// See http://matt.might.net/articles/closure-conversion
fn lift(prog: Core) -> Vec<Core> {
    match prog {
        Let { bindings, body } => {
            // Rest is all the name bindings that are not functions
            let rest: Vec<(Ident, Core)> = bindings
                .iter()
                .filter_map(|(ident, expr)| match expr {
                    Lambda(_) => None,
                    _ => Some((ident.clone(), shrink(lift(expr.clone())))),
                })
                .collect();

            let mut export: Vec<Core> = bindings
                .into_iter()
                .filter_map(|(name, expr)| match expr {
                    Lambda(code) => {
                        let code = Closure {
                            body: code.body.into_iter().flat_map(lift).collect(),
                            ..code
                        };
                        Some(Define { name, val: box Lambda(code) })
                    }
                    _ => None,
                })
                .collect();

            export.push(Let {
                bindings: rest,
                body: body.into_iter().map(|b| shrink(lift(b))).collect(),
            });

            export
        }

        List(list) => vec![List(list.into_iter().map(|l| shrink(lift(l))).collect())],

        Cond { pred, then, alt } => vec![Cond {
            pred: box shrink(lift(*pred)),
            then: box shrink(lift(*then)),
            alt: alt.map(|e| box shrink(lift(*e))),
        }],

        // Lift named code blocks to top level immediately, since names are manged by now.
        Define { name, val: box Lambda(code) } => {
            let body = (code).body.into_iter().flat_map(lift).collect();
            vec![Define { name, val: box Lambda(Closure { body, ..code }) }]
        }

        // Am unnamed literal lambda must be in an inline calling position
        // Lambda(Closure { .. }) => unimplemented!("inline λ"),
        e => vec![e],
    }
}
// Shrink a vector of expressions into a single expression
//
// TODO: Replace with `(begin ...)`, list really isn't the same thing
fn shrink<T: Clone>(es: Vec<Expr<T>>) -> Expr<T> {
    match es.len() {
        0 => Literal(Nil),
        1 => es[0].clone(),
        _ => List(es),
    }
}

/// Inline all references to strings and symbols
fn inline(s: &mut State, prog: Core) -> Core {
    match prog {
        Literal(l) => {
            match &l {
                Str(reference) => {
                    let index = s.strings.len();
                    s.strings.entry(reference.clone()).or_insert(index);
                }

                Symbol(reference) => {
                    let index = s.symbols.len();
                    s.symbols.entry(reference.clone()).or_insert(index);
                }

                _ => {}
            };

            Literal(l)
        }

        Let { bindings, body } => Let {
            bindings: bindings.into_iter().map(|(ident, expr)| (ident, inline(s, expr))).collect(),
            body: body.into_iter().map(|b| inline(s, b)).collect(),
        },

        List(list) => List(list.into_iter().map(|e| inline(s, e)).collect()),

        Vector(list) => Vector(list.into_iter().map(|e| inline(s, e)).collect()),

        Cond { pred, then, alt } => Cond {
            pred: box inline(s, *pred),
            then: box inline(s, *then),
            alt: alt.map(|e| box inline(s, *e)),
        },

        Define { name, val: box Lambda(code) } => Define {
            name,
            val: box Lambda(Closure {
                body: code.body.into_iter().map(|e| inline(s, e)).collect(),
                ..code
            }),
        },

        e => e,
    }
}

/// Convert an expression into [ANF](https://en.wikipedia.org/wiki/A-normal_form)
///
/// Break down complex expressions into a let binding with locals.
///
/// The generated names are NOT guaranteed to be unique and could be a problem
/// down the line.
fn anf(prog: Core) -> Core {
    match prog {
        List(list) => {
            let (car, cdr) = list.split_at(1);

            // IF all arguments are already in normal form, return as is it
            if cdr.iter().all(|e| e.anf()) {
                List(list)
            } else {
                // Collect variables that will be bound to a new let block
                let bindings = cdr
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (Ident::new(format!("_{}", i)), e.clone()))
                    .filter(|(_, e)| !e.anf());

                // Collect arguments for the function call where complex
                // expressions are replaced with a variable name
                let args: Vec<Core> = cdr
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        if e.anf() {
                            e.clone()
                        } else {
                            Identifier(Ident::new(format!("_{}", i)))
                        }
                    })
                    .collect();

                let body: Core = List(car.iter().chain(args.iter()).cloned().collect());

                Let { bindings: bindings.collect(), body: vec![body] }
            }
        }
        e => e,
    }
}

/// Annotate tail calls with a marker
fn tco(expr: Core) -> Core {
    fn is_tail(name: &Ident, code: &Closure<Ident>) -> bool {
        // Get the expression in tail call position
        let last = code.body.last().and_then(tail);

        // Check if the tail call is a list and the first elem is an identifier
        match last {
            Some(List(l)) => match l.first() {
                Some(Identifier(id)) => id == name,
                _ => false,
            },
            _ => false,
        }
    }

    match expr {
        Define { name, val: box Lambda(code) } => Define {
            name: name.clone(),
            val: box Lambda(Closure { tail: is_tail(&name, &code), ..code }),
        },
        Let { bindings, body } => {
            let bindings = bindings
                .into_iter()
                .map(|(name, value)| match value {
                    Lambda(code) => {
                        (name.clone(), Lambda(Closure { tail: is_tail(&name, &code), ..code }))
                    }

                    _ => (name, value),
                })
                .collect();

            Let { bindings, body }
        }

        e => e,
    }
}

/// Return the tail position of the expression
///
/// A tail position is defined recursively as follows:
///
/// 1. The body of a procedure is in tail position.
/// 2. If a let expression is in tail position, then the body of the let is in
///    tail position.
/// 3. If the conditional expression (if test conseq altern) is in tail
///    position, then the conseq and altern branches are also in tail position.
/// 4. All other expressions are not in tail position.
fn tail<T: std::clone::Clone>(e: &Expr<T>) -> Option<&Expr<T>> {
    match e {
        // Lambda(Closure { body, .. }) => body.last().map(tail).flatten(),
        Let { body, .. } => body.last().and_then(tail),
        Cond { alt, .. } => {
            // What do I do with 2?
            alt.as_deref().and_then(|e| tail(&e))
        }
        e => Some(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse, parse1};
    use pretty_assertions::assert_eq;

    fn rename(prog: Syntax) -> Core {
        super::rename(&HashMap::new(), &Ident::empty(), 0, prog)
    }

    fn analyze(prog: Vec<Syntax>) -> Vec<Core> {
        super::analyze(&mut State::new(), prog)
    }

    /// Mock rename, which blindly converts Strings to Identifiers
    fn mock(prog: Syntax) -> Core {
        match prog {
            Identifier(s) => Expr::Identifier(Ident::new(s)),

            Let { bindings, body } => Let {
                bindings: bindings
                    .iter()
                    .map(|(name, value)| (Ident::new(name), mock(value.clone())))
                    .collect(),

                body: body.into_iter().map(mock).collect(),
            },

            List(list) => List(list.into_iter().map(mock).collect()),

            Cond { pred, then, alt } => Cond {
                pred: box mock(*pred),
                then: box mock(*then),
                alt: alt.map(|u| box mock(*u)),
            },

            Lambda(Closure { formals, free, body, tail }) => Lambda(Closure {
                formals: formals.into_iter().map(Ident::new).collect(),
                free: free.into_iter().map(Ident::new).collect(),
                body: body.into_iter().map(mock).collect(),
                tail,
            }),

            Define { name, val } => Define { name: Ident::new(name), val: box mock(*val) },

            Vector(list) => Vector(list.into_iter().map(mock).collect()),

            // All literals and constants evaluate to itself
            Literal(v) => Literal(v),
        }
    }

    #[test]
    fn nest() {
        let x = rename(parse1(
            "(let ((x 1)
                   (y 2))
               (let ((z 3))
                 (+ x y z)))",
        ));

        let y = mock(parse1(
            "(let (({let 0}::x 1)
                  ({let 0}::y 2))
               (let (({let 0}::{let 1}::z 3))
                 (+ {let 0}::x {let 0}::y {let 0}::{let 1}::z))))",
        ));
        assert_eq!(x, y);
    }

    #[test]
    fn closure() {
        let x = rename(parse1(
            "(let ((add (lambda (x y) (+ x y))))
               (add 10 20))",
        ));

        let y = mock(parse1(
            "(let (({let 0}::add (lambda ({let 0}::add::x
                                          {let 0}::add::y)
                                              (+ {let 0}::add::x {let 0}::add::y))))
                                   ({let 0}::add 10 20))",
        ));

        assert_eq!(x, y);
    }

    #[test]
    fn function() {
        let x = rename(parse1("(define (add x y) (+ x y))"));
        let y = mock(parse1("(define (add add::x add::y) (+ add::x add::y))"));

        assert_eq!(x, y);
    }

    #[test]
    fn letrec() {
        let x = rename(parse1(
            "(let ((f (lambda (x) (g x x)))
                   (g (lambda (x y) (+ x y))))
               (f 12))",
        ));

        let y = mock(parse1(
            "(let (({let 0}::f (lambda ({let 0}::f::x) ({let 0}::g {let 0}::f::x {let 0}::f::x)))
                   ({let 0}::g (lambda ({let 0}::g::x {let 0}::g::y) (+ {let 0}::g::x {let 0}::g::y))))
               ({let 0}::f 12))",
        ));

        assert_eq!(x, y);
    }

    #[test]
    fn recursive() {
        let x = rename(parse1(
            "(let ((f (lambda (x)
               (if (zero? x)
                 1
                 (* x (f (dec x))))))) (f 5))",
        ));

        let y = mock(parse1(
            "(let (({let 0}::f (lambda ({let 0}::f::x)
               (if (zero? {let 0}::f::x)
                 1
                 (* {let 0}::f::x ({let 0}::f (dec {let 0}::f::x))))))) ({let 0}::f 5))",
        ));

        assert_eq!(x, y)
    }

    #[test]
    fn a_normal_form() {
        let x = parse1("(f (+ 1 2) 7)");
        let y = Let {
            bindings: vec![(
                Ident::new("_0"),
                List(vec![Ident::expr("+"), Literal(Number(1)), Literal(Number(2))]),
            )],
            body: vec![List(vec![Ident::expr("f"), Ident::expr("_0"), Literal(Number(7))])],
        };

        assert_eq!(y, anf(rename(x)));
    }

    /// OMG! I'm so happy to finally see these tests this way! Took me years! 😢
    #[test]
    fn lift_simple() {
        let prog = r"(let ((id (lambda (x) x))) (id 42))";
        let expr = analyze(parse(prog).unwrap());

        assert_eq!(expr[0], mock(parse1("(define ({let 0}::id {let 0}::id::x ) {let 0}::id::x)")));
        assert_eq!(expr[1], mock(parse1("(let () ({let 0}::id 42))")));
    }

    #[test]
    fn lift_recursive() {
        let prog = r"(let ((even (lambda (x) (if (zero? x) #t (odd (dec x)))))
                           (odd  (lambda (x) (if (zero? x) #f (even (dec x))))))
                       (even 25)))";

        let expr = lift(rename(parse1(prog)));

        assert_eq!(
            expr[0],
            mock(parse1(
                "(define ({let 0}::even {let 0}::even::x)
                   (if (zero? {let 0}::even::x) #t ({let 0}::odd (dec {let 0}::even::x))))"
            ))
        );

        assert_eq!(
            expr[1],
            mock(parse1(
                "(define ({let 0}::odd {let 0}::odd::x)
                   (if (zero? {let 0}::odd::x) #f ({let 0}::even (dec {let 0}::odd::x))))"
            ))
        );

        assert_eq!(expr[2], mock(parse1("(let () ({let 0}::even 25))")));
    }

    #[test]
    fn tails() {
        let prog = "(let ((factorial (lambda (x acc)
                                (if (zero? x)
                                  acc
                                  (factorial (dec x) (* x acc))))))
             (factorial 42 1))";

        let exprs = lift(rename(parse1(prog)));

        match &exprs[0] {
            Define { name: _, val: box Lambda(code) } => assert_eq!(code.tail, false),
            _ => panic!(),
        };

        match tco(exprs[0].clone()) {
            Define { name: _, val: box Lambda(code) } => assert_eq!(code.tail, true),
            _ => panic!(),
        }
    }
}
