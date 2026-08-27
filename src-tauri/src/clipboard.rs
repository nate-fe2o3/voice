use anyhow::Result;
use clipboard_rs::common::ClipboardContent;
use clipboard_rs::{Clipboard, ClipboardContext};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const OWNERSHIP_FORMAT: &str = "com.nbutton.voxtype.transaction";
static TRANSACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct ClipboardTransaction {
    previous: Vec<ClipboardContent>,
    marker: Vec<u8>,
}

impl ClipboardTransaction {
    pub fn begin(text: &str) -> Result<Self> {
        let clipboard =
            ClipboardContext::new().map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let formats = clipboard_result(
            clipboard.available_formats(),
            "enumerate clipboard formats",
        )?;
        let mut previous = Vec::with_capacity(formats.len());
        for format in formats {
            if format == OWNERSHIP_FORMAT {
                continue;
            }
            let bytes = clipboard_result(
                clipboard.get_buffer(&format),
                &format!("preserve clipboard format {format}"),
            )?;
            previous.push(ClipboardContent::Other(format, bytes));
        }
        let marker = unique_marker();
        clipboard_result(
            clipboard.set(vec![
                ClipboardContent::Text(text.to_string()),
                ClipboardContent::Other(OWNERSHIP_FORMAT.into(), marker.clone()),
            ]),
            "write transcript to clipboard",
        )?;
        Ok(Self { previous, marker })
    }

    pub fn restore_if_owned(self) -> Result<bool> {
        let clipboard =
            ClipboardContext::new().map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let owned = clipboard
            .get_buffer(OWNERSHIP_FORMAT)
            .is_ok_and(|current| current == self.marker);
        if !owned {
            return Ok(false);
        }
        if self.previous.is_empty() {
            clipboard_result(clipboard.clear(), "clear temporary transcript")?;
        } else {
            clipboard_result(
                clipboard.set(self.previous),
                "restore previous clipboard contents",
            )?;
        }

        Ok(true)
    }
}

fn clipboard_result<T>(result: clipboard_rs::common::Result<T>, action: &str) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("{action}: {error}"))
}

fn unique_marker() -> Vec<u8> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TRANSACTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp:x}-{counter:x}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_unique() {
        assert_ne!(unique_marker(), unique_marker());
    }
}
