use super::walk_collect_files;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

// Symlinks are indexed unconditionally, classified by their target:
// symlink→file lands in the file list, symlink→dir becomes a dir marker.
// Both backends must behave identically.
#[test]
fn collects_symlinks_like_regular_files_and_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("real_dir")).unwrap();
    fs::write(root.join("real_dir/file.txt"), "x").unwrap();
    fs::write(root.join("real.txt"), "x").unwrap();

    let mut file_link_created = false;
    if symlink(&root.join("real.txt"), &root.join("link.txt")).is_ok() {
        file_link_created = true;
    }

    // Junction creation on Windows works without admin; real symlinks need
    // developer mode. Try junction first, fall back to a real symlink.
    let dir_link_created = symlink_dir(&root.join("real_dir"), &root.join("link_dir")).is_ok();

    if !file_link_created && !dir_link_created {
        eprintln!("skipping symlink test: symlink creation unsupported");
        return;
    }

    let counter = Arc::new(AtomicUsize::new(0));
    // follow_symlinks=false — symlinks must still be visible.
    let out =
        walk_collect_files(root, false, false, 1, &counter, &Default::default(), None).unwrap();
    let names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();

    if file_link_created {
        assert!(
            names.iter().any(|n| n == "link.txt"),
            "symlink→file indexed: {names:?}"
        );
        let file_link = out
            .pairs
            .iter()
            .find(|(_, rel)| rel == "link.txt")
            .expect("file link present");
        assert!(file_link.0.is_symlink());
        assert!(!file_link.0.is_symlink_dir());
    }

    if dir_link_created {
        assert!(
            names.iter().any(|n| n == "link_dir/"),
            "symlink→dir marker present: {names:?}"
        );
        // The marker is flagged so walk_filesystem can filter it from files.
        let marker = out
            .pairs
            .iter()
            .find(|(_, rel)| rel == "link_dir/")
            .expect("marker present");
        assert!(marker.0.is_symlink_dir());
    }

    assert!(
        names.iter().any(|n| n == "real.txt"),
        "regular file still indexed: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "real_dir/file.txt"),
        "real file under real dir indexed: {names:?}"
    );
}

// Helpers to create symlinks cross-platform. Windows dir junctions work
// without admin; real symlinks need developer mode.
#[cfg(unix)]
fn symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    // `symlink_dir` (real symlink) needs developer mode; junction via
    // `mklink /J` needs only normal user rights.
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => Ok(()),
        Err(_) => {
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .status();
            match status {
                Ok(s) if s.success() => Ok(()),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "symlink_dir + mklink /J both failed",
                )),
            }
        }
    }
}
