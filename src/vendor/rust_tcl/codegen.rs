//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/codegen.rs`.

use std::collections::HashMap;

use crate::vendor::rust_tcl::bytecode::{Instr, Procedure, ProcedureParam, Program};
use crate::vendor::rust_tcl::cfg::{Cfg, CfgCommand};
use crate::vendor::rust_tcl::error::{Error, Result};
use crate::vendor::rust_tcl::list_value::parse_list;
use crate::vendor::rust_tcl::registry::Registry;
use crate::vendor::rust_tcl::sema::{BoundCommandTarget, BoundScript, BoundWord, BoundWordPart};

pub fn compile(cfg: &Cfg, registry: &Registry) -> Result<Program> {
    let mut compiler = Compiler::new(registry);
    if let Some(block) = cfg.blocks.first() {
        compiler.compile_commands(&block.commands)?;
    } else {
        compiler.push_empty();
    }
    compiler.emit(Instr::Halt);
    Ok(compiler.finish())
}

struct LoopContext {
    continue_target: Option<usize>,
    continue_jumps: Vec<usize>,
    break_jumps: Vec<usize>,
}

struct Compiler<'a> {
    registry: &'a Registry,
    constants: Vec<String>,
    constant_ids: HashMap<String, usize>,
    expressions: Vec<crate::vendor::rust_tcl::expr::Expr>,
    procedures: Vec<Procedure>,
    instructions: Vec<Instr>,
    loops: Vec<LoopContext>,
}

impl<'a> Compiler<'a> {
    fn new(registry: &'a Registry) -> Self {
        Self {
            registry,
            constants: Vec::new(),
            constant_ids: HashMap::new(),
            expressions: Vec::new(),
            procedures: Vec::new(),
            instructions: Vec::new(),
            loops: Vec::new(),
        }
    }

    fn compile_commands(&mut self, commands: &[CfgCommand]) -> Result<()> {
        if commands.is_empty() {
            self.push_empty();
            return Ok(());
        }

        let mut last_pushed = false;
        for (i, command) in commands.iter().enumerate() {
            last_pushed = self.compile_command(command)?;
            if i + 1 != commands.len() && last_pushed {
                self.emit(Instr::Pop);
            }
        }

        if !last_pushed {
            self.push_empty();
        }
        Ok(())
    }

    fn compile_bound_script_value(&mut self, script: &BoundScript) -> Result<()> {
        let cfg = crate::vendor::rust_tcl::cfg::build(script);
        if let Some(block) = cfg.blocks.first() {
            self.compile_commands(&block.commands)
        } else {
            self.push_empty();
            Ok(())
        }
    }

    fn compile_command(&mut self, command: &CfgCommand) -> Result<bool> {
        if let Some(name) = target_name(&command.target) {
            match name {
                "expr" => return self.compile_expr_command(command),
                "if" => return self.compile_if(command),
                "while" => return self.compile_while(command),
                "foreach" => return self.compile_foreach(command),
                "proc" => return self.compile_proc(command),
                "return" => return self.compile_return(command),
                "break" => return self.compile_break(command),
                "continue" => return self.compile_continue(command),
                _ => {}
            }
        }

        match &command.target {
            BoundCommandTarget::Native { verb, .. } => {
                for arg in &command.args {
                    self.compile_word(arg)?;
                }
                self.emit(Instr::Call(*verb, command.args.len()));
            }
            BoundCommandTarget::Named(name) => {
                for arg in &command.args {
                    self.compile_word(arg)?;
                }
                let id = self.constant(name);
                self.emit(Instr::CallName(id, command.args.len()));
            }
            BoundCommandTarget::Dynamic(word) => {
                self.compile_word(word)?;
                for arg in &command.args {
                    self.compile_word(arg)?;
                }
                self.emit(Instr::CallDynamic(command.args.len()));
            }
        }
        Ok(true)
    }

    fn compile_expr_command(&mut self, command: &CfgCommand) -> Result<bool> {
        if command.args.len() != 1 {
            return Err(Error::sema(command.span, "`expr` expects one argument"));
        }
        self.compile_expr_word(&command.args[0])?;
        Ok(true)
    }

    fn compile_if(&mut self, command: &CfgCommand) -> Result<bool> {
        let mut i = 0usize;
        let mut end_jumps = Vec::new();
        let args = &command.args;

        if args.len() < 2 {
            return Err(Error::sema(command.span, "`if` expects condition and body"));
        }

        loop {
            if i >= args.len() {
                self.push_empty();
                break;
            }

            if literal(&args[i]) == Some("else") {
                let body = args
                    .get(i + 1)
                    .ok_or_else(|| Error::sema(args[i].span, "`else` needs a body"))?;
                self.compile_script_body(body)?;
                i += 2;
                break;
            }

            if literal(&args[i]) == Some("elseif") {
                i += 1;
            }

            let cond = args
                .get(i)
                .ok_or_else(|| Error::sema(command.span, "`if` missing condition"))?;
            self.compile_expr_word(cond)?;
            let false_jump = self.emit_placeholder_jump_if_false();
            i += 1;

            if literal(
                args.get(i).ok_or_else(|| {
                    Error::sema(command.span, "`if` missing body after condition")
                })?,
            ) == Some("then")
            {
                i += 1;
            }

            let body = args
                .get(i)
                .ok_or_else(|| Error::sema(command.span, "`if` missing body"))?;
            self.compile_script_body(body)?;
            i += 1;

            end_jumps.push(self.emit_placeholder_jump());
            self.patch_jump(false_jump, self.instructions.len());

            match args.get(i).and_then(literal) {
                Some("elseif") => continue,
                Some("else") => continue,
                Some(other) => {
                    return Err(Error::sema(
                        args[i].span,
                        format!("unexpected `if` token `{other}`"),
                    ));
                }
                None => {
                    self.push_empty();
                    break;
                }
            }
        }

        if i != args.len() {
            return Err(Error::sema(command.span, "extra words after `if`"));
        }

        let end = self.instructions.len();
        for jump in end_jumps {
            self.patch_jump(jump, end);
        }
        Ok(true)
    }

    fn compile_while(&mut self, command: &CfgCommand) -> Result<bool> {
        if command.args.len() != 2 {
            return Err(Error::sema(
                command.span,
                "`while` expects condition and body",
            ));
        }

        let start = self.instructions.len();
        self.compile_expr_word(&command.args[0])?;
        let exit_jump = self.emit_placeholder_jump_if_false();
        self.loops.push(LoopContext {
            continue_target: Some(start),
            continue_jumps: Vec::new(),
            break_jumps: Vec::new(),
        });
        self.compile_script_body(&command.args[1])?;
        self.emit(Instr::Pop);
        self.emit(Instr::Jump(start));
        let exit = self.instructions.len();
        self.patch_jump(exit_jump, exit);
        let ctx = self.loops.pop().expect("loop context exists");
        for jump in ctx.break_jumps {
            self.patch_jump(jump, exit);
        }
        self.push_empty();
        Ok(true)
    }

    fn compile_foreach(&mut self, command: &CfgCommand) -> Result<bool> {
        if command.args.len() != 3 {
            return Err(Error::sema(
                command.span,
                "`foreach` expects variable, list, and body",
            ));
        }
        let var = literal(&command.args[0]).ok_or_else(|| {
            Error::sema(command.args[0].span, "`foreach` variable must be literal")
        })?;
        let var_id = self.constant(var);
        self.compile_word(&command.args[1])?;
        let start_instr = self.instructions.len();
        self.emit(Instr::ForeachStart {
            var: var_id,
            end: usize::MAX,
        });
        let body_start = self.instructions.len();
        self.loops.push(LoopContext {
            continue_target: None,
            continue_jumps: Vec::new(),
            break_jumps: Vec::new(),
        });
        self.compile_script_body(&command.args[2])?;
        self.emit(Instr::Pop);
        let next_instr = self.instructions.len();
        let mut ctx = self.loops.pop().expect("foreach loop context exists");
        for jump in ctx.continue_jumps.drain(..) {
            self.patch_jump(jump, next_instr);
        }
        self.emit(Instr::ForeachNext {
            body: body_start,
            end: usize::MAX,
        });
        let cleanup = self.instructions.len();
        for jump in ctx.break_jumps {
            self.patch_jump(jump, cleanup);
        }
        self.emit(Instr::ForeachPop);
        let end = self.instructions.len();
        self.patch_foreach_start(start_instr, end);
        self.patch_foreach_next(next_instr, end);
        self.push_empty();
        Ok(true)
    }

    fn compile_proc(&mut self, command: &CfgCommand) -> Result<bool> {
        if command.args.len() != 3 {
            return Err(Error::sema(command.span, "`proc` expects name args body"));
        }
        let name = literal(&command.args[0])
            .ok_or_else(|| Error::sema(command.args[0].span, "`proc` name must be literal"))?;
        let params_source = literal(&command.args[1])
            .ok_or_else(|| Error::sema(command.args[1].span, "`proc` args must be literal"))?;
        let body_source = literal(&command.args[2])
            .ok_or_else(|| Error::sema(command.args[2].span, "`proc` body must be literal"))?;
        let params = parse_params(params_source)?;
        let body = self.compile_script_source(body_source)?;
        let proc_id = self.procedures.len();
        self.procedures.push(Procedure { params, body });
        let name_id = self.constant(name);
        self.emit(Instr::DefineProc {
            name: name_id,
            proc: proc_id,
        });
        Ok(true)
    }

    fn compile_return(&mut self, command: &CfgCommand) -> Result<bool> {
        match command.args.len() {
            0 => self.push_empty(),
            1 => self.compile_word(&command.args[0])?,
            _ => {
                return Err(Error::sema(
                    command.span,
                    "`return` expects zero or one argument",
                ));
            }
        }
        self.emit(Instr::Return);
        Ok(false)
    }

    fn compile_break(&mut self, command: &CfgCommand) -> Result<bool> {
        if !command.args.is_empty() {
            return Err(Error::sema(command.span, "`break` expects no arguments"));
        }
        if self.loops.is_empty() {
            return Err(Error::sema(command.span, "`break` outside loop"));
        }
        let jump = self.emit_placeholder_jump();
        self.loops
            .last_mut()
            .expect("loop checked")
            .break_jumps
            .push(jump);
        Ok(false)
    }

    fn compile_continue(&mut self, command: &CfgCommand) -> Result<bool> {
        if !command.args.is_empty() {
            return Err(Error::sema(command.span, "`continue` expects no arguments"));
        }
        let target = self
            .loops
            .last_mut()
            .ok_or_else(|| Error::sema(command.span, "`continue` outside loop"))?
            .continue_target;
        match target {
            Some(target) => self.emit(Instr::Jump(target)),
            None => {
                let jump = self.emit_placeholder_jump();
                self.loops
                    .last_mut()
                    .expect("loop checked")
                    .continue_jumps
                    .push(jump);
            }
        }
        Ok(false)
    }

    fn compile_script_body(&mut self, word: &BoundWord) -> Result<()> {
        let source = literal(word)
            .ok_or_else(|| Error::sema(word.span, "script body must be a literal word"))?;
        let tokens = crate::vendor::rust_tcl::lexer::lex(source)?;
        let ast = crate::vendor::rust_tcl::parser::parse(&tokens)?;
        let bound = crate::vendor::rust_tcl::sema::analyze(&ast, self.registry)?;
        let cfg = crate::vendor::rust_tcl::cfg::build(&bound);
        if let Some(block) = cfg.blocks.first() {
            self.compile_commands(&block.commands)
        } else {
            self.push_empty();
            Ok(())
        }
    }

    fn compile_script_source(&self, source: &str) -> Result<Program> {
        let tokens = crate::vendor::rust_tcl::lexer::lex(source)?;
        let ast = crate::vendor::rust_tcl::parser::parse(&tokens)?;
        let bound = crate::vendor::rust_tcl::sema::analyze(&ast, self.registry)?;
        let cfg = crate::vendor::rust_tcl::cfg::build(&bound);
        compile(&cfg, self.registry)
    }

    fn compile_word(&mut self, word: &BoundWord) -> Result<()> {
        let parts = word.parts.len();
        if parts == 0 {
            self.push_empty();
            return Ok(());
        }

        for part in &word.parts {
            match part {
                BoundWordPart::Text(text) => {
                    let id = self.constant(text);
                    self.emit(Instr::PushConst(id));
                }
                BoundWordPart::Var(name) => {
                    let id = self.constant(name);
                    self.emit(Instr::LoadVar(id));
                }
                BoundWordPart::Command(script) => self.compile_bound_script_value(script)?,
            }
        }

        if parts > 1 {
            self.emit(Instr::Concat(parts));
        }
        Ok(())
    }

    fn compile_expr_word(&mut self, word: &BoundWord) -> Result<()> {
        let source = literal(word)
            .ok_or_else(|| Error::sema(word.span, "expression must be a literal word"))?;
        let expr = crate::vendor::rust_tcl::expr::parse_expr(source)?;
        let id = self.expressions.len();
        self.expressions.push(expr);
        self.emit(Instr::EvalExpr(id));
        Ok(())
    }

    fn push_empty(&mut self) {
        let empty = self.constant("");
        self.emit(Instr::PushConst(empty));
    }

    fn constant(&mut self, value: &str) -> usize {
        if let Some(id) = self.constant_ids.get(value).copied() {
            return id;
        }
        let id = self.constants.len();
        self.constants.push(value.to_string());
        self.constant_ids.insert(value.to_string(), id);
        id
    }

    fn emit_placeholder_jump_if_false(&mut self) -> usize {
        let pos = self.instructions.len();
        self.emit(Instr::JumpIfFalse(usize::MAX));
        pos
    }

    fn emit_placeholder_jump(&mut self) -> usize {
        let pos = self.instructions.len();
        self.emit(Instr::Jump(usize::MAX));
        pos
    }

    fn patch_jump(&mut self, at: usize, target: usize) {
        match &mut self.instructions[at] {
            Instr::JumpIfFalse(dst) | Instr::Jump(dst) => *dst = target,
            _ => panic!("attempted to patch non-jump instruction"),
        }
    }

    fn patch_foreach_start(&mut self, at: usize, target: usize) {
        match &mut self.instructions[at] {
            Instr::ForeachStart { end, .. } => *end = target,
            _ => panic!("attempted to patch non-foreach-start instruction"),
        }
    }

    fn patch_foreach_next(&mut self, at: usize, target: usize) {
        match &mut self.instructions[at] {
            Instr::ForeachNext { end, .. } => *end = target,
            _ => panic!("attempted to patch non-foreach-next instruction"),
        }
    }

    fn emit(&mut self, instr: Instr) {
        self.instructions.push(instr);
    }

    fn finish(self) -> Program {
        Program {
            constants: self.constants,
            expressions: self.expressions,
            procedures: self.procedures,
            instructions: self.instructions,
        }
    }
}

fn target_name(target: &BoundCommandTarget) -> Option<&str> {
    match target {
        BoundCommandTarget::Native { name, .. } | BoundCommandTarget::Named(name) => Some(name),
        BoundCommandTarget::Dynamic(_) => None,
    }
}

fn literal(word: &BoundWord) -> Option<&str> {
    match word.parts.as_slice() {
        [BoundWordPart::Text(text)] => Some(text),
        _ => None,
    }
}

fn parse_params(source: &str) -> Result<Vec<ProcedureParam>> {
    let raw = parse_list(source)?;
    let mut params = Vec::with_capacity(raw.len());
    for item in raw {
        let nested = parse_list(&item)?;
        match nested.as_slice() {
            [name] => params.push(ProcedureParam {
                name: name.clone(),
                default: None,
            }),
            [name, default] => params.push(ProcedureParam {
                name: name.clone(),
                default: Some(default.clone()),
            }),
            _ => {
                return Err(Error::runtime(format!(
                    "bad procedure parameter spec `{item}`"
                )));
            }
        }
    }
    Ok(params)
}
