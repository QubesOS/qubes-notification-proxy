use crate::{badge, ImageParameters, MAX_APP_IMAGE_SIDE, MAX_ICON_SIDE};
use std::path::{Path, PathBuf};

const MARK_LOOKUP_SIZE: u32 = 128;

const PIXMAPS: &str = "/usr/share/pixmaps";

pub fn resolve_image_path(untrusted_path: &str) -> Option<ImageParameters> {
    if let Some(rest) = untrusted_path.strip_prefix("file://") {
        return match file_uri_path(rest) {
            Some(path) => load_image(Path::new(path)),
            None => {
                eprintln!(
                    "Ignoring file URI {untrusted_path:?}: \
                    only plain absolute local paths are supported"
                );
                None
            }
        };
    }
    if untrusted_path.starts_with('/') {
        return load_image(Path::new(untrusted_path));
    }
    match find_theme_icon(untrusted_path, MAX_ICON_SIDE) {
        Some(path) => load_image(&path),
        None => {
            eprintln!("No PNG icon named {untrusted_path:?} in the icon theme");
            None
        }
    }
}

fn file_uri_path(rest: &str) -> Option<&str> {
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') || path.contains('%') {
        return None;
    }
    Some(path)
}

pub fn qube_icon_image(name: &str) -> Option<badge::Image> {
    if let Some(image) = theme_icon_image(name) {
        return Some(image);
    }
    let fallback = appvm_fallback(name)?;
    eprintln!("No usable {name} icon, trying {fallback}");
    theme_icon_image(&fallback)
}

fn theme_icon_image(name: &str) -> Option<badge::Image> {
    load_rgba(&find_theme_icon(name, MARK_LOOKUP_SIZE)?)
}

fn appvm_fallback(name: &str) -> Option<String> {
    let (kind, colour) = name.split_once('-')?;
    if kind == "appvm" {
        return None;
    }
    Some(format!("appvm-{colour}"))
}

fn hicolor_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(home).join(".local/share/icons/hicolor"));
    }
    roots.push(PathBuf::from("/usr/share/icons/hicolor"));
    roots
}

fn find_theme_icon(name: &str, max_side: u32) -> Option<PathBuf> {
    // The name becomes a file name; anything path-like is refused.
    if name.is_empty() || name.starts_with('.') || name.contains('/') || name.contains('\\') {
        return None;
    }
    let file = format!("{name}.png");
    let mut best: Option<((u8, u32), PathBuf)> = None;
    for root in hicolor_roots() {
        let size_dirs = match std::fs::read_dir(root) {
            Ok(size_dirs) => size_dirs,
            Err(_) => continue,
        };
        for size_dir in size_dirs.flatten() {
            let size = match parse_icon_size(&size_dir.file_name()) {
                Some(size) => size,
                None => continue,
            };
            let key = rank(size, max_side);
            if let Some((best_key, _)) = &best {
                if key <= *best_key {
                    continue;
                }
            }
            let path = match icon_in_size_dir(&size_dir.path(), &file) {
                Some(path) => path,
                None => continue,
            };
            best = Some((key, path));
        }
    }
    if let Some((_, path)) = best {
        return Some(path);
    }
    let candidate = Path::new(PIXMAPS).join(&file);
    if candidate.is_file() {
        return Some(candidate);
    }
    None
}

/// How good an icon of `size` is. Larger being better.
fn rank(size: u32, max_side: u32) -> (u8, u32) {
    if size <= max_side {
        (1, size)
    } else {
        (0, u32::MAX - size)
    }
}

fn icon_in_size_dir(size_dir: &Path, file: &str) -> Option<PathBuf> {
    let categories = match std::fs::read_dir(size_dir) {
        Ok(categories) => categories,
        Err(_) => return None,
    };
    for category in categories.flatten() {
        let candidate = category.path().join(file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn parse_icon_size(dir_name: &std::ffi::OsStr) -> Option<u32> {
    let (width, rest) = dir_name.to_str()?.split_once('x')?;
    let width: u32 = width.parse().ok()?;
    let scale: u32 = match rest.split_once('@') {
        Some((_, scale)) => scale.parse().ok()?,
        None => 1,
    };
    width.checked_mul(scale)
}

fn load_image(path: &Path) -> Option<ImageParameters> {
    Some(ImageParameters::from(load_rgba(path)?))
}

fn load_rgba(path: &Path) -> Option<badge::Image> {
    let file = open_regular_file(path)?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoded_or_log(decoder.read_info(), path)?;
    // Refuse absurd dimensions before decoding any pixel data.
    let info = reader.info();
    if info.width == 0
        || info.height == 0
        || info.width > MAX_APP_IMAGE_SIDE
        || info.height > MAX_APP_IMAGE_SIDE
    {
        eprintln!(
            "Image {path:?} is {}x{}, refusing to decode",
            info.width, info.height
        );
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()];
    let frame = decoded_or_log(reader.next_frame(&mut buf), path)?;
    buf.truncate(frame.buffer_size());
    let rgba = to_rgba(buf, frame.color_type)?;
    let image = badge::Image::from_rgba(frame.width, frame.height, rgba);
    Some(badge::shrink_to(image, MAX_ICON_SIDE))
}

fn open_regular_file(path: &Path) -> Option<std::fs::File> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            eprintln!("Ignoring image {path:?}: not a regular file");
            return None;
        }
        Err(e) => {
            eprintln!("Cannot read image {path:?}: {e}");
            return None;
        }
    }
    match std::fs::File::open(path) {
        Ok(file) => Some(file),
        Err(e) => {
            eprintln!("Cannot read image {path:?}: {e}");
            None
        }
    }
}

fn decoded_or_log<T>(result: Result<T, png::DecodingError>, path: &Path) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("Cannot decode image {path:?}: {e}");
            None
        }
    }
}

fn to_rgba(buf: Vec<u8>, colour: png::ColorType) -> Option<Vec<u8>> {
    match colour {
        png::ColorType::Rgba => Some(buf),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            Some(out)
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(buf.len() * 4);
            for g in &buf {
                out.extend_from_slice(&[*g, *g, *g, 0xFF]);
            }
            Some(out)
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(buf.len() * 2);
            for px in buf.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            Some(out)
        }
        other => {
            eprintln!("Unsupported PNG colour type {other:?}");
            None
        }
    }
}
