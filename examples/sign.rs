//! Release-time signing tool for the update channel (used by tools/release.ps1, never shipped).
//!
//!   cargo run --release --example sign -- pubkey keys/update-signing.key        print the public key (hex) to paste into src/update.rs
//!   cargo run --release --example sign -- sign   keys/update-signing.key FILE   write FILE.sig (hex) next to FILE
//!
//! The key file holds the 32-byte Ed25519 seed as 64 hex digits and stays out of git (`/keys/` in .gitignore).
//! The signed message is `MSG_TAG` + SHA-256(FILE), the same bytes the app rebuilds from the file it downloaded.
use ed25519_dalek::{Signer, SigningKey};

#[path = "../src/sha256.rs"]
#[allow(dead_code)]
mod sha256;

const MSG_TAG: &[u8] = b"glasswidgets-update-v1\0"; // keep in sync with src/update.rs

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let key = |p: &str| -> SigningKey {
        let raw = unhex(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {p}: {e}"))).expect("key file is not hex");
        SigningKey::from_bytes(&raw.try_into().expect("key must be 32 bytes (64 hex digits)"))
    };
    match a.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["pubkey", k] => println!("{}", hex(key(k).verifying_key().as_bytes())),
        ["sign", k, f] => {
            let data = std::fs::read(f).unwrap_or_else(|e| panic!("read {f}: {e}"));
            let mut h = sha256::Sha256::new();
            h.update(&data);
            let mut msg = MSG_TAG.to_vec();
            msg.extend_from_slice(&h.finish());
            let sig = key(k).sign(&msg);
            std::fs::write(format!("{f}.sig"), hex(&sig.to_bytes())).expect("write .sig");
            println!("signed {f}");
        }
        _ => {
            eprintln!("usage: sign pubkey <keyfile> | sign sign <keyfile> <file>");
            std::process::exit(2);
        }
    }
}
