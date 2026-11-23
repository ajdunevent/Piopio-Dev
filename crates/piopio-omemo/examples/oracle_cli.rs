//! A tiny command-line front end used by `tests/oracle/check.py` to test
//! this crate against python-oldmemo. State lives in JSON files; bytes
//! travel as hex. Not part of the library.

use std::fs;

use piopio_omemo::*;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn key32(s: &str) -> [u8; 32] {
    decode_key(&unhex(s)).unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let read = |p: &str| fs::read_to_string(p).unwrap();
    match a[0].as_str() {
        "gen" => {
            let keys = OwnKeys::generate(a[1].parse().unwrap());
            fs::write(&a[2], serde_json::to_string(&keys).unwrap()).unwrap();
            let b = keys.bundle();
            let out = serde_json::json!({
                "identity": hex(&encode_key(&b.identity_key)),
                "spk_id": b.signed_pre_key_id,
                "spk": hex(&encode_key(&b.signed_pre_key)),
                "sig": hex(&b.signed_pre_key_signature),
                "prekeys": b.pre_keys.iter().map(|(id, k)| (id, hex(&encode_key(k)))).collect::<Vec<_>>(),
            });
            println!("{out}");
        }
        "initiate" => {
            let keys: OwnKeys = serde_json::from_str(&read(&a[1])).unwrap();
            let j: serde_json::Value = serde_json::from_str(&read(&a[2])).unwrap();
            let bundle = Bundle {
                identity_key: key32(j["identity"].as_str().unwrap()),
                signed_pre_key_id: j["spk_id"].as_u64().unwrap() as u32,
                signed_pre_key: key32(j["spk"].as_str().unwrap()),
                signed_pre_key_signature: unhex(j["sig"].as_str().unwrap()).try_into().unwrap(),
                pre_keys: j["prekeys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| (p[0].as_u64().unwrap() as u32, key32(p[1].as_str().unwrap())))
                    .collect(),
            };
            let session = initiate(&keys, &bundle).unwrap();
            fs::write(&a[3], serde_json::to_string(&session).unwrap()).unwrap();
        }
        "encrypt" => {
            let mut s: Session = serde_json::from_str(&read(&a[1])).unwrap();
            let e = s.encrypt(&unhex(&a[2]));
            fs::write(&a[1], serde_json::to_string(&s).unwrap()).unwrap();
            println!(
                "{}",
                serde_json::json!({"data": hex(&e.data), "prekey": e.prekey})
            );
        }
        "accept" => {
            let mut keys: OwnKeys = serde_json::from_str(&read(&a[1])).unwrap();
            let kex = parse_key_exchange(&unhex(&a[2])).unwrap();
            let (s, plain) = kex.accept(&mut keys).unwrap();
            fs::write(&a[1], serde_json::to_string(&keys).unwrap()).unwrap();
            fs::write(&a[3], serde_json::to_string(&s).unwrap()).unwrap();
            println!("{}", hex(&plain));
        }
        "decrypt" => {
            let mut s: Session = serde_json::from_str(&read(&a[1])).unwrap();
            let plain = s.decrypt(&unhex(&a[2])).unwrap();
            fs::write(&a[1], serde_json::to_string(&s).unwrap()).unwrap();
            println!("{}", hex(&plain));
        }
        "payload-encrypt" => {
            let p = encrypt_payload(a[1].as_bytes());
            println!(
                "{}",
                serde_json::json!({"key": hex(&p.key_material), "iv": hex(&p.iv), "ct": hex(&p.ciphertext)})
            );
        }
        "payload-decrypt" => {
            let plain = decrypt_payload(&unhex(&a[1]), &unhex(&a[2]), &unhex(&a[3])).unwrap();
            println!("{}", String::from_utf8(plain).unwrap());
        }
        "v2-gen" => {
            let keys = v2::OwnKeys::new(a[1].parse().unwrap(), KeyPair::generate());
            fs::write(&a[2], serde_json::to_string(&keys).unwrap()).unwrap();
            let b = keys.bundle();
            let out = serde_json::json!({
                "identity": hex(&b.identity_key),
                "spk_id": b.signed_pre_key_id,
                "spk": hex(&b.signed_pre_key),
                "sig": hex(&b.signed_pre_key_signature),
                "prekeys": b.pre_keys.iter().map(|(id, k)| (id, hex(k))).collect::<Vec<_>>(),
            });
            println!("{out}");
        }
        "v2-initiate" => {
            let keys: v2::OwnKeys = serde_json::from_str(&read(&a[1])).unwrap();
            let j: serde_json::Value = serde_json::from_str(&read(&a[2])).unwrap();
            let k = |s: &serde_json::Value| -> [u8; 32] {
                unhex(s.as_str().unwrap()).try_into().unwrap()
            };
            let bundle = v2::Bundle {
                identity_key: k(&j["identity"]),
                signed_pre_key_id: j["spk_id"].as_u64().unwrap() as u32,
                signed_pre_key: k(&j["spk"]),
                signed_pre_key_signature: unhex(j["sig"].as_str().unwrap()).try_into().unwrap(),
                pre_keys: j["prekeys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| (p[0].as_u64().unwrap() as u32, k(&p[1])))
                    .collect(),
            };
            let session = v2::initiate(&keys, &bundle).unwrap();
            fs::write(&a[3], serde_json::to_string(&session).unwrap()).unwrap();
        }
        "v2-encrypt" => {
            let mut s: v2::Session = serde_json::from_str(&read(&a[1])).unwrap();
            let e = s.encrypt(&unhex(&a[2]));
            fs::write(&a[1], serde_json::to_string(&s).unwrap()).unwrap();
            println!(
                "{}",
                serde_json::json!({"data": hex(&e.data), "kex": e.kex})
            );
        }
        "v2-accept" => {
            let mut keys: v2::OwnKeys = serde_json::from_str(&read(&a[1])).unwrap();
            let kex = v2::parse_key_exchange(&unhex(&a[2])).unwrap();
            let (s, plain) = kex.accept(&mut keys).unwrap();
            fs::write(&a[1], serde_json::to_string(&keys).unwrap()).unwrap();
            fs::write(&a[3], serde_json::to_string(&s).unwrap()).unwrap();
            println!("{}", hex(&plain));
        }
        "v2-decrypt" => {
            let mut s: v2::Session = serde_json::from_str(&read(&a[1])).unwrap();
            let plain = s.decrypt(&unhex(&a[2])).unwrap();
            fs::write(&a[1], serde_json::to_string(&s).unwrap()).unwrap();
            println!("{}", hex(&plain));
        }
        "v2-payload-encrypt" => {
            let p = v2::encrypt_payload(a[1].as_bytes());
            println!(
                "{}",
                serde_json::json!({"key": hex(&p.key_material), "ct": hex(&p.ciphertext)})
            );
        }
        "v2-payload-decrypt" => {
            let plain = v2::decrypt_payload(&unhex(&a[1]), &unhex(&a[2])).unwrap();
            println!("{}", String::from_utf8(plain).unwrap());
        }
        other => panic!("unknown command {other}"),
    }
}
