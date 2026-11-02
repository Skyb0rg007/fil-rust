//! Concurrency: many Rust threads calling into a thread-safe C library,
//! background jobs, Rust-owned buffers, rooted handles shared across threads,
//! and leaving the runtime to block.
//!
//!     cargo run --example threads

mod support;

use std::ffi::c_char;
use std::sync::mpsc;
use std::time::Duration;

use fil_rust::{FilHandle, Ptr};

fil_rust::foreign! {
    struct WordCount {
        fn count_words(text: Ptr<c_char>, len: usize) -> i64;
        fn word_count(word: Ptr<c_char>) -> i64;
        fn fnv1a(data: Ptr<u8>, len: usize) -> u64;
    }
}

const TEXT: &str = "the quick brown fox jumps over the lazy dog the end";

fn main() -> fil_rust::Result<()> {
    let rt = support::runtime();
    let path = support::build("wordcount.c");
    // `foreign!` structs are `Send + Sync + Clone`: load once, use anywhere.
    let wc = rt.run(|fil| WordCount::load(fil, &path))?;

    // Eight Rust threads hammer the C hash table at once. Each `run` hops onto
    // a Fil-C executor thread; the C mutex does the rest.
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..250 {
                    rt.run(|fil| wc.count_words(fil, fil.c_str(TEXT)?, TEXT.len())).unwrap();
                }
            });
        }
    });
    let the = rt.run(|fil| wc.word_count(fil, fil.c_str("the")?))?;
    println!("\"the\" was counted {the} times (expected {})", 8 * 250 * 3);

    // Background jobs run in parallel on the executor pool, which grows when
    // jobs are waiting.
    let jobs: Vec<_> = (0..16)
        .map(|i| {
            let wc = wc.clone();
            rt.spawn(move |fil| {
                let text = format!("job{i} ").repeat(1000);
                wc.count_words(fil, fil.c_str(&text).unwrap(), text.len()).unwrap()
            })
        })
        .collect();
    let counted: i64 = jobs.into_iter().map(|j| j.join()).sum();
    println!("background jobs counted {counted} words");

    // A `FilBuf` is Fil-C memory that Rust owns exclusively, so it can be
    // filled as a plain `&mut [u8]` without per-access checks...
    let mut buf = rt.run(|fil| fil.buf(1 << 20))?;
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i * 31 % 251) as u8;
    }
    // ...and then handed to C. `share` gives up exclusive access.
    let sum = rt.run(move |fil| {
        let len = buf.len();
        wc.fnv1a(fil, buf.share(fil), len)
    })?;
    println!("checksum of 1 MiB buffer: {sum:#018x}");

    // A rooted handle keeps a Fil-C object alive across scopes and threads.
    let label: FilHandle<c_char> = rt.run(|fil| fil.root(fil.c_str("shared label")?))?;
    let (tx, rx) = mpsc::channel();
    let waiter = rt.spawn(move |fil| {
        // Blocking on a channel while inside the runtime would hold up the
        // garbage collector; `exited` steps out while we wait.
        let suffix: String = fil.exited(move || rx.recv().unwrap());
        format!("{} {suffix}", label.get(fil).read_string(fil).unwrap())
    });
    std::thread::sleep(Duration::from_millis(50));
    rt.run(|fil| fil.collect_garbage())?;
    tx.send("survived a collection".to_string()).unwrap();
    println!("{}", waiter.join());
    Ok(())
}
