//! Text in the frame on screen, as IINA's Live Text finds it: macOS's
//! Vision framework reads a screenshot, and Flick copies the lines it
//! recognizes. Other systems have no built-in text recognition.

use anyhow::Result;
use std::path::Path;

/// The lines of text Vision recognizes in the image at `path`, top first.
#[cfg(target_os = "macos")]
pub fn recognize(path: &Path) -> Result<Vec<String>> {
    use anyhow::{Context as _, ensure};
    use objc2::msg_send;
    use objc2::rc::{Allocated, Retained, autoreleasepool};
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::{CStr, CString};

    #[link(name = "Vision", kind = "framework")]
    unsafe extern "C" {}

    fn class(name: &CStr) -> Result<&'static AnyClass> {
        AnyClass::get(name).with_context(|| format!("{name:?} is missing"))
    }
    let path = CString::new(path.to_str().context("The image path is not UTF-8")?)?;
    autoreleasepool(|_| {
        // SAFETY: documented Foundation and Vision API; every object is
        // retained by `Retained` or the autorelease pool around this call.
        unsafe {
            let string: Retained<AnyObject> =
                msg_send![class(c"NSString")?, stringWithUTF8String: path.as_ptr()];
            let url: Retained<AnyObject> = msg_send![class(c"NSURL")?, fileURLWithPath: &*string];
            let options: Retained<AnyObject> = msg_send![class(c"NSDictionary")?, dictionary];
            let allocated: Allocated<AnyObject> =
                msg_send![class(c"VNImageRequestHandler")?, alloc];
            let handler: Option<Retained<AnyObject>> =
                msg_send![allocated, initWithURL: &*url, options: &*options];
            let handler = handler.context("Vision could not open the frame")?;
            let request: Retained<AnyObject> = msg_send![class(c"VNRecognizeTextRequest")?, new];
            // VNRequestTextRecognitionLevelAccurate.
            let _: () = msg_send![&*request, setRecognitionLevel: 0isize];
            let requests: Retained<AnyObject> =
                msg_send![class(c"NSArray")?, arrayWithObject: &*request];
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let done: bool = msg_send![&*handler, performRequests: &*requests, error: &mut error];
            ensure!(done, "Vision could not read the frame");
            let results: Option<Retained<AnyObject>> = msg_send![&*request, results];
            let mut lines = Vec::new();
            if let Some(results) = results {
                let count: usize = msg_send![&*results, count];
                for index in 0..count {
                    let observation: Retained<AnyObject> =
                        msg_send![&*results, objectAtIndex: index];
                    let candidates: Retained<AnyObject> =
                        msg_send![&*observation, topCandidates: 1usize];
                    let first: Option<Retained<AnyObject>> = msg_send![&*candidates, firstObject];
                    let Some(first) = first else { continue };
                    let text: Retained<AnyObject> = msg_send![&*first, string];
                    let utf8: *const std::ffi::c_char = msg_send![&*text, UTF8String];
                    if !utf8.is_null() {
                        let line = CStr::from_ptr(utf8).to_string_lossy().trim().to_owned();
                        if !line.is_empty() {
                            lines.push(line);
                        }
                    }
                }
            }
            Ok(lines)
        }
    })
}

#[cfg(not(target_os = "macos"))]
pub fn recognize(_path: &Path) -> Result<Vec<String>> {
    anyhow::bail!("Finding text in the frame needs macOS")
}
