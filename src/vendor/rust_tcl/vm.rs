//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/vm.rs`.

use std::collections::HashMap;

use crate::vendor::rust_tcl::bytecode::{Instr, Procedure, Program};
use crate::vendor::rust_tcl::error::{Error, Result};
use crate::vendor::rust_tcl::expr::truthy;
use crate::vendor::rust_tcl::list_value::parse_list;
use crate::vendor::rust_tcl::registry::{Registry, VerbId};
use crate::vendor::rust_tcl::value::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Flow {
    Value(Value),
    Return(Value),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunResult {
    pub value: Value,
    pub output: String,
}

pub struct Vm<'a> {
    registry: &'a Registry,
    scopes: Vec<HashMap<String, Binding>>,
    current_scope: usize,
    procedures: HashMap<String, Procedure>,
    foreach: Vec<ForeachFrame>,
    stack: Vec<Value>,
    output: String,
    proc_depth: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Binding {
    Value(Value),
    Alias(VarRef),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VarRef {
    frame: usize,
    name: String,
}

struct ForeachFrame {
    var: String,
    values: Vec<String>,
    index: usize,
}

impl<'a> Vm<'a> {
    pub fn new(registry: &'a Registry) -> Self {
        Self {
            registry,
            scopes: vec![HashMap::new()],
            current_scope: 0,
            procedures: HashMap::new(),
            foreach: Vec::new(),
            stack: Vec::new(),
            output: String::new(),
            proc_depth: 0,
        }
    }

    pub fn run(&mut self, program: &Program) -> Result<RunResult> {
        let flow = self.execute(program)?;
        let value = match flow {
            Flow::Value(value) => value,
            Flow::Return(_) => return Err(Error::runtime("`return` outside procedure")),
        };
        Ok(RunResult {
            value,
            output: self.output.clone(),
        })
    }

    pub fn get_var(&self, name: &str) -> Option<Value> {
        let resolved = self.resolve_var_ref(self.current_scope, name, 0);
        self.value_at(resolved?)
    }

    pub fn set_var(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        let target = self
            .resolve_var_ref(self.current_scope, &name, 0)
            .unwrap_or(VarRef {
                frame: self.current_scope,
                name,
            });
        self.set_at(target, value);
    }

    pub fn upvar(&mut self, level: &str, pairs: &[(String, String)]) -> Result<()> {
        let target_frame = self.resolve_level(level)?;
        for (other, local) in pairs {
            self.scopes
                .get_mut(self.current_scope)
                .expect("current scope exists")
                .insert(
                    local.clone(),
                    Binding::Alias(VarRef {
                        frame: target_frame,
                        name: other.clone(),
                    }),
                );
        }
        Ok(())
    }

    pub fn uplevel(&mut self, level: &str, script: &str) -> Result<Flow> {
        let target_frame = self.resolve_level(level)?;
        let program = crate::vendor::rust_tcl::compile(script, self.registry)?;
        self.execute_in_scope(&program, target_frame)
    }

    pub fn write(&mut self, text: &str) {
        self.output.push_str(text);
    }

    pub fn write_line(&mut self, text: &str) {
        self.output.push_str(text);
        self.output.push('\n');
    }

    fn execute(&mut self, program: &Program) -> Result<Flow> {
        let stack_base = self.stack.len();
        let mut pc = 0usize;
        loop {
            let instr = program
                .instructions
                .get(pc)
                .ok_or_else(|| Error::runtime("program counter escaped bytecode"))?;
            pc += 1;

            match instr {
                Instr::PushConst(id) => {
                    let value = program
                        .constant(*id)
                        .ok_or_else(|| Error::runtime(format!("missing constant #{id}")))?;
                    self.stack.push(Value::new(value));
                }
                Instr::LoadVar(id) => {
                    let name = program.constant(*id).ok_or_else(|| {
                        Error::runtime(format!("missing variable-name constant #{id}"))
                    })?;
                    let value = self
                        .get_var(name)
                        .ok_or_else(|| Error::runtime(format!("unknown variable `{name}`")))?;
                    self.stack.push(value);
                }
                Instr::EvalExpr(id) => {
                    let expr = program
                        .expressions
                        .get(*id)
                        .ok_or_else(|| Error::runtime(format!("missing expression #{id}")))?;
                    let value = expr.eval(&mut |name| self.get_var(name))?;
                    self.stack.push(value);
                }
                Instr::Concat(n) => {
                    let values = self.pop_many(*n)?;
                    let mut text = String::new();
                    for value in values {
                        text.push_str(value.as_str());
                    }
                    self.stack.push(Value::new(text));
                }
                Instr::Call(verb, argc) => {
                    let args = self.pop_many(*argc)?;
                    let flow = self.call_native(*verb, &args)?;
                    if !self.accept_flow(flow, stack_base)? {
                        return Ok(self.take_flow_return(stack_base));
                    }
                }
                Instr::CallName(name_id, argc) => {
                    let name = program
                        .constant(*name_id)
                        .ok_or_else(|| {
                            Error::runtime(format!("missing command-name constant #{name_id}"))
                        })?
                        .to_string();
                    let args = self.pop_many(*argc)?;
                    let flow = self.call_by_name(&name, &args)?;
                    if !self.accept_flow(flow, stack_base)? {
                        return Ok(self.take_flow_return(stack_base));
                    }
                }
                Instr::CallDynamic(argc) => {
                    let args = self.pop_many(*argc)?;
                    let name = self
                        .stack
                        .pop()
                        .ok_or_else(|| Error::runtime("stack underflow on dynamic call name"))?;
                    let flow = self.call_by_name(name.as_str(), &args)?;
                    if !self.accept_flow(flow, stack_base)? {
                        return Ok(self.take_flow_return(stack_base));
                    }
                }
                Instr::DefineProc { name, proc } => {
                    let name = program
                        .constant(*name)
                        .ok_or_else(|| {
                            Error::runtime(format!("missing proc-name constant #{name}"))
                        })?
                        .to_string();
                    let procedure = program
                        .procedures
                        .get(*proc)
                        .ok_or_else(|| Error::runtime(format!("missing procedure #{proc}")))?
                        .clone();
                    self.procedures.insert(name, procedure);
                    self.stack.push(Value::empty());
                }
                Instr::JumpIfFalse(target) => {
                    let cond = self
                        .stack
                        .pop()
                        .ok_or_else(|| Error::runtime("stack underflow on conditional jump"))?;
                    if !truthy(&cond) {
                        pc = *target;
                    }
                }
                Instr::Jump(target) => {
                    pc = *target;
                }
                Instr::ForeachStart { var, end } => {
                    let list = self
                        .stack
                        .pop()
                        .ok_or_else(|| Error::runtime("stack underflow on foreach list"))?;
                    let values = parse_list(list.as_str())?;
                    if values.is_empty() {
                        pc = *end;
                        continue;
                    }
                    let var = program
                        .constant(*var)
                        .ok_or_else(|| Error::runtime(format!("missing foreach variable #{var}")))?
                        .to_string();
                    self.set_var(var.clone(), Value::new(values[0].clone()));
                    self.foreach.push(ForeachFrame {
                        var,
                        values,
                        index: 0,
                    });
                }
                Instr::ForeachNext { body, end } => {
                    let Some(frame) = self.foreach.last_mut() else {
                        return Err(Error::runtime("foreach frame missing"));
                    };
                    frame.index += 1;
                    if frame.index < frame.values.len() {
                        let var = frame.var.clone();
                        let value = frame.values[frame.index].clone();
                        self.set_var(var, Value::new(value));
                        pc = *body;
                    } else {
                        self.foreach.pop();
                        pc = *end;
                    }
                }
                Instr::ForeachPop => {
                    self.foreach
                        .pop()
                        .ok_or_else(|| Error::runtime("foreach frame missing on break"))?;
                }
                Instr::Return => {
                    let value = self.stack.pop().unwrap_or_else(Value::empty);
                    self.stack.truncate(stack_base);
                    return Ok(Flow::Return(value));
                }
                Instr::Pop => {
                    self.stack
                        .pop()
                        .ok_or_else(|| Error::runtime("stack underflow on pop"))?;
                }
                Instr::Halt => {
                    let value = self.stack.pop().unwrap_or_else(Value::empty);
                    self.stack.truncate(stack_base);
                    return Ok(Flow::Value(value));
                }
            }
        }
    }

    fn accept_flow(&mut self, flow: Flow, stack_base: usize) -> Result<bool> {
        match flow {
            Flow::Value(value) => {
                self.stack.push(value);
                Ok(true)
            }
            Flow::Return(value) => {
                self.stack.truncate(stack_base);
                self.stack.push(value);
                Ok(false)
            }
        }
    }

    fn take_flow_return(&mut self, stack_base: usize) -> Flow {
        let value = self.stack.pop().unwrap_or_else(Value::empty);
        self.stack.truncate(stack_base);
        Flow::Return(value)
    }

    fn call_native(&mut self, verb: VerbId, args: &[Value]) -> Result<Flow> {
        let spec = self
            .registry
            .spec(verb)
            .ok_or_else(|| Error::runtime(format!("unknown verb id {}", verb.0)))?;
        if !spec.arity.accepts(args.len()) {
            return Err(Error::runtime(format!(
                "verb `{}` called with {} arguments",
                spec.name,
                args.len()
            )));
        }
        let handler = spec.handler.clone();
        handler(self, args)
    }

    fn call_by_name(&mut self, name: &str, args: &[Value]) -> Result<Flow> {
        if let Some(verb) = self.registry.resolve(name) {
            return self.call_native(verb, args);
        }
        if let Some(procedure) = self.procedures.get(name).cloned() {
            return self.call_procedure(name, &procedure, args);
        }
        Err(Error::runtime(format!("unknown command `{name}`")))
    }

    fn call_procedure(
        &mut self,
        name: &str,
        procedure: &Procedure,
        args: &[Value],
    ) -> Result<Flow> {
        let required = procedure
            .params
            .iter()
            .filter(|param| param.default.is_none())
            .count();
        if args.len() < required || args.len() > procedure.params.len() {
            return Err(Error::runtime(format!(
                "procedure `{name}` got {} arguments but expects {}..={}",
                args.len(),
                required,
                procedure.params.len()
            )));
        }

        let mut scope = HashMap::new();
        for (i, param) in procedure.params.iter().enumerate() {
            let value = args
                .get(i)
                .cloned()
                .or_else(|| param.default.as_ref().map(|v| Value::new(v)))
                .ok_or_else(|| Error::runtime(format!("missing argument `{}`", param.name)))?;
            scope.insert(param.name.clone(), Binding::Value(value));
        }

        self.proc_depth += 1;
        let previous_scope = self.current_scope;
        let frame = self.scopes.len();
        self.scopes.push(scope);
        self.current_scope = frame;
        let flow = self.execute(&procedure.body);
        self.current_scope = previous_scope;
        self.scopes.pop();
        self.proc_depth -= 1;

        match flow? {
            Flow::Value(value) | Flow::Return(value) => Ok(Flow::Value(value)),
        }
    }

    fn pop_many(&mut self, n: usize) -> Result<Vec<Value>> {
        if self.stack.len() < n {
            return Err(Error::runtime(format!(
                "stack underflow: need {n}, have {}",
                self.stack.len()
            )));
        }
        let start = self.stack.len() - n;
        Ok(self.stack.drain(start..).collect())
    }

    fn execute_in_scope(&mut self, program: &Program, frame: usize) -> Result<Flow> {
        if frame >= self.scopes.len() {
            return Err(Error::runtime(format!(
                "scope frame {frame} does not exist"
            )));
        }
        let previous_scope = self.current_scope;
        self.current_scope = frame;
        let flow = self.execute(program);
        self.current_scope = previous_scope;
        flow
    }

    fn resolve_level(&self, level: &str) -> Result<usize> {
        if let Some(abs) = level.strip_prefix('#') {
            let frame = abs
                .parse::<usize>()
                .map_err(|_| Error::runtime(format!("bad scope level `{level}`")))?;
            if frame >= self.scopes.len() {
                return Err(Error::runtime(format!(
                    "scope level `{level}` does not exist"
                )));
            }
            return Ok(frame);
        }

        let up = level
            .parse::<usize>()
            .map_err(|_| Error::runtime(format!("bad scope level `{level}`")))?;
        self.current_scope
            .checked_sub(up)
            .ok_or_else(|| Error::runtime(format!("scope level `{level}` does not exist")))
    }

    fn resolve_var_ref(&self, frame: usize, name: &str, depth: usize) -> Option<VarRef> {
        if depth > self.scopes.len() {
            return None;
        }
        match self.scopes.get(frame)?.get(name) {
            Some(Binding::Value(_)) => Some(VarRef {
                frame,
                name: name.to_string(),
            }),
            Some(Binding::Alias(target)) => self
                .resolve_var_ref(target.frame, &target.name, depth + 1)
                .or_else(|| Some(target.clone())),
            None => None,
        }
    }

    fn value_at(&self, var: VarRef) -> Option<Value> {
        match self.scopes.get(var.frame)?.get(&var.name)? {
            Binding::Value(value) => Some(value.clone()),
            Binding::Alias(target) => self.value_at(target.clone()),
        }
    }

    fn set_at(&mut self, var: VarRef, value: Value) {
        if var.frame >= self.scopes.len() {
            return;
        }
        let target = match self.scopes[var.frame].get(&var.name) {
            Some(Binding::Alias(target)) => Some(target.clone()),
            _ => None,
        };
        if let Some(target) = target {
            self.set_at(target, value);
        } else {
            self.scopes[var.frame].insert(var.name, Binding::Value(value));
        }
    }
}
