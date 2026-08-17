//! Lexical path handling: `normalize` is used to sanitize untrusted path
//! input, so its safety property is asserted under fuzzing — a normalized
//! path never keeps a `..` that could climb out of an absolute root.
#![no_main]
use libfuzzer_sys::fuzz_target;
use nitr_std::fuzzing as path;

fuzz_target!(|input: (&str, &str)| {
    let (a, b) = input;
    let _ = path::basename(a);
    let _ = path::dirname(a);
    let joined = path::join(&[a.to_string(), b.to_string()]);
    let _ = path::normalize(&joined);

    let normalized = path::normalize(a);
    if path::is_absolute(&normalized) {
        let no_root: String = normalized.replace('\\', "/");
        assert!(
            !no_root.split('/').any(|seg| seg == ".."),
            "normalize left a dot-dot in an absolute path: {a:?} -> {normalized:?}"
        );
    }
});
