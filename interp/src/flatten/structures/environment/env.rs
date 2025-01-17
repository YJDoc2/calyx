use itertools::Itertools;

use super::{assignments::AssignmentBundle, program_counter::ProgramCounter};

use super::super::{
    context::Context, index_trait::IndexRange, indexed_map::IndexedMap,
};
use crate::{
    errors::{InterpreterError, InterpreterResult},
    flatten::{
        flat_ir::{
            prelude::{
                AssignedValue, AssignmentIdx, BaseIndices, ComponentIdx,
                ControlIdx, ControlNode, GlobalCellIdx, GlobalPortIdx,
                GlobalPortRef, GlobalRefCellIdx, GlobalRefPortIdx, GuardIdx,
                PortRef, PortValue,
            },
            wires::guards::Guard,
        },
        primitives::{self, prim_trait::UpdateStatus, Primitive},
        structures::{
            environment::program_counter::ControlPoint, index_trait::IndexRef,
        },
    },
    values::Value,
};
use std::{collections::VecDeque, fmt::Debug};

pub type PortMap = IndexedMap<GlobalPortIdx, PortValue>;

impl PortMap {
    /// Essentially asserts that the port given is undefined, it errors out if
    /// the port is defined and otherwise does nothing
    pub fn write_undef(
        &mut self,
        target: GlobalPortIdx,
    ) -> InterpreterResult<()> {
        if self[target].is_def() {
            todo!("raise error")
        } else {
            Ok(())
        }
    }

    /// Sets the given index to undefined without checking whether or not it was
    /// already defined
    #[inline]
    pub fn write_undef_unchecked(&mut self, target: GlobalPortIdx) {
        self[target] = PortValue::new_undef();
    }

    pub fn insert_val(
        &mut self,
        target: GlobalPortIdx,
        val: AssignedValue,
    ) -> InterpreterResult<UpdateStatus> {
        match self[target].as_option() {
            // unchanged
            Some(t) if *t == val => Ok(UpdateStatus::Unchanged),
            // conflict
            // TODO: Fix to make the error more helpful
            Some(t) if t.has_conflict_with(&val) => InterpreterResult::Err(
                InterpreterError::FlatConflictingAssignments {
                    a1: t.clone(),
                    a2: val,
                }
                .into(),
            ),
            // changed
            Some(_) | None => {
                self[target] = PortValue::new(val);
                Ok(UpdateStatus::Changed)
            }
        }
    }
}

pub(crate) type CellMap = IndexedMap<GlobalCellIdx, CellLedger>;
pub(crate) type RefCellMap =
    IndexedMap<GlobalRefCellIdx, Option<GlobalCellIdx>>;
pub(crate) type RefPortMap =
    IndexedMap<GlobalRefPortIdx, Option<GlobalPortIdx>>;
pub(crate) type AssignmentRange = IndexRange<AssignmentIdx>;

pub(crate) struct ComponentLedger {
    pub(crate) index_bases: BaseIndices,
    pub(crate) comp_id: ComponentIdx,
}

impl ComponentLedger {
    /// Convert a relative offset to a global one. Perhaps should take an owned
    /// value rather than a pointer
    pub fn convert_to_global(&self, port: &PortRef) -> GlobalPortRef {
        match port {
            PortRef::Local(l) => (&self.index_bases + l).into(),
            PortRef::Ref(r) => (&self.index_bases + r).into(),
        }
    }
}

/// An enum encapsulating cell functionality. It is either a pointer to a
/// primitive or information about a calyx component instance
pub(crate) enum CellLedger {
    Primitive {
        // wish there was a better option with this one
        cell_dyn: Box<dyn Primitive>,
    },
    Component(ComponentLedger),
}

impl CellLedger {
    fn new_comp(idx: ComponentIdx, env: &Environment) -> Self {
        Self::Component(ComponentLedger {
            index_bases: BaseIndices::new(
                env.ports.peek_next_idx(),
                (env.cells.peek_next_idx().index() + 1).into(),
                env.ref_cells.peek_next_idx(),
                env.ref_ports.peek_next_idx(),
            ),
            comp_id: idx,
        })
    }

    pub fn as_comp(&self) -> Option<&ComponentLedger> {
        match self {
            Self::Component(comp) => Some(comp),
            _ => None,
        }
    }

    #[inline]
    pub fn unwrap_comp(&self) -> &ComponentLedger {
        self.as_comp()
            .expect("Unwrapped cell ledger as component but received primitive")
    }

    #[must_use]
    pub(crate) fn as_primitive(&self) -> Option<&dyn Primitive> {
        if let Self::Primitive { cell_dyn } = self {
            Some(&**cell_dyn)
        } else {
            None
        }
    }
}

impl Debug for CellLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Primitive { .. } => f.debug_struct("Primitive").finish(),
            Self::Component(ComponentLedger {
                index_bases,
                comp_id,
            }) => f
                .debug_struct("Component")
                .field("index_bases", index_bases)
                .field("comp_id", comp_id)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub struct Environment<'a> {
    /// A map from global port IDs to their current values.
    pub(crate) ports: PortMap,
    /// A map from global cell IDs to their current state and execution info.
    cells: CellMap,
    /// A map from global ref cell IDs to the cell they reference, if any.
    ref_cells: RefCellMap,
    /// A map from global ref port IDs to the port they reference, if any.
    ref_ports: RefPortMap,

    /// The program counter for the whole program execution.
    pc: ProgramCounter,

    /// The immutable context. This is retained for ease of use.
    ctx: &'a Context,
}

impl<'a> Environment<'a> {
    pub fn new(ctx: &'a Context) -> Self {
        let root = ctx.entry_point;
        let aux = &ctx.secondary[root];

        let mut env = Self {
            ports: PortMap::with_capacity(aux.port_offset_map.count()),
            cells: CellMap::with_capacity(aux.cell_offset_map.count()),
            ref_cells: RefCellMap::with_capacity(
                aux.ref_cell_offset_map.count(),
            ),
            ref_ports: RefPortMap::with_capacity(
                aux.ref_port_offset_map.count(),
            ),
            pc: ProgramCounter::new(ctx),
            ctx,
        };

        let root_node = CellLedger::new_comp(root, &env);
        let root = env.cells.push(root_node);
        env.layout_component(root);

        env
    }

    /// Internal function used to layout a given component from a cell id
    ///
    /// Layout is handled in the following order:
    /// 1. component signature (input/output)
    /// 2. group hole ports
    /// 3. cells + ports, primitive
    /// 4. sub-components
    /// 5. ref-cells & ports
    fn layout_component(&mut self, comp: GlobalCellIdx) {
        let ComponentLedger {
            index_bases,
            comp_id,
        } = self.cells[comp]
            .as_comp()
            .expect("Called layout component with a non-component cell.");
        let comp_aux = &self.ctx.secondary[*comp_id];

        // first layout the signature
        for sig_port in comp_aux.signature.iter() {
            let idx = self.ports.push(PortValue::new_undef());
            debug_assert_eq!(index_bases + sig_port, idx);
        }
        // second group ports
        for group_idx in comp_aux.definitions.groups() {
            //go
            let go = self.ports.push(PortValue::new_undef());

            //done
            let done = self.ports.push(PortValue::new_undef());

            // quick sanity check asserts
            let go_actual = index_bases + self.ctx.primary[group_idx].go;
            let done_actual = index_bases + self.ctx.primary[group_idx].done;
            // Case 1 - Go defined before done
            if self.ctx.primary[group_idx].go < self.ctx.primary[group_idx].done
            {
                debug_assert_eq!(done, done_actual);
                debug_assert_eq!(go, go_actual);
            }
            // Case 2 - Done defined before go
            else {
                // in this case go is defined after done, so our variable names
                // are backward, but this is not a problem since they are
                // initialized to the same value
                debug_assert_eq!(go, done_actual);
                debug_assert_eq!(done, go_actual);
            }
        }

        for (cell_off, def_idx) in comp_aux.cell_offset_map.iter() {
            let info = &self.ctx.secondary[*def_idx];
            if !info.prototype.is_component() {
                let port_base = self.ports.peek_next_idx();
                for port in info.ports.iter() {
                    let idx = self.ports.push(PortValue::new_undef());
                    debug_assert_eq!(
                        &self.cells[comp].as_comp().unwrap().index_bases + port,
                        idx
                    );
                }
                let cell_dyn = primitives::build_primitive(info, port_base);
                let cell = self.cells.push(CellLedger::Primitive { cell_dyn });

                debug_assert_eq!(
                    &self.cells[comp].as_comp().unwrap().index_bases + cell_off,
                    cell
                );
            } else {
                let child_comp = info.prototype.as_component().unwrap();
                let child_comp = CellLedger::new_comp(*child_comp, self);

                let cell = self.cells.push(child_comp);
                debug_assert_eq!(
                    &self.cells[comp].as_comp().unwrap().index_bases + cell_off,
                    cell
                );

                self.layout_component(cell);
            }
        }

        // ref cells and ports are initialized to None
        for (ref_cell, def_idx) in comp_aux.ref_cell_offset_map.iter() {
            let info = &self.ctx.secondary[*def_idx];
            for port_idx in info.ports.iter() {
                let port_actual = self.ref_ports.push(None);
                debug_assert_eq!(
                    &self.cells[comp].as_comp().unwrap().index_bases + port_idx,
                    port_actual
                )
            }
            let cell_actual = self.ref_cells.push(None);
            debug_assert_eq!(
                &self.cells[comp].as_comp().unwrap().index_bases + ref_cell,
                cell_actual
            )
        }
    }
}

// ===================== Environment print implementations =====================
impl<'a> Environment<'a> {
    pub fn print_env(&self) {
        let root_idx = GlobalCellIdx::new(0);
        let mut hierarchy = Vec::new();
        self.print_component(root_idx, &mut hierarchy)
    }

    fn print_component(
        &self,
        target: GlobalCellIdx,
        hierarchy: &mut Vec<GlobalCellIdx>,
    ) {
        let info = self.cells[target].as_comp().unwrap();
        let comp = &self.ctx.secondary[info.comp_id];
        hierarchy.push(target);

        // This funky iterator chain first pulls the first element (the
        // entrypoint) and extracts its name. Subsequent element are pairs of
        // global offsets produced by a staggered iteration, yielding `(root,
        // child)` then `(child, grandchild)` and so on. All the strings are
        // finally collected and concatenated with a `.` separator to produce
        // the fully qualified name prefix for the given component instance.
        let name_prefix = hierarchy
            .first()
            .iter()
            .map(|x| {
                let info = self.cells[**x].as_comp().unwrap();
                let prior_comp = &self.ctx.secondary[info.comp_id];
                &self.ctx.secondary[prior_comp.name]
            })
            .chain(hierarchy.iter().zip(hierarchy.iter().skip(1)).map(
                |(l, r)| {
                    let info = self.cells[*l].as_comp().unwrap();
                    let prior_comp = &self.ctx.secondary[info.comp_id];
                    let local_target = r - (&info.index_bases);

                    let def_idx = &prior_comp.cell_offset_map[local_target];

                    let id = &self.ctx.secondary[*def_idx];
                    &self.ctx.secondary[id.name]
                },
            ))
            .join(".");

        for (cell_off, def_idx) in comp.cell_offset_map.iter() {
            let definition = &self.ctx.secondary[*def_idx];

            println!("{}.{}", name_prefix, self.ctx.secondary[definition.name]);
            for port in definition.ports.iter() {
                let definition =
                    &self.ctx.secondary[comp.port_offset_map[port]];
                println!(
                    "    {}: {} ({:?})",
                    self.ctx.secondary[definition.name],
                    self.ports[&info.index_bases + port],
                    &info.index_bases + port
                );
            }

            let cell_idx = &info.index_bases + cell_off;

            if definition.prototype.is_component() {
                self.print_component(cell_idx, hierarchy);
            } else if self.cells[cell_idx]
                .as_primitive()
                .unwrap()
                .has_serializable_state()
            {
                println!(
                    "    INTERNAL_DATA: {}",
                    serde_json::to_string_pretty(
                        &self.cells[cell_idx]
                            .as_primitive()
                            .unwrap()
                            .serialize(None)
                    )
                    .unwrap()
                )
            }
        }

        hierarchy.pop();
    }

    pub fn print_env_stats(&self) {
        println!("Environment Stats:");
        println!("  Ports: {}", self.ports.len());
        println!("  Cells: {}", self.cells.len());
        println!("  Ref Cells: {}", self.ref_cells.len());
        println!("  Ref Ports: {}", self.ref_ports.len());
    }

    pub fn print_pc(&self) {
        println!("{:?}", self.pc)
    }
}

/// A wrapper struct for the environment that provides the functions used to
/// simulate the actual program. This is just to keep the simulation logic under
/// a different namespace than the environment to avoid confusion
pub struct Simulator<'a> {
    env: Environment<'a>,
}

impl<'a> Simulator<'a> {
    pub fn new(env: Environment<'a>) -> Self {
        Self { env }
    }

    pub fn print_env(&self) {
        self.env.print_env()
    }

    pub fn ctx(&self) -> &Context {
        self.env.ctx
    }
}

// =========================== simulation functions ===========================
impl<'a> Simulator<'a> {
    /// pull out the next nodes to search when
    fn extract_next_search(&self, idx: ControlIdx) -> VecDeque<ControlIdx> {
        match &self.env.ctx.primary[idx] {
            ControlNode::Seq(s) => s.stms().iter().copied().collect(),
            ControlNode::Par(p) => p.stms().iter().copied().collect(),
            ControlNode::If(i) => vec![i.tbranch(), i.fbranch()].into(),
            ControlNode::While(w) => vec![w.body()].into(),
            _ => VecDeque::new(),
        }
    }

    #[inline]
    fn lookup_global_port_id(&self, port: GlobalPortRef) -> GlobalPortIdx {
        match port {
            GlobalPortRef::Port(p) => p,
            // TODO Griffin: Please make sure this error message is correct with
            // respect to the compiler
            GlobalPortRef::Ref(r) => self.env.ref_ports[r].expect("A ref port is being queried without a supplied ref-cell. This is an error?"),
        }
    }

    #[inline]
    fn get_global_idx(
        &self,
        port: &PortRef,
        comp: GlobalCellIdx,
    ) -> GlobalPortIdx {
        let ledger = self.env.cells[comp].unwrap_comp();
        self.lookup_global_port_id(ledger.convert_to_global(port))
    }

    #[inline]
    fn get_value(&self, port: &PortRef, comp: GlobalCellIdx) -> &PortValue {
        let port_idx = self.get_global_idx(port, comp);
        &self.env.ports[port_idx]
    }

    /// Attempt to find the parent cell for a port. If no such cell exists (i.e.
    /// it is a hole port, then it returns None)
    fn get_parent_cell(
        &self,
        port: PortRef,
        comp: GlobalCellIdx,
    ) -> Option<GlobalCellIdx> {
        let component = self.env.cells[comp].unwrap_comp();
        let comp_info = &self.env.ctx.secondary[component.comp_id];

        match port {
            PortRef::Local(l) => {
                for (cell_offset, cell_def_idx) in
                    comp_info.cell_offset_map.iter()
                {
                    if self.env.ctx.secondary[*cell_def_idx].ports.contains(l) {
                        return Some(&component.index_bases + cell_offset);
                    }
                }
            }
            PortRef::Ref(r) => {
                for (cell_offset, cell_def_idx) in
                    comp_info.ref_cell_offset_map.iter()
                {
                    if self.env.ctx.secondary[*cell_def_idx].ports.contains(r) {
                        let ref_cell_idx = &component.index_bases + cell_offset;
                        return Some(
                            self.env.ref_cells[ref_cell_idx]
                                .expect("Ref cell has not been instantiated"),
                        );
                    }
                }
            }
        }

        None
    }

    // may want to make this iterate directly if it turns out that the vec
    // allocation is too expensive in this context
    fn get_assignments(
        &self,
        control_points: &[ControlPoint],
    ) -> AssignmentBundle {
        control_points
            .iter()
            .map(|node| {
                match &self.ctx().primary[node.control_node_idx] {
                    ControlNode::Enable(e) => {
                        (node.comp, self.ctx().primary[e.group()].assignments)
                    }

                    ControlNode::Invoke(_) => {
                        todo!("invokes not yet implemented")
                    }

                    ControlNode::Empty(_) => {
                        unreachable!(
                            "called `get_assignments` with an empty node"
                        )
                    }
                    // non-leaf nodes
                    ControlNode::If(_)
                    | ControlNode::While(_)
                    | ControlNode::Seq(_)
                    | ControlNode::Par(_) => {
                        unreachable!(
                            "Called `get_assignments` with non-leaf nodes"
                        )
                    }
                }
            })
            .collect()
    }

    pub fn step(&mut self) -> InterpreterResult<()> {
        // place to keep track of what groups we need to conclude at the end of
        // this step. These are indices into the program counter

        // In the future it may be worthwhile to preallocate some space to these
        // buffers. Can pick anything from zero to the number of nodes in the
        // program counter as the size
        let mut leaf_nodes = vec![];
        let mut done_groups = vec![];

        self.env.pc.vec_mut().retain_mut(|node| {
            // just considering a single node case for the moment
            match &self.env.ctx.primary[node.control_node_idx] {
                ControlNode::Seq(seq) => {
                    if !seq.is_empty() {
                        let next = seq.stms()[0];
                        *node = node.new_retain_comp(next);
                        true
                    } else {
                        node.mutate_into_next(self.env.ctx)
                    }
                }
                ControlNode::Par(_par) => todo!("not ready for par yet"),
                ControlNode::If(i) => {
                    if i.cond_group().is_some() {
                        todo!("if statement has a with clause")
                    }

                    let target = GlobalPortRef::from_local(
                        i.cond_port(),
                        &self.env.cells[node.comp].unwrap_comp().index_bases,
                    );

                    let result = match target {
                        GlobalPortRef::Port(p) => self.env.ports[p]
                            .as_bool()
                            .expect("if condition is undefined"),
                        GlobalPortRef::Ref(r) => {
                            let index = self.env.ref_ports[r].unwrap();
                            self.env.ports[index]
                                .as_bool()
                                .expect("if condition is undefined")
                        }
                    };

                    let target = if result { i.tbranch() } else { i.fbranch() };
                    *node = node.new_retain_comp(target);
                    true
                }
                ControlNode::While(w) => {
                    if w.cond_group().is_some() {
                        todo!("while statement has a with clause")
                    }

                    let target = GlobalPortRef::from_local(
                        w.cond_port(),
                        &self.env.cells[node.comp].unwrap_comp().index_bases,
                    );

                    let result = match target {
                        GlobalPortRef::Port(p) => self.env.ports[p]
                            .as_bool()
                            .expect("while condition is undefined"),
                        GlobalPortRef::Ref(r) => {
                            let index = self.env.ref_ports[r].unwrap();
                            self.env.ports[index]
                                .as_bool()
                                .expect("while condition is undefined")
                        }
                    };

                    if result {
                        // enter the body
                        *node = node.new_retain_comp(w.body());
                        true
                    } else {
                        // ascend the tree
                        node.mutate_into_next(self.env.ctx)
                    }
                }

                // ===== leaf nodes =====
                ControlNode::Empty(_) => node.mutate_into_next(self.env.ctx),
                ControlNode::Enable(e) => {
                    let done_local = self.env.ctx.primary[e.group()].done;
                    let done_idx = &self.env.cells[node.comp]
                        .as_comp()
                        .unwrap()
                        .index_bases
                        + done_local;

                    if !self.env.ports[done_idx].as_bool().unwrap_or_default() {
                        leaf_nodes.push(node.clone());
                        true
                    } else {
                        done_groups.push((
                            node.clone(),
                            self.env.ports[done_idx].clone(),
                        ));
                        // remove from the list now
                        false
                    }
                }
                ControlNode::Invoke(_) => todo!("invokes not implemented yet"),
            }
        });

        self.undef_all_ports();
        for (node, val) in &done_groups {
            match &self.env.ctx.primary[node.control_node_idx] {
                ControlNode::Enable(e) => {
                    let go_local = self.env.ctx.primary[e.group()].go;
                    let done_local = self.env.ctx.primary[e.group()].done;
                    let index_bases = &self.env.cells[node.comp]
                        .as_comp()
                        .unwrap()
                        .index_bases;
                    let done_idx = index_bases + done_local;
                    let go_idx = index_bases + go_local;

                    // retain done condition from before
                    self.env.ports[done_idx] = val.clone();
                    self.env.ports[go_idx] =
                        PortValue::new_implicit(Value::bit_high());
                }
                ControlNode::Invoke(_) => todo!(),
                _ => {
                    unreachable!("non-leaf node included in list of done nodes. This should never happen, please report it.")
                }
            }
        }

        for node in &leaf_nodes {
            match &self.env.ctx.primary[node.control_node_idx] {
                ControlNode::Enable(e) => {
                    let go_local = self.env.ctx.primary[e.group()].go;
                    let index_bases = &self.env.cells[node.comp]
                        .as_comp()
                        .unwrap()
                        .index_bases;

                    // set go high
                    let go_idx = index_bases + go_local;
                    self.env.ports[go_idx] =
                        PortValue::new_implicit(Value::bit_high());
                }
                ControlNode::Invoke(_) => todo!(),
                non_leaf => {
                    unreachable!("non-leaf node {:?} included in list of leaf nodes. This should never happen, please report it.", non_leaf)
                }
            }
        }

        self.simulate_combinational(&leaf_nodes)?;

        for cell in self.env.cells.values_mut() {
            match cell {
                CellLedger::Primitive { cell_dyn } => {
                    cell_dyn.exec_cycle(&mut self.env.ports)?;
                }
                CellLedger::Component(_) => {}
            }
        }

        // need to cleanup the finished groups
        for (node, _) in done_groups {
            if let Some(next) = ControlPoint::get_next(&node, self.env.ctx) {
                self.env.pc.insert_node(next)
            }
        }

        Ok(())
    }

    fn is_done(&self) -> bool {
        assert!(
            self.ctx().primary[self.ctx().entry_point].control.is_some(),
            "flat interpreter doesn't handle a fully structural entrypoint program yet"
        );
        // TODO griffin: need to handle structural components
        self.env.pc.is_done()
    }

    /// Evaluate the entire program
    pub fn run_program(&mut self) -> InterpreterResult<()> {
        while !self.is_done() {
            dbg!("calling step");
            // self.env.print_pc();
            self.print_env();
            self.step()?
        }
        Ok(())
    }

    fn evaluate_guard(
        &self,
        guard: GuardIdx,
        comp: GlobalCellIdx,
    ) -> Option<bool> {
        let guard = &self.ctx().primary[guard];
        match guard {
            Guard::True => Some(true),
            Guard::Or(a, b) => {
                let g1 = self.evaluate_guard(*a, comp)?;
                let g2 = self.evaluate_guard(*b, comp)?;
                Some(g1 || g2)
            }
            Guard::And(a, b) => {
                let g1 = self.evaluate_guard(*a, comp)?;
                let g2 = self.evaluate_guard(*b, comp)?;
                Some(g1 && g2)
            }
            Guard::Not(n) => Some(!self.evaluate_guard(*n, comp)?),
            Guard::Comp(c, a, b) => {
                let comp_v = self.env.cells[comp].unwrap_comp();

                let a = self.lookup_global_port_id(comp_v.convert_to_global(a));
                let b = self.lookup_global_port_id(comp_v.convert_to_global(b));

                let a_val = self.env.ports[a].val()?;
                let b_val = self.env.ports[b].val()?;
                match c {
                    calyx_ir::PortComp::Eq => a_val == b_val,
                    calyx_ir::PortComp::Neq => a_val != b_val,
                    calyx_ir::PortComp::Gt => a_val > b_val,
                    calyx_ir::PortComp::Lt => a_val < b_val,
                    calyx_ir::PortComp::Geq => a_val >= b_val,
                    calyx_ir::PortComp::Leq => a_val <= b_val,
                }
                .into()
            }
            Guard::Port(p) => {
                let comp_v = self.env.cells[comp].unwrap_comp();
                let p_idx =
                    self.lookup_global_port_id(comp_v.convert_to_global(p));
                self.env.ports[p_idx].as_bool()
            }
        }
    }

    fn undef_all_ports(&mut self) {
        for (_idx, port_val) in self.env.ports.iter_mut() {
            port_val.set_undef();
        }
    }

    fn simulate_combinational(
        &mut self,
        control_points: &[ControlPoint],
    ) -> InterpreterResult<()> {
        let assigns_bundle = self.get_assignments(control_points);
        let mut has_changed = true;

        while has_changed {
            has_changed = false;

            // evaluate all the assignments and make updates
            for (cell, assigns) in assigns_bundle.iter() {
                for assign_idx in assigns {
                    let assign = &self.env.ctx.primary[assign_idx];

                    // TODO griffin: Come back to this unwrap default later
                    // since we may want to do something different if the guard
                    // does not have a defined value
                    if self.evaluate_guard(assign.guard, *cell).unwrap_or(false)
                    {
                        let val = self.get_value(&assign.src, *cell);
                        let dest = self.get_global_idx(&assign.dst, *cell);
                        if let Some(v) = val.as_option() {
                            self.env.ports.insert_val(
                                dest,
                                AssignedValue::new(v.val().clone(), assign_idx),
                            )?;
                        } else if self.env.ports[dest].is_def() {
                            todo!("Raise an error here since this assignment is undefining things")
                        }
                    }
                }
            }

            // Run all the primitives
            let changed: bool = self
                .env
                .cells
                .range()
                .iter()
                .filter_map(|x| match &mut self.env.cells[x] {
                    CellLedger::Primitive { cell_dyn } => {
                        Some(cell_dyn.exec_comb(&mut self.env.ports))
                    }
                    CellLedger::Component(_) => None,
                })
                .fold_ok(UpdateStatus::Unchanged, |has_changed, update| {
                    has_changed | update
                })?
                .as_bool();

            has_changed |= changed;
        }

        Ok(())
    }

    pub fn _main_test(&mut self) {
        self.env.print_pc();
        let _ = self.run_program();
        self.env.print_pc();
        self.print_env();
    }
}
