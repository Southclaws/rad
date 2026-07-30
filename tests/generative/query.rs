use std::collections::HashMap;

use rad::engine::catalog::model::{ScalarType, Schema, TableDef};
use rad::engine::lir::{
    AggregateFunction, AggregateTerm, BinaryOp, BranchArm, Expr, GroupTerm, JoinKind, Kind,
    Literal, OrderTerm, ProjectField, Query, RawScalar, Relation, RootCardinality, RowsColumn,
    SetQuantifier, TextComparison, TextMatchPart, UnaryOp,
};

use super::Choices;

#[derive(Clone)]
struct Column {
    name: String,
    scalar_type: ScalarType,
    nullable: bool,
}

#[derive(Clone)]
struct Scope {
    name: String,
    columns: Vec<Column>,
    key: Vec<String>,
}

pub fn generate(schema: &Schema, choices: &mut Choices<'_>) -> (Query, bool) {
    Generator {
        schema,
        choices,
        next_scope: 0,
        next_field: 0,
        next_binding: 0,
    }
    .query()
}

struct Generator<'schema, 'choices, 'decisions> {
    schema: &'schema Schema,
    choices: &'choices mut Choices<'decisions>,
    next_scope: usize,
    next_field: usize,
    next_binding: usize,
}

impl Generator<'_, '_, '_> {
    fn query(&mut self) -> (Query, bool) {
        let (mut relation, mut scopes, bindings) = self.body();
        let cardinality = [
            RootCardinality::Many,
            RootCardinality::First,
            RootCardinality::ExactlyOne,
            RootCardinality::Scalar,
        ][self.choices.index(4)];
        let ordered = match cardinality {
            RootCardinality::First | RootCardinality::Scalar => true,
            RootCardinality::Many | RootCardinality::ExactlyOne => self.choices.coin(),
        };
        let projection_scope = self.scope();
        let mut fields = Vec::new();
        let mut output = Vec::new();
        for scope in &scopes {
            for column in &scope.columns {
                let name = self.field();
                fields.push(ProjectField {
                    name: name.clone(),
                    expression: column_expr(&scope.name, &column.name),
                });
                output.push(Column {
                    name,
                    scalar_type: column.scalar_type,
                    nullable: column.nullable,
                });
            }
        }
        if !ordered {
            for _ in 0..self.choices.index(3) {
                fields.push(self.crossing_field(&scopes));
            }
        }
        if cardinality == RootCardinality::Scalar {
            fields.truncate(1);
            output.truncate(1);
        }
        let has_nested_crossing = fields
            .iter()
            .any(|field| matches!(field.expression, Expr::First(_) | Expr::Array(_)));
        relation = Relation::Project {
            input: Box::new(relation),
            scope: Some(projection_scope.clone()),
            spread: Vec::new(),
            fields,
        };
        if has_nested_crossing {
            relation = Relation::Distinct(Box::new(relation));
        }
        let terms = if ordered {
            output
                .iter()
                .map(|column| OrderTerm {
                    expression: column_expr(&projection_scope, &column.name),
                    descending: self.choices.coin(),
                })
                .collect()
        } else {
            vec![OrderTerm {
                expression: literal_bool(true),
                descending: false,
            }]
        };
        relation = Relation::Order {
            input: Box::new(relation),
            terms,
        };
        if cardinality == RootCardinality::Scalar {
            relation = Relation::Slice {
                input: Box::new(relation),
                offset: 0,
                limit: Some(1),
            };
        }
        scopes.clear();
        (
            Query {
                root: relation,
                cardinality,
                bindings,
            },
            ordered && cardinality == RootCardinality::Many,
        )
    }

    fn body(&mut self) -> (Relation, Vec<Scope>, HashMap<String, Relation>) {
        let mut bindings = HashMap::new();
        let mut declared = Vec::new();
        for _ in 0..self.choices.index(3) {
            let (body, scopes) = self.relation(1);
            let (body, output) = self.flatten(body, &scopes);
            let name = self.binding();
            bindings.insert(name.clone(), body);
            declared.push((name, output.columns));
        }

        let (mut relation, mut scopes) = self.relation(2);
        for (binding, columns) in declared {
            let occurrences = if self.choices.coin() { 2 } else { 1 };
            for _ in 0..occurrences {
                let scope = Scope {
                    name: self.scope(),
                    columns: columns.clone(),
                    key: Vec::new(),
                };
                let right = Relation::Ref {
                    binding: binding.clone(),
                    scope: scope.name.clone(),
                };
                let on = self.join_predicate(&scopes, std::slice::from_ref(&scope));
                relation = Relation::Join {
                    left: Box::new(relation),
                    right: Box::new(right),
                    kind: if self.choices.coin() {
                        JoinKind::Left
                    } else {
                        JoinKind::Inner
                    },
                    on,
                };
                scopes.push(scope);
            }
        }
        (relation, scopes, bindings)
    }

    fn relation(&mut self, fuel: usize) -> (Relation, Vec<Scope>) {
        if fuel == 0 || self.choices.chance(3) {
            return if self.choices.chance(5) {
                self.rows()
            } else {
                self.scan()
            };
        }
        match self.choices.index(8) {
            0 => self.filter(fuel),
            1 => self.project(fuel),
            2 => self.order(fuel),
            3 => self.join(fuel),
            4 => self.aggregate(fuel),
            5 => self.set_operation(fuel),
            6 => self.slice(fuel),
            _ => {
                let (input, scopes) = self.relation(fuel - 1);
                (Relation::Distinct(Box::new(input)), scopes)
            }
        }
    }

    fn scan(&mut self) -> (Relation, Vec<Scope>) {
        let table = &self.schema.tables[self.choices.index(self.schema.tables.len())];
        let scope = Scope {
            name: self.scope(),
            columns: columns(table),
            key: table.primary_key.clone(),
        };
        (
            Relation::Scan {
                table: table.name.clone(),
                scope: scope.name.clone(),
            },
            vec![scope],
        )
    }

    fn rows(&mut self) -> (Relation, Vec<Scope>) {
        let scalar_type = [
            ScalarType::Text,
            ScalarType::Int64,
            ScalarType::Float64,
            ScalarType::Bool,
        ][self.choices.index(4)];
        let nullable = self.choices.coin();
        let name = self.field();
        let scope_name = self.scope();
        let values = (0..self.choices.range(0, 3))
            .map(|_| {
                vec![if nullable && self.choices.chance(4) {
                    RawScalar::Null
                } else {
                    self.raw_scalar(scalar_type)
                }]
            })
            .collect();
        (
            Relation::Rows {
                scope: scope_name.clone(),
                columns: vec![RowsColumn {
                    name: name.clone(),
                    kind: scalar_kind(scalar_type),
                    nullable,
                }],
                values,
            },
            vec![Scope {
                name: scope_name,
                columns: vec![Column {
                    name,
                    scalar_type,
                    nullable,
                }],
                key: Vec::new(),
            }],
        )
    }

    fn filter(&mut self, fuel: usize) -> (Relation, Vec<Scope>) {
        let (input, scopes) = self.relation(fuel - 1);
        let predicate = self.predicate(&scopes);
        (
            Relation::Filter {
                input: Box::new(input),
                predicate,
            },
            scopes,
        )
    }

    fn project(&mut self, fuel: usize) -> (Relation, Vec<Scope>) {
        let (input, scopes) = self.relation(fuel - 1);
        let scope_name = self.scope();
        let spread_scope = self
            .choices
            .coin()
            .then(|| &scopes[self.choices.index(scopes.len())]);
        let spread = spread_scope
            .map(|scope| vec![scope.name.clone()])
            .unwrap_or_default();
        let available = all_columns(&scopes);
        let mut selected = available
            .iter()
            .filter(|_| self.choices.coin())
            .cloned()
            .collect::<Vec<_>>();
        if selected.is_empty() {
            selected.push(available[self.choices.index(available.len())].clone());
        }
        let mut fields = Vec::new();
        let mut output = spread_scope
            .map(|scope| scope.columns.clone())
            .unwrap_or_default();
        for (source_scope, column) in selected {
            let name = self.field();
            fields.push(ProjectField {
                name: name.clone(),
                expression: column_expr(&source_scope, &column.name),
            });
            output.push(Column { name, ..column });
        }
        if let Some((source_scope, column)) = self.pick_column(&scopes, &[ScalarType::Int64])
            && self.choices.coin()
        {
            let name = self.field();
            let arithmetic = Expr::Binary {
                op: [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul, BinaryOp::Div]
                    [self.choices.index(4)],
                left: Box::new(column_expr(&source_scope, &column.name)),
                right: Box::new(literal_int([-1, 0, 1, 2][self.choices.index(4)])),
            };
            let expression = if self.choices.coin() {
                let arm_count = self.choices.range(1, 2);
                Expr::Branch {
                    arms: (0..arm_count)
                        .map(|index| BranchArm {
                            when: Expr::Binary {
                                op: BinaryOp::Gt,
                                left: Box::new(column_expr(&source_scope, &column.name)),
                                right: Box::new(literal_int(index as i64)),
                            },
                            then: arithmetic.clone(),
                        })
                        .collect(),
                    otherwise: Box::new(literal_int(-1)),
                }
            } else if self.choices.coin() {
                Expr::Unary {
                    op: UnaryOp::Negate,
                    expression: Box::new(arithmetic),
                }
            } else {
                arithmetic
            };
            fields.push(ProjectField {
                name: name.clone(),
                expression,
            });
            output.push(Column {
                name,
                scalar_type: ScalarType::Int64,
                nullable: column.nullable,
            });
        }
        if let Some((source_scope, column)) = self.pick_column(&scopes, &[ScalarType::Text])
            && self.choices.coin()
        {
            let name = self.field();
            fields.push(ProjectField {
                name: name.clone(),
                expression: Expr::TextMatch {
                    value: Box::new(column_expr(&source_scope, &column.name)),
                    parts: self.text_pattern(),
                    comparison: if self.choices.coin() {
                        TextComparison::Exact
                    } else {
                        TextComparison::UnicodeSimpleFold
                    },
                },
            });
            output.push(Column {
                name,
                scalar_type: ScalarType::Bool,
                nullable: column.nullable,
            });
        }
        if let Some((source_scope, column)) =
            self.pick_column(&scopes, &[ScalarType::Int64, ScalarType::Float64])
            && self.choices.coin()
        {
            let name = self.field();
            let to = if column.scalar_type == ScalarType::Int64 {
                Kind::Float64
            } else {
                Kind::Int64
            };
            fields.push(ProjectField {
                name: name.clone(),
                expression: Expr::Cast {
                    expression: Box::new(column_expr(&source_scope, &column.name)),
                    to,
                },
            });
            output.push(Column {
                name,
                scalar_type: if column.scalar_type == ScalarType::Int64 {
                    ScalarType::Float64
                } else {
                    ScalarType::Int64
                },
                nullable: column.nullable,
            });
        }
        (
            Relation::Project {
                input: Box::new(input),
                scope: Some(scope_name.clone()),
                spread,
                fields,
            },
            vec![Scope {
                name: scope_name,
                columns: output,
                key: Vec::new(),
            }],
        )
    }

    fn order(&mut self, fuel: usize) -> (Relation, Vec<Scope>) {
        let (input, scopes) = self.relation(fuel - 1);
        let expression = self
            .pick_column(
                &scopes,
                &[ScalarType::Text, ScalarType::Int64, ScalarType::Float64],
            )
            .map(|(scope, column)| column_expr(&scope, &column.name))
            .unwrap_or_else(|| literal_bool(true));
        (
            Relation::Order {
                input: Box::new(input),
                terms: vec![OrderTerm {
                    expression,
                    descending: self.choices.coin(),
                }],
            },
            scopes,
        )
    }

    fn slice(&mut self, fuel: usize) -> (Relation, Vec<Scope>) {
        let (input, scopes) = self.order(fuel);
        (
            Relation::Slice {
                input: Box::new(input),
                offset: self.choices.range(0, 2),
                limit: self.choices.coin().then(|| self.choices.range(0, 3)),
            },
            scopes,
        )
    }

    fn join(&mut self, fuel: usize) -> (Relation, Vec<Scope>) {
        let (left, mut left_scopes) = self.relation(fuel - 1);
        let (right, right_scopes) = self.relation(fuel - 1);
        let on = self.join_predicate(&left_scopes, &right_scopes);
        left_scopes.extend(right_scopes);
        (
            Relation::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind: if self.choices.coin() {
                    JoinKind::Left
                } else {
                    JoinKind::Inner
                },
                on,
            },
            left_scopes,
        )
    }

    fn aggregate(&mut self, fuel: usize) -> (Relation, Vec<Scope>) {
        let (input, scopes) = self.relation(fuel - 1);
        let scope_name = self.scope();
        let mut groups = Vec::new();
        let mut output = Vec::new();
        for _ in 0..self.choices.index(3) {
            if let Some((source_scope, column)) = self.pick_column(
                &scopes,
                &[ScalarType::Text, ScalarType::Int64, ScalarType::Float64],
            ) {
                let name = self.field();
                groups.push(GroupTerm {
                    name: name.clone(),
                    expression: column_expr(&source_scope, &column.name),
                });
                output.push(Column { name, ..column });
            }
        }
        let count_name = self.field();
        let count_argument = self
            .choices
            .coin()
            .then(|| {
                self.pick_column(
                    &scopes,
                    &[
                        ScalarType::Text,
                        ScalarType::Int64,
                        ScalarType::Float64,
                        ScalarType::Bool,
                    ],
                )
            })
            .flatten()
            .map(|(scope, column)| column_expr(&scope, &column.name));
        let mut terms = vec![AggregateTerm {
            function: AggregateFunction::Count,
            argument: count_argument,
            name: count_name.clone(),
        }];
        output.push(Column {
            name: count_name,
            scalar_type: ScalarType::Int64,
            nullable: false,
        });
        if let Some((source_scope, column)) =
            self.pick_column(&scopes, &[ScalarType::Int64, ScalarType::Float64])
            && self.choices.coin()
        {
            let function = [
                AggregateFunction::Sum,
                AggregateFunction::Average,
                AggregateFunction::Min,
                AggregateFunction::Max,
            ][self.choices.index(4)];
            let name = self.field();
            terms.push(AggregateTerm {
                function,
                argument: Some(column_expr(&source_scope, &column.name)),
                name: name.clone(),
            });
            output.push(Column {
                name,
                scalar_type: if function == AggregateFunction::Average {
                    ScalarType::Float64
                } else {
                    column.scalar_type
                },
                nullable: true,
            });
        }
        (
            Relation::Aggregate {
                input: Box::new(input),
                scope: Some(scope_name.clone()),
                groups,
                terms,
            },
            vec![Scope {
                name: scope_name,
                columns: output,
                key: Vec::new(),
            }],
        )
    }

    fn set_operation(&mut self, fuel: usize) -> (Relation, Vec<Scope>) {
        let (left, scopes) = self.relation(fuel - 1);
        let (left, target) = self.flatten(left, &scopes);
        let (right, right_columns) = self.aligned(&target, fuel - 1);
        let scope_name = self.scope();
        let mut output = target.columns.clone();
        for (column, right) in output.iter_mut().zip(right_columns) {
            column.nullable |= right.nullable;
        }
        let relation = match self.choices.index(3) {
            0 => Relation::Concatenate {
                scope: scope_name.clone(),
                inputs: vec![left, right],
            },
            1 => Relation::Intersect {
                scope: scope_name.clone(),
                left: Box::new(left),
                right: Box::new(right),
                quantifier: if self.choices.coin() {
                    SetQuantifier::All
                } else {
                    SetQuantifier::Distinct
                },
            },
            _ => Relation::Except {
                scope: scope_name.clone(),
                left: Box::new(left),
                right: Box::new(right),
                quantifier: if self.choices.coin() {
                    SetQuantifier::All
                } else {
                    SetQuantifier::Distinct
                },
            },
        };
        (
            relation,
            vec![Scope {
                name: scope_name,
                columns: output,
                key: Vec::new(),
            }],
        )
    }

    fn aligned(&mut self, target: &Scope, fuel: usize) -> (Relation, Vec<Column>) {
        let (input, scopes) = self.relation(fuel);
        let scope = self.scope();
        let mut fields = Vec::new();
        let mut output = Vec::new();
        for target_column in &target.columns {
            let (expression, nullable) = if let Some((source_scope, column)) =
                self.pick_column(&scopes, &[target_column.scalar_type])
            {
                (column_expr(&source_scope, &column.name), column.nullable)
            } else {
                (self.literal(target_column.scalar_type), false)
            };
            fields.push(ProjectField {
                name: target_column.name.clone(),
                expression,
            });
            output.push(Column {
                name: target_column.name.clone(),
                scalar_type: target_column.scalar_type,
                nullable,
            });
        }
        (
            Relation::Project {
                input: Box::new(input),
                scope: Some(scope),
                spread: Vec::new(),
                fields,
            },
            output,
        )
    }

    fn flatten(&mut self, input: Relation, scopes: &[Scope]) -> (Relation, Scope) {
        let scope_name = self.scope();
        let mut fields = Vec::new();
        let mut columns = Vec::new();
        for scope in scopes {
            for column in &scope.columns {
                let name = self.field();
                fields.push(ProjectField {
                    name: name.clone(),
                    expression: column_expr(&scope.name, &column.name),
                });
                columns.push(Column {
                    name,
                    scalar_type: column.scalar_type,
                    nullable: column.nullable,
                });
            }
        }
        (
            Relation::Project {
                input: Box::new(input),
                scope: Some(scope_name.clone()),
                spread: Vec::new(),
                fields,
            },
            Scope {
                name: scope_name,
                columns,
                key: Vec::new(),
            },
        )
    }

    fn crossing_field(&mut self, outer: &[Scope]) -> ProjectField {
        let (subquery, scopes) = self.correlated_subquery(outer);
        let expression = match self.choices.index(4) {
            0 => Expr::Exists(Box::new(subquery)),
            1 => Expr::Scalar(Box::new(Relation::Aggregate {
                input: Box::new(subquery),
                scope: None,
                groups: Vec::new(),
                terms: vec![AggregateTerm {
                    function: AggregateFunction::Count,
                    argument: None,
                    name: "n".into(),
                }],
            })),
            2 => Expr::First(Box::new(self.ordered_subquery(subquery, &scopes))),
            _ => Expr::Array(Box::new(self.ordered_subquery(subquery, &scopes))),
        };
        ProjectField {
            name: self.field(),
            expression,
        }
    }

    fn correlated_subquery(&mut self, outer: &[Scope]) -> (Relation, Vec<Scope>) {
        let (scan, scopes) = self.scan();
        let predicate = self.correlation(&scopes, outer);
        (
            Relation::Filter {
                input: Box::new(scan),
                predicate,
            },
            scopes,
        )
    }

    fn ordered_subquery(&mut self, input: Relation, scopes: &[Scope]) -> Relation {
        let keys = scopes
            .iter()
            .flat_map(|scope| {
                scope.key.iter().map(|key| {
                    let column = scope
                        .columns
                        .iter()
                        .find(|column| column.name == *key)
                        .expect("generated key column exists");
                    (scope.name.clone(), column.clone())
                })
            })
            .collect::<Vec<_>>();
        let (projection, output) = self.flatten(input, scopes);
        let terms = keys
            .iter()
            .enumerate()
            .map(|(index, _)| OrderTerm {
                expression: column_expr(&output.name, &output.columns[index].name),
                descending: false,
            })
            .collect();
        Relation::Order {
            input: Box::new(projection),
            terms,
        }
    }

    fn correlation(&mut self, inner: &[Scope], outer: &[Scope]) -> Expr {
        for scalar_type in [
            ScalarType::Text,
            ScalarType::Int64,
            ScalarType::Float64,
            ScalarType::Bool,
        ] {
            if let (Some((inner_scope, inner_column)), Some((outer_scope, outer_column))) = (
                self.pick_column(inner, &[scalar_type]),
                self.pick_column(outer, &[scalar_type]),
            ) {
                let op = if scalar_type != ScalarType::Bool && self.choices.chance(3) {
                    if self.choices.coin() {
                        BinaryOp::Lt
                    } else {
                        BinaryOp::Gt
                    }
                } else {
                    BinaryOp::Eq
                };
                return Expr::Binary {
                    op,
                    left: Box::new(column_expr(&inner_scope, &inner_column.name)),
                    right: Box::new(column_expr(&outer_scope, &outer_column.name)),
                };
            }
        }
        literal_bool(true)
    }

    fn predicate(&mut self, scopes: &[Scope]) -> Expr {
        let first = self.atom(scopes);
        if self.choices.chance(3) {
            Expr::Binary {
                op: if self.choices.coin() {
                    BinaryOp::And
                } else {
                    BinaryOp::Or
                },
                left: Box::new(first),
                right: Box::new(self.atom(scopes)),
            }
        } else {
            first
        }
    }

    fn atom(&mut self, scopes: &[Scope]) -> Expr {
        if self.choices.chance(5) {
            let (subquery, _) = self.correlated_subquery(scopes);
            let exists = Expr::Exists(Box::new(subquery));
            return if self.choices.coin() {
                Expr::Unary {
                    op: UnaryOp::Not,
                    expression: Box::new(exists),
                }
            } else {
                exists
            };
        }
        let Some((scope, column)) = self.pick_column(
            scopes,
            &[
                ScalarType::Text,
                ScalarType::Int64,
                ScalarType::Float64,
                ScalarType::Bool,
            ],
        ) else {
            return literal_bool(true);
        };
        let expression = column_expr(&scope, &column.name);
        if column.scalar_type == ScalarType::Bool {
            return match self.choices.index(4) {
                0 => expression,
                1 => Expr::Unary {
                    op: UnaryOp::Not,
                    expression: Box::new(expression),
                },
                2 => Expr::Unary {
                    op: UnaryOp::IsNull,
                    expression: Box::new(expression),
                },
                _ => Expr::Unary {
                    op: UnaryOp::IsNotNull,
                    expression: Box::new(expression),
                },
            };
        }
        if self.choices.chance(4) {
            return Expr::Unary {
                op: if self.choices.coin() {
                    UnaryOp::IsNull
                } else {
                    UnaryOp::IsNotNull
                },
                expression: Box::new(expression),
            };
        }
        Expr::Binary {
            op: [
                BinaryOp::Eq,
                BinaryOp::Ne,
                BinaryOp::Lt,
                BinaryOp::Lte,
                BinaryOp::Gt,
                BinaryOp::Gte,
            ][self.choices.index(6)],
            left: Box::new(expression),
            right: Box::new(self.literal(column.scalar_type)),
        }
    }

    fn join_predicate(&mut self, left: &[Scope], right: &[Scope]) -> Expr {
        for scalar_type in [ScalarType::Text, ScalarType::Int64, ScalarType::Float64] {
            if let (Some((left_scope, left_column)), Some((right_scope, right_column))) = (
                self.pick_column(left, &[scalar_type]),
                self.pick_column(right, &[scalar_type]),
            ) {
                return Expr::Binary {
                    op: BinaryOp::Eq,
                    left: Box::new(column_expr(&left_scope, &left_column.name)),
                    right: Box::new(column_expr(&right_scope, &right_column.name)),
                };
            }
        }
        literal_bool(true)
    }

    fn pick_column(&mut self, scopes: &[Scope], types: &[ScalarType]) -> Option<(String, Column)> {
        let candidates = all_columns(scopes)
            .into_iter()
            .filter(|(_, column)| types.contains(&column.scalar_type))
            .collect::<Vec<_>>();
        (!candidates.is_empty()).then(|| candidates[self.choices.index(candidates.len())].clone())
    }

    fn literal(&mut self, scalar_type: ScalarType) -> Expr {
        Expr::Literal(Literal {
            raw: self.raw_scalar(scalar_type),
            kind: Some(scalar_kind(scalar_type)),
        })
    }

    fn raw_scalar(&mut self, scalar_type: ScalarType) -> RawScalar {
        match scalar_type {
            ScalarType::Text => RawScalar::Text(["", "a", "b", "c"][self.choices.index(4)].into()),
            ScalarType::Int64 => RawScalar::Number(
                [i64::MIN, -1, 0, 1, 2, 100, i64::MAX][self.choices.index(7)].to_string(),
            ),
            ScalarType::Float64 => {
                RawScalar::Number([-1.5, -0.0, 0.0, 1.5, 2.5][self.choices.index(5)].to_string())
            }
            ScalarType::Bool => RawScalar::Bool(self.choices.coin()),
        }
    }

    fn text_pattern(&mut self) -> Vec<TextMatchPart> {
        let literal = TextMatchPart::Literal(["a", "b", "c"][self.choices.index(3)].into());
        match self.choices.index(5) {
            0 => vec![TextMatchPart::AnyMany],
            1 => vec![literal, TextMatchPart::AnyMany],
            2 => vec![TextMatchPart::AnyMany, literal],
            3 => vec![TextMatchPart::AnyMany, literal, TextMatchPart::AnyMany],
            _ => vec![literal],
        }
    }

    fn scope(&mut self) -> String {
        self.next_scope += 1;
        format!("s{}", self.next_scope)
    }

    fn field(&mut self) -> String {
        self.next_field += 1;
        format!("f{}", self.next_field)
    }

    fn binding(&mut self) -> String {
        self.next_binding += 1;
        format!("b{}", self.next_binding)
    }
}

fn columns(table: &TableDef) -> Vec<Column> {
    table
        .columns
        .iter()
        .map(|column| Column {
            name: column.name.clone(),
            scalar_type: column.scalar_type,
            nullable: column.nullable,
        })
        .collect()
}

fn all_columns(scopes: &[Scope]) -> Vec<(String, Column)> {
    scopes
        .iter()
        .flat_map(|scope| {
            scope
                .columns
                .iter()
                .cloned()
                .map(|column| (scope.name.clone(), column))
        })
        .collect()
}

fn column_expr(scope: &str, name: &str) -> Expr {
    Expr::Column {
        scope: scope.into(),
        name: name.into(),
    }
}

fn literal_int(value: i64) -> Expr {
    literal(RawScalar::Number(value.to_string()), Kind::Int64)
}

fn literal_bool(value: bool) -> Expr {
    literal(RawScalar::Bool(value), Kind::Bool)
}

fn scalar_kind(value: ScalarType) -> Kind {
    match value {
        ScalarType::Text => Kind::Text,
        ScalarType::Int64 => Kind::Int64,
        ScalarType::Float64 => Kind::Float64,
        ScalarType::Bool => Kind::Bool,
    }
}

fn literal(raw: RawScalar, kind: Kind) -> Expr {
    Expr::Literal(Literal {
        raw,
        kind: Some(kind),
    })
}
