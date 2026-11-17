//! Claude 指纹车道回环回归：真实 `build_fingerprint_client` 发出的 ClientHello
//! 必须算出 `TARGET_JA3_HASH` / `TARGET_JA4`。只读第一个 ClientHello 就断开，
//! 不做握手、不联网。btls / wreq 升级导致 Hello 漂移时这里会红。

use ccextra_server::claude_tls_fp::{
    TARGET_JA3_HASH, TARGET_JA4, build_fingerprint_client, ja3_hash,
};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

fn is_grease(v: u16) -> bool {
    v & 0x0f0f == 0x0a0a && (v >> 8) == (v & 0xff)
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn u16_list(b: &[u8]) -> Vec<u16> {
    b.as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes(*c))
        .collect()
}

/// 从握手消息（不含记录头）算 (ja3_string, ja4, ja4_r)。
fn fingerprint(hs: &[u8]) -> (String, String, String) {
    assert_eq!(hs[0], 1, "not a ClientHello");
    let b = &hs[4..];
    let ver = u16_at(b, 0);
    let mut q = 34;
    q += 1 + b[q] as usize; // session id
    let n = u16_at(b, q) as usize;
    let ciphers = u16_list(&b[q + 2..q + 2 + n]);
    q += 2 + n;
    q += 1 + b[q] as usize; // compression
    let end = q + 2 + u16_at(b, q) as usize;
    q += 2;
    let (mut exts, mut groups, mut pf, mut sig, mut sv) = (vec![], vec![], vec![], vec![], vec![]);
    let (mut sni, mut alpn) = (false, String::new());
    while q + 4 <= end {
        let t = u16_at(b, q);
        let l = u16_at(b, q + 2) as usize;
        let d = &b[q + 4..q + 4 + l];
        q += 4 + l;
        exts.push(t);
        match t {
            0 => sni = true,
            10 => groups = u16_list(&d[2..2 + u16_at(d, 0) as usize]),
            11 => pf = d[1..1 + d[0] as usize].to_vec(),
            13 => sig = u16_list(&d[2..2 + u16_at(d, 0) as usize]),
            16 if alpn.is_empty() && d.len() > 3 => {
                alpn = String::from_utf8_lossy(&d[3..3 + d[2] as usize]).into_owned()
            }
            43 => sv = u16_list(&d[1..1 + d[0] as usize]),
            _ => {}
        }
    }
    let ng = |xs: &[u16]| {
        xs.iter()
            .copied()
            .filter(|x| !is_grease(*x))
            .collect::<Vec<_>>()
    };
    let (c, e, g) = (ng(&ciphers), ng(&exts), ng(&groups));
    let join = |xs: &[u16]| xs.iter().map(u16::to_string).collect::<Vec<_>>().join("-");
    let pfs = pf.iter().map(u8::to_string).collect::<Vec<_>>().join("-");
    let ja3 = format!("{ver},{},{},{},{pfs}", join(&c), join(&e), join(&g));

    let top = ng(&sv).into_iter().max().unwrap_or(ver);
    let v = match top {
        0x0304 => "13",
        0x0303 => "12",
        0x0302 => "11",
        0x0301 => "10",
        _ => "00",
    };
    let a = match (alpn.chars().next(), alpn.chars().last()) {
        (Some(f), Some(l)) => format!("{f}{l}"),
        _ => "00".into(),
    };
    let pre = format!(
        "t{v}{}{:02}{:02}{a}",
        if sni { "d" } else { "i" },
        c.len(),
        e.len()
    );
    let hex4 = |xs: Vec<u16>| {
        xs.iter()
            .map(|x| format!("{x:04x}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut cs = c;
    cs.sort();
    let mut es: Vec<u16> = e.iter().copied().filter(|x| *x != 0 && *x != 16).collect();
    es.sort();
    let (cs, es, ss) = (hex4(cs), hex4(es), hex4(ng(&sig)));
    let h12 = |s: &str| hex::encode(Sha256::digest(s.as_bytes()))[..12].to_string();
    let part_c = if ss.is_empty() {
        es.clone()
    } else {
        format!("{es}_{ss}")
    };
    let ja4 = format!("{pre}_{}_{}", h12(&cs), h12(&part_c));
    (ja3, ja4, format!("{pre}_{cs}_{es}_{ss}"))
}

/// 读满第一个握手消息（可能跨多个 TLS 记录），返回去掉记录头后的握手字节。
async fn read_client_hello(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        let n = sock.read(&mut buf).await.expect("read");
        assert!(n > 0, "peer closed before full ClientHello");
        raw.extend_from_slice(&buf[..n]);
        let (mut p, mut hs) = (0, Vec::new());
        while p + 5 <= raw.len() && raw[p] == 0x16 {
            let l = u16_at(&raw, p + 3) as usize;
            if p + 5 + l > raw.len() {
                break;
            }
            hs.extend_from_slice(&raw[p + 5..p + 5 + l]);
            p += 5 + l;
        }
        if hs.len() >= 4 && hs.len() >= 4 + u32::from_be_bytes([0, hs[1], hs[2], hs[3]]) as usize {
            return hs;
        }
    }
}

#[tokio::test]
async fn claude_lane_client_hello_matches_target() {
    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ln.local_addr().unwrap().port();
    let capture = tokio::spawn(async move {
        let (mut sock, _) = ln.accept().await.unwrap();
        read_client_hello(&mut sock).await // drop 即断开，客户端握手失败属预期
    });
    let client = build_fingerprint_client("direct", false).unwrap();
    // 用主机名而不是 IP：要带 SNI，JA4 前缀才是 d
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.get(format!("https://localhost:{port}/")).send(),
    )
    .await;
    let hs = tokio::time::timeout(std::time::Duration::from_secs(10), capture)
        .await
        .expect("no ClientHello captured")
        .unwrap();
    let (ja3, ja4, ja4_r) = fingerprint(&hs);
    assert_eq!(ja4, TARGET_JA4, "JA4 drift; ja4_r={ja4_r}");
    assert_eq!(ja3_hash(&ja3), TARGET_JA3_HASH, "JA3 drift; ja3={ja3}");
}
