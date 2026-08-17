//! `Accept` header negotiation: a hostile header must never panic, and
//! the winner must always be an index into the offers.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (&str, Vec<&str>)| {
    let (accept, offers) = input;
    if let Some(i) = nitr_std::best_match(accept, &offers) {
        assert!(i < offers.len(), "winner {i} out of {} offers", offers.len());
    }
});
