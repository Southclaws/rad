use rad::engine::lir::{BinaryOp, Expr, Kind, Literal, Query, RawScalar, Relation, UnaryOp};

#[derive(Clone, Debug)]
pub struct Variant {
    pub name: &'static str,
    pub query: Query,
}

pub fn variants(query: &Query) -> Vec<Variant> {
    let mut binding_identity = query.clone();
    for relation in binding_identity.bindings.values_mut() {
        let input = relation.clone();
        *relation = identity_filter(input);
    }

    let mut double_negation = query.clone();
    rewrite_predicates(&mut double_negation.root, double_not);
    for relation in double_negation.bindings.values_mut() {
        rewrite_predicates(relation, double_not);
    }

    let mut conjunction_identity = query.clone();
    rewrite_predicates(&mut conjunction_identity.root, conjoin_true);
    for relation in conjunction_identity.bindings.values_mut() {
        rewrite_predicates(relation, conjoin_true);
    }

    let mut disjunction_identity = query.clone();
    rewrite_predicates(&mut disjunction_identity.root, disjoin_false);
    for relation in disjunction_identity.bindings.values_mut() {
        rewrite_predicates(relation, disjoin_false);
    }

    vec![
        Variant {
            name: "root identity filter",
            query: Query {
                root: identity_filter(query.root.clone()),
                ..query.clone()
            },
        },
        Variant {
            name: "binding identity filters",
            query: binding_identity,
        },
        Variant {
            name: "predicate double negation",
            query: double_negation,
        },
        Variant {
            name: "predicate conjunction identity",
            query: conjunction_identity,
        },
        Variant {
            name: "predicate disjunction identity",
            query: disjunction_identity,
        },
        Variant {
            name: "root identity slice",
            query: Query {
                root: Relation::Slice {
                    input: Box::new(query.root.clone()),
                    offset: 0,
                    limit: None,
                },
                ..query.clone()
            },
        },
    ]
}

fn identity_filter(input: Relation) -> Relation {
    Relation::Filter {
        input: Box::new(input),
        predicate: literal_bool(true),
    }
}

fn double_not(expression: &mut Expr) {
    let inner = expression.clone();
    *expression = Expr::Unary {
        op: UnaryOp::Not,
        expression: Box::new(Expr::Unary {
            op: UnaryOp::Not,
            expression: Box::new(inner),
        }),
    };
}

fn conjoin_true(expression: &mut Expr) {
    let left = expression.clone();
    *expression = Expr::Binary {
        op: BinaryOp::And,
        left: Box::new(left),
        right: Box::new(literal_bool(true)),
    };
}

fn disjoin_false(expression: &mut Expr) {
    let left = expression.clone();
    *expression = Expr::Binary {
        op: BinaryOp::Or,
        left: Box::new(left),
        right: Box::new(literal_bool(false)),
    };
}

fn literal_bool(value: bool) -> Expr {
    Expr::Literal(Literal {
        raw: RawScalar::Bool(value),
        kind: Some(Kind::Bool),
    })
}

fn rewrite_predicates(relation: &mut Relation, rewrite: fn(&mut Expr)) {
    match relation {
        Relation::Scan { .. }
        | Relation::Rows { .. }
        | Relation::Ref { .. }
        | Relation::RecursiveRef { .. } => {}
        Relation::Filter { input, predicate } => {
            rewrite_predicates(input, rewrite);
            rewrite(predicate);
        }
        Relation::Project { input, .. }
        | Relation::Order { input, .. }
        | Relation::Slice { input, .. }
        | Relation::Aggregate { input, .. }
        | Relation::Distinct(input) => rewrite_predicates(input, rewrite),
        Relation::Join {
            left, right, on, ..
        } => {
            rewrite_predicates(left, rewrite);
            rewrite_predicates(right, rewrite);
            rewrite(on);
        }
        Relation::Concatenate { inputs, .. } => {
            for input in inputs {
                rewrite_predicates(input, rewrite);
            }
        }
        Relation::Intersect { left, right, .. } | Relation::Except { left, right, .. } => {
            rewrite_predicates(left, rewrite);
            rewrite_predicates(right, rewrite);
        }
        Relation::Recursive { anchor, step, .. } => {
            rewrite_predicates(anchor, rewrite);
            rewrite_predicates(step, rewrite);
        }
    }
}
