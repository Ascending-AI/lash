//! Deleting an attachment that never existed is a no-op whichever answer the
//! S3 implementation gives DeleteObjects for the missing key (FIG-3688).
//!
//! A loopback stand-in for an S3 endpoint answers DeleteObjects with either
//! shape, echoing the requested key, and answers HEAD with 404 or 200.

use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[derive(Clone, Copy)]
enum MissingKeyAnswer {
    /// AWS S3 and MinIO: the missing key is reported deleted.
    Deleted,
    /// Garage: the missing key is a per-key `NoSuchKey` error.
    NoSuchKey,
    /// Any other per-key failure.
    AccessDenied,
}

/// Serves DeleteObjects and HEAD until the test ends; returns the endpoint.
async fn fake_s3(answer: MissingKeyAnswer, object_exists: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake S3");
    let endpoint = format!("http://{}", listener.local_addr().expect("fake S3 address"));
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(serve_connection(socket, answer, object_exists));
        }
    });
    endpoint
}

async fn serve_connection(
    socket: tokio::net::TcpStream,
    answer: MissingKeyAnswer,
    object_exists: bool,
) {
    let (read, mut write) = socket.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
            return;
        }
        let mut content_length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        let response = if request_line.starts_with("POST") {
            let body = String::from_utf8_lossy(&body);
            let key = body
                .split_once("<Key>")
                .and_then(|(_, rest)| rest.split_once("</Key>"))
                .map(|(key, _)| key.to_string())
                .unwrap_or_default();
            let entry = match answer {
                MissingKeyAnswer::Deleted => format!("<Deleted><Key>{key}</Key></Deleted>"),
                MissingKeyAnswer::NoSuchKey => format!(
                    "<Error><Key>{key}</Key><Code>NoSuchKey</Code><Message>Key not found</Message></Error>"
                ),
                MissingKeyAnswer::AccessDenied => format!(
                    "<Error><Key>{key}</Key><Code>AccessDenied</Code><Message>Access denied</Message></Error>"
                ),
            };
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">{entry}</DeleteResult>"
            );
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\ncontent-length: {}\r\n\r\n{xml}",
                xml.len()
            )
        } else if object_exists {
            "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nlast-modified: Thu, 24 Sep 2026 12:00:00 GMT\r\netag: \"e\"\r\n\r\n"
                .to_string()
        } else {
            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_string()
        };
        if write.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn store_at(endpoint: String) -> S3AttachmentStore {
    S3AttachmentStore::builder("bucket", "us-east-1")
        .endpoint_url(endpoint)
        .access_key_id("GK0000000000000000000000")
        .secret_access_key("secret")
        .path_style(true)
        .build()
        .expect("build the S3 store against the fake endpoint")
}

fn never_written() -> AttachmentId {
    AttachmentId::parse("sha256:never-existed").expect("valid attachment id")
}

#[tokio::test]
async fn a_missing_key_reported_deleted_is_a_no_op() {
    let store = store_at(fake_s3(MissingKeyAnswer::Deleted, false).await);
    store
        .delete(&never_written())
        .await
        .expect("the AWS S3 / MinIO answer is a no-op");
}

#[tokio::test]
async fn a_missing_key_reported_no_such_key_is_a_no_op() {
    let store = store_at(fake_s3(MissingKeyAnswer::NoSuchKey, false).await);
    store
        .delete(&never_written())
        .await
        .expect("the Garage answer is a no-op");
}

#[tokio::test]
async fn a_failed_delete_of_an_object_still_present_is_the_failure() {
    let store = store_at(fake_s3(MissingKeyAnswer::AccessDenied, true).await);
    let err = store
        .delete(&never_written())
        .await
        .expect_err("a delete that left the object in place failed");
    assert!(
        matches!(
            err,
            AttachmentStoreError::Backend {
                operation: "delete",
                ..
            }
        ),
        "the delete failure is reported as the delete's, got {err:?}"
    );
}
