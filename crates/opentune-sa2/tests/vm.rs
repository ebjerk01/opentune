//! SA2 VM semantics, assembler and robustness tests.
//!
//! Every script here is written for the test suite. None is an OEM SA2 script.
use opentune_core::protocol::SeedKey;
use opentune_sa2::op::*;
use opentune_sa2::vm::{run_with, Step};
use opentune_sa2::{
    assemble, assemble_text, execute, parse_hex, run, Lint, Op, Program, Sa2Error, Sa2SeedKey,
    MAX_LOOP_DEPTH,
};

fn run_ops(ops: &[Op], seed: u32) -> u32 {
    run(&assemble(ops), seed).expect("script should run")
}

#[test]
fn rotates_and_their_carry() {
    assert_eq!(run_ops(&[Op::Rsl], 0x8000_0001), 0x0000_0003);
    assert_eq!(run_ops(&[Op::Rsr], 0x8000_0001), 0xC000_0000);
    // Carry from rsl = old bit 31: set here, so bcc does NOT branch and the eor runs.
    let s = [Op::Rsl, Op::Bcc(5), Op::Eor(0xFFFF_FFFF), Op::Finish];
    assert_eq!(run_ops(&s, 0x8000_0000), !0x0000_0001);
    // Clear here, so bcc skips the eor.
    assert_eq!(run_ops(&s, 0x4000_0000), 0x8000_0000);
    // Carry from rsr = old bit 0.
    let s = [Op::Rsr, Op::Bcc(5), Op::Eor(0xFFFF_FFFF), Op::Finish];
    assert_eq!(run_ops(&s, 1), !0x8000_0000);
    assert_eq!(run_ops(&s, 2), 1);
}

#[test]
fn add_sub_wrap_and_set_carry() {
    assert_eq!(run_ops(&[Op::Add(2)], 0xFFFF_FFFF), 1);
    assert_eq!(run_ops(&[Op::Sub(2)], 1), 0xFFFF_FFFF);
    // Whether carry is set after `first`: bcc skips the eor only when clear.
    let probe = |first: Op, seed: u32| {
        let v = run_ops(&[first], seed);
        run_ops(&[first, Op::Bcc(5), Op::Eor(1), Op::Finish], seed) != v
    };
    assert!(probe(Op::Add(1), 0xFFFF_FFFF)); // overflow
    assert!(!probe(Op::Add(1), 0xFFFF_FFFE));
    assert!(probe(Op::Sub(2), 1)); // borrow
    assert!(!probe(Op::Sub(1), 1)); // exact: no borrow
}

#[test]
fn eor_keeps_carry() {
    // add sets carry (overflow), eor must not clear it, so bcc falls through to rsl.
    let s = [
        Op::Add(0xFFFF_FFFF),
        Op::Eor(0x1234_5678),
        Op::Bcc(1),
        Op::Rsl,
        Op::Finish,
    ];
    let after_add = 5u32.wrapping_add(0xFFFF_FFFF) ^ 0x1234_5678;
    assert_eq!(run_ops(&s, 5), after_add.rotate_left(1));
}

#[test]
fn loops_run_n_times_and_nest() {
    assert_eq!(run_ops(&[Op::Loop(1), Op::Add(1), Op::Next], 0), 1);
    assert_eq!(run_ops(&[Op::Loop(7), Op::Add(1), Op::Next], 0), 7);
    assert_eq!(run_ops(&[Op::Loop(255), Op::Add(1), Op::Next], 0), 255);
    let nested = [
        Op::Loop(3),
        Op::Loop(4),
        Op::Add(1),
        Op::Next,
        Op::Add(100),
        Op::Next,
        Op::Finish,
    ];
    assert_eq!(run_ops(&nested, 0), 3 * 4 + 3 * 100);
    // 32 single-bit rotations is the identity.
    assert_eq!(
        run_ops(&[Op::Loop(32), Op::Rsl, Op::Next], 0xDEAD_BEEF),
        0xDEAD_BEEF
    );
}

#[test]
fn branches_skip_forward_in_bytes() {
    assert_eq!(run_ops(&[Op::Bra(1), Op::Rsl, Op::Finish], 1), 1);
    assert_eq!(run_ops(&[Op::Bra(0), Op::Rsl, Op::Finish], 1), 2);
    // Past the end ends the run.
    assert_eq!(run_ops(&[Op::Rsl, Op::Bra(200)], 1), 2);
    // Misaligned: lands in the eor's immediate and executes it as code.
    let s = assemble(&[Op::Bra(1), Op::Eor(0x8181_8181), Op::Finish]);
    assert_eq!(run(&s, 1).unwrap(), 1u32.rotate_left(4));
}

#[test]
fn finish_stops_and_seed_key_are_big_endian() {
    assert_eq!(run_ops(&[Op::Finish, Op::Rsl], 1), 1);
    let key = execute(
        &[0x12, 0x34, 0x56, 0x78],
        &assemble(&[Op::Eor(0xFF00_FF00)]),
    )
    .unwrap();
    assert_eq!(key, vec![0xED, 0x34, 0xA9, 0x78]);
    // Empty script: key = seed.
    assert_eq!(execute(&[1, 2, 3, 4], &[]).unwrap(), vec![1, 2, 3, 4]);
}

#[test]
fn typed_errors() {
    assert_eq!(execute(&[1, 2, 3], &[]), Err(Sa2Error::SeedLength(3)));
    assert_eq!(execute(&[0; 5], &[]), Err(Sa2Error::SeedLength(5)));
    assert_eq!(
        run(&[OP_RSL, 0x00], 0),
        Err(Sa2Error::UnknownOpcode {
            offset: 1,
            opcode: 0
        })
    );
    assert_eq!(
        run(&[OP_ADD, 1, 2], 0),
        Err(Sa2Error::Truncated {
            offset: 0,
            opcode: OP_ADD,
            needed: 4
        })
    );
    assert_eq!(
        run(&[OP_RSL, OP_BCC], 0),
        Err(Sa2Error::Truncated {
            offset: 1,
            opcode: OP_BCC,
            needed: 1
        })
    );
    assert_eq!(
        run(&[OP_NEXT], 0),
        Err(Sa2Error::NextWithoutLoop { offset: 0 })
    );
    assert_eq!(
        run(&[OP_LOOP, 0, OP_NEXT], 0),
        Err(Sa2Error::ZeroLoopCount { offset: 0 })
    );
    // Unknown bytes that are never reached do not matter to the VM.
    assert_eq!(run(&[OP_BRA, 1, 0x00, OP_RSL], 1), Ok(2));
    // ...but the strict linear parse reports them.
    assert!(Program::parse(&[OP_BRA, 1, 0x00, OP_RSL]).is_err());
}

#[test]
fn budget_and_loop_depth() {
    // 255^3 = 16.6M steps: over the default budget.
    let deep = assemble(&[
        Op::Loop(255),
        Op::Loop(255),
        Op::Loop(255),
        Op::Rsl,
        Op::Next,
        Op::Next,
        Op::Next,
    ]);
    assert_eq!(
        run(&deep, 0),
        Err(Sa2Error::StepBudgetExceeded(opentune_sa2::MAX_STEPS))
    );
    assert_eq!(
        run_with(&assemble(&[Op::Rsl, Op::Rsl]), 0, 1, None),
        Err(Sa2Error::StepBudgetExceeded(1))
    );
    let mut too_deep = Vec::new();
    for _ in 0..=MAX_LOOP_DEPTH {
        Op::Loop(2).encode(&mut too_deep);
    }
    assert_eq!(
        run(&too_deep, 0),
        Err(Sa2Error::LoopStackOverflow(MAX_LOOP_DEPTH))
    );
}

#[test]
fn trace_reports_every_step() {
    let s = assemble(&[Op::Loop(2), Op::Rsl, Op::Next, Op::Finish]);
    let mut steps: Vec<Step> = Vec::new();
    let mut cb = |st: &Step| steps.push(*st);
    let out = run_with(&s, 1, 100, Some(&mut cb)).unwrap();
    assert_eq!(out.key, 4);
    assert_eq!(out.steps, 6); // loop, rsl, next, rsl, next, finish
    assert_eq!(steps.len(), 6);
    assert_eq!(steps[1].register, 2);
    assert_eq!(steps[1].loop_depth, 1);
    assert_eq!(steps[5].op, Op::Finish);
}

#[test]
fn decode_encode_round_trip_every_opcode() {
    let ops = [
        Op::Rsl,
        Op::Rsr,
        Op::Add(0x0102_0304),
        Op::Sub(0xFFFF_FFFF),
        Op::Eor(0),
        Op::Loop(9),
        Op::Next,
        Op::Bcc(3),
        Op::Bra(255),
        Op::Finish,
    ];
    let bytes = assemble(&ops);
    assert_eq!(Program::parse(&bytes).unwrap().ops(), ops.to_vec());
    for op in ops {
        assert!(OPCODES.contains(&op.opcode()));
        assert_eq!(operand_len(op.opcode()), Some(op.len() - 1));
    }
    assert_eq!(operand_len(0x00), None);
}

#[test]
fn text_assembler_labels_and_errors() {
    let src = "
        ; synthetic test script
        loop 3          # three rounds
          rsl
          bcc skip
          eor 0x5A5A5A5A
        skip:
          add 1_000
        next
        bra +0
        finish
    ";
    let bytes = assemble_text(src).unwrap();
    assert_eq!(
        bytes,
        assemble(&[
            Op::Loop(3),
            Op::Rsl,
            Op::Bcc(5),
            Op::Eor(0x5A5A_5A5A),
            Op::Add(1000),
            Op::Next,
            Op::Bra(0),
            Op::Finish,
        ])
    );
    // Disassembly source re-assembles to the same bytes.
    let prog = Program::parse(&bytes).unwrap();
    assert_eq!(assemble_text(&prog.to_source()).unwrap(), bytes);
    assert!(prog.lints().is_empty(), "{:?}", prog.lints());
    assert!(prog.listing().contains("bcc L000A"));

    assert_eq!(assemble_text(".byte 0x00 255").unwrap(), vec![0x00, 0xFF]);
    for bad in [
        "frob",
        "rsl 1",
        "add",
        "loop 256",
        "add 0x1_0000_0000",
        "bcc nowhere",
        "back:\n rsl\n bra back",
        "x:\nx:",
    ] {
        assert!(
            matches!(assemble_text(bad), Err(Sa2Error::Asm { .. })),
            "{bad:?} should fail"
        );
    }
}

#[test]
fn lints_flag_odd_structure() {
    let p = Program::parse(&assemble(&[Op::Next, Op::Loop(0), Op::Bra(2), Op::Rsl])).unwrap();
    let l = p.lints();
    assert!(l.contains(&Lint::UnmatchedNext { offset: 0 }));
    assert!(l.contains(&Lint::ZeroLoopCount { offset: 1 }));
    assert!(l.contains(&Lint::BranchPastEnd {
        offset: 3,
        target: 7
    }));
    assert!(l.contains(&Lint::UnclosedLoops(1)));
    assert!(l.contains(&Lint::MissingFinish));
    let p = Program::parse(&assemble(&[Op::Bra(1), Op::Add(0), Op::Finish, Op::Rsl])).unwrap();
    assert!(p.lints().contains(&Lint::MisalignedBranch {
        offset: 0,
        target: 3
    }));
    assert!(p.lints().contains(&Lint::CodeAfterFinish { offset: 8 }));
}

#[test]
fn hex_input_forms() {
    let want = vec![0x68, 0x05, 0x81];
    for s in ["680581", "68 05 81", "0x68,0x05,0x81", "68:05:81\n"] {
        assert_eq!(parse_hex(s).unwrap(), want);
    }
    assert!(matches!(parse_hex("6"), Err(Sa2Error::BadHex(_))));
    assert!(matches!(parse_hex("zz"), Err(Sa2Error::BadHex(_))));
}

#[test]
fn seedkey_trait() {
    let sk = Sa2SeedKey::from_hex("87 FF FF FF FF 4C").unwrap();
    assert_eq!(sk.id(), "sa2");
    assert_eq!(
        sk.compute(&[0, 0, 0, 1], 0x11).unwrap(),
        vec![0xFF, 0xFF, 0xFF, 0xFE]
    );
    assert!(sk.compute(&[0, 1], 0x11).is_err());
}

/// Tiny deterministic PRNG so the fuzz loop needs no extra dependency.
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = XorShift(0x05EE_D5A2_u64);
    for _ in 0..20_000 {
        let len = (rng.next() % 48) as usize;
        let script: Vec<u8> = (0..len)
            .map(|_| {
                let r = rng.next();
                // Bias toward real opcodes so execution gets somewhere.
                if r % 4 != 0 {
                    OPCODES[(r >> 8) as usize % OPCODES.len()]
                } else {
                    (r >> 16) as u8
                }
            })
            .collect();
        let seed = rng.next() as u32;
        let _ = run_with(&script, seed, 50_000, None);
        if let Ok(p) = Program::parse(&script) {
            let _ = p.lints();
            let _ = p.listing();
            // The strict parse and the assembler agree byte for byte.
            assert_eq!(assemble(&p.ops()), script);
            assert_eq!(assemble_text(&p.to_source()).unwrap(), script);
        }
    }
}
