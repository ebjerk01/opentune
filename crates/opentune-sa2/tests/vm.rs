//! Synthetic SA2 VM tests.
//!
//! Every program here is hand-written for the test. None is real OEM SA2
//! bytecode; the vectors exist only to pin down the documented opcode semantics.
use opentune_sa2::op::*;
use opentune_sa2::{assemble, execute, Op, Program, Sa2Error, Sa2SeedKey};
use opentune_core::protocol::SeedKey;

/// Run a list of `Op`s over a seed via assemble + execute.
fn run_ops(seed: &[u8], ops: &[Op]) -> Vec<u8> {
    execute(seed, &assemble(ops)).expect("program should run")
}

#[test]
fn xor_immediate() {
    // acc = 0x12345678 ^ 0xFFFFFFFF = 0xEDCBA987
    let key = run_ops(
        &[0x12, 0x34, 0x56, 0x78],
        &[Op::XorImm(0xFFFF_FFFF), Op::OutKey(4)],
    );
    assert_eq!(key, vec![0xED, 0xCB, 0xA9, 0x87]);
}

#[test]
fn add_immediate_wraps() {
    // 0xFFFFFFFF + 2 = 0x00000001 (wrapping)
    let key = run_ops(
        &[0xFF, 0xFF, 0xFF, 0xFF],
        &[Op::AddImm(2), Op::OutKey(4)],
    );
    assert_eq!(key, vec![0x00, 0x00, 0x00, 0x01]);
}

#[test]
fn sub_immediate_wraps() {
    // 0x00000001 - 2 = 0xFFFFFFFF (wrapping)
    let key = run_ops(&[0x00, 0x01], &[Op::SubImm(2), Op::OutKey(4)]);
    assert_eq!(key, vec![0xFF, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn rotate_left_immediate() {
    // 0x80000001 rotate_left 1 = 0x00000003
    let key = run_ops(
        &[0x80, 0x00, 0x00, 0x01],
        &[Op::RolImm(1), Op::OutKey(4)],
    );
    assert_eq!(key, vec![0x00, 0x00, 0x00, 0x03]);
}

#[test]
fn rotate_right_immediate() {
    // 0x00000003 rotate_right 1 = 0x80000001
    let key = run_ops(
        &[0x00, 0x00, 0x00, 0x03],
        &[Op::RorImm(1), Op::OutKey(4)],
    );
    assert_eq!(key, vec![0x80, 0x00, 0x00, 0x01]);
}

#[test]
fn shift_left_and_right_immediate() {
    let key = run_ops(&[0x00, 0x00, 0x00, 0x0F], &[Op::ShlImm(4), Op::OutKey(4)]);
    assert_eq!(key, vec![0x00, 0x00, 0x00, 0xF0]);
    let key = run_ops(&[0x00, 0x00, 0x00, 0xF0], &[Op::ShrImm(4), Op::OutKey(4)]);
    assert_eq!(key, vec![0x00, 0x00, 0x00, 0x0F]);
}

#[test]
fn and_or_immediate() {
    let key = run_ops(
        &[0x12, 0x34, 0x56, 0x78],
        &[Op::AndImm(0x0000_FFFF), Op::OrImm(0xFF00_0000), Op::OutKey(4)],
    );
    assert_eq!(key, vec![0xFF, 0x00, 0x56, 0x78]);
}

#[test]
fn swap_ops() {
    assert_eq!(
        run_ops(&[0x12, 0x34, 0x56, 0x78], &[Op::SwapBytes, Op::OutKey(4)]),
        vec![0x78, 0x56, 0x34, 0x12]
    );
    assert_eq!(
        run_ops(&[0x12, 0x34, 0x56, 0x78], &[Op::SwapWords, Op::OutKey(4)]),
        vec![0x56, 0x78, 0x12, 0x34]
    );
    assert_eq!(
        run_ops(&[0x12, 0x34, 0x56, 0x78], &[Op::SwapNibbles, Op::OutKey(4)]),
        vec![0x21, 0x43, 0x65, 0x87]
    );
}

#[test]
fn register_load_store_and_add() {
    // store acc into reg0, load an immediate, add reg0 back.
    let key = run_ops(
        &[0x00, 0x00, 0x00, 0x10],
        &[
            Op::StoreReg(0),       // reg0 = 0x10
            Op::LoadImm(0x20),     // acc  = 0x20
            Op::AddReg(0),         // acc  = 0x30
            Op::OutKey(4),
        ],
    );
    assert_eq!(key, vec![0x00, 0x00, 0x00, 0x30]);
}

#[test]
fn eight_byte_seed_second_word() {
    // 8-byte seed => word0 in acc, word1 in reg1 and via LOAD_SEED 1.
    let seed = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22];
    let key = run_ops(&seed, &[Op::LoadSeed(1), Op::OutKey(4)]);
    assert_eq!(key, vec![0xEE, 0xFF, 0x11, 0x22]);

    // acc(word0) + reg1(word1), computed independently with wrapping add.
    let expect = 0xAABB_CCDDu32.wrapping_add(0xEEFF_1122);
    let key = run_ops(&seed, &[Op::AddReg(1), Op::OutKey(4)]);
    assert_eq!(key, expect.to_be_bytes().to_vec());
}

#[test]
fn subroutine_call_ret() {
    // main: CALL sub; OUTKEY 4; HALT.  sub: ADD_IMM 0x11; RET.
    // Targets are byte offsets, so assemble and compute them explicitly.
    // CALL(3 bytes)@0, OUTKEY(2)@3, HALT(1)@5, ADD_IMM(5)@6, RET(1)@11.
    let program = assemble(&[
        Op::Call(6),
        Op::OutKey(4),
        Op::Halt,
        Op::AddImm(0x11),
        Op::Ret,
    ]);
    let key = execute(&[0x00, 0x00, 0x00, 0x10], &program).unwrap();
    assert_eq!(key, vec![0x00, 0x00, 0x00, 0x21]);
}

#[test]
fn countdown_loop() {
    // reg0 = 3; acc = seed word0; { acc += 1 } loop reg0; OUTKEY 4.
    // LOAD_IMM(5)@0, STORE_REG(2)@5, LOAD_SEED(2)@7, ADD_IMM(5)@9,
    // LOOP(4)@14, OUTKEY(2)@18, HALT(1)@20.  loop body target = 9.
    let program = assemble(&[
        Op::LoadImm(3),
        Op::StoreReg(0),
        Op::LoadSeed(0),
        Op::AddImm(1),
        Op::Loop { reg: 0, target: 9 },
        Op::OutKey(4),
        Op::Halt,
    ]);
    let key = execute(&[0x00, 0x00, 0x00, 0x05], &program).unwrap();
    assert_eq!(key, vec![0x00, 0x00, 0x00, 0x08]); // 5 + 3 iterations
}

#[test]
fn partial_and_multiple_outkey() {
    // 0x1122AABB: OUTKEY 2 twice => the low two bytes, emitted twice.
    let key = run_ops(
        &[0x11, 0x22, 0xAA, 0xBB],
        &[Op::OutKey(2), Op::OutKey(2)],
    );
    assert_eq!(key, vec![0xAA, 0xBB, 0xAA, 0xBB]);
}

#[test]
fn default_key_without_outkey() {
    // No OUTKEY => key is the final accumulator, big-endian.
    let key = run_ops(&[0x00, 0x01], &[Op::AddImm(1)]);
    assert_eq!(key, vec![0x00, 0x00, 0x00, 0x02]);
}

#[test]
fn parse_round_trip() {
    let ops = vec![
        Op::Nop,
        Op::LoadImm(0xDEAD_BEEF),
        Op::StoreReg(3),
        Op::LoadSeed(1),
        Op::AddImm(0x10),
        Op::SubReg(2),
        Op::AndImm(0x00FF_00FF),
        Op::OrReg(1),
        Op::XorImm(0x1),
        Op::RolImm(7),
        Op::RorReg(4),
        Op::ShlImm(3),
        Op::ShrImm(5),
        Op::SwapWords,
        Op::SwapBytes,
        Op::SwapNibbles,
        Op::Call(0x0030),
        Op::Loop { reg: 2, target: 0x0005 },
        Op::Jmp(0x0001),
        Op::Ret,
        Op::OutKey(4),
        Op::Halt,
    ];
    let bytes = assemble(&ops);
    let program = Program::parse(&bytes).expect("round-trip parse");
    assert_eq!(program.ops(), ops.as_slice());
    assert_eq!(assemble(program.ops()), bytes);
}

#[test]
fn unknown_opcode_rejected() {
    // 0xAB is not a defined opcode.
    let err = execute(&[0x00, 0x01], &[0xAB]).unwrap_err();
    assert!(matches!(
        err,
        Sa2Error::UnknownOpcode { opcode: 0xAB, offset: 0 }
    ));
}

#[test]
fn truncated_immediate_rejected() {
    let err = execute(&[0x00, 0x01], &[OP_LOAD_IMM, 0x00]).unwrap_err();
    assert!(matches!(err, Sa2Error::Truncated { opcode: OP_LOAD_IMM, offset: 0 }));
}

#[test]
fn bad_register_rejected() {
    let err = execute(&[0x00, 0x01], &[OP_LOAD_REG, REG_COUNT]).unwrap_err();
    assert!(matches!(err, Sa2Error::BadRegister { index, .. } if index == REG_COUNT));
}

#[test]
fn bad_outkey_operand_rejected() {
    let err = execute(&[0x00, 0x01], &[OP_OUTKEY, 0x05]).unwrap_err();
    assert!(matches!(err, Sa2Error::BadOperand { opcode: OP_OUTKEY, .. }));
}

#[test]
fn seed_length_rejected() {
    assert!(matches!(
        execute(&[0x00], &assemble(&[Op::OutKey(4)])).unwrap_err(),
        Sa2Error::SeedLength(1)
    ));
    assert!(matches!(
        execute(&[0u8; 9], &assemble(&[Op::OutKey(4)])).unwrap_err(),
        Sa2Error::SeedLength(9)
    ));
}

#[test]
fn bad_branch_target_rejected() {
    // Call to offset 0x00FF, which is not an instruction boundary.
    let program = assemble(&[Op::Call(0x00FF), Op::Halt]);
    assert!(matches!(
        execute(&[0x00, 0x01], &program).unwrap_err(),
        Sa2Error::BadTarget { target: 0xFF }
    ));
}

#[test]
fn ret_without_call_rejected() {
    assert!(matches!(
        execute(&[0x00, 0x01], &assemble(&[Op::Ret])).unwrap_err(),
        Sa2Error::CallStackUnderflow
    ));
}

#[test]
fn infinite_loop_hits_step_budget() {
    // JMP to its own offset 0 => never terminates.
    let program = assemble(&[Op::Jmp(0)]);
    assert!(matches!(
        execute(&[0x00, 0x01], &program).unwrap_err(),
        Sa2Error::StepBudgetExceeded(_)
    ));
}

#[test]
fn seedkey_trait_round_trip() {
    let program = assemble(&[Op::XorImm(0xFFFF_FFFF), Op::OutKey(4)]);
    let sk = Sa2SeedKey::new(program.clone());
    assert_eq!(sk.id(), "sa2");

    let seed = [0x12, 0x34, 0x56, 0x78];
    let via_trait = sk.compute(&seed, 0x01).unwrap();
    let via_vm = execute(&seed, &program).unwrap();
    assert_eq!(via_trait, via_vm);
    assert_eq!(via_trait, vec![0xED, 0xCB, 0xA9, 0x87]);
}

#[test]
fn seedkey_surfaces_errors() {
    // Unknown opcode should come back as a core protocol error, not a panic.
    let sk = Sa2SeedKey::new(vec![0xAB]);
    assert!(sk.compute(&[0x00, 0x01], 0).is_err());
}
