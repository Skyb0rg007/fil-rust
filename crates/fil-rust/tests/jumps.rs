//! `setjmp` and `longjmp`, on either backend.

mod common;

use common::{fixture, runtime};

fn library() -> fil_rust::Library {
    runtime().load(fixture("jumps.c")).unwrap()
}

#[test]
fn a_jump_lands_at_the_setjmp_with_its_value() {
    let library = library();
    runtime().run(|fil| {
        let jump_up = library.function::<fn(i32, i32) -> i32>(fil, "jump_up").unwrap();
        for depth in [0, 1, 100] {
            assert_eq!(jump_up.call(fil, (depth, 42)).unwrap(), 42);
        }
        assert_eq!(library.function::<fn() -> i32>(fil, "jump_with_zero").unwrap().call(fil, ()).unwrap(), 1);
    });
}

#[test]
fn what_a_frame_held_survives_a_jump_and_a_collection() {
    let library = library();
    runtime().run(|fil| {
        let collect = fil.callback::<fn(), _>(|fil, ()| fil.collect_garbage().unwrap()).unwrap();
        let held = library.function::<fn(fn()) -> i32>(fil, "jump_after_collection").unwrap();
        assert_eq!(held.call(fil, (collect.get(fil),)).unwrap(), 42);
        let changed = library.function::<fn(fn()) -> i32>(fil, "pointer_changed_after_setjmp").unwrap();
        assert!(matches!(changed.call(fil, (collect.get(fil),)).unwrap(), 1 | 2));
    });
}

#[test]
fn a_setjmp_can_run_again_and_again() {
    let library = library();
    runtime().run(|fil| {
        let looped = library.function::<fn(i64) -> i64>(fil, "jump_in_a_loop").unwrap();
        assert_eq!(looped.call(fil, (100_000,)).unwrap(), 100_000);
    });
}

#[test]
fn the_signal_mask_comes_back_only_if_it_was_saved() {
    let library = library();
    runtime().run(|fil| {
        let blocked = library.function::<fn(i32) -> i32>(fil, "blocked_after_jump").unwrap();
        assert_eq!(blocked.call(fil, (1,)).unwrap(), 0);
        assert_eq!(blocked.call(fil, (0,)).unwrap(), 1);
    });
}

#[test]
fn a_frame_that_returned_cannot_be_jumped_to() {
    const CHILD: &str = "FIL_RUST_JUMPS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let library = library();
        runtime().run(|fil| {
            library.function::<fn()>(fil, "jump_to_a_returned_frame").unwrap().call(fil, ()).unwrap();
        });
        return;
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "a_frame_that_returned_cannot_be_jumped_to", "--nocapture"]).env(CHILD, "1");
    let output = common::without_core_dumps(&mut command).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("cannot longjmp unless the setjmp destination is on the stack"), "{stderr}");
}
