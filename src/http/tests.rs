use super::*;
use crate::{fetch, policy::Role};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

const CONFIG_HOST: &str = "configserver-dre.platform.hicloud.com";
const CDN_HOST: &str = "configdownload-dre.dbankcdn.com";
const AGNSS_HOST: &str = "geo-dre.platform.dbankcloud.com";

fn local_client(listener: &TcpListener) -> Result<Client> {
    let mut client = Client::new()?;
    let address = listener.local_addr()?;
    client.huawei = huawei_builder()
        .resolve(CONFIG_HOST, address)
        .resolve(CDN_HOST, address)
        .resolve(AGNSS_HOST, address)
        .build()?;
    Ok(client)
}

fn read_request(stream: &TcpStream) -> Result<Vec<String>> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream);
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        ensure!(reader.read_line(&mut line)? != 0, "incomplete request");
        if line == "\r\n" {
            return Ok(headers);
        }
        headers.push(line.trim_end().to_owned());
    }
}

fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

#[test]
fn huawei_headers_redirects_and_gzip_layers() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let client = local_client(&listener)?;
    let rtcm = [
        0xd3, 0, 0x0b, 0xfd, 0x80, 6, 0x0b, 3, 0xff, 0xfe, 0x2e, 6, 0xfe, 0xf8, 0x49, 0xcf, 0x19,
    ];
    let product = gzip(&rtcm)?;
    let wire_product = product.clone();
    let server = thread::spawn(move || -> Result<Vec<Vec<String>>> {
        let mut captured = Vec::new();
        for index in 0..5 {
            let (mut stream, _) = listener.accept()?;
            captured.push(read_request(&stream)?);
            if index < 2 {
                let target = if index == 0 { CDN_HOST } else { "127.0.0.1" };
                write!(
                    stream,
                    "HTTP/1.1 302 Found\r\nLocation: http://{target}:{port}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )?;
            } else {
                let (body, encoding) = if index == 4 {
                    (gzip(&wire_product)?, "Content-Encoding: gzip\r\n")
                } else {
                    (wire_product.clone(), "")
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{encoding}Connection: close\r\n\r\n",
                    body.len()
                )?;
                stream.write_all(&body)?;
            }
        }
        Ok(captured)
    });
    let config = Url::parse(&format!("http://{CONFIG_HOST}:{port}/"))?;
    assert_eq!(
        client.get(&config, Some("private-validator"), None)?.bytes,
        product
    );
    for _ in 0..2 {
        let url = Url::parse(&format!("http://{AGNSS_HOST}:{port}/"))?;
        let response = client.get(&url, None, None)?;
        assert_eq!(response.bytes, product);
        assert_eq!(decompress(&response.bytes)?.as_ref(), rtcm);
        fetch::validate(Role::Agnss, &response.bytes)?;
    }
    let requests = server.join().expect("HTTP test thread panicked")?;
    for (index, host) in [CONFIG_HOST, CDN_HOST].into_iter().enumerate() {
        let headers = &requests[index];
        let uuid = headers[1]
            .strip_prefix("traceId: ")
            .context("traceId case or order")?;
        assert_eq!(uuid::Uuid::parse_str(uuid)?.get_version_num(), 4);
        assert_eq!(
            &headers[2..7],
            &[
                "App-ID: HealthApp".to_owned(),
                format!("Host: {host}:{port}"),
                "Connection: Keep-Alive".into(),
                "Accept-Encoding: gzip".into(),
                format!("User-Agent: {HUAWEI_USER_AGENT}"),
            ]
        );
        assert_eq!(headers.len(), if index == 0 { 8 } else { 7 });
    }
    assert_ne!(requests[0][1], requests[1][1]);
    assert!(requests[2].iter().any(|h| h == "Accept-Encoding: identity"));
    assert!(
        requests[2]
            .iter()
            .any(|h| h.starts_with("User-Agent: weinav-forge/"))
    );
    assert!(!requests[2].iter().any(|h| h.contains("HealthApp")
        || h.contains("traceId")
        || h.contains("private-validator")));
    for headers in &requests[3..] {
        assert_eq!(headers.len(), 8);
        assert_eq!(headers[1], "X-Device-Type: SportsHealth");
        let uuid = headers[2]
            .strip_prefix("X-Request-ID: ")
            .context("request ID case or order")?;
        assert_eq!(uuid::Uuid::parse_str(uuid)?.get_version_num(), 4);
        assert_eq!(
            &headers[3..],
            &[
                "Content-Type: application/json".to_owned(),
                format!("Host: {AGNSS_HOST}:{port}"),
                "Connection: Keep-Alive".into(),
                "Accept-Encoding: gzip".into(),
                format!("User-Agent: {HUAWEI_USER_AGENT}"),
            ]
        );
    }
    assert_ne!(requests[3][2], requests[4][2]);
    Ok(())
}

#[test]
fn tls_client_hello_matches_known_networkkit_constraints() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let client = local_client(&listener)?;
    let url = Url::parse(&format!(
        "https://{AGNSS_HOST}:{}/",
        listener.local_addr()?.port()
    ))?;
    let server = thread::spawn(move || -> Result<Vec<u8>> {
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut header = [0; 5];
        stream.read_exact(&mut header)?;
        assert_eq!(header[0], 22);
        let mut hello = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
        stream.read_exact(&mut hello)?;
        Ok(hello)
    });
    assert!(client.get(&url, None, None).is_err());
    let hello = server.join().expect("TLS test thread panicked")?;
    assert_eq!(hello[0], 1);
    let mut offset = 4 + 2 + 32;
    offset += 1 + hello[offset] as usize;
    let cipher_length = u16::from_be_bytes([hello[offset], hello[offset + 1]]) as usize;
    offset += 2;
    let ciphers: Vec<u16> = hello[offset..offset + cipher_length]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| u16::from_be_bytes([v[0], v[1]]))
        .collect();
    assert_eq!(
        ciphers,
        [
            0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca8
        ]
    );
    offset += cipher_length;
    offset += 1 + hello[offset] as usize;
    offset += 2;
    let mut extensions = std::collections::BTreeMap::new();
    while offset < hello.len() {
        let kind = u16::from_be_bytes([hello[offset], hello[offset + 1]]);
        let length = u16::from_be_bytes([hello[offset + 2], hello[offset + 3]]) as usize;
        offset += 4;
        extensions.insert(kind, &hello[offset..offset + length]);
        offset += length;
    }
    assert_eq!(extensions[&16], b"\x00\x0c\x02h2\x08http/1.1");
    assert_eq!(extensions[&43], [4, 3, 4, 3, 3]);
    assert!(extensions[&0].ends_with(AGNSS_HOST.as_bytes()));
    assert!(extensions.contains_key(&35));
    assert!(!extensions.contains_key(&17513));
    assert!(!extensions.contains_key(&17613));
    assert!(!extensions.keys().any(|id| id & 0x0f0f == 0x0a0a));
    Ok(())
}

#[test]
fn http2_frames_match_networkkit_settings_and_pseudo_headers() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut client = local_client(&listener)?;
    let address = listener.local_addr()?;
    client.huawei = huawei_builder()
        .http2_only()
        .resolve(AGNSS_HOST, address)
        .build()?;
    let server = thread::spawn(move || -> Result<()> {
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut preface = [0; 24];
        stream.read_exact(&mut preface)?;
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])?;
        let mut settings = false;
        let mut window = false;
        loop {
            let mut header = [0; 9];
            stream.read_exact(&mut header)?;
            let length = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
            let stream_id = u32::from_be_bytes(header[5..9].try_into()?);
            let mut payload = vec![0; length];
            stream.read_exact(&mut payload)?;
            match header[3] {
                4 if header[4] == 0 => {
                    assert_eq!(payload, [0, 4, 1, 0, 0, 0]);
                    settings = true;
                    stream.write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])?;
                }
                8 => {
                    assert_eq!(stream_id, 0);
                    assert_eq!(u32::from_be_bytes(payload.try_into().unwrap()), 16_711_681);
                    window = true;
                }
                1 => {
                    assert!(settings && window);
                    assert_eq!(stream_id, 3);
                    assert_eq!(header[4] & 0x20, 0);
                    assert_eq!(&payload[..3], &[0x82, 0x84, 0x41]);
                    let authority_length = (payload[3] & 0x7f) as usize;
                    assert_eq!(payload[4 + authority_length], 0x86);
                    stream.write_all(&[0, 0, 1, 1, 5, 0, 0, 0, 3, 0x88])?;
                    return Ok(());
                }
                2 => panic!("unexpected HTTP/2 PRIORITY frame"),
                4 => {}
                kind => panic!("unexpected HTTP/2 frame {kind}"),
            }
        }
    });
    let response = client.get(
        &Url::parse(&format!("http://{AGNSS_HOST}:{}/", address.port()))?,
        None,
        None,
    );
    server.join().expect("HTTP/2 test thread panicked")?;
    assert!(response?.bytes.is_empty());
    Ok(())
}
