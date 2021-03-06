use prolog_parser::ast::*;
use prolog_parser::parser::*;
use prolog_parser::tabled_rc::*;

use crate::prolog::clause_types::*;
use crate::prolog::forms::*;
use crate::prolog::heap_print::*;
use crate::prolog::instructions::*;
use crate::prolog::machine::code_repo::CodeRepo;
use crate::prolog::machine::copier::*;
use crate::prolog::machine::code_walker::*;
use crate::prolog::machine::machine_errors::*;
use crate::prolog::machine::machine_indices::*;
use crate::prolog::machine::machine_state::*;
use crate::prolog::machine::streams::*;
use crate::prolog::machine::toplevel::to_op_decl;
use crate::prolog::ordered_float::OrderedFloat;
use crate::prolog::read::readline;
use crate::prolog::rug::Integer;

use crate::ref_thread_local::RefThreadLocal;

use indexmap::{IndexMap, IndexSet};

use std::io::{stdout, Write};
use std::iter::once;
use std::mem;
use std::rc::Rc;

use crate::crossterm::event::{read, Event, KeyCode, KeyEvent};
use crate::crossterm::terminal::{enable_raw_mode, disable_raw_mode};

pub enum ContinueResult {
    ContinueQuery,
    Conclude,
    Help,
    PrintWithoutMaxDepth,
    PrintWithMaxDepth
}

pub fn next_keypress() -> ContinueResult {
    loop {
        match read() {
            Ok(Event::Key(KeyEvent { code, .. })) => {
                match code {
                    KeyCode::Char('w') => {
                        return ContinueResult::PrintWithoutMaxDepth;
                    }
                    KeyCode::Char('p') => {
                        return ContinueResult::PrintWithMaxDepth;
                    }
                    KeyCode::Char(' ') | KeyCode::Char(';') | KeyCode::Char('n') => {
                        return ContinueResult::ContinueQuery;
                    }
                    KeyCode::Char('.') => {
                        return ContinueResult::Conclude;
                    }
                    KeyCode::Char('h') => {
                        return ContinueResult::Help;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

struct BrentAlgState {
    hare: Addr,
    tortoise: Addr,
    power: usize,
    steps: usize,
}

impl BrentAlgState {
    fn new(hare: Addr) -> Self {
        BrentAlgState {
            hare: hare.clone(),
            tortoise: hare,
            power: 2,
            steps: 0,
        }
    }

    #[inline]
    fn conclude_or_move_tortoise(&mut self) -> Option<CycleSearchResult> {
        if self.tortoise == self.hare {
            return Some(CycleSearchResult::NotList);
        } else if self.steps == self.power {
            self.tortoise = self.hare.clone();
            self.power <<= 1;
        }

        None
    }

    #[inline]
    fn step(&mut self, hare: Addr) -> Option<CycleSearchResult> {
        self.hare = hare;
        self.steps += 1;

        self.conclude_or_move_tortoise()
    }

    fn to_result(self) -> CycleSearchResult {
        match self.hare {
            addr @ Addr::HeapCell(_) | addr @ Addr::StackCell(..) | addr @ Addr::AttrVar(_) => {
                CycleSearchResult::PartialList(self.steps, addr.as_var().unwrap())
            }
            Addr::PStrLocation(h, n) => {
                CycleSearchResult::PStrLocation(self.steps, h, n)
            }
            Addr::Con(Constant::EmptyList) => {
                CycleSearchResult::ProperList(self.steps)
            }
            _ => {
                CycleSearchResult::NotList
            }
        }
    }
}

fn is_builtin_predicate(name: &ClauseName) -> bool {
    let in_builtins = name.owning_module().as_str() == "builtins";
    let hidden_name = name.as_str().starts_with("$");

    in_builtins || hidden_name
}

impl MachineState {
    // a step in Brent's algorithm.
    fn brents_alg_step(&self, brent_st: &mut BrentAlgState) -> Option<CycleSearchResult> {
        match self.store(self.deref(brent_st.hare.clone())) {
            Addr::Con(Constant::EmptyList) => {
                Some(CycleSearchResult::ProperList(brent_st.steps))
            }
            addr @ Addr::HeapCell(_) | addr @ Addr::StackCell(..) | addr @ Addr::AttrVar(_) => {
                Some(CycleSearchResult::PartialList(
                    brent_st.steps,
                    addr.as_var().unwrap(),
                ))
            }
            Addr::Con(Constant::String(n, s)) if !self.flags.double_quotes.is_atom() => {
                if let Some(c) = s.chars().next() {
                    brent_st.step(Addr::Con(Constant::String(n + c.len_utf8(), s)))
                } else {
                    Some(CycleSearchResult::CompleteString(s.len(), s))
                }
            }
            Addr::PStrLocation(h, n) => {
                match &self.heap[h] {
                    HeapCellValue::PartialString(ref pstr) => {
                        let s = pstr.block_as_str();

                        if let Some(c) = s[n ..].chars().next() {
                            brent_st.step(Addr::PStrLocation(h, n + c.len_utf8()))
                        } else {
                            unreachable!()
                        }
                    }
                    _ => {
                        unreachable!()
                    }
                }
            }
            Addr::Lis(l) => {
                brent_st.step(Addr::HeapCell(l + 1))
            }
            _ => {
                Some(CycleSearchResult::NotList)
            }
        }
    }

    pub(super)
    fn detect_cycles_with_max(&self, max_steps: usize, addr: Addr) -> CycleSearchResult {
        let hare = match self.store(self.deref(addr)) {
            Addr::Lis(offset) if max_steps > 0 => {
                Addr::Lis(offset)
            }
            Addr::Lis(offset) => {
                return CycleSearchResult::UntouchedList(offset);
            }
            Addr::PStrLocation(h, n) if max_steps > 0 => {
                Addr::PStrLocation(h, n)
            }
            Addr::PStrLocation(h, _) => {
                return CycleSearchResult::UntouchedList(h);
            }
            Addr::Con(Constant::EmptyList) => {
                return CycleSearchResult::EmptyList;
            }
            Addr::Con(Constant::String(0, ref s))
                if max_steps > 0 && !self.flags.double_quotes.is_atom() => {
                    if max_steps >= s.len() {
                        return CycleSearchResult::CompleteString(s.len(), s.clone());
                    } else {
                        return CycleSearchResult::UntouchedString(max_steps, s.clone());
                    }
                }
            Addr::Con(Constant::String(0, ref s))
                if !self.flags.double_quotes.is_atom() => {
                    return CycleSearchResult::UntouchedString(0, s.clone());
                }
            Addr::Con(Constant::String(n, ref s))
                if max_steps > 0 && !self.flags.double_quotes.is_atom() => {
                    if max_steps >= s.len() - n {
                        let s = Rc::new(String::from(&s[n ..]));
                        return CycleSearchResult::CompleteString(s.len(), s);
                    } else {
                        return CycleSearchResult::UntouchedString(max_steps, s.clone());
                    }
                }
            Addr::Con(Constant::String(n, ref s))
                if !self.flags.double_quotes.is_atom() => {
                    return CycleSearchResult::UntouchedString(n, s.clone());
                }
            _ => {
                return CycleSearchResult::NotList;
            }
        };

        let mut brent_st = BrentAlgState::new(hare);

        loop {
            if brent_st.steps == max_steps {
                return brent_st.to_result();
            }

            if let Some(result) = self.brents_alg_step(&mut brent_st) {
                return result;
            }
        }
    }

    pub(super)
    fn detect_cycles(&self, addr: Addr) -> CycleSearchResult {
        let addr = self.store(self.deref(addr));
        let hare = match addr {
            Addr::Lis(offset) => {
                Addr::Lis(offset)
            }
            Addr::Con(Constant::EmptyList) => {
                return CycleSearchResult::EmptyList;
            }
            Addr::PStrLocation(h, n) => {
                Addr::PStrLocation(h, n)
            }
            Addr::Con(Constant::String(0, ref s)) if !self.flags.double_quotes.is_atom() => {
                return CycleSearchResult::CompleteString(s.len(), s.clone());
            }
            Addr::Con(Constant::String(n, ref s)) if !self.flags.double_quotes.is_atom() => {
                let s = Rc::new(String::from(&s[n ..]));
                return CycleSearchResult::CompleteString(s.len(), s);
            }
            _ => {
                return CycleSearchResult::NotList;
            }
        };

        let mut brent_st = BrentAlgState::new(hare);

        loop {
            if let Some(result) = self.brents_alg_step(&mut brent_st) {
                return result;
            }
        }
    }

    fn finalize_skip_max_list(&mut self, n: usize, addr: Addr) {
        let target_n = self[temp_v!(1)].clone();
        self.unify(Addr::Con(Constant::Integer(Integer::from(n))), target_n);

        if !self.fail {
            let xs = self[temp_v!(4)].clone();
            self.unify(addr, xs);
        }
    }

    pub(super) fn skip_max_list(&mut self) -> CallResult {
        let max_steps = self.store(self.deref(self[temp_v!(2)].clone()));

        match max_steps {
            Addr::Con(Constant::Integer(ref max_steps)) => {
                if max_steps.to_isize().map(|i| i >= -1).unwrap_or(false) {
                    let n = self.store(self.deref(self[temp_v!(1)].clone()));

                    match n {
                        Addr::Con(Constant::Integer(ref n)) if n == &0 => {
                            let xs0 = self[temp_v!(3)].clone();
                            let xs = self[temp_v!(4)].clone();

                            self.unify(xs0, xs);
                        }
                        _ => {
                            let search_result =
                                if let Some(max_steps) = max_steps.to_isize() {
                                    if max_steps == -1 {
                                        self.detect_cycles(self[temp_v!(3)].clone())
                                    } else {
                                        self.detect_cycles_with_max(
                                            max_steps as usize,
                                            self[temp_v!(3)].clone(),
                                        )
                                    }
                                } else {
                                    self.detect_cycles(self[temp_v!(3)].clone())
                                };

                            match search_result {
                                CycleSearchResult::PStrLocation(steps, h, n) => {
                                    self.finalize_skip_max_list(steps, Addr::PStrLocation(h, n));
                                }
                                CycleSearchResult::CompleteString(n, _) => {
                                    self.finalize_skip_max_list(n, Addr::Con(Constant::EmptyList));
                                }
                                CycleSearchResult::UntouchedString(n, s) => {
                                    self.finalize_skip_max_list(0, Addr::Con(Constant::String(n, s)));
                                }
                                CycleSearchResult::UntouchedList(l) => {
                                    self.finalize_skip_max_list(0, Addr::Lis(l))
                                }
                                CycleSearchResult::EmptyList => {
                                    self.finalize_skip_max_list(0, Addr::Con(Constant::EmptyList))
                                }
                                CycleSearchResult::PartialList(n, r) => {
                                    self.finalize_skip_max_list(n, r.as_addr())
                                }
                                CycleSearchResult::ProperList(steps) => {
                                    self.finalize_skip_max_list(steps, Addr::Con(Constant::EmptyList))
                                }
                                CycleSearchResult::NotList => {
                                    let xs0 = self[temp_v!(3)].clone();
                                    self.finalize_skip_max_list(0, xs0);
                                }
                            }
                        }
                    }
                } else {
                    self.fail = true;
                }
            }
            Addr::HeapCell(_) | Addr::StackCell(..) => {
                let stub = MachineError::functor_stub(clause_name!("$skip_max_list"), 4);
                return Err(self.error_form(MachineError::instantiation_error(), stub));
            }
            addr => {
                let stub = MachineError::functor_stub(clause_name!("$skip_max_list"), 4);
                return Err(
                    self.error_form(MachineError::type_error(ValidType::Integer, addr), stub)
                );
            }
        };

        Ok(())
    }

    fn get_stream_or_alias(
        &self,
        addr: Addr,
        indices: &IndexStore,
        caller: &'static str,
    ) -> Result<Stream, MachineStub>
    {
        Ok(match addr {
            Addr::Con(Constant::Atom(atom, op_spec)) => {
                match indices.stream_aliases.get(&atom) {
                    Some(stream) => {
                        stream.clone()
                    }
                    None => {
                        let stub = MachineError::functor_stub(clause_name!(caller), 1);
                        let addr = Addr::Con(Constant::Atom(atom, op_spec));

                        let h = self.heap.h();
                        
                        return Err(self.error_form(
                            MachineError::existence_error(h, ExistenceError::Stream(addr)),
                            stub,
                        ));
                    }
                }
            }
            Addr::Stream(stream) => {
                stream
            }
            _ => {
                let stub = MachineError::functor_stub(clause_name!(caller), 1);
                
                return Err(self.error_form(
                    MachineError::domain_error(DomainError::StreamOrAlias, addr),
                    stub,
                ));
            }
        })     
    }

    fn read_term(&mut self,
                 current_input_stream: &mut Stream,
                 indices: &mut IndexStore)
                 -> CallResult
    {
        match self.read(
            &mut parsing_stream(current_input_stream.clone()),
            indices.atom_tbl.clone(),
            &indices.op_dir,
        ) {
            Ok(term_write_result) => {
                let a1 = self[temp_v!(1)].clone();
                self.unify(Addr::HeapCell(term_write_result.heap_loc), a1);

                if self.fail {
                    return Ok(());
                }

                let mut list_of_var_eqs = vec![];

                for (var, binding) in term_write_result.var_dict.into_iter().rev() {
                    let var_atom = clause_name!(var.to_string(), indices.atom_tbl);
                    let var_atom = Constant::Atom(var_atom, None);

                    let h = self.heap.h();
                    let spec = fetch_atom_op_spec(clause_name!("="), None, &indices.op_dir);

                    self.heap.push(HeapCellValue::NamedStr(2, clause_name!("="), spec));
                    self.heap.push(HeapCellValue::Addr(Addr::Con(var_atom)));
                    self.heap.push(HeapCellValue::Addr(binding));

                    list_of_var_eqs.push(Addr::Str(h));
                }

                let a2 = self[temp_v!(2)].clone();
                let list_offset =
                    Addr::HeapCell(self.heap.to_list(list_of_var_eqs.into_iter()));

                Ok(self.unify(list_offset, a2))
            }
            Err(err) => {
                if let ParserError::UnexpectedEOF = err {
                    std::process::exit(0);
                }

                // reset the input stream after an input failure.
                *current_input_stream = readline::input_stream();

                let h = self.heap.h();
                let syntax_error = MachineError::syntax_error(h, err);
                let stub = MachineError::functor_stub(clause_name!("read_term"), 2);

                Err(self.error_form(syntax_error, stub))
            }
        }
    }

    #[inline]
    fn install_new_block(&mut self, r: RegType) -> usize {
        self.block = self.b;

        let c = Constant::Usize(self.block);
        let addr = self[r].clone();

        self.write_constant_to_var(addr, c);
        self.block
    }

    fn copy_findall_solution(&mut self, lh_offset: usize, copy_target: Addr) -> usize {
        let threshold = self.lifted_heap.h() - lh_offset;

        let mut copy_ball_term = CopyBallTerm::new(
            &mut self.stack,
            &mut self.heap,
            &mut self.lifted_heap,
        );

        copy_ball_term.push(HeapCellValue::Addr(Addr::Lis(threshold + 1)));
        copy_ball_term.push(HeapCellValue::Addr(Addr::HeapCell(threshold + 3)));
        copy_ball_term.push(HeapCellValue::Addr(Addr::HeapCell(threshold + 2)));

        copy_term(copy_ball_term, copy_target, AttrVarPolicy::DeepCopy);

        threshold + lh_offset + 2
    }

    fn repl_redirect(&mut self, repl_code_ptr: REPLCodePtr) -> CallResult {
        let p = if self.last_call {
            self.cp
        } else {
            self.p.local() + 1
        };

        Ok(self.p = CodePtr::REPL(repl_code_ptr, p))
    }

    fn truncate_if_no_lifted_heap_diff<AddrConstr>(&mut self, addr_constr: AddrConstr)
    where
        AddrConstr: Fn(usize) -> Addr,
    {
        match self.store(self.deref(self[temp_v!(1)].clone())) {
            Addr::Con(Constant::Usize(lh_offset)) => {
                if lh_offset >= self.lifted_heap.h() {
                    self.lifted_heap.truncate(lh_offset);
                } else {
                    let threshold = self.lifted_heap.h() - lh_offset;
                    self.lifted_heap.push(HeapCellValue::Addr(addr_constr(threshold)));
                }
            }
            _ => self.fail = true,
        }
    }

    fn get_next_db_ref(&mut self, indices: &IndexStore, db_ref: &DBRef) {
        match db_ref {
            &DBRef::NamedPred(ref name, arity, _) => {
                let key = (name.clone(), arity);
                let mut iter = indices.code_dir.range(key..).skip(1);

                while let Some(((name, arity), idx)) = iter.next() {
                    if idx.is_undefined() {
                        self.fail = true;
                        return;
                    }

                    if is_builtin_predicate(&name) {
                        continue;
                    }

                    let a2 = self[temp_v!(2)].clone();

                    if let Some(r) = a2.as_var() {
                        let spec =
                            get_clause_spec(name.clone(), *arity, composite_op!(&indices.op_dir));
                        self.bind(r, Addr::DBRef(DBRef::NamedPred(name.clone(), *arity, spec)));
                        return;
                    }
                }

                self.fail = true;
            }
            &DBRef::Op(_, spec, ref name, ref op_dir, _) => {
                let fixity = match spec {
                    XF | YF => Fixity::Post,
                    FX | FY => Fixity::Pre,
                    _ => Fixity::In,
                };

                let key = OrderedOpDirKey(name.clone(), fixity);

                match op_dir.range(key..).skip(1).next() {
                    Some((OrderedOpDirKey(name, _), (priority, spec))) => {
                        let a2 = self[temp_v!(2)].clone();

                        if let Some(r) = a2.as_var() {
                            self.bind(
                                r,
                                Addr::DBRef(DBRef::Op(
                                    *priority,
                                    *spec,
                                    name.clone(),
                                    op_dir.clone(),
                                    SharedOpDesc::new(*priority, *spec),
                                )),
                            );
                        } else {
                            self.fail = true;
                        }
                    }
                    None => self.fail = true,
                }
            }
        }
    }

    fn int_to_char_code(
        &mut self,
        n: &Integer,
        stub: &'static str,
        arity: usize,
    ) -> Result<u32, MachineStub> {
        if let Some(c) = n.to_u32() {
            Ok(c)
        } else {
            let stub = MachineError::functor_stub(clause_name!(stub), arity);
            let err = MachineError::representation_error(RepFlag::CharacterCode);
            let err = self.error_form(err, stub);

            Err(err)
        }
    }

    fn parse_number_from_string(
        &mut self,
        mut string: String,
        indices: &IndexStore,
        stub: MachineStub,
    ) -> CallResult {
        let nx = self[temp_v!(2)].clone();

        if let Some(c) = string.chars().last() {
            if layout_char!(c) {
                let (line_num, col_num) = string.chars().fold((0, 0), |(line_num, col_num), c| {
                    if new_line_char!(c) {
                        (1 + line_num, 0)
                    } else {
                        (line_num, col_num + 1)
                    }
                });
                let err = ParserError::UnexpectedChar(c, line_num, col_num);

                let h = self.heap.h();
                let err = MachineError::syntax_error(h, err);

                return Err(self.error_form(err, stub));
            }
        }

        string.push('.');

        let mut stream = parsing_stream(std::io::Cursor::new(string));
        let mut parser = Parser::new(&mut stream, indices.atom_tbl.clone(), self.machine_flags());

        match parser.read_term(composite_op!(&indices.op_dir)) {
            Err(err) => {
                let h = self.heap.h();
                let err = MachineError::syntax_error(h, err);

                return Err(self.error_form(err, stub));
            }
            Ok(Term::Constant(_, Constant::Rational(n))) => {
                self.unify(nx, Addr::Con(Constant::Rational(n)))
            }
            Ok(Term::Constant(_, Constant::Float(n))) => {
                self.unify(nx, Addr::Con(Constant::Float(n)))
            }
            Ok(Term::Constant(_, Constant::Integer(n))) => {
                self.unify(nx, Addr::Con(Constant::Integer(n)))
            }
            Ok(Term::Constant(_, Constant::CharCode(c))) => {
                self.unify(nx, Addr::Con(Constant::CharCode(c)))
            }
            _ => {
                let err = ParserError::ParseBigInt(0, 0);

                let h = self.heap.h();
                let err = MachineError::syntax_error(h, err);

                return Err(self.error_form(err, stub));
            }
        }

        Ok(())
    }

    fn fetch_attribute_goals(&mut self, mut attr_goals: Vec<Addr>) {
        attr_goals.sort_unstable_by(|a1, a2| self.compare_term_test(a1, a2));
        self.term_dedup(&mut attr_goals);

        let attr_goals = Addr::HeapCell(self.heap.to_list(attr_goals.into_iter()));
        let target = self[temp_v!(1)].clone();

        self.unify(attr_goals, target);
    }

    fn call_continuation_chunk(&mut self, chunk: Addr, return_p: LocalCodePtr) -> LocalCodePtr {
        let chunk = self.store(self.deref(chunk));

        match chunk {
            Addr::Str(s) => {
                match &self.heap[s] {
                    HeapCellValue::NamedStr(arity, ..) => {
                        let num_cells = arity - 1;
                        let p_functor = self.heap[s+1].as_addr(s+1);

                        let cp = self.heap.to_local_code_ptr(&p_functor).unwrap();
                        let prev_e = self.e;

                        let e = self.stack.allocate_and_frame(num_cells);
                        let and_frame = self.stack.index_and_frame_mut(e);

                        and_frame.prelude.e  = prev_e;
                        and_frame.prelude.cp = return_p;

                        self.p = CodePtr::Local(cp + 1);

                        // adjust cut point to occur after call_continuation.
                        if num_cells > 0 {
                            if let Addr::Con(Constant::CutPoint(_)) = self.heap[s+2].as_addr(s+2) {
                                and_frame[1] = Addr::Con(Constant::CutPoint(self.b));
                            } else {
                                and_frame[1] = self.heap[s+2].as_addr(s+2);
                            }
                        }

                        for index in s+3 .. s+2+num_cells {
                            and_frame[index - (s+1)] = self.heap[index].as_addr(index);
                        }

                        self.e = e;

                        self.p.local()
                    }
                    _ => unreachable!()
                }
            }
            _ => unreachable!()
        }
    }

    pub(super)
    fn system_call(
        &mut self,
        ct: &SystemClauseType,
        code_repo: &CodeRepo,
        indices: &mut IndexStore,
        call_policy: &mut Box<dyn CallPolicy>,
        cut_policy: &mut Box<dyn CutPolicy>,
        current_input_stream: &mut Stream,
        current_output_stream: &mut Stream,
    ) -> CallResult {
        match ct {
            &SystemClauseType::AbolishClause => {
                let p = self.cp;
                let trans_type = DynamicTransactionType::Abolish;

                self.p = CodePtr::DynamicTransaction(trans_type, p);
                return Ok(());
            }
            &SystemClauseType::AbolishModuleClause => {
                let p = self.cp;
                let trans_type = DynamicTransactionType::ModuleAbolish;

                self.p = CodePtr::DynamicTransaction(trans_type, p);
                return Ok(());
            }
            &SystemClauseType::BindFromRegister => {
                let reg = self.store(self.deref(self[temp_v!(2)].clone()));
                let n = match reg {
                    Addr::Con(Constant::Integer(n)) => n.to_usize(),
                    _ => unreachable!()
                };

                if let Some(n) = n {
                    if n <= MAX_ARITY {
                        let target = self[temp_v!(n)].clone();
                        let addr   = self[temp_v!(1)].clone();

                        self.unify(addr, target);
                        return return_from_clause!(self.last_call, self);
                    }
                }

                self.fail = true;
            }
            &SystemClauseType::AssertDynamicPredicateToFront => {
                let p = self.cp;
                let trans_type = DynamicTransactionType::Assert(DynamicAssertPlace::Front);

                self.p = CodePtr::DynamicTransaction(trans_type, p);
                return Ok(());
            }
            &SystemClauseType::AssertDynamicPredicateToBack => {
                let p = self.cp;
                let trans_type = DynamicTransactionType::Assert(DynamicAssertPlace::Back);

                self.p = CodePtr::DynamicTransaction(trans_type, p);
                return Ok(());
            }
            &SystemClauseType::CurrentInput => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));
                let stream = current_input_stream.clone();

                match addr {
                    addr if addr.is_ref() => {
                        self.unify(Addr::Stream(stream), addr);
                    }
                    Addr::Stream(other_stream) => {
                        self.fail = stream != other_stream;
                    }
                    addr => {
                        let stub = MachineError::functor_stub(
                            clause_name!("current_input"),
                            1,
                        );

                        let err = MachineError::domain_error(
                            DomainError::Stream,
                            addr,
                        );

                        return Err(self.error_form(err, stub));
                    }
                }
            }
            &SystemClauseType::CurrentOutput => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));
                let stream = current_output_stream.clone();

                match addr {
                    addr if addr.is_ref() => {
                        self.unify(Addr::Stream(stream), addr);
                    }
                    Addr::Stream(other_stream) => {
                        self.fail = stream != other_stream;
                    }
                    addr => {
                        let stub = MachineError::functor_stub(
                            clause_name!("current_input"),
                            1,
                        );

                        let err = MachineError::domain_error(
                            DomainError::Stream,
                            addr,
                        );

                        return Err(self.error_form(err, stub));
                    }
                }
            }
            &SystemClauseType::AtEndOfExpansion => {
                if self.cp == LocalCodePtr::TopLevel(0, 0) {
                    self.at_end_of_expansion = true;
                }
            }
            &SystemClauseType::AtomChars => {
                let a1 = self[temp_v!(1)].clone();

                match self.store(self.deref(a1)) {
                    Addr::Con(Constant::Char(c)) => {
                        let iter = once(Addr::Con(Constant::Char(c)));
                        let list_of_chars = Addr::HeapCell(self.heap.to_list(iter));

                        let a2 = self[temp_v!(2)].clone();
                        self.unify(a2, list_of_chars);
                    }
                    Addr::Con(Constant::Atom(name, _)) => {
                        let iter = name.as_str().chars().map(|c| Addr::Con(Constant::Char(c)));
                        let list_of_chars = Addr::HeapCell(self.heap.to_list(iter));

                        let a2 = self[temp_v!(2)].clone();

                        match self.store(self.deref(a2)) {
                            Addr::Con(Constant::String(..))
                                if !self.flags.double_quotes.is_chars() => {
                                    self.fail = true;
                                }
                            a2 => {
                                self.unify(a2, list_of_chars);
                            }
                        }
                    }
                    Addr::Con(Constant::EmptyList) => {
                        let a2 = self[temp_v!(2)].clone();
                        let chars = vec![
                            Addr::Con(Constant::Char('[')),
                            Addr::Con(Constant::Char(']')),
                        ];

                        let list_of_chars = Addr::HeapCell(self.heap.to_list(chars.into_iter()));

                        self.unify(a2, list_of_chars);
                    }
                    ref addr if addr.is_ref() => {
                        let stub = MachineError::functor_stub(clause_name!("atom_chars"), 2);

                        match self.try_from_list(temp_v!(2), stub.clone()) {
                            Err(e) => return Err(e),
                            Ok(addrs) => match self.try_char_list(addrs) {
                                Ok(string) => {
                                    let chars = clause_name!(string, indices.atom_tbl);
                                    self.unify(
                                        addr.clone(),
                                        Addr::Con(Constant::Atom(chars, None)),
                                    );
                                }
                                Err(err) => return Err(self.error_form(err, stub)),
                            },
                        }
                    }
                    _ => unreachable!(),
                };
            }
            &SystemClauseType::AtomCodes => {
                let a1 = self[temp_v!(1)].clone();

                match self.store(self.deref(a1)) {
                    Addr::Con(Constant::Char(c)) => {
                        let iter = once(Addr::Con(Constant::CharCode(c as u32)));
                        let list_of_codes = Addr::HeapCell(self.heap.to_list(iter));

                        let a2 = self[temp_v!(2)].clone();
                        self.unify(a2, list_of_codes);
                    }
                    Addr::Con(Constant::Atom(name, _)) => {
                        let a2 = self[temp_v!(2)].clone();

                        match self.store(self.deref(a2)) {
                            a2 @ Addr::Con(Constant::String(..)) => {
                                if !self.flags.double_quotes.is_codes() {
                                    self.fail = true;
                                } else {
                                    let iter = name
                                        .as_str()
                                        .chars()
                                        .map(|c| Addr::Con(Constant::Char(c)));

                                    let list_of_codes = Addr::HeapCell(self.heap.to_list(iter));

                                    self.unify(a2, list_of_codes);
                                }
                            }
                            a2 => {
                                let iter = name
                                    .as_str()
                                    .chars()
                                    .map(|c| Addr::Con(Constant::CharCode(c as u32)));

                                let list_of_codes = Addr::HeapCell(self.heap.to_list(iter));

                                self.unify(a2, list_of_codes);
                            }
                        }
                    }
                    Addr::Con(Constant::EmptyList) => {
                        let chars = vec![
                            Addr::Con(Constant::CharCode('[' as u32)),
                            Addr::Con(Constant::CharCode(']' as u32)),
                        ];

                        let list_of_codes = Addr::HeapCell(self.heap.to_list(chars.into_iter()));
                        let a2 = self[temp_v!(2)].clone();

                        self.unify(a2, list_of_codes);
                    }
                    ref addr if addr.is_ref() => {
                        let stub = MachineError::functor_stub(clause_name!("atom_codes"), 2);

                        match self.try_from_list(temp_v!(2), stub.clone()) {
                            Err(e) => return Err(e),
                            Ok(addrs) => {
                                let mut chars = String::new();

                                for addr in addrs {
                                    match addr {
                                        Addr::Con(Constant::Integer(n)) => {
                                            let c = self.int_to_char_code(&n, "atom_codes", 2)?;
                                            chars.push(std::char::from_u32(c).unwrap());
                                        }
                                        Addr::Con(Constant::CharCode(c)) => {
                                            chars.push(std::char::from_u32(c).unwrap());
                                        }
                                        _ => {
                                            let err = MachineError::type_error(
                                                ValidType::Integer,
                                                addr.clone(),
                                            );
                                            return Err(self.error_form(err, stub));
                                        }
                                    }
                                }

                                let chars = clause_name!(chars, indices.atom_tbl);
                                self.unify(addr.clone(), Addr::Con(Constant::Atom(chars, None)));
                            }
                        }
                    }
                    _ => unreachable!(),
                };
            }
            &SystemClauseType::AtomLength => {
                let a1 = self[temp_v!(1)].clone();

                let atom = match self.store(self.deref(a1)) {
                    Addr::Con(Constant::Atom(name, _)) => name,
                    Addr::Con(Constant::EmptyList) => clause_name!("[]"),
                    Addr::Con(Constant::Char(c)) => clause_name!(c.to_string(), indices.atom_tbl),
                    _ => unreachable!(),
                };

                let len = Integer::from(atom.as_str().chars().count());
                let a2 = self[temp_v!(2)].clone();

                self.unify(a2, Addr::Con(Constant::Integer(len)));
            }
            &SystemClauseType::CallAttributeGoals => {
                let p = self.attr_var_init.project_attrs_loc;

                if self.last_call {
                    self.execute_at_index(2, dir_entry!(p));
                } else {
                    self.call_at_index(2, dir_entry!(p));
                }

                return Ok(());
            }
            &SystemClauseType::CallContinuation => {
                let stub = MachineError::functor_stub(clause_name!("call_continuation"), 1);

                match self.try_from_list(temp_v!(1), stub) {
                    Err(e) => return Err(e),
                    Ok(cont_chunks) => {
                        let mut return_p = if self.last_call {
                            self.cp
                        } else {
                            self.p.local() + 1
                        };

                        self.p = CodePtr::Local(return_p);

                        for chunk in cont_chunks.into_iter().rev() {
                            return_p = self.call_continuation_chunk(chunk, return_p);
                        }
                    }
                }

                return Ok(());
            }
            &SystemClauseType::CharsToNumber => {
                let stub = MachineError::functor_stub(clause_name!("number_chars"), 2);

                match self.try_from_list(temp_v!(1), stub.clone()) {
                    Err(e) => return Err(e),
                    Ok(addrs) => match self.try_char_list(addrs) {
                        Ok(string) => self.parse_number_from_string(string, indices, stub)?,
                        Err(err) => return Err(self.error_form(err, stub)),
                    },
                }
            }
            &SystemClauseType::CreatePartialString => {
                let atom = match self.store(self.deref(self[temp_v!(1)].clone())) {
                    Addr::Con(Constant::Atom(atom, _)) => {
                        atom
                    }
                    _ => {
                        unreachable!()
                    }
                };

                let h = self.heap.h();
                let pstr =
                    match self.heap.allocate_pstr(atom.as_str()) {
                        Some(pstr_addr) => {
                            pstr_addr
                        }
                        None => {
                            let stub = MachineError::functor_stub(clause_name!("partial_string"), 3);
                            let err = MachineError::representation_error(
                                RepFlag::Character,
                            );

                            return Err(self.error_form(err, stub));
                        }
                    };

                let pstr_tail = self.heap[h + 1].as_addr(h + 1);
                
                self.unify(self[temp_v!(2)].clone(), pstr);

                if !self.fail {
                    self.unify(self[temp_v!(3)].clone(), pstr_tail);
                }
            }
            &SystemClauseType::IsPartialString => {
                let pstr = self.store(self.deref(self[temp_v!(1)].clone()));

                match pstr {
                    Addr::PStrLocation(..) => {
                    }
                    _ => {
                        self.fail = true;
                    }
                }
            }
            &SystemClauseType::PartialStringTail => {
                let pstr = self.store(self.deref(self[temp_v!(1)].clone()));

                match pstr {
                    Addr::PStrLocation(h, _) => {
                        let tail = self.heap[h + 1].as_addr(h + 1);
                        let target = self[temp_v!(2)].clone();
                        
                        self.unify(tail, target);
                    }
                    _ => {
                        unreachable!()
                    }
                }
            }
            &SystemClauseType::NumberToChars => {
                let n = self[temp_v!(1)].clone();
                let chs = self[temp_v!(2)].clone();

                let string = match self.store(self.deref(n)) {
                    Addr::Con(Constant::Float(OrderedFloat(n))) => format!("{0:<20?}", n),
                    Addr::Con(Constant::Integer(n)) => n.to_string(),
                    _ => unreachable!(),
                };

                let chars = string.trim().chars().map(|c| Addr::Con(Constant::Char(c)));
                let char_list = Addr::HeapCell(self.heap.to_list(chars));

                self.unify(char_list, chs);
            }
            &SystemClauseType::NumberToCodes => {
                let n = self[temp_v!(1)].clone();
                let chs = self[temp_v!(2)].clone();

                let string = match self.store(self.deref(n)) {
                    Addr::Con(Constant::Float(OrderedFloat(n))) => format!("{0:<20?}", n),
                    Addr::Con(Constant::Integer(n)) => n.to_string(),
                    _ => unreachable!(),
                };

                let codes = string
                    .trim()
                    .chars()
                    .map(|c| Addr::Con(Constant::CharCode(c as u32)));
                let codes_list = Addr::HeapCell(self.heap.to_list(codes));

                self.unify(codes_list, chs);
            }
            &SystemClauseType::CodesToNumber => {
                let stub = MachineError::functor_stub(clause_name!("number_codes"), 2);

                match self.try_from_list(temp_v!(1), stub.clone()) {
                    Err(e) => return Err(e),
                    Ok(addrs) => match self.try_char_list(addrs) {
                        Ok(chars) => {
                            self.parse_number_from_string(chars, indices, stub)?
                        }
                        Err(err) => return Err(self.error_form(err, stub)),
                    },
                }
            }
            &SystemClauseType::ModuleAssertDynamicPredicateToFront => {
                let p = self.cp;
                let trans_type = DynamicTransactionType::ModuleAssert(DynamicAssertPlace::Front);

                self.p = CodePtr::DynamicTransaction(trans_type, p);
                return Ok(());
            }
            &SystemClauseType::ModuleAssertDynamicPredicateToBack => {
                let p = self.cp;
                let trans_type = DynamicTransactionType::ModuleAssert(DynamicAssertPlace::Back);

                self.p = CodePtr::DynamicTransaction(trans_type, p);
                return Ok(());
            }
            &SystemClauseType::LiftedHeapLength => {
                let a1 = self[temp_v!(1)].clone();
                let lh_len = Addr::Con(Constant::Usize(self.lifted_heap.h()));

                self.unify(a1, lh_len);
            }
            &SystemClauseType::CharCode => {
                let a1 = self[temp_v!(1)].clone();

                match self.store(self.deref(a1)) {
                    Addr::Con(Constant::Atom(name, _)) => {
                        let c = name.as_str().chars().next().unwrap();
                        let a2 = self[temp_v!(2)].clone();
                        let c = Integer::from(c as u32);

                        self.unify(Addr::Con(Constant::Integer(c)), a2);
                    }
                    Addr::Con(Constant::Char(c)) => {
                        let a2 = self[temp_v!(2)].clone();
                        let c = Integer::from(c as u32);

                        self.unify(Addr::Con(Constant::Integer(c)), a2);
                    }
                    addr if addr.is_ref() => {
                        let a2 = self[temp_v!(2)].clone();

                        match self.store(self.deref(a2)) {
                            Addr::Con(Constant::CharCode(code)) => {
                                if let Some(c) = std::char::from_u32(code) {
                                    self.unify(Addr::Con(Constant::Char(c)), addr);
                                } else {
                                    self.fail = true;
                                }
                            }
                            Addr::Con(Constant::Integer(n)) => {
                                let c = self.int_to_char_code(&n, "char_code", 2)?;

                                if let Some(c) = std::char::from_u32(c) {
                                    self.unify(Addr::Con(Constant::Char(c)), addr);
                                } else {
                                    self.fail = true;
                                }
                            }
                            _ => self.fail = true,
                        };
                    }
                    _ => unreachable!(),
                };
            }
            &SystemClauseType::CheckCutPoint => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));

                match addr {
                    Addr::Con(Constant::Usize(old_b)) | Addr::Con(Constant::CutPoint(old_b)) => {
                        let prev_b = self.stack.index_or_frame(self.b).prelude.b;
                        let prev_b = self.stack.index_or_frame(prev_b).prelude.b;

                        if prev_b > old_b {
                            self.fail = true;
                        }
                    }
                    _ => self.fail = true,
                };
            }
            &SystemClauseType::CopyTermWithoutAttrVars => {
                self.copy_term(AttrVarPolicy::StripAttributes);
            }
            &SystemClauseType::FetchGlobalVar => {
                let key = self[temp_v!(1)].clone();

                let key = match self.store(self.deref(key)) {
                    Addr::Con(Constant::Atom(atom, _)) => atom,
                    _ => unreachable!(),
                };

                let addr = self[temp_v!(2)].clone();

                match indices.global_variables.get_mut(&key) {
                    Some((ref mut ball, None)) => {
                        let h = self.heap.h();
                        let stub = ball.copy_and_align(h);

                        self.heap.extend(stub.into_iter());
                        self.unify(addr, Addr::HeapCell(h));
                    }
                    Some((_, Some(h))) => {
                        self.unify(addr, Addr::HeapCell(*h))
                    }
                    None => self.fail = true,
                };
            }
            &SystemClauseType::FetchGlobalVarWithOffset => {
                let key = self[temp_v!(1)].clone();

                let key = match self.store(self.deref(key)) {
                    Addr::Con(Constant::Atom(atom, _)) => atom,
                    _ => unreachable!(),
                };

                let addr = self[temp_v!(2)].clone();

                match indices.global_variables.get_mut(&key) {
                    Some((ref mut ball, ref mut offset @ None)) => {
                        let h = self.heap.h();
                        let stub = ball.copy_and_align(h);

                        self.heap.extend(stub.into_iter());
                        self.unify(addr, Addr::HeapCell(h));

                        *offset = Some(h);
                    }
                    Some((_, Some(h))) => {
                        let offset = self[temp_v!(3)].clone();

                        self.unify(offset, Addr::Con(Constant::Usize(*h)));

                        if !self.fail {
                            self.unify(addr, Addr::HeapCell(*h));
                        }
                    }
                    None => {
                        self.fail = true
                    }
                };
            }
            &SystemClauseType::GetChar => {
                let mut iter = parsing_stream(current_input_stream.clone());
                let result = iter.next();

                let a1 = self[temp_v!(1)].clone();

                match result {
                    Some(Ok(b)) => self.unify(Addr::Con(Constant::Char(b as char)), a1),
                    Some(Err(_)) => {
                        let end_of_file = clause_name!("end_of_file");
                        self.unify(a1, Addr::Con(Constant::Atom(end_of_file, None)));
                    }
                    None => {
                        let stub = MachineError::functor_stub(clause_name!("get_char"), 1);
                        let err = MachineError::representation_error(RepFlag::Character);
                        let err = self.error_form(err, stub);

                        return Err(err);
                    }
                }
            }
            &SystemClauseType::GetModuleClause => {
                let module = self[temp_v!(3)].clone();
                let head = self[temp_v!(1)].clone();

                let module = match self.store(self.deref(module)) {
                    Addr::Con(Constant::Atom(module, _)) => module,
                    _ => {
                        self.fail = true;
                        return Ok(());
                    }
                };

                let subsection = match self.store(self.deref(head)) {
                    Addr::Str(s) => match self.heap[s].clone() {
                        HeapCellValue::NamedStr(arity, name, ..) => {
                            indices.get_clause_subsection(module, name, arity)
                        }
                        _ => unreachable!(),
                    },
                    Addr::Con(Constant::Atom(name, _)) => {
                        indices.get_clause_subsection(module, name, 0)
                    }
                    _ => unreachable!(),
                };

                match subsection {
                    Some(dynamic_predicate_info) => {
                        self.execute_at_index(
                            2,
                            dir_entry!(dynamic_predicate_info.clauses_subsection_p)
                        );
                        return Ok(());
                    }
                    None => self.fail = true,
                }
            }
            &SystemClauseType::ModuleHeadIsDynamic => {
                let module = self[temp_v!(2)].clone();
                let head = self[temp_v!(1)].clone();

                let module = match self.store(self.deref(module)) {
                    Addr::Con(Constant::Atom(module, _)) => module,
                    _ => {
                        self.fail = true;
                        return Ok(());
                    }
                };

                self.fail = !match self.store(self.deref(head)) {
                    Addr::Str(s) => match self.heap[s].clone() {
                        HeapCellValue::NamedStr(arity, name, ..) => {
                            indices.get_clause_subsection(module, name, arity).is_some()
                        }
                        _ => unreachable!(),
                    },
                    Addr::Con(Constant::Atom(name, _)) => {
                        indices.get_clause_subsection(module, name, 0).is_some()
                    }
                    _ => unreachable!(),
                };
            }
            &SystemClauseType::HeadIsDynamic => {
                let head = self[temp_v!(1)].clone();

                self.fail = !match self.store(self.deref(head)) {
                    Addr::Str(s) => match self.heap[s].clone() {
                        HeapCellValue::NamedStr(arity, name, ..) => indices
                            .get_clause_subsection(name.owning_module(), name, arity)
                            .is_some(),
                        _ => unreachable!(),
                    },
                    Addr::Con(Constant::Atom(name, _)) => indices
                        .get_clause_subsection(name.owning_module(), name, 0)
                        .is_some(),
                    _ => unreachable!(),
                };
            }
            &SystemClauseType::CopyToLiftedHeap => {
                match self.store(self.deref(self[temp_v!(1)].clone())) {
                    Addr::Con(Constant::Usize(lh_offset)) => {
                        let copy_target = self[temp_v!(2)].clone();

                        let old_threshold = self.copy_findall_solution(lh_offset, copy_target);
                        let new_threshold = self.lifted_heap.h() - lh_offset;

                        self.lifted_heap[old_threshold] =
                            HeapCellValue::Addr(Addr::HeapCell(new_threshold));

                        for addr in self.lifted_heap.iter_mut_from(old_threshold + 1) {
                            match addr {
                                HeapCellValue::Addr(ref mut addr) => {
                                    *addr -= self.heap.h() + lh_offset
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => self.fail = true,
                }
            }
            &SystemClauseType::DeleteAttribute => {
                let ls0 = self.store(self.deref(self[temp_v!(1)].clone()));

                if let Addr::Lis(l1) = ls0 {
                    if let Addr::Lis(l2) = self.store(self.deref(Addr::HeapCell(l1 + 1))) {
                        let old_addr = self.heap[l1 + 1].as_addr(l1 + 1);

                        let tail = self.store(self.deref(Addr::HeapCell(l2 + 1)));
                        let tail = if tail.is_ref() {
                            Addr::HeapCell(l1 + 1)
                        } else {
                            tail
                        };

                        let trail_ref = match old_addr {
                            Addr::HeapCell(h) => TrailRef::AttrVarHeapLink(h),
                            Addr::Lis(l) => TrailRef::AttrVarListLink(l1 + 1, l),
                            _ => unreachable!()
                        };

                        self.heap[l1 + 1] = HeapCellValue::Addr(tail);
                        self.trail(trail_ref);
                    }
                }
            }
            &SystemClauseType::DeleteHeadAttribute => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));

                match addr {
                    Addr::AttrVar(h) => {
                        let addr = self.heap[h + 1].as_addr(h + 1).clone();
                        let addr = self.store(self.deref(addr));

                        match addr {
                            Addr::Lis(l) => {
                                let tail = self.store(self.deref(Addr::HeapCell(l + 1)));
                                let tail = if tail.is_ref() {
                                    Addr::HeapCell(h + 1)
                                } else {
                                    tail
                                };

                                self.heap[h + 1] = HeapCellValue::Addr(tail);
                                self.trail(TrailRef::AttrVarListLink(h + 1, l));
                            }
                            _ => unreachable!(),
                        }
                    }
                    _ => unreachable!(),
                }
            }
            &SystemClauseType::DynamicModuleResolution(narity) => {
                let module_name = self.store(self.deref(self[temp_v!(1 + narity)].clone()));

                if let Addr::Con(Constant::Atom(module_name, _)) = module_name {
                    match self.store(self.deref(self[temp_v!(2 + narity)].clone())) {
                        Addr::Str(a) => {
                            if let HeapCellValue::NamedStr(arity, name, _) = self.heap[a].clone() {
                                for i in (arity + 1 .. arity + narity + 1).rev() {
                                    self.registers[i] = self.registers[i - arity].clone();
                                }

                                for i in 1 .. arity + 1 {
                                    self.registers[i] = self.heap[a + i].as_addr(a + i);
                                }

                                return self.module_lookup(
                                    indices,
                                    (name, arity + narity),
                                    module_name,
                                    true,
                                );
                            }
                        }
                        Addr::Con(Constant::Atom(name, _)) => {
                            return self.module_lookup(indices, (name, narity), module_name, true)
                        }
                        addr => {
                            let stub = MachineError::functor_stub(clause_name!("(:)"), 2);

                            let type_error = MachineError::type_error(ValidType::Callable, addr);
                            let type_error = self.error_form(type_error, stub);

                            return Err(type_error);
                        }
                    }
                };
            }
            &SystemClauseType::EnqueueAttributeGoal => {
                let addr = self[temp_v!(1)].clone();
                self.attr_var_init.attribute_goals.push(addr);
            }
            &SystemClauseType::EnqueueAttributedVar => {
                let addr = self[temp_v!(1)].clone();

                match self.store(self.deref(addr)) {
                    Addr::AttrVar(h) => self.attr_var_init.attr_var_queue.push(h),
                    _ => {}
                }
            }
            &SystemClauseType::ExpandGoal => {
                self.p = CodePtr::Local(LocalCodePtr::UserGoalExpansion(0));
                return Ok(());
            }
            &SystemClauseType::ExpandTerm => {
                self.p = CodePtr::Local(LocalCodePtr::UserTermExpansion(0));
                return Ok(());
            }
            &SystemClauseType::GetNextDBRef => {
                let a1 = self[temp_v!(1)].clone();

                match self.store(self.deref(a1)) {
                    addr @ Addr::HeapCell(_)
                    | addr @ Addr::StackCell(..)
                    | addr @ Addr::AttrVar(_) => {
                        let mut iter = indices.code_dir.iter();

                        while let Some(((name, arity), _)) = iter.next() {
                            if is_builtin_predicate(&name) {
                                continue;
                            }

                            let spec = get_clause_spec(
                                name.clone(),
                                *arity,
                                composite_op!(&indices.op_dir),
                            );
                            let db_ref = DBRef::NamedPred(name.clone(), *arity, spec);
                            let r = addr.as_var().unwrap();

                            self.bind(r, Addr::DBRef(db_ref));
                            return return_from_clause!(self.last_call, self);
                        }

                        self.fail = true;
                    }
                    Addr::DBRef(DBRef::Op(..)) => self.fail = true,
                    Addr::DBRef(ref db_ref) => self.get_next_db_ref(&indices, db_ref),
                    _ => {
                        self.fail = true;
                    }
                };
            }
            &SystemClauseType::GetNextOpDBRef => {
                let a1 = self[temp_v!(1)].clone();

                match self.store(self.deref(a1)) {
                    addr @ Addr::HeapCell(_)
                    | addr @ Addr::StackCell(..)
                    | addr @ Addr::AttrVar(_) => {
                        let mut unossified_op_dir = OssifiedOpDir::new();

                        unossified_op_dir.extend(indices.op_dir.iter().filter_map(
                            |(key, op_dir_val)| {
                                let (name, fixity) = key.clone();

                                let prec = op_dir_val.shared_op_desc().prec();

                                if prec == 0 {
                                    return None;
                                }

                                let assoc = op_dir_val.shared_op_desc().assoc();

                                Some((OrderedOpDirKey(name, fixity), (prec, assoc)))
                            },
                        ));

                        let ossified_op_dir = Rc::new(unossified_op_dir);

                        match ossified_op_dir.iter().next() {
                            Some((OrderedOpDirKey(name, _), (priority, spec))) => {
                                let db_ref = DBRef::Op(
                                    *priority,
                                    *spec,
                                    name.clone(),
                                    ossified_op_dir.clone(),
                                    SharedOpDesc::new(*priority, *spec),
                                );
                                let r = addr.as_var().unwrap();

                                self.bind(r, Addr::DBRef(db_ref));
                            }
                            None => {
                                self.fail = true;
                                return Ok(());
                            }
                        }
                    }
                    Addr::DBRef(DBRef::NamedPred(..)) => self.fail = true,
                    Addr::DBRef(ref db_ref) => self.get_next_db_ref(&indices, db_ref),
                    _ => {
                        self.fail = true;
                    }
                }
            }
            &SystemClauseType::LookupDBRef => {
                let a1 = self[temp_v!(1)].clone();

                match self.store(self.deref(a1)) {
                    Addr::DBRef(db_ref) => match db_ref {
                        DBRef::NamedPred(name, arity, spec) => {
                            let a2 = self[temp_v!(2)].clone();
                            let a3 = self[temp_v!(3)].clone();

                            let arity = Integer::from(arity);

                            self.unify(a2, Addr::Con(Constant::Atom(name, spec)));

                            if !self.fail {
                                self.unify(a3, Addr::Con(Constant::Integer(arity)));
                            }
                        }
                        _ => self.fail = true,
                    },
                    _ => self.fail = true,
                }
            }
            &SystemClauseType::LookupOpDBRef => {
                let a1 = self[temp_v!(1)].clone();

                match self.store(self.deref(a1)) {
                    Addr::DBRef(db_ref) => match db_ref {
                        DBRef::Op(priority, spec, name, _, shared_op_desc) => {
                            let prec = self[temp_v!(2)].clone();
                            let specifier = self[temp_v!(3)].clone();
                            let op = self[temp_v!(4)].clone();

                            let spec = match spec {
                                FX => "fx",
                                FY => "fy",
                                XF => "xf",
                                YF => "yf",
                                XFX => "xfx",
                                XFY => "xfy",
                                YFX => "yfx",
                                _ => {
                                    self.fail = true;
                                    return Ok(());
                                }
                            };

                            let a2 = Integer::from(priority);
                            let a3 = Addr::Con(Constant::Atom(clause_name!(spec), None));
                            let a4 = Addr::Con(Constant::Atom(name, Some(shared_op_desc)));

                            self.unify(Addr::Con(Constant::Integer(a2)), prec);

                            if !self.fail {
                                self.unify(a3, specifier);
                            }

                            if !self.fail {
                                self.unify(a4, op);
                            }
                        }
                        _ => self.fail = true,
                    },
                    _ => self.fail = true,
                }
            }
            &SystemClauseType::Maybe => {
                let result = {
                    let mut rand = RANDOM_STATE.borrow_mut();

                    rand.bits(1) == 0
                };

                self.fail = result;
            }
            &SystemClauseType::OpDeclaration => {
                let priority = self[temp_v!(1)].clone();
                let specifier = self[temp_v!(2)].clone();
                let op = self[temp_v!(3)].clone();

                let priority = match self.store(self.deref(priority)) {
                    Addr::Con(Constant::Integer(n)) => n.to_usize().unwrap(),
                    _ => unreachable!(),
                };

                let specifier = match self.store(self.deref(specifier)) {
                    Addr::Con(Constant::Atom(name, _)) => name,
                    _ => unreachable!(),
                };

                let op = match self.store(self.deref(op)) {
                    Addr::Con(Constant::Atom(name, _)) => name,
                    Addr::Con(Constant::Char(c)) => clause_name!(c.to_string(), indices.atom_tbl),
                    _ => unreachable!(),
                };

                let module = op.owning_module();

                let result = to_op_decl(priority, specifier.as_str(), op)
                    .map_err(SessionError::from)
                    .and_then(|op_decl| {
                        if op_decl.0 == 0 {
                            Ok(op_decl.remove(&mut indices.op_dir))
                        } else {
                            let spec = get_desc(op_decl.name(), composite_op!(&indices.op_dir));
                            op_decl.submit(module, spec, &mut indices.op_dir)
                        }
                    });

                match result {
                    Ok(()) => {}
                    Err(e) => {
                        // 8.14.3.3 l)
                        let e = MachineError::session_error(self.heap.h(), e);
                        let stub = MachineError::functor_stub(clause_name!("op"), 3);
                        let permission_error = self.error_form(e, stub);

                        return Err(permission_error);
                    }
                };
            }
            &SystemClauseType::TruncateIfNoLiftedHeapGrowthDiff => {
                self.truncate_if_no_lifted_heap_diff(|h| Addr::HeapCell(h))
            }
            &SystemClauseType::TruncateIfNoLiftedHeapGrowth => {
                self.truncate_if_no_lifted_heap_diff(|_| Addr::Con(Constant::EmptyList))
            }
            &SystemClauseType::ClearAttributeGoals => {
                self.attr_var_init.attribute_goals.clear();
            }
            &SystemClauseType::CloneAttributeGoals => {
                let attr_goals = self.attr_var_init.attribute_goals.clone();
                self.fetch_attribute_goals(attr_goals);
            }
            &SystemClauseType::FetchAttributeGoals => {
                let attr_goals = mem::replace(&mut self.attr_var_init.attribute_goals, vec![]);
                self.fetch_attribute_goals(attr_goals);
            }
            &SystemClauseType::GetAttributedVariableList => {
                let attr_var = self.store(self.deref(self[temp_v!(1)].clone()));
                let attr_var_list = match attr_var {
                    Addr::AttrVar(h) => h + 1,
                    attr_var @ Addr::HeapCell(_) | attr_var @ Addr::StackCell(..) => {
                        // create an AttrVar in the heap.
                        let h = self.heap.h();

                        self.heap.push(HeapCellValue::Addr(Addr::AttrVar(h)));
                        self.heap.push(HeapCellValue::Addr(Addr::HeapCell(h + 1)));

                        self.bind(Ref::AttrVar(h), attr_var);
                        h + 1
                    }
                    _ => {
                        self.fail = true;
                        return Ok(());
                    }
                };

                let list_addr = self[temp_v!(2)].clone();
                self.unify(Addr::HeapCell(attr_var_list), list_addr);
            }
            &SystemClauseType::GetAttrVarQueueDelimiter => {
                let addr = self[temp_v!(1)].clone();
                let value = Addr::Con(Constant::Usize(self.attr_var_init.attr_var_queue.len()));

                self.unify(addr, value);
            }
            &SystemClauseType::GetAttrVarQueueBeyond => {
                let addr = self[temp_v!(1)].clone();

                match self.store(self.deref(addr)) {
                    Addr::Con(Constant::Usize(b)) => {
                        let iter = self.gather_attr_vars_created_since(b);

                        let var_list_addr = Addr::HeapCell(self.heap.to_list(iter));
                        let list_addr = self[temp_v!(2)].clone();

                        self.unify(var_list_addr, list_addr);
                    }
                    Addr::Con(Constant::Integer(n)) => {
                        if let Some(b) = n.to_usize() {
                            let iter = self.gather_attr_vars_created_since(b);

                            let var_list_addr = Addr::HeapCell(self.heap.to_list(iter));
                            let list_addr = self[temp_v!(2)].clone();

                            self.unify(var_list_addr, list_addr);
                        } else {
                            self.fail = true;
                        }
                    }
                    _ => self.fail = true,
                }
            }
            &SystemClauseType::GetContinuationChunk => {
                let e = self.store(self.deref(self[temp_v!(1)].clone()));

                let e = if let Addr::Con(Constant::Usize(e)) = e {
                    e
                } else {
                    self.fail = true;
                    return Ok(());
                };

                let p_functor = self.store(self.deref(self[temp_v!(2)].clone()));
                let p = self.heap.to_local_code_ptr(&p_functor).unwrap();

                let num_cells = match code_repo.lookup_instr(self.last_call, &CodePtr::Local(p)) {
                    Some(line) => {
                        let perm_vars = match line.as_ref() {
                            Line::Control(ref ctrl_instr) => ctrl_instr.perm_vars(),
                            _ => None
                        };

                        perm_vars.unwrap()
                    }
                    _ => unreachable!()
                };

                let mut addrs = vec![];

                for index in 1 .. num_cells + 1 {
                    addrs.push(self.stack.index_and_frame(e)[index].clone());
                }

                let chunk = Addr::HeapCell(self.heap.h());

                self.heap.push(HeapCellValue::NamedStr(
                    1 + num_cells,
                    clause_name!("cont_chunk"),
                    None,
                ));

                self.heap.push(HeapCellValue::Addr(p_functor));
                self.heap.extend(addrs.into_iter().map(HeapCellValue::Addr));

                self.unify(self[temp_v!(3)].clone(), chunk);
            }
            &SystemClauseType::GetLiftedHeapFromOffsetDiff => {
                let lh_offset = self[temp_v!(1)].clone();

                match self.store(self.deref(lh_offset)) {
                    Addr::Con(Constant::Usize(lh_offset)) => {
                        if lh_offset >= self.lifted_heap.h() {
                            let solutions = self[temp_v!(2)].clone();
                            let diff = self[temp_v!(3)].clone();

                            self.unify(solutions, Addr::Con(Constant::EmptyList));
                            self.unify(diff, Addr::Con(Constant::EmptyList));
                        } else {
                            let h = self.heap.h();
                            let mut last_index = h;

                            for value in self.lifted_heap.iter_from(lh_offset) {
                                last_index = self.heap.h();

                                match value {
                                    HeapCellValue::Addr(ref addr) => {
                                        self.heap.push(HeapCellValue::Addr(addr.clone() + h));
                                    }
                                    HeapCellValue::PartialString(ref pstr) => {
                                        let new_pstr = pstr.clone();
                                        self.heap.push(HeapCellValue::PartialString(new_pstr));
                                    }
                                    value => {
                                        self.heap.push(value.clone());
                                    }
                                }
                            }

                            if last_index < self.heap.h() {
                                let addr_opt =
                                    if let HeapCellValue::Addr(ref addr) = &self.heap[last_index] {
                                        Some(addr.clone())
                                    } else {
                                        None
                                    };

                                addr_opt.map(|addr| {
                                    let diff = self[temp_v!(3)].clone();
                                    self.unify(diff, addr);
                                });
                            }

                            self.lifted_heap.truncate(lh_offset);

                            let solutions = self[temp_v!(2)].clone();
                            self.unify(Addr::HeapCell(h), solutions);
                        }
                    }
                    _ => self.fail = true,
                }
            }
            &SystemClauseType::GetLiftedHeapFromOffset => {
                let lh_offset = self[temp_v!(1)].clone();

                match self.store(self.deref(lh_offset)) {
                    Addr::Con(Constant::Usize(lh_offset)) => {
                        if lh_offset >= self.lifted_heap.h() {
                            let solutions = self[temp_v!(2)].clone();
                            self.unify(solutions, Addr::Con(Constant::EmptyList));
                        } else {
                            let h = self.heap.h();

                            for addr in self.lifted_heap.iter_from(lh_offset) {
                                match addr {
                                    HeapCellValue::Addr(ref addr) => {
                                        self.heap.push(HeapCellValue::Addr(addr.clone() + h))
                                    }
                                    HeapCellValue::PartialString(ref pstr) => {
                                        let new_pstr = pstr.clone();
                                        self.heap.push(HeapCellValue::PartialString(new_pstr));
                                    }
                                    value => {
                                        self.heap.push(value.clone());
                                    }
                                }
                            }

                            self.lifted_heap.truncate(lh_offset);

                            let solutions = self[temp_v!(2)].clone();
                            self.unify(Addr::HeapCell(h), solutions);
                        }
                    }
                    _ => self.fail = true,
                }
            }
            &SystemClauseType::GetDoubleQuotes => {
                let a1 = self[temp_v!(1)].clone();

                match self.flags.double_quotes {
                    DoubleQuotes::Chars => self.unify(a1, Addr::Con(atom!("chars"))),
                    DoubleQuotes::Atom => self.unify(a1, Addr::Con(atom!("atom"))),
                    DoubleQuotes::Codes => self.unify(a1, Addr::Con(atom!("codes"))),
                }
            }
            &SystemClauseType::GetSCCCleaner => {
                let dest = self[temp_v!(1)].clone();

                match cut_policy.downcast_mut::<SCCCutPolicy>().ok() {
                    Some(sgc_policy) => {
                        if let Some((addr, b_cutoff, prev_b)) = sgc_policy.pop_cont_pt() {
                            let b = self.stack.index_or_frame(self.b).prelude.b;

                            if b <= b_cutoff {
                                self.block = prev_b;

                                if let Some(r) = dest.as_var() {
                                    self.bind(r, addr.clone());
                                    return return_from_clause!(self.last_call, self);
                                }
                            } else {
                                sgc_policy.push_cont_pt(addr, b_cutoff, prev_b);
                            }
                        }
                    }
                    None => panic!("expected SCCCutPolicy trait object."),
                };

                self.fail = true;
            }
            &SystemClauseType::Halt => std::process::exit(0),
            &SystemClauseType::InstallSCCCleaner => {
                let addr = self[temp_v!(1)].clone();
                let b = self.b;
                let prev_block = self.block;

                if cut_policy.downcast_ref::<SCCCutPolicy>().is_err() {
                    let (r_c_w_h, r_c_wo_h) = indices.get_cleaner_sites();
                    *cut_policy = Box::new(SCCCutPolicy::new(r_c_w_h, r_c_wo_h));
                }

                match cut_policy.downcast_mut::<SCCCutPolicy>().ok() {
                    Some(cut_policy) => {
                        self.install_new_block(temp_v!(2));
                        cut_policy.push_cont_pt(addr, b, prev_block);
                    }
                    None => panic!(
                        "install_cleaner: should have installed \\
                         SCCCutPolicy."
                    ),
                };
            }
            &SystemClauseType::InstallInferenceCounter => {
                // A1 = B, A2 = L
                let a1 = self.store(self.deref(self[temp_v!(1)].clone()));
                let a2 = self.store(self.deref(self[temp_v!(2)].clone()));

                if call_policy.downcast_ref::<CWILCallPolicy>().is_err() {
                    CWILCallPolicy::new_in_place(call_policy);
                }

                match (a1, a2.clone()) {
                    (Addr::Con(Constant::Usize(bp)), Addr::Con(Constant::Integer(n)))
                  | (Addr::Con(Constant::CutPoint(bp)), Addr::Con(Constant::Integer(n))) => {
                        match call_policy.downcast_mut::<CWILCallPolicy>().ok() {
                            Some(call_policy) => {
                                let count = call_policy.add_limit(n, bp);
                                let count = Addr::Con(Constant::Integer(count.clone()));

                                let a3 = self[temp_v!(3)].clone();

                                self.unify(a3, count);
                            }
                            None => panic!(
                                "install_inference_counter: should have installed \\
                                 CWILCallPolicy."
                            ),
                        }
                    }
                    _ => {
                        let stub = MachineError::functor_stub(
                            clause_name!("call_with_inference_limit"),
                            3,
                        );
                        let type_error =
                            self.error_form(MachineError::type_error(ValidType::Integer, a2), stub);
                        self.throw_exception(type_error)
                    }
                };
            }
            &SystemClauseType::ModuleExists => {
                let module = self.store(self.deref(self[temp_v!(1)].clone()));

                match module {
                    Addr::Con(Constant::Atom(ref name, _)) => {
                        self.fail = !indices.modules.contains_key(name);
                    }
                    _ => unreachable!()
                };
            }
            &SystemClauseType::ModuleOf => {
                let module = self.store(self.deref(self[temp_v!(2)].clone()));

                match module {
                    Addr::Con(Constant::Atom(name, _)) => {
                        let module = Addr::Con(Constant::Atom(name.owning_module(), None));
                        let target = self[temp_v!(1)].clone();

                        self.unify(target, module);
                    }
                    Addr::Str(s) => match self.heap[s].clone() {
                        HeapCellValue::NamedStr(_, name, ..) => {
                            let module = Addr::Con(Constant::Atom(name.owning_module(), None));
                            let target = self[temp_v!(1)].clone();

                            self.unify(target, module);
                        }
                        _ => {
                            unreachable!()
                        }
                    },
                    _ => self.fail = true,
                };
            }
            &SystemClauseType::NoSuchPredicate => {
                let head = self[temp_v!(1)].clone();

                self.fail = match self.store(self.deref(head)) {
                    Addr::Str(s) => match self.heap[s].clone() {
                        HeapCellValue::NamedStr(arity, name, op_spec) => {
                            let module = name.owning_module();
                            indices.predicate_exists(name, module, arity, op_spec)
                        }
                        _ => unreachable!(),
                    },
                    Addr::Con(Constant::Atom(name, spec)) => {
                        let module = name.owning_module();
                        let spec = fetch_atom_op_spec(name.clone(), spec, &indices.op_dir);

                        indices.predicate_exists(name, module, 0, spec)
                    }
                    head => {
                        let err = MachineError::type_error(ValidType::Callable, head);
                        let stub = MachineError::functor_stub(clause_name!("clause"), 2);

                        return Err(self.error_form(err, stub));
                    }
                };
            }
            &SystemClauseType::RedoAttrVarBinding => {
                let var = self.store(self.deref(self[temp_v!(1)].clone()));
                let value = self.store(self.deref(self[temp_v!(2)].clone()));

                match var {
                    Addr::AttrVar(h) => {
                        if let Addr::AttrVar(h1) = value {
                            self.heap[h] = HeapCellValue::Addr(Addr::AttrVar(h1));

                            // append h's attributes list to h1's.
                            let mut l = h1 + 1;

                            while let Addr::Lis(l1) = self.store(self.deref(self.heap[l].as_addr(l))) {
                                l = l1 + 1;
                            }

                            self.heap[l] = HeapCellValue::Addr(Addr::HeapCell(h + 1));
                            self.trail(TrailRef::Ref(Ref::HeapCell(l)));
                        } else {
                            self.heap[h] = HeapCellValue::Addr(value);
                        }
                    }
                    _ => unreachable!()
                }
            }
            &SystemClauseType::ResetGlobalVarAtKey => {
                let key = self[temp_v!(1)].clone();

                let key = match self.store(self.deref(key)) {
                    Addr::Con(Constant::Atom(atom, _)) => atom,
                    _ => unreachable!(),
                };

                indices.global_variables.swap_remove(&key);
            }
            &SystemClauseType::ResetGlobalVarAtOffset => {
                let key = self[temp_v!(1)].clone();

                let key = match self.store(self.deref(key)) {
                    Addr::Con(Constant::Atom(atom, _)) => atom,
                    _ => unreachable!(),
                };

                let value = self[temp_v!(2)].clone();
                let mut ball = Ball::new();
                let h = self.heap.h();

                ball.boundary = h;
                copy_term(
                    CopyBallTerm::new(&mut self.stack, &mut self.heap, &mut ball.stub),
                    value,
                    AttrVarPolicy::DeepCopy,
                );

                let offset = self[temp_v!(3)].clone();

                match self.store(self.deref(offset)) {
                    Addr::Con(Constant::Usize(offset)) => {
                        indices.global_variables.insert(key, (ball, Some(offset)))
                    }
                    _ => {
                        indices.global_variables.insert(key, (ball, None))
                    }
                };
            },
            &SystemClauseType::ResetAttrVarState => {
                self.attr_var_init.reset();
            }
            &SystemClauseType::RemoveCallPolicyCheck => {
                let restore_default = match call_policy.downcast_mut::<CWILCallPolicy>().ok() {
                    Some(call_policy) => {
                        let a1 = self.store(self.deref(self[temp_v!(1)].clone()));

                        match a1 {
                            Addr::Con(Constant::Usize(bp)) | Addr::Con(Constant::CutPoint(bp)) => {
                                if call_policy.is_empty() && bp == self.b {
                                    Some(call_policy.into_inner())
                                } else {
                                    None
                                }
                            }
                            _ => {
                                panic!("remove_call_policy_check: expected Usize in A1.");
                            }
                        }
                    }
                    None => panic!(
                        "remove_call_policy_check: requires \\
                         CWILCallPolicy."
                    ),
                };

                if let Some(new_policy) = restore_default {
                    *call_policy = new_policy;
                }
            }
            &SystemClauseType::RemoveInferenceCounter => {
                match call_policy.downcast_mut::<CWILCallPolicy>().ok() {
                    Some(call_policy) => {
                        let a1 = self.store(self.deref(self[temp_v!(1)].clone()));

                        match a1 {
                            Addr::Con(Constant::Usize(bp)) | Addr::Con(Constant::CutPoint(bp)) => {
                                let count = call_policy.remove_limit(bp);
                                let count = Addr::Con(Constant::Integer(count.clone()));

                                let a2 = self[temp_v!(2)].clone();

                                self.unify(a2, count);
                            }
                            _ => {
                                panic!("remove_inference_counter: expected Usize in A1.");
                            }
                        }
                    }
                    None => panic!(
                        "remove_inference_counter: requires \\
                         CWILCallPolicy."
                    ),
                }
            }
            &SystemClauseType::REPL(repl_code_ptr) => return self.repl_redirect(repl_code_ptr),
            &SystemClauseType::ModuleRetractClause => {
                let p = self.cp;
                let trans_type = DynamicTransactionType::ModuleRetract;

                self.p = CodePtr::DynamicTransaction(trans_type, p);
                return Ok(());
            }
            &SystemClauseType::RetractClause => {
                let p = self.cp;
                let trans_type = DynamicTransactionType::Retract;

                self.p = CodePtr::DynamicTransaction(trans_type, p);
                return Ok(());
            }
            &SystemClauseType::ReturnFromVerifyAttr => {
                let e = self.e;
                let frame_len = self.stack.index_and_frame(e).prelude.univ_prelude.num_cells;

                for i in 1 .. frame_len - 1 {
                    self[RegType::Temp(i)] = self.stack.index_and_frame(e)[i].clone();
                }

                if let &Addr::Con(Constant::CutPoint(b0)) = &self.stack.index_and_frame(e)[frame_len - 1] {
                    self.b0 = b0;
                }

                if let &Addr::Con(Constant::Usize(num_of_args)) = &self.stack.index_and_frame(e)[frame_len] {
                    self.num_of_args = num_of_args;
                }

                self.deallocate();
                self.p = CodePtr::Local(self.stack.index_and_frame(e).prelude.interrupt_cp);

                return Ok(());
            }
            &SystemClauseType::RestoreCutPolicy => {
                let restore_default =
                    if let Ok(cut_policy) = cut_policy.downcast_ref::<SCCCutPolicy>() {
                        cut_policy.out_of_cont_pts()
                    } else {
                        false
                    };

                if restore_default {
                    *cut_policy = Box::new(DefaultCutPolicy {});
                }
            }
            &SystemClauseType::SetCutPoint(r) => {
                if cut_policy.cut(self, r) {
                    return Ok(());
                }
            }
            &SystemClauseType::SetCutPointByDefault(r) => {
                deref_cut(self, r)
            }
            &SystemClauseType::SetInput => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));
                let stream = self.get_stream_or_alias(addr, indices, "set_input")?;

                if stream.is_output_stream() {
                    let stub = MachineError::functor_stub(clause_name!("set_input"), 1);
                    let err = MachineError::permission_error(
                        PermissionError::InputStream,
                        "stream",
                        Addr::Stream(stream),
                    );

                    return Err(self.error_form(err, stub));
                }
                
                *current_input_stream = stream;
            }
            &SystemClauseType::SetOutput => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));
                let stream = self.get_stream_or_alias(addr, indices, "set_output")?;

                if stream.is_input_stream() {
                    let stub = MachineError::functor_stub(clause_name!("set_input"), 1);
                    let err = MachineError::permission_error(
                        PermissionError::OutputStream,
                        "stream",
                        Addr::Stream(stream),
                    );

                    return Err(self.error_form(err, stub));
                }

                *current_output_stream = stream;
            }
            &SystemClauseType::SetDoubleQuotes => match self[temp_v!(1)].clone() {
                Addr::Con(Constant::Atom(ref atom, _)) if atom.as_str() == "chars" => {
                    self.flags.double_quotes = DoubleQuotes::Chars
                }
                Addr::Con(Constant::Atom(ref atom, _)) if atom.as_str() == "atom" => {
                    self.flags.double_quotes = DoubleQuotes::Atom
                }
                Addr::Con(Constant::Atom(ref atom, _)) if atom.as_str() == "codes" => {
                    self.flags.double_quotes = DoubleQuotes::Codes
                }
                _ => self.fail = true,
            },
            &SystemClauseType::InferenceLevel => {
                let a1 = self[temp_v!(1)].clone();
                let a2 = self.store(self.deref(self[temp_v!(2)].clone()));

                match a2 {
                    Addr::Con(Constant::CutPoint(bp)) | Addr::Con(Constant::Usize(bp)) => {
                        let prev_b = self.stack.index_or_frame(self.b).prelude.b;

                        if prev_b <= bp {
                            let a2 = Addr::Con(atom!("!"));
                            self.unify(a1, a2);
                        } else {
                            let a2 = Addr::Con(atom!("true"));
                            self.unify(a1, a2);
                        }
                    }
                    _ => self.fail = true,
                };
            }
            &SystemClauseType::CleanUpBlock => {
                let nb = self.store(self.deref(self[temp_v!(1)].clone()));

                match nb {
                    Addr::Con(Constant::Usize(nb)) => {
                        let b = self.b;

                        if nb > 0 && self.stack.index_or_frame(b).prelude.b == nb {
                            self.b = self.stack.index_or_frame(nb).prelude.b;
                        }
                    }
                    _ => self.fail = true,
                };
            }
            &SystemClauseType::EraseBall => self.ball.reset(),
            &SystemClauseType::Fail => self.fail = true,
            &SystemClauseType::GetBall => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));
                let h = self.heap.h();

                if self.ball.stub.h() > 0 {
                    let stub = self.ball.copy_and_align(h);
                    self.heap.extend(stub.into_iter());
                } else {
                    self.fail = true;
                    return Ok(());
                }

                let ball = self.heap[h].as_addr(h);

                match addr.as_var() {
                    Some(r) => self.bind(r, ball),
                    _ => self.fail = true,
                };
            }
            &SystemClauseType::GetCurrentBlock => {
                let c = Constant::Usize(self.block);
                let addr = self[temp_v!(1)].clone();

                self.write_constant_to_var(addr, c);
            }
            &SystemClauseType::GetBValue => {
                let a1 = self[temp_v!(1)].clone();
                let a2 = Addr::Con(Constant::Usize(self.b));

                self.unify(a1, a2);
            }
            &SystemClauseType::GetClause => {
                let head = self[temp_v!(1)].clone();

                let subsection = match self.store(self.deref(head)) {
                    Addr::Str(s) => match self.heap[s].clone() {
                        HeapCellValue::NamedStr(arity, name, ..) => {
                            indices.get_clause_subsection(name.owning_module(), name, arity)
                        }
                        _ => unreachable!(),
                    },
                    Addr::Con(Constant::Atom(name, _)) => {
                        indices.get_clause_subsection(name.owning_module(), name, 0)
                    }
                    _ => unreachable!(),
                };

                match subsection {
                    Some(dynamic_predicate_info) => {
                        self.execute_at_index(
                            2,
                            dir_entry!(dynamic_predicate_info.clauses_subsection_p)
                        );
                        return Ok(());
                    }
                    _ => unreachable!(),
                }
            }
            &SystemClauseType::GetCutPoint => {
                let a1 = self[temp_v!(1)].clone();
                let a2 = Addr::Con(Constant::CutPoint(self.b0));

                self.unify(a1, a2);
            }
            &SystemClauseType::InstallNewBlock => {
                self.install_new_block(temp_v!(1));
            }
            &SystemClauseType::RawInputReadChar => {
		let keypress = {
                    enable_raw_mode().expect("failed to transition into raw mode");
		    let result = next_keypress();
                    disable_raw_mode().expect("failed to transition out of raw mode");

                    result
		};

                let c = match keypress {
                    ContinueResult::ContinueQuery => ';',
                    ContinueResult::Conclude => '.',
                    ContinueResult::Help => 'h',
                    ContinueResult::PrintWithoutMaxDepth => 'w',
                    ContinueResult::PrintWithMaxDepth => 'p',
                };

                let target = self[temp_v!(1)].clone();
                self.unify(Addr::Con(Constant::Char(c)), target);
            }
            &SystemClauseType::NextEP => {
                let first_arg = self.store(self.deref(self[temp_v!(1)].clone()));

                match first_arg {
                    Addr::Con(Constant::Atom(ref name, _))
                        if name.as_str() == "first" => {
                            if self.e == 0 {
                                self.fail = true;
                                return Ok(());
                            }

                            let cp = (self.stack.index_and_frame(self.e).prelude.cp - 1).unwrap();

                            let e = self.stack.index_and_frame(self.e).prelude.e;
                            let e = Addr::Con(Constant::Usize(e));

                            let p = cp.as_functor(&mut self.heap);

                            self.unify(self[temp_v!(2)].clone(), e);

                            if !self.fail {
                                self.unify(self[temp_v!(3)].clone(), p);
                            }
                        },
                    Addr::Con(Constant::Usize(e)) => {
                        if e == 0 {
                            self.fail = true;
                            return Ok(());
                        }

                        // get the call site so that the number of active permanent variables can be read
                        // from it later.
                        let cp = (self.stack.index_and_frame(e).prelude.cp - 1).unwrap();

                        let p = cp.as_functor(&mut self.heap);
                        let e = self.stack.index_and_frame(e).prelude.e;

                        let e = Addr::Con(Constant::Usize(e));

                        self.unify(self[temp_v!(2)].clone(), e);

                        if !self.fail {
                            self.unify(self[temp_v!(3)].clone(), p);
                        }
                    }
                    _ => unreachable!()
                }
            }
            &SystemClauseType::PointsToContinuationResetMarker => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));

                let p = match self.heap.to_local_code_ptr(&addr) {
                    Some(p) => p + 1,
                    None => {
                        self.fail = true;
                        return Ok(());
                    }
                };

                if p.is_reset_cont_marker(code_repo, self.last_call) {
                    return return_from_clause!(self.last_call, self);
                }

                self.fail = true;
                return Ok(());
            }
            &SystemClauseType::QuotedToken => {
                let addr = self.store(self.deref(self[temp_v!(1)].clone()));

                match addr {
                    Addr::Con(Constant::CharCode(c)) => {
                        self.fail = match std::char::from_u32(c) {
                            Some(c) => {
                                non_quoted_token(once(c))
                            }
                            None => {
                                true
                            }
                        };
                    }
                    Addr::Con(Constant::Char(c)) => {
                        self.fail = non_quoted_token(once(c));
                    }
                    Addr::Con(Constant::Atom(atom, _)) => {
                        self.fail = non_quoted_token(atom.as_str().chars());
                    }
                    _ => {
                        self.fail = true;
                    }
                }
            }
            &SystemClauseType::ReadQueryTerm => {
                readline::set_prompt(true);
                let result = self.read_term(current_input_stream, indices);
                readline::set_prompt(false);

                let _ = result?;
            }
            &SystemClauseType::ReadTerm => {
                readline::set_prompt(false);
                self.read_term(current_input_stream, indices)?;
            }
            &SystemClauseType::ResetBlock => {
                let addr = self.deref(self[temp_v!(1)].clone());
                self.reset_block(addr);
            }
            &SystemClauseType::ResetContinuationMarker => {
                self[temp_v!(3)] = Addr::Con(Constant::Atom(clause_name!("none"), None));

                let h = self.heap.h();
                self.heap.push(HeapCellValue::Addr(Addr::HeapCell(h)));

                self[temp_v!(4)] = Addr::HeapCell(h);
            }
            &SystemClauseType::SetBall =>
                self.set_ball(),
            &SystemClauseType::SetSeed => {
                let seed = self.store(self.deref(self[temp_v!(1)].clone()));

                let seed = match seed {
                    Addr::Con(Constant::Integer(n)) =>
                        n,
                    Addr::Con(Constant::CharCode(c)) =>
                        Integer::from(c),
                    Addr::Con(Constant::Rational(r)) => {
                        if r.denom() == &1 {
                            r.numer().clone()
                        } else {
                            self.fail = true;
                            return Ok(());
                        }
                    }
                    _ => {
                        self.fail = true;
                        return Ok(());
                    }
                };

                let mut rand = RANDOM_STATE.borrow_mut();
                rand.seed(&seed);
            }
            &SystemClauseType::SkipMaxList =>
                if let Err(err) = self.skip_max_list() {
                    return Err(err);
                },
            &SystemClauseType::StoreGlobalVar => {
                let key = self[temp_v!(1)].clone();

                let key = match self.store(self.deref(key)) {
                    Addr::Con(Constant::Atom(atom, _)) => atom,
                    _ => unreachable!(),
                };

                let value = self[temp_v!(2)].clone();
                let mut ball = Ball::new();

                ball.boundary = self.heap.h();
                copy_term(
                    CopyBallTerm::new(&mut self.stack, &mut self.heap, &mut ball.stub),
                    value,
                    AttrVarPolicy::DeepCopy,
                );

                indices.global_variables.insert(key, (ball, None));
            }
            &SystemClauseType::StoreGlobalVarWithOffset => {
                let key = self[temp_v!(1)].clone();

                let key = match self.store(self.deref(key)) {
                    Addr::Con(Constant::Atom(atom, _)) => atom,
                    _ => unreachable!(),
                };

                let value = self[temp_v!(2)].clone();
                let mut ball = Ball::new();
                let h = self.heap.h();

                ball.boundary = h;
                copy_term(
                    CopyBallTerm::new(&mut self.stack, &mut self.heap, &mut ball.stub),
                    value.clone(),
                    AttrVarPolicy::DeepCopy,
                );

                let stub = ball.copy_and_align(h);
                self.heap.extend(stub.into_iter());
                indices.global_variables.insert(key, (ball, Some(h)));

                self.unify(value, Addr::HeapCell(h));
            }
            &SystemClauseType::Succeed => {}
            &SystemClauseType::TermVariables => {
                let a1 = self[temp_v!(1)].clone();
                let mut seen_vars = IndexSet::new();

                for item in self.acyclic_pre_order_iter(a1) {
                    match item {
                        HeapCellValue::Addr(addr) => {
                            if addr.is_ref() {
                                seen_vars.insert(addr);
                            }
                        }
                        _ => {}
                    }
                }

                let outcome = Addr::HeapCell(self.heap.to_list(seen_vars.into_iter()));

                let a2 = self[temp_v!(2)].clone();
                self.unify(a2, outcome);
            }
            &SystemClauseType::TruncateLiftedHeapTo => {
                match self.store(self.deref(self[temp_v!(1)].clone())) {
                    Addr::Con(Constant::Usize(lh_offset)) => self.lifted_heap.truncate(lh_offset),
                    _ => self.fail = true,
                }
            }
            &SystemClauseType::UnifyWithOccursCheck => {
                let a1 = self[temp_v!(1)].clone();
                let a2 = self[temp_v!(2)].clone();

                self.unify_with_occurs_check(a1, a2);
            }
            &SystemClauseType::UnwindEnvironments => {
                let mut e = self.e;
                let mut cp = self.cp;

                while e > 0 {
                    if cp.is_reset_cont_marker(code_repo, self.last_call) {
                        self.e = e;
                        self.p = CodePtr::Local(cp + 1); // skip the reset marker.

                        return Ok(());
                    }

                    cp = self.stack.index_and_frame(e).prelude.cp;
                    e = self.stack.index_and_frame(e).prelude.e;
                }
            }
            &SystemClauseType::UnwindStack => self.unwind_stack(),
            &SystemClauseType::Variant => self.fail = self.structural_eq_test(),
            &SystemClauseType::WAMInstructions => {
                let name = self[temp_v!(1)].clone();
                let arity = self[temp_v!(2)].clone();

                let name = match self.store(self.deref(name)) {
                    Addr::Con(Constant::Atom(name, _)) => name,
                    _ => unreachable!(),
                };

                let arity = match self.store(self.deref(arity)) {
                    Addr::Con(Constant::Integer(n)) => n,
                    _ => unreachable!(),
                };

                let first_idx = match indices
                    .code_dir
                    .get(&(name.clone(), arity.to_usize().unwrap()))
                {
                    Some(ref idx) => {
                        if let Some(idx) = idx.local() {
                            idx
                        } else {
                            let arity = arity.to_usize().unwrap();
                            let stub = MachineError::functor_stub(name.clone(), arity);
                            let h = self.heap.h();

                            let err = MachineError::existence_error(
                                h,
                                ExistenceError::Procedure(name, arity),
                            );
                            let err = self.error_form(err, stub);

                            self.throw_exception(err);
                            return Ok(());
                        }
                    }
                    None => {
                        let arity = arity.to_usize().unwrap();
                        let stub = MachineError::functor_stub(name.clone(), arity);
                        let h = self.heap.h();

                        let err = MachineError::existence_error(
                            h,
                            ExistenceError::Procedure(name, arity),
                        );
                        let err = self.error_form(err, stub);

                        self.throw_exception(err);
                        return Ok(());
                    }
                };

                let mut h = self.heap.h();
                let mut functors = vec![];

                walk_code(
                    &code_repo.code,
                    first_idx,
                    |instr| {
                        let section = instr.to_functor(h);
                        functors.push(Addr::HeapCell(h));

                        h += section.len();
                        self.heap.extend(section.into_iter());
                    },
                );

                let listing = Addr::HeapCell(self.heap.to_list(functors.into_iter()));
                let listing_var = self[temp_v!(3)].clone();

                self.unify(listing, listing_var);
            }
            &SystemClauseType::WriteTerm => {
                let addr = self[temp_v!(1)].clone();

                let ignore_ops = self.store(self.deref(self[temp_v!(2)].clone()));
                let numbervars = self.store(self.deref(self[temp_v!(3)].clone()));
                let quoted = self.store(self.deref(self[temp_v!(4)].clone()));
                let max_depth = self.store(self.deref(self[temp_v!(6)].clone()));

                let mut printer = HCPrinter::new(&self, &indices.op_dir, PrinterOutputter::new());

                if let &Addr::Con(Constant::Atom(ref name, ..)) = &ignore_ops {
                    printer.ignore_ops = name.as_str() == "true";
                }

                if let &Addr::Con(Constant::Atom(ref name, ..)) = &numbervars {
                    printer.numbervars = name.as_str() == "true";
                }

                if let &Addr::Con(Constant::Atom(ref name, ..)) = &quoted {
                    printer.quoted = name.as_str() == "true";
                }

                if let &Addr::Con(Constant::Integer(ref n)) = &max_depth {
                    if let Some(n) = n.to_usize() {
                        printer.max_depth = n;
                    } else {
                        self.fail = true;
                        return Ok(());
                    }
                }

                let stub = MachineError::functor_stub(clause_name!("write_term"), 2);

                match self.try_from_list(temp_v!(5), stub.clone()) {
                    Ok(addrs) => {
                        let mut var_names: IndexMap<Addr, String> = IndexMap::new();

                        for addr in addrs {
                            match addr {
                                Addr::Str(s) => match &self.heap[s] {
                                    &HeapCellValue::NamedStr(2, ref name, _)
                                        if name.as_str() == "=" =>
                                    {
                                        let atom = self.heap[s + 1].as_addr(s + 1);
                                        let var = self.heap[s + 2].as_addr(s + 2);

                                        let atom = match self.store(self.deref(atom)) {
                                            Addr::Con(Constant::Atom(atom, _)) => atom.to_string(),
                                            Addr::Con(Constant::Char(c)) => c.to_string(),
                                            _ => unreachable!(),
                                        };

                                        let var = self.store(self.deref(var));

                                        if var_names.contains_key(&var) {
                                            continue;
                                        }

                                        var_names.insert(var, atom);
                                    }
                                    _ => unreachable!(),
                                },
                                _ => unreachable!(),
                            }
                        }

                        printer.var_names = var_names;
                    }
                    Err(err) => return Err(err),
                }

                let output = printer.print(addr);
                print!("{}", output.result());
                stdout().flush().unwrap();
            }
        };

        return_from_clause!(self.last_call, self)
    }
}
