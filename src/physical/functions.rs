// Copyright 2020 The OctoSQL Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;
use std::collections::HashMap;

use arrow::array::{Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, Int8Array, StringArray, TimestampNanosecondArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array, StringBuilder, TimestampNanosecondBuilder, DurationNanosecondArray};
use arrow::array::ArrayRef;
use arrow::compute::kernels::comparison::{lt, lt_eq, eq, gt_eq, gt, lt_utf8, lt_eq_utf8, eq_utf8, gt_eq_utf8, gt_utf8};
use arrow::compute::kernels::arithmetic::{add, subtract, multiply, divide};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use anyhow::Result;

use crate::physical::expression::Expression;
use crate::physical::physical::{ExecutionContext, SchemaContext};

use chrono::{DateTime};
use arrow::datatypes::TimeUnit::Nanosecond;

// TODO: Add "custom function" expression.

type EvaluateFunction = Arc<dyn Fn(Vec<ArrayRef>) -> Result<ArrayRef> + Send + Sync>;
type MetaFunction = Arc<dyn Fn(Vec<Field>) -> Result<Field> + Send + Sync>;

pub struct FunctionExpression {
    meta_function: MetaFunction,
    evaluate_function: EvaluateFunction,
    args: Vec<Arc<dyn Expression>>,
}

impl FunctionExpression
{
    pub fn new(
        meta_function: MetaFunction,
        evaluate_function: EvaluateFunction,
        args: Vec<Arc<dyn Expression>>,
    ) -> FunctionExpression {
        FunctionExpression {
            meta_function,
            evaluate_function,
            args
        }
    }
}

impl Expression for FunctionExpression
{
    fn evaluate(&self, ctx: &ExecutionContext, record: &RecordBatch) -> Result<ArrayRef> {
        let args = self.args.iter()
            .map(|expr| expr.evaluate(ctx, record))
            .collect::<Result<Vec<ArrayRef>>>()?;

        (self.evaluate_function)(args)
    }
}

macro_rules! make_const_meta_body {
    ($data_type: expr) => {
        Arc::new(|args| {
            Ok(Field::new("", $data_type, false))
        })
    }
}

// TODO: Fixme check if args[0] is actually a numeric type.
macro_rules! make_numeric_meta_body {
    () => {
        Arc::new(|args| {
            if args[0].data_type() == args[1].data_type() {
                Ok(Field::new("", args[0].data_type().clone(), true))
            } else {
                Err(anyhow!("Both arguments of a numeric operator must be of the same type."))
            }
        })
    }
}

// macro_rules! make_binary_primitive_array_evaluate_function {
//     ($function: ident) => {
//         Arc::new(|args: Vec<ArrayRef>| {
//             let output: Result<_, ArrowError> = binary_primitive_array_op!(args[0], args[1], $function);
//             Ok(output? as ArrayRef)
//         })
//     }
// }

macro_rules! make_binary_array_evaluate_function {
    ($function: ident) => {
        Arc::new(|args: Vec<ArrayRef>| {
            let output: Result<_, ArrowError> = binary_array_op!(args[0], args[1], $function);
            Ok(output? as ArrayRef)
        })
    }
}

macro_rules! make_binary_numeric_array_evaluate_function {
    ($function: ident) => {
        Arc::new(|args: Vec<ArrayRef>| {
            let output: Result<_, ArrowError> = binary_numeric_array_op!(args[0], args[1], $function);
            Ok(output? as ArrayRef)
        })
    }
}

// macro_rules! make_single_arg_evaluate_function {
//     ($function: ident) => {
//         Arc::new(|args: Vec<ArrayRef>| {
//             let output: Result<_, ArrowError> = binary_primitive_array_op!(args[0], args[1], $function);
//             Ok(output? as ArrayRef)
//         })
//     }
// }

macro_rules! register_function {
    ($map: expr, $name: expr, $meta_fn: expr, $eval_fn: expr) => {
        $map.insert($name, ($meta_fn, Arc::new(|args: Vec<Arc<dyn Expression>>| Arc::new(FunctionExpression::new($meta_fn, $eval_fn, args)))));
    }
}

lazy_static! {
    pub static ref BUILTIN_FUNCTIONS: HashMap<&'static str, (MetaFunction, Arc<dyn Fn(Vec<Arc<dyn Expression>>)-> Arc<FunctionExpression> + Send + Sync>)> = {
        let mut m: HashMap<&'static str, (MetaFunction, Arc<dyn Fn(Vec<Arc<dyn Expression>>)-> Arc<FunctionExpression> + Send + Sync>)> = HashMap::new();
        register_function!(m, "<", make_const_meta_body!(DataType::Boolean), make_binary_array_evaluate_function!(lt));
        register_function!(m, "<=", make_const_meta_body!(DataType::Boolean), make_binary_array_evaluate_function!(lt_eq));
        register_function!(m, "=", make_const_meta_body!(DataType::Boolean), make_binary_array_evaluate_function!(eq));
        register_function!(m, ">=", make_const_meta_body!(DataType::Boolean), make_binary_array_evaluate_function!(gt_eq));
        register_function!(m, ">", make_const_meta_body!(DataType::Boolean), make_binary_array_evaluate_function!(gt));
        register_function!(m, "+", make_numeric_meta_body!(), make_binary_numeric_array_evaluate_function!(add));
        register_function!(m, "-", make_numeric_meta_body!(), make_binary_numeric_array_evaluate_function!(subtract));
        register_function!(m, "*", make_numeric_meta_body!(), make_binary_numeric_array_evaluate_function!(multiply));
        register_function!(m, "/", make_numeric_meta_body!(), make_binary_numeric_array_evaluate_function!(divide));
        register_function!(m, "upper", make_const_meta_body!(DataType::Utf8), Arc::new(|args: Vec<ArrayRef>| {
            let output: Result<_, ArrowError> = compute_single_arg_str!(args[0], StringArray, StringBuilder, |text: &str| {
                text.to_uppercase()
            });
            Ok(output? as ArrayRef)
        }));
        register_function!(m, "parse_datetime_rfc3339", make_const_meta_body!(DataType::Timestamp(Nanosecond, None)), Arc::new(|args: Vec<ArrayRef>| {
            let output: Result<_> = compute_single_arg!(args[0], StringArray, TimestampNanosecondBuilder, |text: &str| -> Result<i64> {
                match DateTime::parse_from_rfc3339(text) {
                    Ok(dt) => Ok(dt.timestamp_nanos()),
                    Err(err) => Err(err)?,
                }
            });
            Ok(output? as ArrayRef)
        }));
        register_function!(m, "parse_datetime_tz", make_const_meta_body!(DataType::Timestamp(Nanosecond, None)), Arc::new(|args: Vec<ArrayRef>| {
            let output: Result<_> = compute_two_arg!(args[0], args[1], StringArray, StringArray, TimestampNanosecondBuilder, |fmt: &str, text: &str| -> Result<i64> {
                match DateTime::parse_from_str(text, fmt) {
                    Ok(dt) => Ok(dt.timestamp_nanos()),
                    Err(err) => Err(err)?
                }
            });
            Ok(output? as ArrayRef)
        }));
        m
    };
}
