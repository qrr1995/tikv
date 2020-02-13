// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::BTreeMap;

use tidb_query_codegen::rpn_fn;
use tidb_query_datatype::EvalType;

use crate::codec::data_type::*;
use crate::codec::mysql::json::*;
use crate::Result;

#[rpn_fn]
#[inline]
fn json_depth(arg: &Option<Json>) -> Result<Option<i64>> {
    Ok(arg.as_ref().map(|json_arg| json_arg.depth()))
}

#[rpn_fn]
#[inline]
fn json_type(arg: &Option<Json>) -> Result<Option<Bytes>> {
    Ok(arg
        .as_ref()
        .map(|json_arg| Bytes::from(json_arg.json_type())))
}

#[rpn_fn(raw_varg, min_args = 2, extra_validator = json_modify_validator)]
#[inline]
fn json_set(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    json_modify(args, ModifyType::Set)
}

#[rpn_fn(raw_varg, min_args = 2, extra_validator = json_modify_validator)]
#[inline]
fn json_insert(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    json_modify(args, ModifyType::Insert)
}

#[rpn_fn(raw_varg, min_args = 2, extra_validator = json_modify_validator)]
#[inline]
fn json_replace(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    json_modify(args, ModifyType::Replace)
}

#[inline]
fn json_modify(args: &[ScalarValueRef], mt: ModifyType) -> Result<Option<Json>> {
    assert!(args.len() >= 2);
    // base Json argument
    let base: &Option<Json> = args[0].as_ref();
    let mut base = base.as_ref().map_or(Json::None, |json| json.to_owned());

    let buf_size = args.len() / 2;

    let mut path_expr_list = Vec::with_capacity(buf_size);
    let mut values = Vec::with_capacity(buf_size);

    for chunk in args[1..].chunks(2) {
        let path: &Option<Bytes> = chunk[0].as_ref();
        let value: &Option<Json> = chunk[1].as_ref();

        path_expr_list.push(try_opt!(parse_json_path(path)));

        let value = value.as_ref().map_or(Json::None, |json| json.to_owned());
        values.push(value);
    }
    base.modify(&path_expr_list, values, mt)?;

    Ok(Some(base))
}

/// validate the arguments are `(&Option<Json>, &[(Option<Bytes>, Option<Json>)])`
fn json_modify_validator(expr: &tipb::Expr) -> Result<()> {
    let children = expr.get_children();
    assert!(children.len() >= 2);
    if children.len() % 2 != 1 {
        return Err(other_err!(
            "Incorrect parameter count in the call to native function 'JSON_OBJECT'"
        ));
    }
    super::function::validate_expr_return_type(&children[0], EvalType::Json)?;
    for chunk in children[1..].chunks(2) {
        super::function::validate_expr_return_type(&chunk[0], EvalType::Bytes)?;
        super::function::validate_expr_return_type(&chunk[1], EvalType::Json)?;
    }
    Ok(())
}

#[rpn_fn(varg)]
#[inline]
fn json_array(args: &[&Option<Json>]) -> Result<Option<Json>> {
    Ok(Some(Json::Array(
        args.iter()
            .map(|json| match json {
                None => Json::None,
                Some(json) => json.to_owned(),
            })
            .collect(),
    )))
}

fn json_object_validator(expr: &tipb::Expr) -> Result<()> {
    let chunks = expr.get_children();
    if chunks.len() % 2 == 1 {
        return Err(other_err!(
            "Incorrect parameter count in the call to native function 'JSON_OBJECT'"
        ));
    }
    for chunk in chunks.chunks(2) {
        super::function::validate_expr_return_type(&chunk[0], EvalType::Bytes)?;
        super::function::validate_expr_return_type(&chunk[1], EvalType::Json)?;
    }
    Ok(())
}

/// Required args like `&[(&Option<Byte>, &Option<Json>)]`.
#[rpn_fn(raw_varg, extra_validator = json_object_validator)]
#[inline]
fn json_object(raw_args: &[ScalarValueRef]) -> Result<Option<Json>> {
    let mut pairs = BTreeMap::new();
    for chunk in raw_args.chunks(2) {
        assert_eq!(chunk.len(), 2);
        let key: &Option<Bytes> = chunk[0].as_ref();
        if key.is_none() {
            return Err(other_err!(
                "Data truncation: JSON documents may not contain NULL member names."
            ));
        }
        let key = String::from_utf8(key.as_ref().unwrap().to_owned())
            .map_err(|e| crate::codec::Error::from(e))?;

        let value: &Option<Json> = chunk[1].as_ref();
        let value = match value {
            None => Json::None,
            Some(v) => v.to_owned(),
        };

        pairs.insert(key, value);
    }
    Ok(Some(Json::Object(pairs)))
}

// According to mysql 5.7,
// arguments of json_merge should not be less than 2.
#[rpn_fn(varg, min_args = 2)]
#[inline]
pub fn json_merge(args: &[&Option<Json>]) -> Result<Option<Json>> {
    // min_args = 2, so it's ok to call args[0]
    let base_json = match args[0] {
        None => return Ok(None),
        Some(json) => json.to_owned(),
    };

    Ok(args[1..]
        .iter()
        .try_fold(base_json, move |base, json_to_merge| {
            json_to_merge
                .as_ref()
                .map(|json| base.merge(json.to_owned()))
        }))
}

#[rpn_fn]
#[inline]
fn json_unquote(arg: &Option<Json>) -> Result<Option<Bytes>> {
    arg.as_ref().map_or(Ok(None), |json_arg| {
        Ok(Some(Bytes::from(json_arg.unquote()?)))
    })
}

// Args should be like `(&Option<Json> , &[&Option<Bytes>])`.
fn json_with_paths_validator(expr: &tipb::Expr) -> Result<()> {
    assert!(expr.get_children().len() >= 2);
    // args should be like `&Option<Json> , &[&Option<Bytes>]`.
    valid_paths(expr)
}

fn valid_paths(expr: &tipb::Expr) -> Result<()> {
    let children = expr.get_children();
    super::function::validate_expr_return_type(&children[0], EvalType::Json)?;
    for i in 1..children.len() {
        super::function::validate_expr_return_type(&children[i], EvalType::Bytes)?;
    }
    Ok(())
}

#[rpn_fn(raw_varg, min_args = 2, extra_validator = json_with_paths_validator)]
#[inline]
fn json_extract(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    assert!(args.len() >= 2);
    let j: &Option<Json> = args[0].as_ref();
    let j = match j.as_ref() {
        None => return Ok(None),
        Some(j) => j.to_owned(),
    };

    let path_expr_list = try_opt!(parse_json_path_list(&args[1..]));

    Ok(j.extract(&path_expr_list))
}

// Args should be like `(&Option<Json> , &[&Option<Bytes>])`.
fn json_with_path_validator(expr: &tipb::Expr) -> Result<()> {
    assert!(expr.get_children().len() == 2 || expr.get_children().len() == 1);
    valid_paths(expr)
}

#[rpn_fn(raw_varg,min_args= 1, max_args = 2, extra_validator = json_with_path_validator)]
#[inline]
fn json_length(args: &[ScalarValueRef]) -> Result<Option<Int>> {
    assert!(!args.is_empty() && args.len() <= 2);
    let j: &Option<Json> = args[0].as_ref();
    let j = match j.as_ref() {
        None => return Ok(None),
        Some(j) => j.to_owned(),
    };
    Ok(parse_json_path_list(&args[1..])?.and_then(|path_expr_list| j.json_length(&path_expr_list)))
}

#[rpn_fn(raw_varg, min_args = 2, extra_validator = json_with_paths_validator)]
#[inline]
fn json_remove(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    assert!(args.len() >= 2);
    let j: &Option<Json> = args[0].as_ref();
    let mut j = match j.as_ref() {
        None => return Ok(None),
        Some(j) => j.to_owned(),
    };

    let path_expr_list = try_opt!(parse_json_path_list(&args[1..]));

    j.remove(&path_expr_list)?;
    Ok(Some(j))
}

fn parse_json_path_list(args: &[ScalarValueRef]) -> Result<Option<Vec<PathExpression>>> {
    let mut path_expr_list = Vec::with_capacity(args.len());
    for arg in args {
        let json_path: &Option<Bytes> = arg.as_ref();

        path_expr_list.push(try_opt!(parse_json_path(json_path)));
    }
    Ok(Some(path_expr_list))
}

#[inline]
fn parse_json_path(path: &Option<Bytes>) -> Result<Option<PathExpression>> {
    let json_path = match path.as_ref() {
        None => return Ok(None),
        Some(p) => std::str::from_utf8(&p).map_err(crate::codec::Error::from),
    }?;

    Ok(Some(parse_json_path_expr(&json_path)?))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    use tipb::ScalarFuncSig;

    use crate::rpn_expr::types::test_util::RpnFnScalarEvaluator;

    #[test]
    fn test_json_depth() {
        let cases = vec![
            (None, None),
            (Some("null"), Some(1)),
            (Some("[true, 2017]"), Some(2)),
            (
                Some(r#"{"a": {"a1": [3]}, "b": {"b1": {"c": {"d": [5]}}}}"#),
                Some(6),
            ),
            (Some("{}"), Some(1)),
            (Some("[]"), Some(1)),
            (Some("true"), Some(1)),
            (Some("1"), Some(1)),
            (Some("-1"), Some(1)),
            (Some(r#""a""#), Some(1)),
            (Some(r#"[10, 20]"#), Some(2)),
            (Some(r#"[[], {}]"#), Some(2)),
            (Some(r#"[10, {"a": 20}]"#), Some(3)),
            (Some(r#"[[2], 3, [[[4]]]]"#), Some(5)),
            (Some(r#"{"Name": "Homer"}"#), Some(2)),
            (Some(r#"[10, {"a": 20}]"#), Some(3)),
            (
                Some(
                    r#"{"Person": {"Name": "Homer", "Age": 39, "Hobbies": ["Eating", "Sleeping"]} }"#,
                ),
                Some(4),
            ),
            (Some(r#"{"a":1}"#), Some(2)),
            (Some(r#"{"a":[1]}"#), Some(3)),
            (Some(r#"{"b":2, "c":3}"#), Some(2)),
            (Some(r#"[1]"#), Some(2)),
            (Some(r#"[1,2]"#), Some(2)),
            (Some(r#"[1,2,[1,3]]"#), Some(3)),
            (Some(r#"[1,2,[1,[5,[3]]]]"#), Some(5)),
            (Some(r#"[1,2,[1,[5,{"a":[2,3]}]]]"#), Some(6)),
            (Some(r#"[{"a":1}]"#), Some(3)),
            (Some(r#"[{"a":1,"b":2}]"#), Some(3)),
            (Some(r#"[{"a":{"a":1},"b":2}]"#), Some(4)),
        ];
        for (arg, expect_output) in cases {
            let arg = arg.map(|input| Json::from_str(input).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::JsonDepthSig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_json_type() {
        let cases = vec![
            (None, None),
            (Some(r#"true"#), Some("BOOLEAN")),
            (Some(r#"null"#), Some("NULL")),
            (Some(r#"-3"#), Some("INTEGER")),
            (Some(r#"3"#), Some("INTEGER")),
            (Some(r#"9223372036854775808"#), Some("DOUBLE")),
            (Some(r#"3.14"#), Some("DOUBLE")),
            (Some(r#"[1, 2, 3]"#), Some("ARRAY")),
            (Some(r#"{"name": 123}"#), Some("OBJECT")),
        ];

        for (arg, expect_output) in cases {
            let arg = arg.map(|input| Json::from_str(input).unwrap());
            let expect_output = expect_output.map(|s| Bytes::from(s));

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::JsonTypeSig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_json_modify() {
        let cases: Vec<(_, Vec<ScalarValue>, _)> = vec![
            (
                ScalarFuncSig::JsonSetSig,
                vec![
                    None::<Json>.into(),
                    None::<Bytes>.into(),
                    None::<Json>.into(),
                ],
                None::<Json>,
            ),
            (
                ScalarFuncSig::JsonSetSig,
                vec![
                    Some(Json::I64(9)).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(Json::U64(3)).into(),
                ],
                Some(r#"[9,3]"#.parse().unwrap()),
            ),
            (
                ScalarFuncSig::JsonInsertSig,
                vec![
                    Some(Json::I64(9)).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(Json::U64(3)).into(),
                ],
                Some(r#"[9,3]"#.parse().unwrap()),
            ),
            (
                ScalarFuncSig::JsonReplaceSig,
                vec![
                    Some(Json::I64(9)).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(Json::U64(3)).into(),
                ],
                Some(r#"9"#.parse().unwrap()),
            ),
            (
                ScalarFuncSig::JsonSetSig,
                vec![
                    Some(Json::from_str(r#"{"a":"x"}"#).unwrap()).into(),
                    Some(b"$.a".to_vec()).into(),
                    None::<Json>.into(),
                ],
                Some(r#"{"a":null}"#.parse().unwrap()),
            ),
        ];
        for (sig, args, expect_output) in cases {
            let output: Option<Json> = RpnFnScalarEvaluator::new()
                .push_params(args.clone())
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", args);
        }
    }

    #[test]
    fn test_json_array() {
        let cases = vec![
            (vec![], Some(r#"[]"#)),
            (vec![Some(r#"1"#), None], Some(r#"[1, null]"#)),
            (
                vec![
                    Some(r#"1"#),
                    None,
                    Some(r#"2"#),
                    Some(r#""sdf""#),
                    Some(r#""k1""#),
                    Some(r#""v1""#),
                ],
                Some(r#"[1, null, 2, "sdf", "k1", "v1"]"#),
            ),
        ];

        for (vargs, expected) in cases {
            let vargs = vargs
                .into_iter()
                .map(|input| input.map(|s| Json::from_str(s).unwrap()))
                .collect::<Vec<_>>();
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonArraySig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_merge() {
        let cases = vec![
            (vec![None, None], None),
            (vec![Some("{}"), Some("[]")], Some("[{}]")),
            (
                vec![Some(r#"{}"#), Some(r#"[]"#), Some(r#"3"#), Some(r#""4""#)],
                Some(r#"[{}, 3, "4"]"#),
            ),
            (
                vec![Some("[1, 2]"), Some("[3, 4]")],
                Some(r#"[1, 2, 3, 4]"#),
            ),
        ];

        for (vargs, expected) in cases {
            let vargs = vargs
                .into_iter()
                .map(|input| input.map(|s| Json::from_str(s).unwrap()))
                .collect::<Vec<_>>();
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonMergeSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_object() {
        let cases = vec![
            (vec![], r#"{}"#),
            (vec![("1", None)], r#"{"1":null}"#),
            (
                vec![
                    ("1", None),
                    ("2", Some(r#""sdf""#)),
                    ("k1", Some(r#""v1""#)),
                ],
                r#"{"1":null,"2":"sdf","k1":"v1"}"#,
            ),
        ];

        for (vargs, expected) in cases {
            let vargs = vargs
                .into_iter()
                .map(|(key, value)| (Bytes::from(key), value.map(|s| Json::from_str(s).unwrap())))
                .collect::<Vec<_>>();

            let mut new_vargs: Vec<ScalarValue> = vec![];
            for (key, value) in vargs.into_iter() {
                new_vargs.push(ScalarValue::from(key));
                new_vargs.push(ScalarValue::from(value));
            }

            let expected = Json::from_str(expected).unwrap();

            let output: Json = RpnFnScalarEvaluator::new()
                .push_params(new_vargs)
                .evaluate(ScalarFuncSig::JsonObjectSig)
                .unwrap()
                .unwrap();
            assert_eq!(output, expected);
        }

        let err_cases = vec![
            vec![
                ScalarValue::from(Bytes::from("1")),
                ScalarValue::from(None::<Json>),
                ScalarValue::from(Bytes::from("1")),
            ],
            vec![
                ScalarValue::from(None::<Bytes>),
                ScalarValue::from(Json::from_str("1").unwrap()),
            ],
        ];

        for err_args in err_cases {
            let output: Result<Option<Json>> = RpnFnScalarEvaluator::new()
                .push_params(err_args)
                .evaluate(ScalarFuncSig::JsonObjectSig);

            assert!(output.is_err());
        }
    }

    #[test]
    fn test_json_unquote() {
        let cases = vec![
            (None, false, None),
            (Some(r"a"), false, Some("a")),
            (Some(r#""3""#), false, Some(r#""3""#)),
            (Some(r#""3""#), true, Some(r#"3"#)),
            (Some(r#"{"a":  "b"}"#), false, Some(r#"{"a":  "b"}"#)),
            (Some(r#"{"a":  "b"}"#), true, Some(r#"{"a":"b"}"#)),
            (
                Some(r#"hello,\"quoted string\",world"#),
                false,
                Some(r#"hello,"quoted string",world"#),
            ),
        ];

        for (arg, parse, expect_output) in cases {
            let arg = arg.map(|input| {
                if parse {
                    input.parse().unwrap()
                } else {
                    Json::String(input.to_string())
                }
            });
            let expect_output = expect_output.map(Bytes::from);

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::JsonUnquoteSig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_json_extract() {
        let cases: Vec<(Vec<ScalarValue>, _)> = vec![
            (vec![None::<Json>.into(), None::<Bytes>.into()], None),
            (
                vec![
                    Some(Json::from_str("[10, 20, [30, 40]]").unwrap()).into(),
                    Some(b"$[1]".to_vec()).into(),
                ],
                Some("20"),
            ),
            (
                vec![
                    Some(Json::from_str("[10, 20, [30, 40]]").unwrap()).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(b"$[0]".to_vec()).into(),
                ],
                Some("[20, 10]"),
            ),
            (
                vec![
                    Some(Json::from_str("[10, 20, [30, 40]]").unwrap()).into(),
                    Some(b"$[2][*]".to_vec()).into(),
                ],
                Some("[30, 40]"),
            ),
            (
                vec![
                    Some(Json::from_str("[10, 20, [30, 40]]").unwrap()).into(),
                    Some(b"$[2][*]".to_vec()).into(),
                    None::<Bytes>.into(),
                ],
                None,
            ),
        ];

        for (vargs, expected) in cases {
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonExtractSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_remove() {
        let cases: Vec<(Vec<ScalarValue>, _)> = vec![(
            vec![
                Some(Json::from_str(r#"["a", ["b", "c"], "d"]"#).unwrap()).into(),
                Some(b"$[1]".to_vec()).into(),
            ],
            Some(r#"["a", "d"]"#),
        )];

        for (vargs, expected) in cases {
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonRemoveSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_length() {
        let cases: Vec<(Vec<ScalarValue>, Option<i64>)> = vec![
            (
                vec![
                    Some(Json::from_str("null").unwrap()).into(),
                    None::<Bytes>.into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str("false").unwrap()).into(),
                    None::<Bytes>.into(),
                ],
                None,
            ),
            (vec![Some(Json::from_str("1").unwrap()).into()], Some(1)),
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(b"$.*".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a":{"a":1},"b":2}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                ],
                Some(2),
            ),
        ];

        for (vargs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonLengthSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }
}
