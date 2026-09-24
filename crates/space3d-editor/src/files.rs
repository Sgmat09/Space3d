//! Non-blocking file I/O for the editor tools.
//!
//! Native: reads, writes and the CPU work around them (decoding, encoding, parsing) run on the
//! `space3d_tasks` compute pool, never on the frame. Web: writing triggers a browser download
//! of the bytes; reading local paths is unsupported.

use space3d_tasks::Task;

/// Read a file in the background.
pub fn read_file(path: String) -> Task<Result<Vec<u8>, String>> {
    read_file_with(path, |_, bytes| Ok(bytes))
}

/// Read a file and turn its bytes into a `T` (decode, parse...) in the background.
/// `parse` gets the path and the bytes.
pub fn read_file_with<T, F>(path: String, parse: F) -> Task<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&str, Vec<u8>) -> Result<T, String> + Send + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        space3d_tasks::spawn_compute(move || {
            let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            parse(&path, bytes)
        })
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = parse;
        space3d_tasks::spawn_compute(move || Err(format!("{path}: reading local files is not supported on the web")))
    }
}

/// Write a file in the background (web: download it). Resolves to a status message.
pub fn write_file(path: String, bytes: Vec<u8>) -> Task<Result<String, String>> {
    write_file_with(path, move || Ok(bytes))
}

/// Produce the bytes of a file (encode, serialize...) and write it, both in the background
/// (web: the bytes are produced inline and downloaded). Resolves to a status message.
pub fn write_file_with<F>(path: String, produce: F) -> Task<Result<String, String>>
where
    F: FnOnce() -> Result<Vec<u8>, String> + Send + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        space3d_tasks::spawn_compute(move || {
            let bytes = produce()?;
            let p = std::path::Path::new(&path);
            if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            std::fs::write(p, &bytes).map_err(|e| format!("{path}: {e}"))?;
            Ok(format!("saved {path} ({} bytes)", bytes.len()))
        })
    }
    #[cfg(target_arch = "wasm32")]
    {
        let result = produce().and_then(|bytes| download(&path, &bytes));
        space3d_tasks::spawn_compute(move || result)
    }
}

/// Offer `bytes` as a browser download named after the last path component.
///
/// The anchor is attached to the document for the click (some browsers ignore clicks on
/// detached anchors) and the object URL is revoked only after a delay: revoking it right after
/// `click()` can cancel or empty the download, since the download starts asynchronously.
#[cfg(target_arch = "wasm32")]
fn download(path: &str, bytes: &[u8]) -> Result<String, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::closure::Closure;
    const REVOKE_AFTER_MS: i32 = 30_000;
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned();
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::of1(&array);
    let blob = web_sys::Blob::new_with_u8_array_sequence(&parts).map_err(|e| format!("{e:?}"))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|e| format!("{e:?}"))?;
    let a: web_sys::HtmlAnchorElement = document
        .create_element("a")
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|_| "not an anchor")?;
    a.set_href(&url);
    a.set_download(&name);
    let _ = a.style().set_property("display", "none");
    let body = document.body();
    if let Some(body) = &body {
        let _ = body.append_child(&a);
    }
    a.click();
    if body.is_some() {
        a.remove();
    }
    let revoke_url = url.clone();
    let revoke = Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&revoke_url);
    });
    if window
        .set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), REVOKE_AFTER_MS)
        .is_err()
    {
        // No timers (unusual): leaking one object URL beats breaking the download.
        tracing::warn!("could not schedule revoking the download URL of {name}");
    }
    Ok(format!("downloaded {name} ({} bytes)", bytes.len()))
}

/// Pending background file jobs with their completion handlers.
pub(crate) struct IoJobs<T> {
    jobs: Vec<(String, Task<Result<T, String>>)>,
}

impl<T> Default for IoJobs<T> {
    fn default() -> Self {
        Self { jobs: Vec::new() }
    }
}

impl<T: 'static> IoJobs<T> {
    pub(crate) fn push(&mut self, label: impl Into<String>, task: Task<Result<T, String>>) {
        self.jobs.push((label.into(), task));
    }

    /// Finished jobs as `(label, result)`.
    pub(crate) fn poll(&mut self) -> Vec<(String, Result<T, String>)> {
        let mut done = Vec::new();
        self.jobs.retain_mut(|(label, task)| match task.poll() {
            Some(r) => {
                done.push((std::mem::take(label), r));
                false
            }
            None => match task.state() {
                space3d_tasks::TaskState::Cancelled => {
                    done.push((std::mem::take(label), Err("task cancelled".into())));
                    false
                }
                _ => true,
            },
        });
        done
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

/// File name for `id` (story ids, entity names...): path separators and odd characters
/// become `_`.
pub fn sanitize_file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_in_background() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/x.bin").to_string_lossy().into_owned();
        let msg = write_file(path.clone(), vec![1, 2, 3]).block().unwrap().unwrap();
        assert!(msg.contains("3 bytes"));
        assert_eq!(read_file(path.clone()).block().unwrap().unwrap(), vec![1, 2, 3]);
        let sum = read_file_with(path.clone(), |_, b| Ok(b.iter().map(|&x| x as u32).sum::<u32>()));
        assert_eq!(sum.block().unwrap().unwrap(), 6);
        let failed = write_file_with(path.clone(), || Err("encode failed".into()));
        assert_eq!(failed.block().unwrap(), Err("encode failed".into()));
        assert_eq!(read_file(path).block().unwrap().unwrap(), vec![1, 2, 3], "untouched");
        assert!(read_file("/definitely/missing".into()).block().unwrap().is_err());
        assert_eq!(sanitize_file_name("UI/Button: big"), "UI_Button__big");
    }
}
