//! The module's timing program (`doc/governance/rust-style.md` section
//! 5c): this transport held to how long Xmip takes — the millisecond rule
//! (`CONTRIBUTING.md`, *Rust is here for one reason*). A plain `cargo test`
//! leaves it out; the landing runs it after every other test of the module,
//! alone, one test at a time
//! (`cargo test --test measured --all-features -- --test-threads=1`), so
//! what is measured is Xmip's work and not its neighbors':
//!
//! - a thousand writes on one session take a millisecond each at most;
//! - a thousand polls on one session take a millisecond each at most.
//!
//! Each the millisecond beyond the round's wakes, round by round beside a
//! plain wake, the fastest of five (`xcore::latency::Rounds`).

use std::time::Duration;

use transport::Configured;
use transport::Transport;
use xcore::latency::Rounds;
use xmip_core_transport_s7comm::{Area, Event, S7Transport, Session};

/// What a write or a poll wakes where a plain wake wakes one: the far end
/// at the job, and this end at its acknowledgement.
const WAKES: u32 = 2;

fn node(plc: &str, address: &str) -> S7Transport {
    S7Transport::new(plc, address).timing_out_after(Duration::from_secs(5))
}

#[test]
fn a_thousand_writes_take_a_millisecond_each_at_most() {
    const SENDS: usize = 1000;
    let far_end = node("127.0.0.1:0", "M0");
    let (listener, address) = far_end.bind().expect("binding");
    let near = node(&address, "M0");
    let sender = std::thread::spawn(move || {
        let mut rounds = Rounds::crossing(WAKES);
        for n in 0..SENDS {
            let byte = u8::try_from(n % 256).expect("a byte");
            rounds.round(|| near.send("M0", &[byte]))?;
        }
        Ok::<_, transport::TransportError>(rounds)
    });
    let mut session =
        far_end
            .accept_one(&listener)
            .expect("accepting")
            .with_area(Area::Flag, 0, vec![0u8; 8]);
    serve(&mut session, SENDS, |event| {
        matches!(event, Event::Written { served: true, .. })
    });
    let rounds = sender.join().expect("thread").expect("sending");
    rounds.hold("a write");
}

#[test]
fn a_thousand_polls_take_a_millisecond_each_at_most() {
    const RECEIVES: usize = 1000;
    let far_end = node("127.0.0.1:0", "M0");
    let (listener, address) = far_end.bind().expect("binding");
    let near = node(&address, "M0");
    let receiver = std::thread::spawn(move || {
        let mut rounds = Rounds::crossing(WAKES);
        for _ in 0..RECEIVES {
            let polled = rounds.round(|| near.receive()?.remove(0).taken())?;
            assert_eq!(polled.bytes, [7]);
        }
        Ok::<_, transport::TransportError>(rounds)
    });
    let mut session =
        far_end
            .accept_one(&listener)
            .expect("a session")
            .with_area(Area::Flag, 0, vec![7u8; 8]);
    serve(&mut session, RECEIVES, |event| {
        matches!(event, Event::Read { served: true, .. })
    });
    let rounds = receiver.join().expect("thread").expect("receiving");
    rounds.hold("a poll");
}

/// Served until `calls` events that `counts` have been answered, the one
/// Setup Communication passed over.
fn serve(session: &mut Session, calls: usize, counts: impl Fn(&Event) -> bool) {
    let mut served = 0;
    while served < calls {
        let event = session.next_event().expect("serving").expect("one");
        if counts(&event) {
            served += 1;
        } else if !matches!(event, Event::Setup { .. }) {
            panic!("{event:?}");
        }
    }
}
