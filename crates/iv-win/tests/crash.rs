//! The panic hook of the windowless release build, in a test binary of its own (the hook is
//! process-wide). A crash of the window's thread leaves `crash.log` and tells the user; a panic
//! contained by `iv_core::panic::catch` or on a helper thread is only logged.

use std::sync::{Arc, Mutex};

use iv_win::diag::{self, CRASH_FILE, LOG_FILE};
use iv_win::i18n::Language;

#[test]
fn crashes_are_reported_and_contained_panics_only_logged() {
    let dir = tempfile::tempdir().unwrap();
    diag::init(Some(dir.path().to_path_buf()));
    let shown: Arc<Mutex<Vec<(String, String)>>> = Arc::default();

    let (seen, so_far) = (shown.clone(), shown.clone());
    let ui = std::thread::Builder::new()
        .name("ui".into())
        .spawn(move || {
            // this thread plays the window's thread
            diag::install_panic_hook(Language::English, move |title, text| {
                seen.lock().unwrap().push((title.to_string(), text.to_string()));
            });
            // a decoder crashing on a malformed file: an error, nothing shown
            let r: iv_core::Result<()> = iv_core::panic::catch("test-codec", || panic!("bad huffman table"));
            assert!(r.is_err());
            // a helper thread dying: logged, nothing shown
            assert!(std::thread::spawn(|| panic!("helper went away")).join().is_err());
            assert!(so_far.lock().unwrap().is_empty(), "nothing shown for contained panics");
            panic!("invariant broken in the window's thread");
        })
        .unwrap();
    assert!(ui.join().is_err());
    let _ = std::panic::take_hook();

    let shown = shown.lock().unwrap();
    assert_eq!(shown.len(), 1, "{shown:?}");
    let crash = dir.path().join(CRASH_FILE);
    assert_eq!(shown[0].0, "image-viewer error");
    assert!(shown[0].1.contains(&crash.display().to_string()), "the box says where the report is: {}", shown[0].1);

    let report = std::fs::read_to_string(&crash).unwrap();
    assert!(report.contains("invariant broken in the window's thread"), "{report}");
    assert!(report.contains("crash.rs:"), "with its location: {report}");
    assert!(report.contains("backtrace"), "{report}");
    // what the symbols file needs: the build, where it was loaded, and frames as offsets into it
    let exe = std::env::current_exe().unwrap();
    let exe = exe.file_name().unwrap().to_string_lossy();
    assert!(report.contains(&format!("image: {exe} at 0x")) && report.contains(".pdb {"), "{report}");
    assert!(report.contains(&format!("  [{exe}+0x")), "frame offsets: {report}");
    assert!(!report.contains("bad huffman table") && !report.contains("helper went away"), "{report}");

    let log = std::fs::read_to_string(dir.path().join(LOG_FILE)).unwrap();
    assert!(log.contains("contained panic on thread 'ui'") && log.contains("bad huffman table"), "{log}");
    assert!(log.contains("helper went away"), "{log}");
    assert!(log.contains("crash: thread 'ui'"), "{log}");
}
