//! Offline check of the GitHub App JWT: mint one against a throwaway
//! RSA key and verify the RS256 signature with openssl itself, plus
//! sanity-check the claims. No network, no real credentials.

use choir_bridge::github::app_jwt;

fn b64url_decode(s: &str) -> Vec<u8> {
    let table: std::collections::HashMap<u8, u32> = (b'A'..=b'Z')
        .chain(b'a'..=b'z')
        .chain(b'0'..=b'9')
        .chain(*b"-_")
        .enumerate()
        .map(|(i, c)| (c, i as u32))
        .collect();
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        buf = (buf << 6) | table[&c];
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

#[test]
fn jwt_signs_and_verifies() {
    let work = std::env::temp_dir().join(format!("choir-jwt-test-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pem = work.join("app.pem");
    let pubkey = work.join("app.pub");

    // Throwaway RSA keypair.
    let ok = std::process::Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&pem)
        .arg("2048")
        .output()
        .expect("openssl runs")
        .status
        .success();
    assert!(ok);
    let ok = std::process::Command::new("openssl")
        .args(["rsa", "-pubout", "-in"])
        .arg(&pem)
        .arg("-out")
        .arg(&pubkey)
        .output()
        .expect("openssl runs")
        .status
        .success();
    assert!(ok);

    let jwt = app_jwt("12345", &pem).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3, "header.payload.signature");

    // Claims: issuer matches, expiry ~9 minutes out.
    let payload: serde_json::Value = serde_json::from_slice(&b64url_decode(parts[1])).unwrap();
    assert_eq!(payload["iss"], "12345");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(payload["exp"].as_i64().unwrap() > now + 400);
    assert!(payload["iat"].as_i64().unwrap() < now);

    // RS256 signature verifies against the public key.
    let signing_input = work.join("input");
    let sig = work.join("sig");
    std::fs::write(&signing_input, format!("{}.{}", parts[0], parts[1])).unwrap();
    std::fs::write(&sig, b64url_decode(parts[2])).unwrap();
    let out = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-verify"])
        .arg(&pubkey)
        .arg("-signature")
        .arg(&sig)
        .arg(&signing_input)
        .output()
        .expect("openssl runs");
    assert!(
        out.status.success(),
        "verify: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&work).ok();
}
