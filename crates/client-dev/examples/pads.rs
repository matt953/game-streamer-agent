//! List gamepads the OS is offering us.
//!
//! Some backends only surface a pad once its events are pumped, so this polls
//! for a few seconds rather than asking once and concluding there are none.

fn main() {
    let mut gilrs = match gilrs::Gilrs::new() {
        Ok(g) => g,
        Err(e) => {
            println!("gilrs init failed: {e}");
            return;
        }
    };
    let start = std::time::Instant::now();
    let mut seen = std::collections::BTreeSet::new();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        while let Some(event) = gilrs.next_event() {
            println!("event from {:?}: {:?}", event.id, event.event);
        }
        for (id, pad) in gilrs.gamepads() {
            if seen.insert(format!("{id:?}")) {
                println!(
                    "gamepad {id:?}: {} connected={} power={:?}",
                    pad.name(),
                    pad.is_connected(),
                    pad.power_info()
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if seen.is_empty() {
        println!("no gamepads after 5s of polling");
    }
}
