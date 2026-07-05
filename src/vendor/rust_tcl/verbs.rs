//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise identical
//! to upstream `src/verbs.rs` EXCEPT one addition: the `incr` verb
//! (upstream has none) — see `VENDOR.md` and this file's own comment
//! at the registration site.

use crate::vendor::rust_tcl::error::{Error, Result};
use crate::vendor::rust_tcl::list_value::{format_list, parse_list};
use crate::vendor::rust_tcl::registry::{Arity, Registry};
use crate::vendor::rust_tcl::value::Value;

pub fn register_core_verbs(registry: &mut Registry) {
    registry.register("set", Arity::range(1, 2), |vm, args| {
        let name = args[0].as_str();
        if args.len() == 1 {
            return vm
                .get_var(name)
                .ok_or_else(|| Error::runtime(format!("unknown variable `{name}`")));
        }
        let value = args[1].clone();
        vm.set_var(name, value.clone());
        Ok(value)
    });

    registry.register("append", Arity::at_least(2), |vm, args| {
        let name = args[0].as_str();
        let mut value = vm
            .get_var(name)
            .map(|v| v.into_string())
            .unwrap_or_default();
        for arg in &args[1..] {
            value.push_str(arg.as_str());
        }
        let value = Value::new(value);
        vm.set_var(name, value.clone());
        Ok(value)
    });

    registry.register("puts", Arity::exact(1), |vm, args| {
        vm.write_line(args[0].as_str());
        Ok(Value::empty())
    });

    // MACVM addition (not upstream — see VENDOR.md): real Tcl's `incr
    // varName ?increment?`, missing from this crate entirely. Same shape
    // as `set`/`append` just above: read the variable, compute, write it
    // back, return the new value.
    registry.register("incr", Arity::range(1, 2), |vm, args| {
        let name = args[0].as_str();
        let current = match vm.get_var(name) {
            Some(v) => parse_i64(&v, "incr")?,
            None => 0,
        };
        let step = match args.get(1) {
            Some(v) => parse_i64(v, "incr")?,
            None => 1,
        };
        let value = Value::new((current + step).to_string());
        vm.set_var(name, value.clone());
        Ok(value)
    });

    registry.register("add", Arity::exact(2), |_, args| {
        int_binop(args, "add", |a, b| a + b)
    });
    registry.register("sub", Arity::exact(2), |_, args| {
        int_binop(args, "sub", |a, b| a - b)
    });
    registry.register("mul", Arity::exact(2), |_, args| {
        int_binop(args, "mul", |a, b| a * b)
    });
    registry.register("div", Arity::exact(2), |_, args| {
        let a = parse_i64(&args[0], "div")?;
        let b = parse_i64(&args[1], "div")?;
        if b == 0 {
            return Err(Error::runtime("div by zero"));
        }
        Ok(Value::new((a / b).to_string()))
    });

    registry.register("eq", Arity::exact(2), |_, args| {
        Ok(Value::new(if args[0] == args[1] { "1" } else { "0" }))
    });

    registry.register("concat", Arity::at_least(0), |_, args| {
        Ok(Value::new(
            args.iter().map(Value::as_str).collect::<Vec<_>>().join(" "),
        ))
    });

    registry.register("list", Arity::at_least(0), |_, args| {
        Ok(Value::new(format_list(
            &args
                .iter()
                .map(|value| value.as_str().to_string())
                .collect::<Vec<_>>(),
        )))
    });

    registry.register("llength", Arity::exact(1), |_, args| {
        Ok(Value::new(parse_list(args[0].as_str())?.len().to_string()))
    });

    registry.register("lindex", Arity::exact(2), |_, args| {
        let values = parse_list(args[0].as_str())?;
        let Some(index) = parse_index(args[1].as_str(), values.len())? else {
            return Ok(Value::empty());
        };
        Ok(Value::new(values.get(index).cloned().unwrap_or_default()))
    });

    registry.register("lrange", Arity::exact(3), |_, args| {
        let values = parse_list(args[0].as_str())?;
        if values.is_empty() {
            return Ok(Value::empty());
        }
        let Some(first) = parse_index(args[1].as_str(), values.len())? else {
            return Ok(Value::empty());
        };
        let Some(last) = parse_index(args[2].as_str(), values.len())? else {
            return Ok(Value::empty());
        };
        if first > last {
            return Ok(Value::empty());
        }
        Ok(Value::new(format_list(&values[first..=last])))
    });

    registry.register("lappend", Arity::at_least(2), |vm, args| {
        let name = args[0].as_str();
        let mut values = vm
            .get_var(name)
            .map(|value| parse_list(value.as_str()))
            .transpose()?
            .unwrap_or_default();
        values.extend(args[1..].iter().map(|value| value.as_str().to_string()));
        let value = Value::new(format_list(&values));
        vm.set_var(name, value.clone());
        Ok(value)
    });

    registry.register("dict", Arity::at_least(1), |vm, args| {
        dict_command(vm, args)
    });

    registry.register_control("upvar", Arity::at_least(2), |vm, args| {
        let (level, pair_start) = if args.len() % 2 == 1 {
            (args[0].as_str(), 1)
        } else {
            ("1", 0)
        };
        let pair_count = args.len() - pair_start;
        if pair_count == 0 || pair_count % 2 != 0 {
            return Err(Error::runtime(
                "upvar expects ?level? otherVar localVar ?otherVar localVar ...?",
            ));
        }
        let pairs = args[pair_start..]
            .chunks_exact(2)
            .map(|chunk| (chunk[0].as_str().to_string(), chunk[1].as_str().to_string()))
            .collect::<Vec<_>>();
        vm.upvar(level, &pairs)?;
        Ok(crate::vendor::rust_tcl::vm::Flow::Value(Value::empty()))
    });

    registry.register_control("uplevel", Arity::at_least(1), |vm, args| {
        let (level, script_start) = if args.len() > 1 && looks_like_level(args[0].as_str()) {
            (args[0].as_str(), 1)
        } else {
            ("1", 0)
        };
        let script = args[script_start..]
            .iter()
            .map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        vm.uplevel(level, &script)
    });

    registry.register("error", Arity::exact(1), |_, args| {
        Err(Error::runtime(args[0].as_str().to_string()))
    });
}

fn int_binop(args: &[Value], verb: &str, op: fn(i64, i64) -> i64) -> Result<Value> {
    let a = parse_i64(&args[0], verb)?;
    let b = parse_i64(&args[1], verb)?;
    Ok(Value::new(op(a, b).to_string()))
}

fn parse_i64(value: &Value, verb: &str) -> Result<i64> {
    value
        .as_str()
        .parse::<i64>()
        .map_err(|_| Error::runtime(format!("`{verb}` expected an integer, got `{value}`")))
}

fn looks_like_level(source: &str) -> bool {
    let digits = source.strip_prefix('#').unwrap_or(source);
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn parse_index(source: &str, len: usize) -> Result<Option<usize>> {
    if len == 0 {
        return Ok(None);
    }
    if source == "end" {
        return Ok(Some(len - 1));
    }
    let index = source
        .parse::<isize>()
        .map_err(|_| Error::runtime(format!("bad list index `{source}`")))?;
    if index < 0 || index as usize >= len {
        Ok(None)
    } else {
        Ok(Some(index as usize))
    }
}

fn dict_command(vm: &mut crate::vendor::rust_tcl::vm::Vm<'_>, args: &[Value]) -> Result<Value> {
    match args[0].as_str() {
        "create" => {
            if (args.len() - 1) % 2 != 0 {
                return Err(Error::runtime("dict create expects key/value pairs"));
            }
            let values = args[1..]
                .iter()
                .map(|value| value.as_str().to_string())
                .collect::<Vec<_>>();
            Ok(Value::new(format_list(&values)))
        }
        "get" => match args.len() {
            2 => Ok(args[1].clone()),
            3 => {
                let pairs = dict_pairs(args[1].as_str())?;
                pairs
                    .into_iter()
                    .find(|(key, _)| key == args[2].as_str())
                    .map(|(_, value)| Value::new(value))
                    .ok_or_else(|| Error::runtime(format!("key `{}` not found", args[2])))
            }
            _ => Err(Error::runtime("dict get expects dictionary ?key?")),
        },
        "exists" => {
            if args.len() != 3 {
                return Err(Error::runtime("dict exists expects dictionary key"));
            }
            let pairs = dict_pairs(args[1].as_str())?;
            Ok(Value::new(
                if pairs.iter().any(|(key, _)| key == args[2].as_str()) {
                    "1"
                } else {
                    "0"
                },
            ))
        }
        "set" => {
            if args.len() != 4 {
                return Err(Error::runtime("dict set expects variable key value"));
            }
            let name = args[1].as_str();
            let mut pairs = vm
                .get_var(name)
                .map(|value| dict_pairs(value.as_str()))
                .transpose()?
                .unwrap_or_default();
            set_pair(&mut pairs, args[2].as_str(), args[3].as_str());
            let value = Value::new(format_pairs(&pairs));
            vm.set_var(name, value.clone());
            Ok(value)
        }
        "keys" => {
            if args.len() != 2 {
                return Err(Error::runtime("dict keys expects dictionary"));
            }
            let keys = dict_pairs(args[1].as_str())?
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>();
            Ok(Value::new(format_list(&keys)))
        }
        "values" => {
            if args.len() != 2 {
                return Err(Error::runtime("dict values expects dictionary"));
            }
            let values = dict_pairs(args[1].as_str())?
                .into_iter()
                .map(|(_, value)| value)
                .collect::<Vec<_>>();
            Ok(Value::new(format_list(&values)))
        }
        other => Err(Error::runtime(format!("unknown dict subcommand `{other}`"))),
    }
}

fn dict_pairs(source: &str) -> Result<Vec<(String, String)>> {
    let values = parse_list(source)?;
    if values.len() % 2 != 0 {
        return Err(Error::runtime("dictionary has an odd number of elements"));
    }
    Ok(values
        .chunks_exact(2)
        .map(|chunk| (chunk[0].clone(), chunk[1].clone()))
        .collect())
}

fn set_pair(pairs: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, existing)) = pairs.iter_mut().find(|(existing, _)| existing == key) {
        *existing = value.to_string();
        return;
    }
    pairs.push((key.to_string(), value.to_string()));
}

fn format_pairs(pairs: &[(String, String)]) -> String {
    let values = pairs
        .iter()
        .flat_map(|(key, value)| [key.clone(), value.clone()])
        .collect::<Vec<_>>();
    format_list(&values)
}
