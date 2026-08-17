//! End-to-end tests for the `nitr` binary: version, effective-config
//! printing, `nitr build` artifacts, and pidfile-based reload.

use std::path::PathBuf;
use std::process::Command;

fn nitr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nitr"))
}

/// A scratch application directory scaffolded by `nitr init`.
fn scaffold(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nitr-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let out = nitr()
        .arg("init")
        .arg(&dir)
        .output()
        .expect("run nitr init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

#[test]
fn version_flag_prints_the_crate_version() {
    for flag in ["-v", "--version"] {
        let out = nitr().arg(flag).output().expect("run nitr");
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            stdout.trim(),
            format!("nitr {}", env!("CARGO_PKG_VERSION")),
            "flag {flag}"
        );
    }
}

#[test]
fn check_print_config_shows_the_effective_layering() {
    let dir = scaffold("print-config");
    let out = nitr()
        .current_dir(&dir)
        .env("NITR_WORKERS", "3")
        .args(["check", "--print-config"])
        .output()
        .expect("run check");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The file's value…
    assert!(
        stdout.contains("listen = \"127.0.0.1:3000\""),
        "got: {stdout}"
    );
    // …and the environment override that beat the default.
    assert!(stdout.contains("workers = 3"), "got: {stdout}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn build_produces_a_self_contained_artifact() {
    let dir = scaffold("build");
    let artifact = dir.join("myapp");
    let out = nitr()
        .current_dir(&dir)
        .args(["build", "--output"])
        .arg(&artifact)
        .output()
        .expect("run build");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Bigger than the plain binary: the application rides along.
    let base = std::fs::metadata(env!("CARGO_BIN_EXE_nitr"))
        .expect("meta")
        .len();
    let bundled = std::fs::metadata(&artifact).expect("meta").len();
    assert!(bundled > base, "artifact {bundled} <= binary {base}");

    // The artifact validates its own embedded application from an empty
    // working directory — no dependency on the build layout.
    let empty = dir.join("elsewhere");
    std::fs::create_dir_all(&empty).expect("mkdir");
    let out = Command::new(&artifact)
        .current_dir(&empty)
        .arg("check")
        .output()
        .expect("run the artifact");
    assert!(
        out.status.success(),
        "bundled check failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ok:"), "got: {stdout}");

    // Building from a bundle is refused: bundles are built from the plain
    // binary, not stacked.
    let out = Command::new(&artifact)
        .current_dir(&dir)
        .args(["build", "--output", "twice"])
        .output()
        .expect("run build on bundle");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already carries a bundle"),
        "got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// `nitr run` writes the configured pidfile, `nitr reload` signals through
/// it, and a graceful exit removes it.
#[cfg(unix)]
#[test]
fn pidfile_reload_and_cleanup() {
    let dir = scaffold("pidfile");
    // Port 0 so parallel test runs cannot collide; the pidfile is the
    // contract under test, not the address.
    std::fs::write(
        dir.join("nitr.toml"),
        "listen = \"127.0.0.1:0\"\nhandler_script = \"app.lua\"\npidfile = \"nitr.pid\"\n\
         [shutdown]\ngrace = 5\n",
    )
    .expect("write config");

    // tracing's fmt subscriber writes to stdout: that is where the
    // "listening" line will appear.
    let log = std::fs::File::create(dir.join("server.log")).expect("log file");
    let mut child = nitr()
        .current_dir(&dir)
        .arg("run")
        .stdout(log)
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn nitr run");

    // Wait for the "listening" line: it is logged after the SIGHUP handler
    // is installed, so a reload sent from here on cannot hit the default
    // disposition (which would terminate the process).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let logged = std::fs::read_to_string(dir.join("server.log")).unwrap_or_default();
        if logged.contains("listening on") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server never started listening; log so far: {logged}"
        );
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "server exited early"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let pidfile = dir.join("nitr.pid");
    assert!(
        pidfile.is_file(),
        "pidfile must exist once the server is up"
    );
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .expect("read pidfile")
        .trim()
        .parse()
        .expect("pid");
    assert_eq!(pid, child.id());

    // `nitr reload` finds the server through the pidfile.
    let out = nitr()
        .current_dir(&dir)
        .arg("reload")
        .output()
        .expect("run reload");
    assert!(
        out.status.success(),
        "reload failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A graceful stop (SIGTERM) removes the pidfile on the way out.
    signal(pid, "-TERM");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while child.try_wait().expect("try_wait").is_none() {
        assert!(std::time::Instant::now() < deadline, "server never exited");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(!pidfile.exists(), "a clean exit must remove the pidfile");

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
fn signal(pid: u32, sig: &str) {
    let status = Command::new("kill")
        .args([sig, &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill {sig} {pid} failed");
}
