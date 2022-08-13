//! Implementation of a standard Riscv64 ABI.

use core::panic;

use crate::ir;

use crate::ir::types::*;
use crate::ir::AbiParam;
use crate::ir::ExternalName;
use crate::ir::MemFlags;
use crate::isa;

use crate::isa::riscv64::{inst::EmitState, inst::*};
use crate::isa::CallConv;
use crate::machinst::*;

use crate::ir::LibCall;
use crate::ir::Signature;
use crate::isa::riscv64::settings::Flags as RiscvFlags;
use crate::isa::unwind::UnwindInst;
use crate::settings;
use crate::CodegenError;
use crate::CodegenResult;
use alloc::boxed::Box;
use alloc::vec::Vec;
use regalloc2::PRegSet;
use regs::x_reg;

use smallvec::{smallvec, SmallVec};

/// Support for the Riscv64 ABI from the callee side (within a function body).
pub(crate) type Riscv64Callee = ABICalleeImpl<Riscv64MachineDeps>;

/// Support for the Riscv64 ABI from the caller side (at a callsite).
pub(crate) type Riscv64ABICaller = ABICallerImpl<Riscv64MachineDeps>;

/// This is the limit for the size of argument and return-value areas on the
/// stack. We place a reasonable limit here to avoid integer overflow issues
/// with 32-bit arithmetic: for now, 128 MB.
static STACK_ARG_RET_SIZE_LIMIT: u64 = 128 * 1024 * 1024;

/// Riscv64-specific ABI behavior. This struct just serves as an implementation
/// point for the trait; it is never actually instantiated.
pub(crate) struct Riscv64MachineDeps;

impl IsaFlags for RiscvFlags {}

impl ABIMachineSpec for Riscv64MachineDeps {
    type I = Inst;
    type F = RiscvFlags;

    fn word_bits() -> u32 {
        64
    }

    /// Return required stack alignment in bytes.
    fn stack_align(_call_conv: isa::CallConv) -> u32 {
        16
    }

    fn compute_arg_locs(
        call_conv: isa::CallConv,
        _flags: &settings::Flags,
        params: &[ir::AbiParam],
        args_or_rets: ArgsOrRets,
        add_ret_area_ptr: bool,
    ) -> CodegenResult<(ABIArgVec, i64, Option<usize>)> {
        // all registers can be used as parameter.
        // both start and all included.
        let (x_start, x_end, f_start, f_end) = if args_or_rets == ArgsOrRets::Args {
            (10, 17, 10, 17)
        } else {
            (10, 11, 10, 11)
        };
        let mut next_x_reg = x_start;
        let mut next_f_reg = f_start;
        // stack space
        let mut next_stack: u64 = 0;
        let mut abi_args = smallvec![];
        // When run out register , We should use stack space for parameter,
        // We should deal with parameter backwards.
        // But We need result to be the same order with `params`.
        let mut abi_args_for_stack = smallvec![];
        let mut step_last_parameter = {
            let mut params_last = if params.len() > 0 {
                params.len() - 1
            } else {
                0
            };
            move || -> AbiParam {
                params_last -= 1;
                params[params_last].clone()
            }
        };

        for i in 0..params.len() {
            let mut param = params[i];
            let run_out_of_registers = {
                (param.value_type.is_float() && next_f_reg > f_end)
                    || (param.value_type.is_int() && next_x_reg > x_end)
            };
            param = if run_out_of_registers {
                step_last_parameter()
            } else {
                param
            };
            // Validate "purpose".
            match &param.purpose {
                &ir::ArgumentPurpose::VMContext
                | &ir::ArgumentPurpose::Normal
                | &ir::ArgumentPurpose::StructReturn
                | &ir::ArgumentPurpose::StackLimit
                | &ir::ArgumentPurpose::StructArgument(_) => {}
                _ => panic!(
                    "Unsupported argument purpose {:?} in signature: {:?}",
                    param.purpose, params
                ),
            }
            let abi_args = if run_out_of_registers {
                &mut abi_args_for_stack
            } else {
                &mut abi_args
            };
            if let Some(p) = special_purpose_register(param) {
                abi_args.push(p);
                continue;
            }
            if let ir::ArgumentPurpose::StructArgument(size) = param.purpose {
                let offset = next_stack;
                assert!(size % 8 == 0, "StructArgument size is not properly aligned");
                next_stack += size as u64;
                abi_args.push(ABIArg::StructArg {
                    pointer: None,
                    offset: offset as i64,
                    size: size as u64,
                    purpose: param.purpose,
                });
                continue;
            }
            match param.value_type {
                F32 | F64 => {
                    if next_f_reg <= f_end {
                        let arg = ABIArg::reg(
                            f_reg(next_f_reg).to_real_reg().unwrap(),
                            param.value_type,
                            param.extension,
                            param.purpose,
                        );
                        abi_args.push(arg);
                        next_f_reg += 1;
                    } else {
                        let arg = ABIArg::stack(
                            next_stack as i64,
                            param.value_type,
                            param.extension,
                            param.purpose,
                        );
                        abi_args.push(arg);
                        next_stack += 8
                    }
                }
                B1 | B8 | B16 | B32 | B64 | I8 | I16 | I32 | I64 | R32 | R64 => {
                    if next_x_reg <= x_end {
                        let arg = ABIArg::reg(
                            x_reg(next_x_reg).to_real_reg().unwrap(),
                            param.value_type,
                            param.extension,
                            param.purpose,
                        );
                        next_x_reg += 1;
                        abi_args.push(arg);
                    } else {
                        let arg = ABIArg::stack(
                            next_stack as i64,
                            param.value_type,
                            param.extension,
                            param.purpose,
                        );
                        abi_args.push(arg);
                        next_stack += 8
                    }
                }
                I128 | B128 => {
                    let elem_type = if param.value_type == I128 { I64 } else { B64 };
                    let mut slots = smallvec![];
                    if next_x_reg + 1 <= x_end {
                        for i in 0..2 {
                            slots.push(ABIArgSlot::Reg {
                                reg: x_reg(next_x_reg + i).to_real_reg().unwrap(),
                                ty: elem_type,
                                extension: param.extension,
                            });
                        }
                        next_x_reg += 2;
                    } else if next_x_reg <= x_end {
                        // put in register
                        slots.push(ABIArgSlot::Reg {
                            reg: x_reg(next_x_reg).to_real_reg().unwrap(),
                            ty: elem_type,
                            extension: param.extension,
                        });
                        next_x_reg += 1;
                        slots.push(ABIArgSlot::Stack {
                            offset: next_stack as i64,
                            ty: elem_type,
                            extension: param.extension,
                        });
                        next_stack += 8;
                    } else {
                        for _i in 0..2 {
                            slots.push(ABIArgSlot::Stack {
                                offset: next_stack as i64,
                                ty: elem_type,
                                extension: param.extension,
                            });
                            next_stack += 8;
                        }
                    }
                    abi_args.push(ABIArg::Slots {
                        slots,
                        purpose: ir::ArgumentPurpose::Normal,
                    });
                }
                _ => todo!("type not supported {}", param.value_type),
            };
        }

        abi_args_for_stack.reverse();
        abi_args.extend(abi_args_for_stack.into_iter());
        let pos: Option<usize> = if add_ret_area_ptr {
            assert!(ArgsOrRets::Args == args_or_rets);
            if next_x_reg <= x_end {
                let arg = ABIArg::reg(
                    x_reg(next_x_reg).to_real_reg().unwrap(),
                    I64,
                    ir::ArgumentExtension::None,
                    ir::ArgumentPurpose::Normal,
                );
                abi_args.push(arg);
                Some(abi_args.len() - 1)
            } else {
                let arg = ABIArg::stack(
                    next_stack as i64,
                    I64,
                    ir::ArgumentExtension::None,
                    ir::ArgumentPurpose::Normal,
                );
                abi_args.push(arg);
                next_stack += 8;
                Some(abi_args.len() - 1)
            }
        } else {
            None
        };
        next_stack = align_to(next_stack, Self::stack_align(call_conv) as u64);

        // To avoid overflow issues, limit the arg/return size to something
        // reasonable -- here, 128 MB.
        if next_stack > STACK_ARG_RET_SIZE_LIMIT {
            return Err(CodegenError::ImplLimitExceeded);
        }

        CodegenResult::Ok((abi_args, next_stack as i64, pos))
    }

    fn fp_to_arg_offset(_call_conv: isa::CallConv, _flags: &settings::Flags) -> i64 {
        // lr fp.
        16
    }

    fn gen_load_stack(mem: StackAMode, into_reg: Writable<Reg>, ty: Type) -> Inst {
        Inst::gen_load(into_reg, mem.into(), ty, MemFlags::trusted())
    }

    fn gen_store_stack(mem: StackAMode, from_reg: Reg, ty: Type) -> Inst {
        Inst::gen_store(mem.into(), from_reg, ty, MemFlags::trusted())
    }

    fn gen_move(to_reg: Writable<Reg>, from_reg: Reg, ty: Type) -> Inst {
        Inst::gen_move(to_reg, from_reg, ty)
    }

    fn gen_extend(
        to_reg: Writable<Reg>,
        from_reg: Reg,
        signed: bool,
        from_bits: u8,
        to_bits: u8,
    ) -> Inst {
        assert!(from_bits < to_bits);
        Inst::Extend {
            rd: to_reg,
            rn: from_reg,
            signed,
            from_bits,
            to_bits,
        }
    }

    fn get_ext_mode(
        _call_conv: isa::CallConv,
        specified: ir::ArgumentExtension,
    ) -> ir::ArgumentExtension {
        specified
    }

    fn gen_ret(_setup_frame: bool, _isa_flags: &Self::F, rets: Vec<Reg>) -> Inst {
        Inst::Ret { rets }
    }

    fn get_stacklimit_reg() -> Reg {
        spilltmp_reg()
    }

    fn gen_add_imm(into_reg: Writable<Reg>, from_reg: Reg, imm: u32) -> SmallInstVec<Inst> {
        let mut insts = SmallInstVec::new();
        if let Some(imm12) = Imm12::maybe_from_u64(imm as u64) {
            insts.push(Inst::AluRRImm12 {
                alu_op: AluOPRRI::Andi,
                rd: into_reg,
                rs: from_reg,
                imm12,
            });
        } else {
            insts.extend(Inst::load_constant_u32(
                writable_spilltmp_reg2(),
                imm as u64,
            ));
            insts.push(Inst::AluRRR {
                alu_op: AluOPRRR::Add,
                rd: into_reg,
                rs1: spilltmp_reg2(),
                rs2: from_reg,
            });
        }
        insts
    }

    fn gen_stack_lower_bound_trap(limit_reg: Reg) -> SmallInstVec<Inst> {
        let mut insts = SmallVec::new();
        insts.push(Inst::TrapIfC {
            cc: IntCC::UnsignedLessThan,
            rs1: stack_reg(),
            rs2: limit_reg,
            trap_code: ir::TrapCode::StackOverflow,
        });
        insts
    }

    fn gen_get_stack_addr(mem: StackAMode, into_reg: Writable<Reg>, _ty: Type) -> Inst {
        Inst::LoadAddr {
            rd: into_reg,
            mem: mem.into(),
        }
    }

    fn gen_load_base_offset(into_reg: Writable<Reg>, base: Reg, offset: i32, ty: Type) -> Inst {
        let mem = AMode::RegOffset(base, offset as i64, ty);
        Inst::gen_load(into_reg, mem, ty, MemFlags::trusted())
    }

    fn gen_store_base_offset(base: Reg, offset: i32, from_reg: Reg, ty: Type) -> Inst {
        let mem = AMode::RegOffset(base, offset as i64, ty);
        Inst::gen_store(mem, from_reg, ty, MemFlags::trusted())
    }

    fn gen_sp_reg_adjust(amount: i32) -> SmallInstVec<Inst> {
        let mut insts = SmallVec::new();
        if amount == 0 {
            return insts;
        }
        insts.push(Inst::AjustSp {
            amount: amount as i64,
        });
        insts
    }

    fn gen_nominal_sp_adj(offset: i32) -> Inst {
        Inst::VirtualSPOffsetAdj {
            amount: offset as i64,
        }
    }

    fn gen_prologue_frame_setup(flags: &settings::Flags) -> SmallInstVec<Inst> {
        // add  sp , sp. -16    ;; alloc stack space for fp.
        // st   ra , sp+8       ;; save ra.
        // st   fp , sp+0       ;; store old fp.
        // mv   fp , sp          ;; set fp to sp.
        let mut insts = SmallVec::new();
        insts.push(Inst::AjustSp { amount: -16 });
        insts.push(Self::gen_store_stack(
            StackAMode::SPOffset(8, I64),
            link_reg(),
            I64,
        ));
        insts.push(Self::gen_store_stack(
            StackAMode::SPOffset(0, I64),
            fp_reg(),
            I64,
        ));
        if flags.unwind_info() {
            insts.push(Inst::Unwind {
                inst: UnwindInst::PushFrameRegs {
                    offset_upward_to_caller_sp: 16, // FP, LR
                },
            });
        }
        insts.push(Inst::Mov {
            rd: writable_fp_reg(),
            rm: stack_reg(),
            ty: I64,
        });
        insts
    }
    /// reverse of gen_prologue_frame_setup.
    fn gen_epilogue_frame_restore(_: &settings::Flags) -> SmallInstVec<Inst> {
        let mut insts = SmallVec::new();
        insts.push(Self::gen_load_stack(
            StackAMode::SPOffset(8, I64),
            writable_link_reg(),
            I64,
        ));
        insts.push(Self::gen_load_stack(
            StackAMode::SPOffset(0, I64),
            writable_fp_reg(),
            I64,
        ));
        insts.push(Inst::AjustSp { amount: 16 });
        insts
    }

    fn gen_probestack(frame_size: u32) -> SmallInstVec<Self::I> {
        let mut insts = SmallVec::new();
        insts.extend(Inst::load_constant_u32(writable_a0(), frame_size as u64));
        insts.push(Inst::Call {
            info: Box::new(CallInfo {
                dest: ExternalName::LibCall(LibCall::Probestack),
                uses: smallvec![a0()],
                defs: smallvec![],
                clobbers: PRegSet::empty(),
                opcode: Opcode::Call,
                callee_callconv: CallConv::SystemV,
                caller_callconv: CallConv::SystemV,
            }),
        });
        insts
    }

    // Returns stack bytes used as well as instructions. Does not adjust
    // nominal SP offset; abi_impl generic code will do that.
    fn gen_clobber_save(
        _call_conv: isa::CallConv,
        setup_frame: bool,
        flags: &settings::Flags,
        clobbered_callee_saves: &[Writable<RealReg>],
        fixed_frame_storage_size: u32,
        _outgoing_args_size: u32,
    ) -> (u64, SmallVec<[Inst; 16]>) {
        let mut insts = SmallVec::new();
        let clobbered_size = compute_clobber_size(&clobbered_callee_saves);
        // Adjust the stack pointer downward for clobbers and the function fixed
        // frame (spillslots and storage slots).
        let stack_size = fixed_frame_storage_size + clobbered_size;

        if flags.unwind_info() && setup_frame {
            // The *unwind* frame (but not the actual frame) starts at the
            // clobbers, just below the saved FP/LR pair.
            insts.push(Inst::Unwind {
                inst: UnwindInst::DefineNewFrame {
                    offset_downward_to_clobbers: clobbered_size,
                    offset_upward_to_caller_sp: 16, // FP, LR
                },
            });
        }
        // Store each clobbered register in order at offsets from SP,
        // placing them above the fixed frame slots.
        if stack_size > 0 {
            insts.push(Inst::AjustSp {
                amount: -(stack_size as i64),
            });
            // since we use fp, we didn't need use UnwindInst::StackAlloc.
            let mut cur_offset = 0;
            for reg in clobbered_callee_saves {
                let r_reg = reg.to_reg();
                let ty = match r_reg.class() {
                    regalloc2::RegClass::Int => I64,
                    regalloc2::RegClass::Float => F64,
                };
                if flags.unwind_info() {
                    insts.push(Inst::Unwind {
                        inst: UnwindInst::SaveReg {
                            clobber_offset: cur_offset as u32,
                            reg: r_reg,
                        },
                    });
                }
                insts.push(Self::gen_store_stack(
                    StackAMode::SPOffset(cur_offset, ty),
                    real_reg_to_reg(reg.to_reg()),
                    ty,
                ));
                cur_offset += 8
            }
        }
        (clobbered_size as u64, insts)
    }

    fn gen_clobber_restore(
        call_conv: isa::CallConv,
        sig: &Signature,
        _flags: &settings::Flags,
        clobbers: &[Writable<RealReg>],
        fixed_frame_storage_size: u32,
        _outgoing_args_size: u32,
    ) -> SmallVec<[Inst; 16]> {
        let mut insts = SmallVec::new();
        let clobbered_callee_saves =
            Self::get_clobbered_callee_saves(call_conv, _flags, sig, clobbers);
        let stack_size = fixed_frame_storage_size + compute_clobber_size(&clobbered_callee_saves);
        let mut cur_offset = 0;
        for reg in &clobbered_callee_saves {
            let rreg = reg.to_reg();
            let ty = match rreg.class() {
                regalloc2::RegClass::Int => I64,
                regalloc2::RegClass::Float => F64,
            };
            insts.push(Self::gen_load_stack(
                StackAMode::SPOffset(cur_offset, ty),
                Writable::from_reg(real_reg_to_reg(reg.to_reg())),
                ty,
            ));
            cur_offset += 8
        }
        if stack_size > 0 {
            insts.push(Inst::AjustSp {
                amount: stack_size as i64,
            });
        }
        insts
    }

    fn gen_call(
        dest: &CallDest,
        uses: SmallVec<[Reg; 8]>,
        defs: SmallVec<[Writable<Reg>; 8]>,
        clobbers: PRegSet,
        opcode: ir::Opcode,
        tmp: Writable<Reg>,
        callee_conv: isa::CallConv,
        caller_conv: isa::CallConv,
    ) -> SmallVec<[Self::I; 2]> {
        let mut insts = SmallVec::new();
        fn use_direct_call(name: &ir::ExternalName, distance: RelocDistance) -> bool {
            if let &ExternalName::User {
                namespace: _namespace,
                index: _index,
            } = name
            {
                if RelocDistance::Near == distance {
                    return true;
                }
            }
            false
        }
        match &dest {
            &CallDest::ExtName(ref name, distance) => {
                let direct = use_direct_call(name, *distance);
                if direct {
                    insts.push(Inst::Call {
                        info: Box::new(CallInfo {
                            uses,
                            defs,
                            opcode,
                            caller_callconv: caller_conv,
                            callee_callconv: callee_conv,
                            dest: name.clone(),
                            clobbers,
                        }),
                    });
                } else {
                    insts.push(Inst::LoadExtName {
                        rd: tmp,
                        name: Box::new(name.clone()),
                        offset: 0,
                    });
                    insts.push(Inst::CallInd {
                        info: Box::new(CallIndInfo {
                            rn: tmp.to_reg(),
                            uses,
                            defs,
                            opcode,
                            caller_callconv: caller_conv,
                            callee_callconv: callee_conv,
                            clobbers,
                        }),
                    });
                }
            }
            &CallDest::Reg(reg) => insts.push(Inst::CallInd {
                info: Box::new(CallIndInfo {
                    rn: *reg,
                    uses,
                    defs,
                    opcode,
                    caller_callconv: caller_conv,
                    callee_callconv: callee_conv,
                    clobbers,
                }),
            }),
        }
        insts
    }

    fn gen_memcpy(
        call_conv: isa::CallConv,
        dst: Reg,
        src: Reg,
        size: usize,
    ) -> SmallVec<[Self::I; 8]> {
        let mut insts = SmallVec::new();
        let arg0 = writable_a0();
        let arg1 = writable_a1();
        let arg2 = writable_a2();
        insts.push(Inst::gen_move(arg0, dst, I64));
        insts.push(Inst::gen_move(arg1, src, I64));
        insts.extend(Inst::load_constant_u64(arg2, size as u64));
        insts.push(Inst::Call {
            info: Box::new(CallInfo {
                dest: ExternalName::LibCall(LibCall::Memcpy),
                uses: smallvec![arg0.to_reg(), arg1.to_reg(), arg2.to_reg()],
                defs: smallvec![],
                clobbers: Self::get_regs_clobbered_by_call(call_conv),
                opcode: Opcode::Call,
                caller_callconv: call_conv,
                callee_callconv: call_conv,
            }),
        });
        insts
    }

    fn get_number_of_spillslots_for_value(rc: RegClass, _target_vector_bytes: u32) -> u32 {
        // We allocate in terms of 8-byte slots.
        match rc {
            RegClass::Int => 1,
            RegClass::Float => 1,
        }
    }

    /// Get the current virtual-SP offset from an instruction-emission state.
    fn get_virtual_sp_offset_from_state(s: &EmitState) -> i64 {
        s.virtual_sp_offset
    }

    /// Get the nominal-SP-to-FP offset from an instruction-emission state.
    fn get_nominal_sp_to_fp(s: &EmitState) -> i64 {
        s.nominal_sp_to_fp
    }

    fn get_regs_clobbered_by_call(_call_conv_of_callee: isa::CallConv) -> PRegSet {
        let mut v = PRegSet::empty();
        for (k, need_save) in CALLER_SAVE_X_REG.iter().enumerate() {
            if !*need_save {
                continue;
            }
            v.add(px_reg(k));
        }
        for (k, need_save) in CALLER_SAVE_F_REG.iter().enumerate() {
            if !*need_save {
                continue;
            }
            v.add(pf_reg(k));
        }
        v
    }

    fn get_clobbered_callee_saves(
        call_conv: isa::CallConv,
        _flags: &settings::Flags,
        _sig: &Signature,
        regs: &[Writable<RealReg>],
    ) -> Vec<Writable<RealReg>> {
        let mut regs: Vec<Writable<RealReg>> = regs
            .iter()
            .cloned()
            .filter(|r| is_reg_saved_in_prologue(call_conv, r.to_reg()))
            .collect();

        regs.sort();
        regs
    }

    fn is_frame_setup_needed(
        is_leaf: bool,
        stack_args_size: u32,
        num_clobbered_callee_saves: usize,
        fixed_frame_storage_size: u32,
    ) -> bool {
        true
        // !is_leaf
        //     // The function arguments that are passed on the stack are addressed
        //     // relative to the Frame Pointer.
        //     || stack_args_size > 0
        //     || num_clobbered_callee_saves > 0
        //     || fixed_frame_storage_size > 0
    }
}

const CALLER_SAVE_X_REG: [bool; 32] = [
    false, true, false, false, false, true, true, true, // 0-7
    false, false, true, true, true, true, true, true, // 8-15
    true, true, false, false, false, false, false, false, // 16-23
    false, false, false, false, true, true, true, true, // 24-31
];
const CALLEE_SAVE_X_REG: [bool; 32] = [
    false, false, true, false, false, false, false, false, // 0-7
    true, true, false, false, false, false, false, false, // 8-15
    false, false, true, true, true, true, true, true, // 16-23
    true, true, true, true, false, false, false, false, // 24-31
];
const CALLER_SAVE_F_REG: [bool; 32] = [
    true, true, true, true, true, true, true, true, // 0-7
    false, true, true, true, true, true, true, true, // 8-15
    true, true, false, false, false, false, false, false, // 16-23
    false, false, false, false, true, true, true, true, // 24-31
];
const CALLEE_SAVE_F_REG: [bool; 32] = [
    false, false, false, false, false, false, false, false, // 0-7
    true, false, false, false, false, false, false, false, // 8-15
    false, false, true, true, true, true, true, true, // 16-23
    true, true, true, true, false, false, false, false, // 24-31
];

/// this should be the registers must be save by callee
#[inline]
fn is_reg_saved_in_prologue(_conv: CallConv, reg: RealReg) -> bool {
    if reg.class() == RegClass::Int {
        CALLEE_SAVE_X_REG[reg.hw_enc() as usize]
    } else {
        CALLEE_SAVE_F_REG[reg.hw_enc() as usize]
    }
}

fn compute_clobber_size(clobbers: &[Writable<RealReg>]) -> u32 {
    let mut clobbered_size = 0;
    for reg in clobbers {
        match reg.to_reg().class() {
            RegClass::Int => {
                clobbered_size += 8;
            }
            RegClass::Float => {
                clobbered_size += 8;
            }
        }
    }
    align_to(clobbered_size, 16)
}

fn special_purpose_register(p: AbiParam) -> Option<ABIArg> {
    match p.purpose {
        // ir::ArgumentPurpose::VMContext => {
        //     assert!(p.value_type == I64);
        //     Some(ABIArg::reg(
        //         x_reg(3).to_real_reg().unwrap(),
        //         p.value_type,
        //         p.extension,
        //         p.purpose,
        //     ))
        // }
        _ => None,
    }
}