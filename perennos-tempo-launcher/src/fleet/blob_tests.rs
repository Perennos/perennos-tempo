//! Blob streams between two in-process nodes on this computer's own network.

use super::blob::{self, CHUNK};
use super::msg::{build, Frame};
use super::store::{self, Machine};
use super::tests::{link, rig, Rig};
use crate::setup::json::J;
use crate::setup::sha256::file_sha256;
use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

const KEY: [u8; 32] = [7; 32];

/// Two linked nodes; `b` takes files (it allowed `stage`), and has the blob handler on.
fn pair(tag: &str) -> (Rig, Rig, Machine) {
    let a = rig(&format!("a-{tag}"), KEY);
    let b = rig(&format!("b-{tag}"), KEY);
    let mb = link(&a, &b);
    store::update(&b.paths, |f| f.allow.push("stage".into())).unwrap();
    blob::attach(&b.node);
    // a takes the answers (`bsdone`) the ordinary way: its node stores them
    (a, b, mb)
}

/// A file of `n` bytes that is not the same all through (a hash that holds for a run of one
/// byte would hold for a lot of wrong files).
fn make(rig: &Rig, name: &str, n: usize) -> PathBuf {
    let dir = rig.paths.home.join("work");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut left = n;
    while left > 0 {
        let k = left.min(1 << 20);
        let buf: Vec<u8> = (0..k)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect();
        f.write_all(&buf).unwrap();
        left -= k;
    }
    p
}

fn landed_files(r: &Rig) -> Vec<PathBuf> {
    std::fs::read_dir(blob::in_dir(&r.paths))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

fn parts(r: &Rig) -> usize {
    std::fs::read_dir(blob::blobs_dir(&r.paths)).map_or(0, |d| d.flatten().count())
}

#[test]
fn a_big_blob_crosses_intact_past_the_old_64_mib_limit() {
    // `fleet invite` starts `<this program> fleet serve` and in a test that is the test program,
    // which runs every test with "fleet" in its name: that copy has no need of the slow one
    if std::env::args().any(|a| a == "serve") {
        return;
    }
    let (a, b, mb) = pair("big");
    // 65 MiB and a bit: more than the 64 MiB the call channel took, and not a whole number of pieces
    let size = (65 << 20) + 12_345;
    let src = make(&a, "clip.mp4", size);
    let want = file_sha256(&src, &mut |_| {}).unwrap();
    let sent = blob::send_file(
        &a.node,
        &mb,
        "x-big.1",
        &src,
        "clip.mp4",
        Duration::from_secs(900),
    )
    .unwrap();
    assert_eq!((sent.size, sent.sha.as_str()), (size as u64, want.as_str()));
    let got = landed_files(&b);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with("clip.mp4"));
    assert_eq!(std::fs::metadata(&got[0]).unwrap().len(), size as u64);
    assert_eq!(file_sha256(&got[0], &mut |_| {}).unwrap(), want);
    // nothing half done is left, and the pieces never went into the inbox
    assert_eq!(parts(&b), 0);
    assert!(b
        .node
        .inbox(None, 50)
        .iter()
        .all(|m| !m.topic.starts_with("bs:")));
    // the receiver's own program can find it
    let l = blob::wait_landed(
        &b.node,
        "x-big.1",
        size as u64,
        &want,
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(l.path, got[0]);
}

#[test]
fn a_file_over_the_cap_is_turned_away_before_it_is_sent() {
    let (a, b, mb) = pair("cap");
    store::update(&b.paths, |f| f.blob_cap = 1 << 20).unwrap();
    let src = make(&a, "big.bin", 3 * CHUNK * 20); // 15 MiB
    let e = blob::send_file(
        &a.node,
        &mb,
        "x-cap.1",
        &src,
        "big.bin",
        Duration::from_secs(20),
    )
    .unwrap_err();
    assert!(
        e.contains("turned the file away") && e.contains("takes at most"),
        "{e}"
    );
    assert!(landed_files(&b).is_empty() && parts(&b) == 0);
    // the sender's own cap works the same way, and says how to change it
    store::update(&a.paths, |f| f.blob_cap = 1 << 20).unwrap();
    let e = blob::send_file(
        &a.node,
        &mb,
        "x-cap.2",
        &src,
        "big.bin",
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(e.contains("fleet cap GIB"), "{e}");
    // and the default is 4 GiB, set in fleet.json as blob_cap_gib
    let fresh = store::load(&super::tests::paths("capdef"));
    assert_eq!(fresh.blob_cap(), 4 << 30);
    store::update(&a.paths, |f| f.blob_cap = 8 << 30).unwrap();
    let text = std::fs::read_to_string(a.paths.file("fleet.json")).unwrap();
    assert!(text.contains("\"blob_cap_gib\": 8"), "{text}");
    assert_eq!(store::load(&a.paths).blob_cap(), 8 << 30);
}

#[test]
fn a_computer_takes_files_only_when_allowed_or_waiting_for_them() {
    let (a, b, mb) = pair("allow");
    store::update(&b.paths, |f| f.allow.clear()).unwrap();
    let src = make(&a, "in.png", 300_000);
    let e = blob::send_file(
        &a.node,
        &mb,
        "x-al.1",
        &src,
        "in.png",
        Duration::from_secs(10),
    )
    .unwrap_err();
    assert!(e.contains("fleet allow stage"), "{e}");
    assert!(landed_files(&b).is_empty());
    // the answer to a call it made: waiting for that call's files from that computer
    blob::expect(&b.paths, "call9", &a.node.me).unwrap();
    blob::send_file(
        &a.node,
        &mb,
        "call9.0",
        &src,
        "in.png",
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(landed_files(&b).len(), 1);
    // from another computer's name it is still refused
    blob::expect(&b.paths, "call8", "someone-else").unwrap();
    assert!(blob::send_file(
        &a.node,
        &mb,
        "call8.0",
        &src,
        "in.png",
        Duration::from_secs(10)
    )
    .is_err());
}

/// A frame that is well formed and signed, with one bit of its sealed body changed (a piece
/// altered on the wire): sent raw to `to`'s port.
fn spoiled_piece(from: &Rig, to: &Rig, topic: &str, body: &[u8]) {
    let sk = super::pair::identity(&from.paths).unwrap();
    let f = Frame {
        kind: 'm',
        id: format!("spoil-{topic}"),
        from: from.node.me.clone(),
        to: to.node.me.clone(),
        topic: topic.into(),
        ts: store::now_ms(),
        nonce: format!("spoil-nonce-{topic}"),
        body: body.to_vec(),
        ..Frame::default()
    };
    let mut s = TcpStream::connect(("127.0.0.1", to.port)).unwrap();
    s.write_all(&build(&KEY, &sk, &f, true)).unwrap();
    // the receiver ends the connection without an answer
    let mut one = [0u8; 1];
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    assert!(std::io::Read::read(&mut s, &mut one).map_or(true, |n| n == 0));
}

#[test]
fn a_piece_changed_on_the_wire_fails_closed_and_the_file_is_not_kept() {
    let (a, b, mb) = pair("spoil");
    let size = 2 * CHUNK + 1000;
    let src = make(&a, "clip.mp4", size);
    let bytes = std::fs::read(&src).unwrap();
    let sha = crate::setup::sha256::sha256_hex(&bytes);
    let begin = J::Obj(vec![
        ("xfer".into(), J::s("x-sp.1")),
        ("name".into(), J::s("clip.mp4")),
        ("size".into(), J::n(size as f64)),
        ("sha".into(), J::s(&sha)),
        ("n".into(), J::n(3.0)),
    ])
    .compact();
    a.node
        .send_direct(&mb, &[("bs:x-sp.1:begin", begin.as_bytes())])
        .unwrap();
    let before = super::msg::unopened();
    // piece 0 is altered in transit: the seal does not open, the piece is dropped
    spoiled_piece(&a, &b, "bs:x-sp.1:0", &bytes[..CHUNK]);
    assert!(super::msg::unopened() > before);
    a.node
        .send_direct(&mb, &[("bs:x-sp.1:1", &bytes[CHUNK..2 * CHUNK])])
        .unwrap();
    a.node
        .send_direct(&mb, &[("bs:x-sp.1:2", &bytes[2 * CHUNK..])])
        .unwrap();
    a.node.send_direct(&mb, &[("bs:x-sp.1:end", b"")]).unwrap();
    let r = a
        .node
        .wait_for("bsdone", "xfer", "x-sp.1", Duration::from_secs(10))
        .expect("the receiver says what happened");
    let t = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        t.contains("\"ok\":false") && t.contains("2 of 3 pieces"),
        "{t}"
    );
    assert!(landed_files(&b).is_empty(), "nothing is put in place");
    assert!(b
        .node
        .wait_for("blobin", "xfer", "x-sp.1", Duration::from_millis(100))
        .is_none());
    // the half file went to the trash, not into the blobs folder
    assert_eq!(parts(&b), 0);
    assert!(std::fs::read_dir(b.paths.home.join("trash")).is_ok_and(|d| d.count() > 0));
}

#[test]
fn a_piece_with_the_wrong_bytes_fails_the_hash_check_and_nothing_is_renamed() {
    let (a, b, mb) = pair("hash");
    let size = 2 * CHUNK;
    let src = make(&a, "clip.mp4", size);
    let mut bytes = std::fs::read(&src).unwrap();
    let sha = crate::setup::sha256::sha256_hex(&bytes);
    // a sender whose second piece is not what the hash was made from
    bytes[CHUNK + 17] ^= 0x40;
    let begin = J::Obj(vec![
        ("xfer".into(), J::s("x-h.1")),
        ("name".into(), J::s("clip.mp4")),
        ("size".into(), J::n(size as f64)),
        ("sha".into(), J::s(&sha)),
        ("n".into(), J::n(2.0)),
    ])
    .compact();
    a.node
        .send_direct(&mb, &[("bs:x-h.1:begin", begin.as_bytes())])
        .unwrap();
    a.node
        .send_direct(
            &mb,
            &[
                ("bs:x-h.1:0", &bytes[..CHUNK]),
                ("bs:x-h.1:1", &bytes[CHUNK..]),
                ("bs:x-h.1:end", b""),
            ],
        )
        .unwrap();
    let r = a
        .node
        .wait_for("bsdone", "xfer", "x-h.1", Duration::from_secs(10))
        .unwrap();
    let t = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        t.contains("\"ok\":false") && t.contains("SHA-256 does not match"),
        "{t}"
    );
    assert!(landed_files(&b).is_empty() && parts(&b) == 0);
    // the sender's own wait says so too
    let e = blob::wait_landed(
        &a.node,
        "x-h.1",
        size as u64,
        &sha,
        Duration::from_millis(300),
    )
    .unwrap_err();
    assert!(
        e.contains("turned away") || e.contains("never arrived"),
        "{e}"
    );
}

#[test]
fn a_piece_of_the_wrong_size_or_from_another_computer_is_not_written() {
    let (a, b, mb) = pair("odd");
    let c = rig("c-odd", KEY);
    let mc = link(&c, &b);
    let _ = mc;
    let size = CHUNK + 5;
    let bytes = vec![9u8; size];
    let sha = crate::setup::sha256::sha256_hex(&bytes);
    let begin = J::Obj(vec![
        ("xfer".into(), J::s("x-o.1")),
        ("name".into(), J::s("a.bin")),
        ("size".into(), J::n(size as f64)),
        ("sha".into(), J::s(&sha)),
        ("n".into(), J::n(2.0)),
    ])
    .compact();
    a.node
        .send_direct(&mb, &[("bs:x-o.1:begin", begin.as_bytes())])
        .unwrap();
    // c is paired with b too, but this transfer is a's: c's piece is ignored
    let mcb = store::load(&c.paths)
        .peers()
        .find(|m| m.name == b.node.me)
        .cloned()
        .unwrap();
    c.node
        .send_direct(&mcb, &[("bs:x-o.1:0", &bytes[..CHUNK])])
        .unwrap();
    // a piece too long for its place poisons the transfer
    a.node
        .send_direct(&mb, &[("bs:x-o.1:0", &bytes[..CHUNK - 1])])
        .unwrap();
    a.node
        .send_direct(
            &mb,
            &[("bs:x-o.1:1", &bytes[CHUNK..]), ("bs:x-o.1:end", b"")],
        )
        .unwrap();
    let r = a
        .node
        .wait_for("bsdone", "xfer", "x-o.1", Duration::from_secs(10))
        .unwrap();
    let t = String::from_utf8_lossy(&r.body).into_owned();
    assert!(
        t.contains("\"ok\":false") && t.contains("not the size"),
        "{t}"
    );
    assert!(landed_files(&b).is_empty());
}
