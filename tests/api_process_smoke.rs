use std::{
    collections::HashMap,
    env,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

const API_TOKEN: &str = "process-smoke-token-0123456789abcdef012345";
const PROJECT_ID: &str = "018f2e40-4d28-7e31-8b63-611d49c43b0a";

#[test]
fn real_api_process_uploads_renders_and_downloads_over_loopback_http() {
    let Some(ffmpeg) = configured_tool("STUDIO_TEST_FFMPEG", "ffmpeg") else {
        eprintln!("skipping API process smoke test: ffmpeg not found");
        return;
    };
    let Some(ffprobe) = configured_tool("STUDIO_TEST_FFPROBE", "ffprobe") else {
        eprintln!("skipping API process smoke test: ffprobe not found");
        return;
    };

    let root = tempdir().unwrap();
    let source_path = root.path().join("http-source.mp4");
    make_video(&ffmpeg, &source_path);
    let source_bytes = std::fs::read(&source_path).unwrap();
    let database = root.path().join("projects.sqlite");
    let workspace = root.path().join("workspace");
    let address = available_loopback_address();
    let mut server = start_server(&database, &workspace, address, &ffmpeg, &ffprobe);

    let unauthenticated = http_request(address, "GET", "/v1/projects", None, &[]);
    assert_eq!(unauthenticated.status, 401);

    let session = json_request(
        address,
        "POST",
        "/v1/assets/uploads",
        Some(API_TOKEN),
        Some(json!({"expectedBytes": source_bytes.len()})),
    );
    assert_eq!(session.status, 201, "{}", session.body_text());
    let session: Value = session.json();

    let uploaded = http_request(
        address,
        "PUT",
        session["uploadUrl"].as_str().unwrap(),
        Some(API_TOKEN),
        &source_bytes,
    );
    assert_eq!(uploaded.status, 200, "{}", uploaded.body_text());

    let completed = http_request(
        address,
        "POST",
        session["completeUrl"].as_str().unwrap(),
        Some(API_TOKEN),
        &[],
    );
    assert_eq!(completed.status, 201, "{}", completed.body_text());
    let completed: Value = completed.json();
    let asset_id = completed["asset"]["assetId"].as_str().unwrap();
    let asset_sha256 = completed["asset"]["sha256"].as_str().unwrap();
    assert_eq!(asset_sha256, hex::encode(Sha256::digest(&source_bytes)));

    let timeline = json!({
        "schemaVersion": "1.0.0",
        "projectId": PROJECT_ID,
        "revision": 0,
        "canvas": {
            "width": 320,
            "height": 240,
            "fps": {"num": 24, "den": 1},
            "background": "#101820"
        },
        "assets": {
            (asset_id): {
                "kind": "video",
                "uri": format!("asset://{asset_id}"),
                "sha256": asset_sha256
            }
        },
        "tracks": [{
            "id": "video-main",
            "kind": "video",
            "order": 0,
            "clips": [{
                "id": "uploaded-clip",
                "kind": "video",
                "assetId": asset_id,
                "startFrame": 0,
                "durationFrames": 24,
                "sourceIn": {"ticks": 0, "timebase_num": 1, "timebase_den": 24},
                "sourceOut": {"ticks": 24, "timebase_num": 1, "timebase_den": 24}
            }]
        }]
    });
    let created_project = json_request(
        address,
        "POST",
        "/v1/projects",
        Some(API_TOKEN),
        Some(timeline),
    );
    assert_eq!(
        created_project.status,
        201,
        "{}",
        created_project.body_text()
    );
    let created_project: Value = created_project.json();
    let project_id = created_project["projectId"].as_str().unwrap();

    let queued = json_request(
        address,
        "POST",
        &format!("/v1/projects/{project_id}/renders"),
        Some(API_TOKEN),
        Some(json!({
            "revision": 1,
            "presetId": "export",
            "idempotencyKey": "loopback-smoke-render"
        })),
    );
    assert_eq!(queued.status, 202, "{}", queued.body_text());
    let queued: Value = queued.json();
    let job_id = queued["job"]["jobId"].as_str().unwrap();

    let deadline = Instant::now() + Duration::from_secs(150);
    let job = loop {
        assert!(
            Instant::now() < deadline,
            "render job did not finish before the smoke-test deadline"
        );
        let response = http_request(
            address,
            "GET",
            &format!("/v1/jobs/{job_id}"),
            Some(API_TOKEN),
            &[],
        );
        assert_eq!(response.status, 200, "{}", response.body_text());
        let job: Value = response.json();
        match job["status"].as_str().unwrap() {
            "queued" | "running" => thread::sleep(Duration::from_millis(150)),
            "succeeded" => break job,
            status => panic!("render ended in unexpected state {status}: {job}"),
        }
    };
    let output_asset_id = job["outputAssetId"].as_str().unwrap();
    let metadata = http_request(
        address,
        "GET",
        &format!("/v1/assets/{output_asset_id}"),
        Some(API_TOKEN),
        &[],
    );
    assert_eq!(metadata.status, 200, "{}", metadata.body_text());
    let metadata: Value = metadata.json();
    assert_eq!(metadata["mimeType"], "video/mp4");
    let expected_sha256 = job["result"]["outputSha256"].as_str().unwrap();
    assert_eq!(metadata["sha256"], expected_sha256);

    let output = http_request(
        address,
        "GET",
        &format!("/v1/assets/{output_asset_id}/content"),
        Some(API_TOKEN),
        &[],
    );
    assert_eq!(output.status, 200);
    assert!(!output.body.is_empty());
    assert_eq!(hex::encode(Sha256::digest(&output.body)), expected_sha256);

    let shutdown = server.stop();
    #[cfg(unix)]
    {
        assert!(
            shutdown.success(),
            "API server did not shut down cleanly: {shutdown}"
        );
    }
    #[cfg(not(unix))]
    let _ = shutdown;
    assert!(server.0.try_wait().unwrap().is_some());
}

struct ApiProcess(Child);

impl ApiProcess {
    fn stop(&mut self) -> ExitStatus {
        if let Some(status) = self.0.try_wait().unwrap() {
            return status;
        }

        #[cfg(unix)]
        {
            let signal_sent = Command::new("kill")
                .arg("-INT")
                .arg(self.0.id().to_string())
                .status()
                .is_ok_and(|status| status.success());
            if signal_sent {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    if let Some(status) = self.0.try_wait().unwrap() {
                        return status;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }

        if self.0.try_wait().unwrap().is_none() {
            let _ = self.0.kill();
        }
        self.0.wait().unwrap()
    }
}

impl Drop for ApiProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("invalid JSON response ({error}): {}", self.body_text()))
    }
}

fn start_server(
    database: &Path,
    workspace: &Path,
    address: SocketAddr,
    ffmpeg: &Path,
    ffprobe: &Path,
) -> ApiProcess {
    let child = Command::new(env!("CARGO_BIN_EXE_studio"))
        .args(["api", "serve", "--database"])
        .arg(database)
        .arg("--workspace")
        .arg(workspace)
        .arg("--bind")
        .arg(address.to_string())
        .arg("--ffmpeg")
        .arg(ffmpeg)
        .arg("--ffprobe")
        .arg(ffprobe)
        .arg("--timeout-seconds")
        .arg("120")
        .env("STUDIO_API_TOKEN", API_TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("could not start studio API process");
    let mut server = ApiProcess(child);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            return server;
        }
        if let Some(status) = server.0.try_wait().unwrap() {
            panic!("studio API exited before listening: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "studio API did not listen on {address} before the startup deadline"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn available_loopback_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

fn http_request(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &[u8],
) -> HttpResponse {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("could not connect to API at {address}: {error}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    if let Some(token) = token {
        write!(stream, "Authorization: Bearer {token}\r\n").unwrap();
    }
    if method == "PUT" {
        write!(stream, "Content-Type: application/octet-stream\r\n").unwrap();
    } else if !body.is_empty() {
        write!(stream, "Content-Type: application/json\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    stream.write_all(body).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    parse_response(&bytes)
}

fn json_request(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> HttpResponse {
    let body = body.map_or_else(Vec::new, |body| serde_json::to_vec(&body).unwrap());
    http_request(address, method, path, token, &body)
}

fn parse_response(bytes: &[u8]) -> HttpResponse {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response did not contain a header boundary");
    let header_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = header_text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("HTTP response had an invalid status line");
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let response_body = &bytes[header_end + 4..];
    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|encoding| encoding.eq_ignore_ascii_case("chunked"))
    {
        decode_chunked(response_body)
    } else {
        response_body.to_vec()
    };
    if let Some(expected_length) = headers.get("content-length") {
        assert_eq!(body.len(), expected_length.parse::<usize>().unwrap());
    }
    HttpResponse { status, body }
}

fn decode_chunked(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    let mut offset = 0;
    loop {
        let line_end = bytes[offset..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|relative| offset + relative)
            .expect("malformed chunked response size");
        let size_text = std::str::from_utf8(&bytes[offset..line_end]).unwrap();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap(), 16).unwrap();
        offset = line_end + 2;
        if size == 0 {
            return decoded;
        }
        let chunk_end = offset.checked_add(size).expect("chunk size overflowed");
        assert!(
            chunk_end + 2 <= bytes.len(),
            "truncated HTTP response chunk"
        );
        decoded.extend_from_slice(&bytes[offset..chunk_end]);
        assert_eq!(&bytes[chunk_end..chunk_end + 2], b"\r\n");
        offset = chunk_end + 2;
    }
}

fn configured_tool(variable: &str, fallback: &str) -> Option<PathBuf> {
    let path = env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(fallback));
    Command::new(&path)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(|status| status.success())
        .map(|_| path)
}

fn make_video(ffmpeg: &Path, output_path: &Path) {
    let output = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=320x240:r=24:d=1",
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(output_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "could not create smoke-test media: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
