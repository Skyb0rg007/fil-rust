//! Starting the runtime: configuration, restarts, failed starts, `argv`, the
//! dependency search path, and the elastic executor pool. The runtime is a
//! process-wide singleton, so each case runs in a child process (this test
//! binary re-executed with `FIL_RUST_CHILD` naming the case).

mod common;

use std::ffi::c_char;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};

use common::{config, fixture_with, flavor};
use fil_rust::{Config, Error, Flavor, Ptr, Runtime};

fn child(case: &str, env: &[(&str, Option<&str>)]) -> Output {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    common::without_core_dumps(&mut cmd).args(["child_entry", "--exact", "--nocapture", "--test-threads=1"]).env("FIL_RUST_CHILD", case);
    for (k, v) in env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.output().expect("running the child process")
}

/// Runs `case` in a child and requires it to pass.
fn passes(case: &str, env: &[(&str, Option<&str>)]) {
    let out = child(case, env);
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("child ok"),
        "child {case} failed ({}):\n{}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn other(flavor: Flavor) -> Flavor {
    match flavor {
        Flavor::Musl => Flavor::Glibc,
        Flavor::Glibc => Flavor::Musl,
    }
}

/// `libstartup_user.so`, linked against `libstartup_dep.so` in the
/// directory returned alongside it, with no RPATH pointing there.
fn user_library() -> (std::path::PathBuf, std::path::PathBuf) {
    // Preloaded dependencies satisfy `DT_NEEDED` entries by soname.
    let dep = fixture_with("startup_dep.c", &["-Wl,-soname,libstartup_dep.so"]);
    let dir = dep.parent().unwrap().to_path_buf();
    let user = fixture_with("startup_user.c", &["-L", dir.to_str().unwrap(), "-lstartup_dep"]);
    (user, dir)
}

#[test]
fn child_entry() {
    let Ok(case) = std::env::var("FIL_RUST_CHILD") else { return };
    match case.as_str() {
        "get-before-start" => {
            assert!(Runtime::get().is_none());
            let rt = Runtime::start(config()).unwrap();
            let got = Runtime::get().expect("running after start");
            assert_eq!(got.flavor(), flavor());
            assert_eq!(rt.run(|_| 7), 7);
        }
        "restart" => {
            let rt = Runtime::start(config().workers(3)).unwrap();
            // The same distribution again: the running runtime, unchanged.
            let again = Runtime::start(config().workers(5)).unwrap();
            assert_eq!(again.config().worker_count(), 3);
            assert_eq!(again.run(|_| 1) + rt.run(|_| 1), 2);
            // A different distribution, or flavor: refused.
            let elsewhere = Config::new(flavor(), "/nonexistent/fil-c");
            assert!(matches!(Runtime::start(elsewhere), Err(Error::AlreadyStarted)));
            let other_flavor = Config::new(other(flavor()), config().root());
            assert!(matches!(Runtime::start(other_flavor), Err(Error::AlreadyStarted)));
            assert_eq!(rt.run(|_| 2), 2);
        }
        "bad-root-then-good" => {
            let empty = Path::new(env!("CARGO_TARGET_TMPDIR")).join("empty-fil-c-root");
            std::fs::create_dir_all(&empty).unwrap();
            match Runtime::start(Config::new(flavor(), &empty)) {
                Err(Error::Load(msg)) => assert!(msg.contains("libpizlo.so"), "{msg}"),
                Err(e) => panic!("unexpected error {e}"),
                Ok(_) => panic!("started from an empty directory"),
            }
            assert!(Runtime::get().is_none(), "a failed start leaves no runtime");
            // The failure left nothing behind: a good configuration starts.
            let rt = Runtime::start(config()).unwrap();
            assert_eq!(rt.run(|fil| fil.c_str("ok").unwrap().read_string(fil).unwrap()), "ok");
        }
        "wrong-flavor-root" => {
            // This flavor's distribution described as the other flavor.
            let wrong = Config::new(other(flavor()), config().root());
            assert!(Runtime::start(wrong).is_err());
            assert!(Runtime::get().is_none());
            Runtime::start(config()).unwrap();
        }
        "from-env-unknown-flavor" => match Config::from_env() {
            Err(Error::Load(msg)) => assert!(msg.contains("FILC_FLAVOR") && msg.contains("bogus"), "{msg}"),
            other => panic!("expected an error, got {other:?}"),
        },
        "from-env-no-root" => {
            let var = match flavor() {
                Flavor::Musl => "FILC_MUSL_ROOT",
                Flavor::Glibc => "FILC_GLIBC_ROOT",
            };
            match Config::from_env_for(flavor()) {
                Err(Error::Load(msg)) => assert!(msg.contains(var), "{msg}"),
                other => panic!("expected an error, got {other:?}"),
            }
            assert!(matches!(Runtime::start_from_env(), Err(Error::Load(_))));
        }
        "from-env-workers" => {
            let c = Config::from_env_for(flavor()).unwrap();
            assert_eq!(c.worker_count(), 3);
            assert_eq!(c.flavor(), flavor());
            let rt = Runtime::start(c).unwrap();
            assert_eq!(rt.config().worker_count(), 3);
        }
        "argv" => {
            let (user, dir) = user_library();
            let rt = Runtime::start(config().args(["custom-name", "--flag"]).search_path(dir)).unwrap();
            let name = rt.run(|fil| {
                let lib = fil.load(&user).unwrap();
                let argv0 = lib.function::<fn() -> Ptr<c_char>>(fil, "argv0").unwrap();
                argv0.call(fil, ()).unwrap().read_string(fil).unwrap()
            });
            assert_eq!(name, "custom-name");
        }
        "search-path" => {
            let (user, dir) = user_library();
            let rt = Runtime::start(config().search_path(&dir)).unwrap();
            let v = rt.run(|fil| {
                let lib = fil.load(&user).unwrap();
                lib.function::<fn(i32) -> i32>(fil, "use_dep").unwrap().call(fil, (2,)).unwrap()
            });
            assert_eq!(v, 1007);
        }
        "no-search-path" => {
            let (user, _) = user_library();
            let rt = Runtime::start(config()).unwrap();
            match rt.load(&user) {
                Err(Error::Load(msg)) => assert!(msg.contains("libstartup_dep.so"), "{msg}"),
                Err(e) => panic!("unexpected error {e}"),
                Ok(_) => panic!("loaded a library whose dependency can't be found"),
            }
            // The runtime is still fine.
            assert_eq!(rt.run(|_| 3), 3);
        }
        "pool-grows" => {
            // One executor up front; four jobs that can only finish together.
            let rt = Runtime::start(config().workers(1).max_workers(4)).unwrap();
            let barrier = Arc::new(Barrier::new(4));
            let jobs: Vec<_> = (0..4)
                .map(|i| {
                    let barrier = barrier.clone();
                    rt.spawn(move |fil| {
                        fil.exited(|| barrier.wait());
                        i
                    })
                })
                .collect();
            let sum: i32 = jobs.into_iter().map(|j| j.join()).sum();
            assert_eq!(sum, 6);
        }
        "concurrent-start" => {
            // Good and bad configurations race to start the runtime.
            let start = Arc::new(Barrier::new(12));
            let threads: Vec<_> = (0..12)
                .map(|i| {
                    let start = start.clone();
                    std::thread::spawn(move || {
                        let config = if i % 3 == 2 { Config::new(flavor(), "/nonexistent/fil-c") } else { config() };
                        start.wait();
                        (i % 3 == 2, Runtime::start(config))
                    })
                })
                .collect();
            let mut started = 0;
            for t in threads {
                match t.join().unwrap() {
                    (false, Ok(rt)) => {
                        assert_eq!(rt.run(|fil| fil.c_str("up").unwrap().read_string(fil).unwrap()), "up");
                        started += 1;
                    }
                    (false, Err(e)) => panic!("a good configuration failed: {e}"),
                    (true, Err(Error::Load(_) | Error::AlreadyStarted)) => {}
                    (true, other) => panic!("a bad configuration gave {other:?}"),
                }
            }
            assert_eq!(started, 8);
            // Only one runtime came up: one set of executors.
            assert_eq!(Runtime::get().unwrap().config().root(), config().root());
        }
        other => panic!("unknown case {other}"),
    }
    println!("child ok");
}

#[test]
fn get_returns_nothing_until_started() {
    passes("get-before-start", &[]);
}

#[test]
fn starting_again_returns_the_running_runtime_or_refuses() {
    passes("restart", &[]);
}

#[test]
fn a_failed_start_can_be_retried() {
    passes("bad-root-then-good", &[]);
}

#[test]
fn a_distribution_of_the_wrong_flavor_is_refused() {
    passes("wrong-flavor-root", &[]);
}

#[test]
fn environment_configuration_errors() {
    passes("from-env-unknown-flavor", &[("FILC_FLAVOR", Some("bogus"))]);
    passes(
        "from-env-no-root",
        &[("FILC_MUSL_ROOT", None), ("FILC_ROOT", None), ("FILC_GLIBC_ROOT", None)],
    );
}

#[test]
fn environment_sets_the_worker_count() {
    passes("from-env-workers", &[("FIL_RUST_WORKERS", Some("3"))]);
}

#[test]
fn the_fil_c_side_sees_the_configured_argv() {
    passes("argv", &[]);
}

#[test]
fn dependencies_are_found_on_the_search_path() {
    passes("search-path", &[]);
    passes("no-search-path", &[]);
}

#[test]
fn the_executor_pool_grows_when_jobs_wait() {
    passes("pool-grows", &[]);
}

#[test]
fn worker_counts_are_clamped() {
    let c = config().workers(0);
    assert_eq!(c.worker_count(), 1);
    let c = config().workers(6).max_workers(2);
    assert_eq!((c.worker_count(), c.max_worker_count()), (2, 2));
    let c = config().max_workers(2).workers(6);
    assert_eq!((c.worker_count(), c.max_worker_count()), (6, 6));
    let c = config().max_workers(0);
    assert_eq!(c.max_worker_count(), 1);
}

#[test]
fn concurrent_starts_bring_up_one_runtime() {
    for _ in 0..3 {
        passes("concurrent-start", &[]);
    }
}
