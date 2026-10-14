use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

use feishufs::{OpenLarkOptions, OpenLarkSource};

// Exercise SDK request builders, auth selection and raw decoding without
// real credentials or a cloud service. Every unexpected request fails the test.
pub fn server(
    responses: Vec<(String, &'static str, Vec<u8>)>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    server_with_status(
        responses
            .into_iter()
            .map(|(path, mime, bytes)| (path, 200, mime, bytes))
            .collect(),
    )
}

pub fn server_with_status(
    responses: Vec<(String, u16, &'static str, Vec<u8>)>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (expected, status, content_type, body) in responses {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "SDK did not send expected request"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let (headers_end, length) = loop {
                let mut buffer = [0; 2048];
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (end + 4, length);
                }
            };
            while bytes.len() < headers_end + length {
                let mut buffer = [0; 2048];
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(
                request.starts_with(&expected),
                "unexpected API path: {request}"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-user-token")
            );
            requests.push(request);
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
        requests
    });
    (address, handle)
}

pub fn source(base_url: String) -> OpenLarkSource {
    OpenLarkSource::new(OpenLarkOptions {
        app_id: "test-app".into(),
        app_secret: "test-secret".into(),
        user_access_token: Some("test-user-token".into()),
        api_base_url: base_url,
        allow_custom_api_base_url: true,
        max_download_bytes: 4096,
        request_timeout: Duration::from_secs(5),
    })
    .unwrap()
}
