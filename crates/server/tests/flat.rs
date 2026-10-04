//! The move to one flat threads directory (issue #9): `migrate` renames
//! every `<base>/<project>/<id>.jsonl` to `<base>/<id>.jsonl`, leaves a
//! log another process is driving where it is, and never rewrites a
//! byte. Nothing here touches a real threads directory: every base is a
//! `tempdir`.

use std::fs::File;
use std::path::{Path, PathBuf};

use aigentic_server::migrate::migrate;
use ulid::Ulid;

/// Write `<dir>/<id>.jsonl` holding `body`, making `dir` first.
fn write_log(dir: &Path, id: Ulid, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, body).unwrap();
    path
}

/// A log's body, so the test can compare bytes across the move.
fn body(id: Ulid) -> String {
    format!("{{\"kind\":\"thread_started\",\"id\":\"{id}\"}}\n")
}

/// Hold `path` — creating it — the way a running build does, so
/// `migrate` sees `WouldBlock`.
fn hold(path: &Path) -> File {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.try_lock().expect("the lock is free");
    file
}

fn flat(base: &Path, id: Ulid) -> PathBuf {
    base.join(format!("{id}.jsonl"))
}

#[test]
fn migrate_moves_legacy_logs_flat_and_leaves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    std::fs::create_dir_all(&base).unwrap();

    let a = Ulid::generate();
    let b = Ulid::generate();
    let c = Ulid::generate();
    let d = Ulid::generate();
    let e = Ulid::generate();
    let f = Ulid::generate();

    let p = base.join("p");
    let q = base.join("q");
    let none = base.join("_none");
    let hidden = base.join(".hidden");

    write_log(&p, a, &body(a));
    let b_legacy = write_log(&p, b, &body(b));
    // b is a lead someone drives: its lock file is there, free.
    std::fs::write(p.join(format!("{b}.lock")), b"lock\n").unwrap();
    write_log(&q, c, &body(c));
    write_log(&none, d, &body(d));
    // A pre-phase-4 log, already flat.
    let e_flat = write_log(&base, e, &body(e));
    let f_legacy = write_log(&hidden, f, &body(f));
    // Not a log, and not ours to touch.
    std::fs::write(p.join("issue-7.lock"), b"issue\n").unwrap();

    let before_b = std::fs::read(&b_legacy).unwrap();
    let before_e = std::fs::read(&e_flat).unwrap();
    let before_f = std::fs::read(&f_legacy).unwrap();

    let out = migrate(&base).unwrap();
    assert_eq!(out.moved, 4, "a, b, c and d move: {out:?}");
    assert!(out.held.is_empty(), "{out:?}");
    assert!(out.clashes.is_empty(), "{out:?}");

    for id in [a, b, c, d] {
        assert!(flat(&base, id).is_file(), "{id} is flat");
    }
    assert!(!b_legacy.exists(), "b's legacy log is gone");
    assert!(
        !p.join(format!("{b}.lock")).exists(),
        "b's lock goes with it"
    );
    assert!(!q.join(format!("{c}.jsonl")).exists());
    assert!(!none.join(format!("{d}.jsonl")).exists());
    // The bytes are the same bytes.
    assert_eq!(std::fs::read(flat(&base, b)).unwrap(), before_b);
    // Untouched: another project's issue lock, a dot-named directory, a
    // flat log that had no legacy file.
    assert_eq!(std::fs::read(p.join("issue-7.lock")).unwrap(), b"issue\n");
    assert_eq!(std::fs::read(&f_legacy).unwrap(), before_f);
    assert_eq!(std::fs::read(&e_flat).unwrap(), before_e);

    // Idempotent: nothing is left to move.
    let again = migrate(&base).unwrap();
    assert_eq!(again.moved, 0, "{again:?}");

    // A base that doesn't exist is nothing to do, not an error.
    let missing = migrate(&dir.path().join("nope")).unwrap();
    assert_eq!(missing.moved, 0, "{missing:?}");
    assert!(missing.held.is_empty() && missing.clashes.is_empty());
}

#[test]
fn migrate_holds_a_driven_lead_and_skips_a_clash() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    std::fs::create_dir_all(&base).unwrap();

    let b = Ulid::generate();
    let c = Ulid::generate();

    let p = base.join("p");
    let q = base.join("q");
    let b_legacy = write_log(&p, b, &body(b));
    let b_lock = p.join(format!("{b}.lock"));
    let held = hold(&b_lock);
    // c has a legacy log and a flat one: a clash.
    let c_legacy = write_log(&q, c, &body(c));
    let c_flat = write_log(&base, c, &body(c));

    let out = migrate(&base).unwrap();
    assert_eq!(out.moved, 0, "{out:?}");
    assert_eq!(out.held, vec![b], "{out:?}");
    assert_eq!(out.clashes, vec![c], "{out:?}");
    // b stays where it is, log and lock both.
    assert!(b_legacy.is_file());
    assert!(!flat(&base, b).exists());
    assert!(b_lock.is_file());
    // The flat log wins and the legacy file is left in place.
    assert!(c_flat.is_file());
    assert!(c_legacy.is_file());

    // Release it, as the driven build ending would, and the next start
    // moves b: a held lead is moved by a later daemon start.
    held.unlock().unwrap();
    drop(held);
    let after = migrate(&base).unwrap();
    assert_eq!(after.moved, 1, "{after:?}");
    assert!(after.held.is_empty(), "{after:?}");
    assert!(flat(&base, b).is_file());
    assert!(!b_legacy.exists());
    assert!(!b_lock.exists());
}

#[test]
fn two_migrations_at_once_move_every_log_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    std::fs::create_dir_all(&base).unwrap();

    let mut ids = Vec::new();
    for i in 0..50 {
        let id = Ulid::generate();
        let sub = base.join(format!("p{}", i % 5));
        write_log(&sub, id, &body(id));
        ids.push(id);
    }

    let one = base.clone();
    let two = base.clone();
    let t1 = std::thread::spawn(move || migrate(&one).unwrap().moved);
    let t2 = std::thread::spawn(move || migrate(&two).unwrap().moved);
    let (m1, m2) = (t1.join().unwrap(), t2.join().unwrap());
    assert_eq!(m1 + m2, 50, "{m1} + {m2}");
    for id in ids {
        assert!(flat(&base, id).is_file(), "{id} is flat");
    }
    for i in 0..5 {
        let sub = base.join(format!("p{i}"));
        assert!(
            std::fs::read_dir(&sub).unwrap().next().is_none(),
            "p{i} kept something"
        );
    }
}
