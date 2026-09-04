/// Size of the square that a badge is drawn on.
const BADGE_SIZE: u32 = 64;

/// How much of the square the mark takes up.
const OVERLAY_PERCENT: u32 = 44;

/// Size of the mark itself.
const MARK_SIZE: u32 = BADGE_SIZE * OVERLAY_PERCENT / 100;

/// How much of the square the image itself takes up.
const IMAGE_PERCENT: u32 = 75;

/// Size of the image.
const IMAGE_SIZE: u32 = BADGE_SIZE * IMAGE_PERCENT / 100;

#[derive(Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// An RGBA8 image. Rows have no padding, so each row is `width * 4` bytes.
#[derive(Clone, Debug)]
pub struct Image {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) data: Vec<u8>,
}

impl Image {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; (width as usize) * (height as usize) * 4],
        }
    }

    pub(crate) fn from_rgba(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    #[inline]
    fn offset(&self, x: u32, y: u32) -> usize {
        ((y as usize) * (self.width as usize) + (x as usize)) * 4
    }

    #[inline]
    pub(crate) fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let o = self.offset(x, y);
        [self.data[o], self.data[o + 1], self.data[o + 2], self.data[o + 3]]
    }

    #[inline]
    pub(crate) fn set_pixel(&mut self, x: u32, y: u32, px: [u8; 4]) {
        let o = self.offset(x, y);
        self.data[o..o + 4].copy_from_slice(&px);
    }

    fn rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: self.width,
            h: self.height,
        }
    }
}

fn scale_into(src: &Image, src_rect: Rect, dst: &mut Image, dst_rect: Rect) {
    for y in 0..dst_rect.h {
        let sy0 = y * src_rect.h / dst_rect.h;
        let sy1 = ((y + 1) * src_rect.h / dst_rect.h).max(sy0 + 1);
        for x in 0..dst_rect.w {
            let sx0 = x * src_rect.w / dst_rect.w;
            let sx1 = ((x + 1) * src_rect.w / dst_rect.w).max(sx0 + 1);
            let (mut sa, mut sr, mut sg, mut sb, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let p = src.pixel(src_rect.x + sx, src_rect.y + sy);
                    let a = p[3] as u32;
                    sa += a;
                    sr += p[0] as u32 * a;
                    sg += p[1] as u32 * a;
                    sb += p[2] as u32 * a;
                    n += 1;
                }
            }
            let px = if sa == 0 {
                [0, 0, 0, 0]
            } else {
                [
                    (sr / sa) as u8,
                    (sg / sa) as u8,
                    (sb / sa) as u8,
                    (sa / n) as u8,
                ]
            };
            dst.set_pixel(dst_rect.x + x, dst_rect.y + y, px);
        }
    }
}

/// Puts `rect` of `src` on a transparent `size` x `size` square. Keeps its
/// shape and centres it.
fn fit_square(src: &Image, rect: Rect, size: u32) -> Image {
    let mut out = Image::new(size, size);

    if rect.w == 0 || rect.h == 0 {
        return out;
    }

    let (w, h) = if rect.w >= rect.h {
        (size, (size as u64 * rect.h as u64 / rect.w as u64).max(1) as u32)
    } else {
        ((size as u64 * rect.w as u64 / rect.h as u64).max(1) as u32, size)
    };

    let dst = Rect {
        x: (size - w) / 2,
        y: (size - h) / 2,
        w,
        h,
    };
    scale_into(src, rect, &mut out, dst);
    out
}

/// Scales `image` down so neither side passes `max_side`.
pub(crate) fn shrink_to(image: Image, max_side: u32) -> Image {
    let side = image.width.max(image.height);
    if side <= max_side {
        return image;
    }
    let w = (image.width as u64 * max_side as u64 / side as u64).max(1) as u32;
    let h = (image.height as u64 * max_side as u64 / side as u64).max(1) as u32;
    let mut out = Image::new(w, h);
    let dst = out.rect();
    scale_into(&image, image.rect(), &mut out, dst);
    out
}

/// The smallest rectangle holding every visible pixel of `img`.
fn ink_bounds(img: &Image) -> Option<Rect> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    let mut any = false;
    for y in 0..img.height {
        for x in 0..img.width {
            if img.pixel(x, y)[3] > 8 {
                any = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if !any {
        return None;
    }
    Some(Rect {
        x: x0,
        y: y0,
        w: x1 - x0 + 1,
        h: y1 - y0 + 1,
    })
}

/// Fits a qube's icon to the size the mark is drawn at.
pub fn fit_mark(icon: &Image) -> Image {
    match ink_bounds(icon) {
        Some(rect) => fit_square(icon, rect, MARK_SIZE),
        None => Image::new(MARK_SIZE, MARK_SIZE),
    }
}

/// Puts `fg` on top of `bg`. Both are RGBA8 with plain (not premultiplied)
/// alpha.
fn blend(bg: [u8; 4], fg: [u8; 4]) -> [u8; 4] {
    let fa = fg[3] as u32;
    if fa == 0xFF {
        return fg;
    }
    if fa == 0 {
        return bg;
    }
    let back = (bg[3] as u32) * (255 - fa) / 255;
    let out_a = fa + back;
    let mut out = [0; 4];
    for i in 0..3 {
        out[i] = ((fg[i] as u32 * fa + bg[i] as u32 * back) / out_a) as u8;
    }
    out[3] = out_a as u8;
    out
}

/// Puts `top` on `bottom`.
fn overlay(mut bottom: Image, top: &Image, x: u32, y: u32) -> Image {
    for ty in 0..top.height {
        for tx in 0..top.width {
            let under = bottom.pixel(x + tx, y + ty);
            bottom.set_pixel(x + tx, y + ty, blend(under, top.pixel(tx, ty)));
        }
    }
    bottom
}

/// Puts a qube's icon and its mark together.
pub fn compose(base: &Image, mark: &Image) -> Image {
    let image = fit_square(base, base.rect(), IMAGE_SIZE);
    let badge = overlay(Image::new(BADGE_SIZE, BADGE_SIZE), &image, 0, 0);
    overlay(badge, mark, BADGE_SIZE - MARK_SIZE, BADGE_SIZE - MARK_SIZE)
}
