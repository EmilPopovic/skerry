// SPDX-License-Identifier: Apache-2.0
//! Checks panic recovery using the actual release profile, outside the test harness.
use sk_util::{Cancel, Lane, Pool};
use std::sync::mpsc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::panic::set_hook(Box::new(|_| eprintln!("skerry: expected worker panic")));
    let pool = Pool::with_width(1);
    let (sender, receiver) = mpsc::channel();
    pool.spawn(Lane::Device(1), Cancel::new(), |_| panic!("regression fixture"));
    pool.spawn(Lane::Device(1), Cancel::new(), move |_| {
        let _ = sender.send(());
    });
    receiver.recv_timeout(Duration::from_secs(5))?;
    if pool.stats().1 != 1 {
        return Err("worker panic was not counted".into());
    }
    Ok(())
}
