//! Lexical path manipulation for Lua handlers: `nitr.path`.
//!
//! Foundation string operations on paths — URL paths, mount points, file
//! names in multipart uploads. Both POSIX (`/`) and Windows (`\`, drive
//! letters, UNC) styles are understood: separators are recognized in
//! either form, and outputs keep the input's separator style.
//!
//! Component splitting is delegated to [`std::path::Path`]
//! (`file_name`/`parent`/`extension`/`components`) over a canonicalized
//! (`\` → `/`, root split off) form of the input. The canonicalization is
//! what keeps behavior platform-independent: `std::path` only recognizes
//! backslashes and drive prefixes when *compiled for* Windows, and a Lua
//! script must get the same answer from a server on either OS. What std
//! cannot provide stays hand-written: lexical `..` resolution (std's only
//! resolver, `canonicalize`, reads the filesystem — off-limits here) and
//! a `join` that refuses to discard the base when a later segment is
//! absolute (unlike `Path::join`).
//!
//! Everything here is pure text: nothing reads the filesystem, so the
//! sandbox story is unchanged, and `normalize` makes prefix checks on
//! untrusted input safe against dot-dot escapes.

use std::path::{Component, Path};

use mlua::{Lua, Table, Value};

/// Whether the path is Windows-styled (backslashes or a drive prefix), so
/// outputs can keep the input's separator.
fn is_windows_style(path: &str) -> bool {
    path.contains('\\') || (!path.contains('/') && drive(path).is_some())
}

fn restyle(canonical: String, windows: bool) -> String {
    if windows {
        canonical.replace('/', "\\")
    } else {
        canonical
    }
}

/// The byte length of a leading `C:` drive prefix, if present.
fn drive(path: &str) -> Option<usize> {
    let mut chars = path.chars();
    (chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':'))
        .then_some(2)
}

/// Splits a path into its root (`/`, `C:\`, `C:`, `\\` for UNC, or empty)
/// and the rest, both canonicalized to `/` separators. The root is split
/// off by hand because `std::path` only parses drive/UNC prefixes when
/// compiled for Windows.
fn split_root(path: &str) -> (String, String) {
    let canonical = path.replace('\\', "/");
    let root_len = if canonical.starts_with("//") {
        // UNC: both leading separators are the root marker.
        2
    } else {
        match drive(&canonical) {
            Some(d) if canonical[d..].starts_with('/') => d + 1,
            Some(d) => d,
            None if canonical.starts_with('/') => 1,
            None => 0,
        }
    };
    let (root, rest) = canonical.split_at(root_len);
    (root.to_string(), rest.to_string())
}

fn is_absolute(path: &str) -> bool {
    let (root, _) = split_root(path);
    // A bare drive (`C:`) is drive-relative, not absolute.
    !root.is_empty() && !(root.len() == 2 && root.ends_with(':'))
}

/// Splits off the final component, e.g. `"/a/b/c.txt"` → `"c.txt"`.
fn basename(path: &str) -> String {
    let (_, rest) = split_root(path);
    Path::new(&rest)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The directory part, without the trailing separator: `"/a/b/c"` →
/// `"/a/b"`, `"c"` → `"."`, `"/c"` → `"/"`, `"C:\\x"` → `"C:\\"`.
fn dirname(path: &str) -> String {
    let windows = is_windows_style(path);
    let (root, rest) = split_root(path);
    let parent = Path::new(&rest)
        .parent()
        .map(|p| p.to_string_lossy().into_owned());
    let out = match parent {
        Some(parent) if parent.is_empty() && root.is_empty() => ".".into(),
        Some(parent) => format!("{root}{parent}"),
        // No parent: the rest was empty or a lone component root-ward.
        None if root.is_empty() => ".".into(),
        None => root,
    };
    restyle(out, windows)
}

/// The extension of the final component, without the dot; nil when there
/// is none. `Path::extension` also gives dotfiles (`.env`) no extension.
fn extension(path: &str) -> Option<String> {
    let (_, rest) = split_root(path);
    Path::new(&rest)
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
}

/// Joins segments with exactly one separator at each joint, in the style
/// of the first segment. A later absolute segment does NOT reset the
/// result (unlike `std::path::Path::join`): joining untrusted input
/// should never silently discard the base.
fn join(segments: &[String]) -> String {
    let windows = segments
        .iter()
        .find(|s| !s.is_empty())
        .is_some_and(|s| is_windows_style(s));
    let mut out = String::new();
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        let canonical = segment.replace('\\', "/");
        if out.is_empty() {
            out.push_str(&canonical);
        } else {
            if !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(canonical.trim_start_matches('/'));
        }
    }
    restyle(out, windows)
}

/// Resolves `.` and `..` lexically, collapsing duplicate separators into
/// the path's own style. `..` never climbs above the root of an absolute
/// path (or drive) or the start of a relative one, which is what makes
/// the result safe to hand to a mount: after `normalize`, a checked
/// prefix cannot be escaped with dot-dot segments.
fn normalize(path: &str) -> String {
    let windows = is_windows_style(path);
    let (root, rest) = split_root(path);
    let rooted = !root.is_empty();
    let mut parts: Vec<String> = Vec::new();
    // `Components` collapses duplicate separators and `.` for free; `..`
    // resolution is the part std does not offer lexically.
    for component in Path::new(&rest).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|p| p != "..") {
                    parts.pop();
                } else if !rooted {
                    // A relative path keeps the `..`s it cannot resolve.
                    parts.push("..".into());
                }
            }
            other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
        }
    }
    let joined = parts.join("/");
    let out = match (rooted, joined.is_empty()) {
        (true, _) => format!("{root}{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    };
    restyle(out, windows)
}

/// Builds the `nitr.path` table.
pub(crate) fn create_path_table(lua: &Lua) -> mlua::Result<Table> {
    let path = lua.create_table()?;

    path.set(
        "join",
        lua.create_function(|_, segments: mlua::Variadic<String>| Ok(join(&segments)))?,
    )?;
    path.set(
        "basename",
        lua.create_function(|_, path: String| Ok(basename(&path)))?,
    )?;
    path.set(
        "dirname",
        lua.create_function(|_, path: String| Ok(dirname(&path)))?,
    )?;
    path.set(
        "extension",
        lua.create_function(|lua, path: String| match extension(&path) {
            Some(ext) => Ok(Value::String(lua.create_string(ext)?)),
            None => Ok(Value::Nil),
        })?,
    )?;
    path.set(
        "normalize",
        lua.create_function(|_, path: String| Ok(normalize(&path)))?,
    )?;
    path.set(
        "is_absolute",
        lua.create_function(|_, path: String| Ok(is_absolute(&path)))?,
    )?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_split_like_posix() {
        assert_eq!(basename("/a/b/c.txt"), "c.txt");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(basename("plain"), "plain");
        assert_eq!(basename("/"), "");

        assert_eq!(dirname("/a/b/c"), "/a/b");
        assert_eq!(dirname("/a/b/"), "/a");
        assert_eq!(dirname("/c"), "/");
        assert_eq!(dirname("c"), ".");
        assert_eq!(dirname("/"), "/");

        assert_eq!(extension("a/b.tar.gz").as_deref(), Some("gz"));
        assert_eq!(extension("a/.env"), None);
        assert_eq!(extension("a/no_ext"), None);
    }

    #[test]
    fn windows_paths_are_understood() {
        assert_eq!(basename(r"C:\Users\ada\file.txt"), "file.txt");
        assert_eq!(basename(r"C:\Users\ada\"), "ada");
        assert_eq!(dirname(r"C:\Users\ada"), r"C:\Users");
        assert_eq!(dirname(r"C:\Users"), r"C:\");
        assert_eq!(dirname(r"C:\"), r"C:\");
        assert_eq!(extension(r"C:\a\b.TXT").as_deref(), Some("TXT"));

        assert!(is_absolute(r"C:\Users"));
        assert!(is_absolute("C:/Users"));
        assert!(is_absolute(r"\\server\share"));
        assert!(is_absolute(r"\windows-root-relative"));
        assert!(!is_absolute("C:relative"));
        assert!(!is_absolute(r"relative\file"));

        // Mixed separators split correctly.
        assert_eq!(basename(r"C:\a/b\c.txt"), "c.txt");
    }

    #[test]
    fn join_never_discards_the_base() {
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert_eq!(join(&s(&["/srv", "app", "file.txt"])), "/srv/app/file.txt");
        assert_eq!(join(&s(&["/srv/", "/app/"])), "/srv/app/");
        // An absolute later segment joins instead of resetting.
        assert_eq!(join(&s(&["/srv", "/etc/passwd"])), "/srv/etc/passwd");
        assert_eq!(join(&s(&["a", "", "b"])), "a/b");
        assert_eq!(join(&s(&["/"])), "/");
        // The first segment picks the separator style.
        assert_eq!(
            join(&s(&[r"C:\Users", "ada", "x.txt"])),
            r"C:\Users\ada\x.txt"
        );
        assert_eq!(join(&s(&[r"C:\srv", r"\etc\passwd"])), r"C:\srv\etc\passwd");
    }

    #[test]
    fn degenerate_inputs_hold_up() {
        // Empty and separator-only inputs.
        assert_eq!(join(&[]), "");
        assert_eq!(join(&["".into(), "".into()]), "");
        assert_eq!(basename(""), "");
        assert_eq!(dirname(""), ".");
        assert_eq!(extension(""), None);
        assert!(!is_absolute(""));
        assert_eq!(normalize("//"), "//");

        // Trailing separators do not create phantom components.
        assert_eq!(normalize("/a/b/"), "/a/b");
        assert_eq!(normalize(r"C:\a\"), r"C:\a");

        // A bare drive keeps its (drive-relative) meaning.
        assert_eq!(normalize("C:x"), "C:x");
        assert_eq!(dirname(r"C:x"), "C:");

        // UNC dirname walks down to the root marker, not past it.
        assert_eq!(dirname(r"\\server\share"), r"\\server");
        assert_eq!(dirname(r"\\server"), r"\\");

        // A hostile depth of `..` cannot climb out, however long.
        let attack = format!("/base/{}etc/passwd", "../".repeat(1000));
        assert_eq!(normalize(&attack), "/etc/passwd");
        let relative_attack = "../".repeat(500) + "x";
        assert_eq!(
            normalize(&relative_attack),
            format!("{}x", "../".repeat(500))
        );
    }

    #[test]
    fn normalize_resolves_dots_without_escaping_the_root() {
        assert_eq!(normalize("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(normalize("/a/../../etc"), "/etc");
        assert_eq!(normalize("a//b///c"), "a/b/c");
        assert_eq!(normalize("./x"), "x");
        assert_eq!(normalize("../x"), "../x");
        assert_eq!(normalize("a/.."), ".");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize(""), ".");
        // Windows: the drive root cannot be escaped either, and the
        // output keeps the backslash style.
        assert_eq!(normalize(r"C:\a\..\..\etc"), r"C:\etc");
        assert_eq!(normalize(r"C:\a\.\b"), r"C:\a\b");
        assert_eq!(normalize(r"C:/mixed\style/x"), r"C:\mixed\style\x");
        assert_eq!(normalize(r"\\server\share\..\other"), r"\\server\other");
    }
}
