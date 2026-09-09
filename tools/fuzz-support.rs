// SPDX-License-Identifier: Apache-2.0
//! Shared deterministic mutation driver, compiled only into parser tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Run reproducible mutations and retain an input if it panics.
pub fn campaign(name: &str, seeds: &[&[u8]], mut target: impl FnMut(&[u8])) {
    let cases =
        std::env::var("SKERRY_FUZZ_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(256usize);
    let seed =
        std::env::var("SKERRY_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5ce77u64);
    let mut random = seed.max(1);
    let mut next = || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random as usize
    };
    for input in seeds {
        target(input);
    }
    for iteration in 0..cases {
        let mut bytes = seeds[next() % seeds.len()].to_vec();
        for _ in 0..=(next() % 8) {
            let at = next() % (bytes.len() + 1);
            match next() % 6 {
                0 => bytes.truncate(at),
                1 if at < bytes.len() => {
                    bytes[at] ^= 1 << (next() % 8);
                }
                2 if at < bytes.len() => {
                    bytes.remove(at);
                }
                3 => {
                    bytes.splice(at..at, [0xff; 8]);
                }
                4 if at < bytes.len() => {
                    let end = (at + 8).min(bytes.len());
                    bytes[at..end].fill(if next() & 1 == 0 { 0 } else { 0xff });
                }
                _ => {
                    bytes.insert(at, next() as u8);
                }
            }
        }
        bytes.truncate(64 * 1024);
        if let Err(panic) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| target(&bytes)))
        {
            let file =
                std::env::temp_dir().join(format!("skerry-fuzz-{name}-{seed}-{iteration}.bin"));
            std::fs::write(&file, &bytes).expect("save crashing parser input");
            eprintln!("{name}: seed={seed} iteration={iteration} input={}", file.display());
            std::panic::resume_unwind(panic);
        }
    }
    eprintln!("{name}: {cases} mutations, seed={seed}, {} valid seeds", seeds.len());
}
