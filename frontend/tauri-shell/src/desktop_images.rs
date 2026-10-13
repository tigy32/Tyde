use std::path::PathBuf;

use base64::{Engine, engine::general_purpose::STANDARD};
#[cfg(not(target_os = "linux"))]
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use protocol::{ImageData, InputImageData};

pub fn read_path(path: PathBuf) -> Result<InputImageData, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "The dropped image has no UTF-8 file name".to_owned())?
        .to_owned();
    let bytes = std::fs::read(path).map_err(|error| format!("Could not read {name}: {error}"))?;
    let format = image::guess_format(&bytes)
        .map_err(|error| format!("{name} is not a supported image: {error}"))?;
    Ok(InputImageData {
        name,
        image: ImageData {
            media_type: format.to_mime_type().to_owned(),
            data: STANDARD.encode(bytes),
        },
    })
}

#[cfg(not(target_os = "linux"))]
pub fn read_clipboard() -> Result<Option<ImageData>, String> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| format!("Could not open the image clipboard: {error}"))?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(error) => return Err(format!("Could not read the clipboard image: {error}")),
    };
    let width = u32::try_from(image.width)
        .map_err(|error| format!("Clipboard image width is invalid: {error}"))?;
    let height = u32::try_from(image.height)
        .map_err(|error| format!("Clipboard image height is invalid: {error}"))?;
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(&image.bytes, width, height, ExtendedColorType::Rgba8)
        .map_err(|error| format!("Could not encode the clipboard image: {error}"))?;
    Ok(Some(ImageData {
        media_type: "image/png".to_owned(),
        data: STANDARD.encode(bytes),
    }))
}

#[cfg(target_os = "linux")]
pub async fn read_clipboard(app: tauri::AppHandle) -> Result<Option<ImageData>, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        // Use the webview's GDK display, not a separate Wayland/X11 backend.
        let Some(display) = gtk::gdk::Display::default() else {
            let _ = sender.send(Err(
                "The desktop clipboard display is unavailable".to_owned()
            ));
            return;
        };
        let clipboard = gtk::Clipboard::for_display(&display, &gtk::gdk::SELECTION_CLIPBOARD);
        clipboard.request_contents(
            &gtk::gdk::Atom::intern("TARGETS"),
            move |clipboard, data| {
                if !data.targets_include_image(false) {
                    let _ = sender.send(Ok(None));
                    return;
                }
                clipboard.request_image(move |_, image| {
                    let result = image
                        .ok_or_else(|| {
                            "The clipboard offered an image but did not provide its pixels"
                                .to_owned()
                        })
                        .and_then(|image| {
                            image.save_to_bufferv("png", &[]).map_err(|error| {
                                format!("Could not encode the clipboard image: {error}")
                            })
                        })
                        .map(|bytes| {
                            Some(ImageData {
                                media_type: "image/png".to_owned(),
                                data: STANDARD.encode(bytes),
                            })
                        });
                    let _ = sender.send(result);
                });
            },
        );
    })
    .map_err(|error| format!("Could not request the desktop clipboard image: {error}"))?;
    receiver
        .await
        .map_err(|error| format!("Clipboard image reader ended without a result: {error}"))?
}
