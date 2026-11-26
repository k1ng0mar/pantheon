//! Native-only screenshot inputs. Remote pages never supply file paths to this
//! module: the browser capture registry resolves and validates them first.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

const MAX_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 12 * 1024 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

pub(super) async fn load(_provider: &str, paths: &[PathBuf]) -> Result<Vec<Value>, String> {
    if paths.len() > MAX_IMAGES {
        return Err(format!("Attach up to {MAX_IMAGES} screenshots per message"));
    }
    let mut total = 0;
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        if !path.is_absolute() {
            return Err("Screenshot path must be absolute".into());
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| "A screenshot is no longer available. Capture it again.")?;
        let metadata = file
            .metadata()
            .await
            .map_err(|_| "Could not read a screenshot. Capture it again.")?;
        if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES {
            return Err("Each screenshot must be a PNG file smaller than 5 MB".into());
        }
        // Bound the actual read as well as the metadata check, in case the file
        // changes while the turn is starting.
        let mut bytes = Vec::with_capacity(metadata.len().min(MAX_IMAGE_BYTES) as usize);
        file.take(MAX_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "Could not read a screenshot. Capture it again.")?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err("Each screenshot must be smaller than 5 MB".into());
        }
        total += bytes.len();
        if total > MAX_TOTAL_BYTES {
            return Err("Screenshots in one message must total less than 12 MB".into());
        }
        if bytes.len() < 33 || !bytes.starts_with(PNG_SIGNATURE) || &bytes[12..16] != b"IHDR" {
            return Err("A screenshot is not a PNG image. Capture it again.".into());
        }
        // ACP image content: the local-image input shape in the installed agent server.
        // Protocol reference: https://agentclientprotocol.com/protocol/v1/content#image-content
        // ACP v1 image content block: the only shape the Pantheon agent accepts.
        images.push(json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(&bytes)}));
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid one-pixel PNG; no UI or model inference required.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    struct TempImage(PathBuf);
    impl TempImage {
        fn new(bytes: &[u8]) -> Self {
            let path =
                std::env::temp_dir().join(format!("pantheon-image-{}.png", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }
    impl Drop for TempImage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn screenshots_are_real_image_blocks_for_the_agent() {
        let png = STANDARD.decode(PNG).unwrap();
        let file = TempImage::new(&png);
        let blocks = load("pantheon", std::slice::from_ref(&file.0))
            .await
            .unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["mimeType"], "image/png");
        assert_eq!(
            STANDARD
                .decode(blocks[0]["data"].as_str().unwrap())
                .unwrap(),
            png
        );
    }

    #[tokio::test]
    async fn missing_invalid_and_oversized_images_fail_instead_of_becoming_text() {
        assert!(load("pantheon", &[PathBuf::from("relative.png")])
            .await
            .unwrap_err()
            .contains("absolute"));
        let missing = std::env::temp_dir().join(format!("missing-{}.png", uuid::Uuid::new_v4()));
        assert!(load("pantheon", &[missing])
            .await
            .unwrap_err()
            .contains("no longer available"));
        let invalid = TempImage::new(b"This is text, not a PNG image");
        assert!(load("pantheon", std::slice::from_ref(&invalid.0))
            .await
            .unwrap_err()
            .contains("not a PNG"));
        let large = TempImage::new(&[]);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&large.0)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        assert!(load("pantheon", std::slice::from_ref(&large.0))
            .await
            .unwrap_err()
            .contains("5 MB"));
    }

    #[tokio::test]
    async fn image_count_and_total_bytes_are_bounded() {
        assert!(load("pantheon", &vec![PathBuf::new(); MAX_IMAGES + 1])
            .await
            .unwrap_err()
            .contains("up to 4"));
        let mut bytes = STANDARD.decode(PNG).unwrap();
        bytes.resize(4 * 1024 * 1024 + 1, 0);
        let file = TempImage::new(&bytes);
        assert!(load("pantheon", &vec![file.0.clone(); 3])
            .await
            .unwrap_err()
            .contains("12 MB"));
        assert!(load("pantheon", &[]).await.unwrap().is_empty());
    }
}
