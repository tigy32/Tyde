#![cfg(unix)]

use std::process::Command;
use std::sync::Barrier;

#[test]
fn login_shell_initialization_from_background_terminal() {
    match std::env::var("TYDE_PATH_PROBE_TEST_ROLE").as_deref() {
        Ok("probe") => {
            let barrier = Barrier::new(16);
            std::thread::scope(|scope| {
                let threads: Vec<_> = (0..16)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            tyde_process_env::initialize_process_env()
                                .expect("resolve login-shell PATH")
                        })
                    })
                    .collect();
                let paths: Vec<_> = threads
                    .into_iter()
                    .map(|thread| thread.join().expect("join PATH caller"))
                    .collect();
                assert!(!paths[0].is_empty());
                assert!(paths.iter().all(|path| *path == paths[0]));
                let output = tyde_process_env::std_command("/bin/sh")
                    .expect("create child command")
                    .args(["-c", "printf '%s' \"$PATH\""])
                    .output()
                    .expect("run child with resolved PATH");
                assert!(output.status.success());
                use std::os::unix::ffi::OsStrExt;
                assert_eq!(output.stdout, paths[0].as_bytes());
            });
        }
        Ok("failure") => {
            let first = tyde_process_env::initialize_process_env().unwrap_err();
            assert!(first.contains("failed to query login-shell PATH"));
            assert_eq!(
                tyde_process_env::initialize_process_env().unwrap_err(),
                first,
                "subsequent callers must retain the original shell failure"
            );
        }
        _ => {
            let output = Command::new("python3")
                .arg("-c")
                .arg(include_str!("login_shell_terminal.py"))
                .arg(std::env::current_exe().expect("locate test executable"))
                .output()
                .expect("run real PTY and shell scenario");
            assert!(
                output.status.success(),
                "terminal PATH scenario failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
