use rad::engine::exec::{ProgramResult, Result as ExecResult};
use rad::engine::lir::{Datum, Value};

pub trait ExactResult {
    fn exact_eq(&self, other: &Self) -> bool;
}

pub fn outcomes_eq<T: ExactResult>(left: &ExecResult<T>, right: &ExecResult<T>) -> bool {
    match (left, right) {
        (Ok(left), Ok(right)) => left.exact_eq(right),
        (Err(left), Err(right)) => error_eq(left, right),
        _ => false,
    }
}

impl ExactResult for ProgramResult {
    fn exact_eq(&self, other: &Self) -> bool {
        datum_eq(&self.result, &other.result)
            && self.statements == other.statements
            && self.plans == other.plans
    }
}

impl ExactResult for Datum {
    fn exact_eq(&self, other: &Self) -> bool {
        datum_eq(self, other)
    }
}

fn error_eq(left: &rad::engine::exec::Error, right: &rad::engine::exec::Error) -> bool {
    left.kind() == right.kind()
        && left.reason() == right.reason()
        && left.to_string() == right.to_string()
}

fn datum_eq(left: &Datum, right: &Datum) -> bool {
    match (left, right) {
        (Datum::Null, Datum::Null) => true,
        (Datum::Scalar(left), Datum::Scalar(right)) => value_eq(left, right),
        (Datum::Object(left), Datum::Object(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|(left, right)| {
                    left.name == right.name && datum_eq(&left.datum, &right.datum)
                })
        }
        (Datum::Array(left), Datum::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| datum_eq(left, right))
        }
        _ => false,
    }
}

fn value_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float64(left), Value::Float64(right)) => left.to_bits() == right.to_bits(),
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_results_distinguish_float_bit_patterns_and_row_order() {
        let negative_zero = Ok(Datum::Scalar(Value::Float64(-0.0)));
        let positive_zero = Ok(Datum::Scalar(Value::Float64(0.0)));
        assert!(!outcomes_eq(&negative_zero, &positive_zero));

        let first = Ok(Datum::Array(vec![
            Datum::Scalar(Value::Int64(1)),
            Datum::Scalar(Value::Int64(2)),
        ]));
        let reversed = Ok(Datum::Array(vec![
            Datum::Scalar(Value::Int64(2)),
            Datum::Scalar(Value::Int64(1)),
        ]));
        assert!(!outcomes_eq(&first, &reversed));
        assert!(outcomes_eq(&first, &first));
    }
}
