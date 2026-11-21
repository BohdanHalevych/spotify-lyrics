use std::{
    ffi::{c_char, c_double, c_int, c_uchar, c_uint, c_void},
    fmt::Write as _,
    io::{self, Write as _},
    ptr::NonNull,
};

#[link(name = "cairo")]
unsafe extern "C" {
    fn cairo_image_surface_create(format: c_int, width: c_int, height: c_int) -> *mut c_void;
    fn cairo_surface_status(surface: *mut c_void) -> c_int;
    fn cairo_surface_flush(surface: *mut c_void);
    fn cairo_image_surface_get_data(surface: *mut c_void) -> *mut c_uchar;
    fn cairo_image_surface_get_stride(surface: *mut c_void) -> c_int;
    fn cairo_surface_destroy(surface: *mut c_void);
    fn cairo_create(surface: *mut c_void) -> *mut c_void;
    fn cairo_destroy(context: *mut c_void);
    fn cairo_move_to(context: *mut c_void, x: c_double, y: c_double);
    fn cairo_set_source_rgb(context: *mut c_void, r: c_double, g: c_double, b: c_double);
    fn cairo_surface_write_to_png_stream(
        surface: *mut c_void,
        write: unsafe extern "C" fn(*mut c_void, *const c_uchar, c_uint) -> c_int,
        sink: *mut c_void,
    ) -> c_int;
}

#[link(name = "pangocairo-1.0")]
unsafe extern "C" {
    fn pango_cairo_create_layout(context: *mut c_void) -> *mut c_void;
    fn pango_cairo_show_layout(context: *mut c_void, layout: *mut c_void);
    fn pango_cairo_context_set_resolution(context: *mut c_void, dpi: c_double);
}

#[link(name = "pango-1.0")]
unsafe extern "C" {
    fn pango_layout_get_context(layout: *mut c_void) -> *mut c_void;
    fn pango_layout_context_changed(layout: *mut c_void);
    fn pango_layout_set_markup(layout: *mut c_void, text: *const c_char, length: c_int);
    fn pango_layout_get_pixel_size(layout: *mut c_void, width: *mut c_int, height: *mut c_int);
}

#[link(name = "gobject-2.0")]
unsafe extern "C" {
    fn g_object_unref(object: *mut c_void);
}

struct Handle(NonNull<c_void>, unsafe extern "C" fn(*mut c_void));

impl Handle {
    fn new(pointer: *mut c_void, release: unsafe extern "C" fn(*mut c_void)) -> io::Result<Self> {
        NonNull::new(pointer)
            .map(|pointer| Self(pointer, release))
            .ok_or_else(|| io::Error::other("Cannot allocate graphics resources"))
    }

    fn ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Each handle owns one reference, released by its library.
        unsafe { (self.1)(self.ptr()) };
    }
}

pub type Size = (usize, usize);

#[derive(Clone, Copy)]
pub enum Protocol {
    Kitty,
    Sixel,
}

pub fn cell_size(grid: Size, pixels: Size) -> Option<Size> {
    let cell = (pixels.0 / grid.0, pixels.1 / grid.1);
    (cell.0 > 0 && cell.1 > 0).then_some(cell)
}

#[derive(PartialEq)]
struct Frame {
    grid: Size,
    cell: Size,
    rows: Vec<String>,
    shift: usize,
}

impl Frame {
    fn rasterize(&self, first: usize, last: usize, start_row: usize) -> io::Result<(Handle, Size)> {
        let width = self.grid.0 * self.cell.0;
        let height = (last.min(self.grid.1 - 1) + 1 - start_row) * self.cell.1;
        // Bound the allocation even if the PTY reports corrupt pixel dimensions.
        if width * height > 16_777_216 {
            return Err(io::Error::other(
                "Terminal viewport is too large for pixel rendering",
            ));
        }

        // Cairo format 0 is ARGB32. New image surfaces are initialized to transparent.
        unsafe {
            let surface = Handle::new(
                cairo_image_surface_create(0, width as c_int, height as c_int),
                cairo_surface_destroy,
            )?;
            if cairo_surface_status(surface.ptr()) != 0 {
                return Err(io::Error::other("Cannot create graphics surface"));
            }
            let context = Handle::new(cairo_create(surface.ptr()), cairo_destroy)?;
            let layout = Handle::new(pango_cairo_create_layout(context.ptr()), g_object_unref)?;
            pango_cairo_context_set_resolution(pango_layout_get_context(layout.ptr()), 72.0);
            pango_layout_context_changed(layout.ptr());
            cairo_set_source_rgb(context.ptr(), 0.93, 0.93, 0.93);

            // Monospace faces are typically about 0.6 em per cell.
            let font_size = (self.cell.0 as f64 / 0.6).min(self.cell.1 as f64 * 0.85);
            for (index, row) in self
                .rows
                .iter()
                .enumerate()
                .skip(first)
                .take(last - first + 1)
                .filter(|(_, row)| !row.is_empty())
            {
                let markup = markup(row, (font_size * 1024.0).round() as usize);
                pango_layout_set_markup(
                    layout.ptr(),
                    markup.as_ptr().cast(),
                    markup.len() as c_int,
                );
                let mut text_height = 0;
                pango_layout_get_pixel_size(layout.ptr(), std::ptr::null_mut(), &mut text_height);
                let top = ((index - start_row) * self.cell.1) as f64 - self.shift as f64;
                let inset = (self.cell.1 as f64 - f64::from(text_height)) / 2.0;
                cairo_move_to(context.ptr(), 0.0, top + inset.round());
                pango_cairo_show_layout(context.ptr(), layout.ptr());
            }

            Ok((surface, (width, height)))
        }
    }
}

fn png(surface: &Handle) -> io::Result<Vec<u8>> {
    let mut png = Vec::new();
    // The surface and output vector remain live for the synchronous callback.
    if unsafe {
        cairo_surface_write_to_png_stream(
            surface.ptr(),
            append_png,
            (&mut png as *mut Vec<u8>).cast(),
        )
    } != 0
    {
        return Err(io::Error::other("Cannot encode Kitty graphics frame"));
    }
    Ok(png)
}

fn sixel(surface: &Handle, (width, height): Size, start_row: usize) -> io::Result<()> {
    // Cairo's flushed ARGB32 surface is readable for the lifetime of this call.
    let payload = unsafe {
        cairo_surface_flush(surface.ptr());
        let stride = cairo_image_surface_get_stride(surface.ptr()) as usize;
        let pixels = std::slice::from_raw_parts(
            cairo_image_surface_get_data(surface.ptr()),
            stride * height,
        );
        crate::sixel::encode(pixels, width, height, stride)
    };
    let mut out = io::stdout().lock();
    write!(
        out,
        "\x1b[?2026h\x1b[2J\x1b[{};1H{payload}\x1b[?2026l",
        start_row + 1
    )?;
    out.flush()
}

unsafe extern "C" fn append_png(sink: *mut c_void, data: *const c_uchar, length: c_uint) -> c_int {
    // Cairo calls synchronously with a live output vector and a valid byte slice.
    unsafe {
        let output = &mut *sink.cast::<Vec<u8>>();
        if output.try_reserve(length as usize).is_err() {
            return 1;
        }
        output.extend_from_slice(std::slice::from_raw_parts(data, length as usize));
    }
    0
}

fn markup(row: &str, font_size: usize) -> String {
    let mut result = format!("<span font_family=\"monospace\" size=\"{font_size}\">");
    let mut rest = row;
    let mut styled = false;
    while let Some((text, after)) = rest.split_once("\x1b[") {
        escape_markup(&mut result, text);
        if styled {
            result.push_str("</span>");
        }
        let Some((style, tail)) = after.split_once('m') else {
            rest = after;
            styled = false;
            break;
        };
        rest = tail;
        result.push_str("<span");
        let mut codes = style.split(';');
        while let Some(code) = codes.next() {
            match code {
                "1" => result.push_str(" weight=\"bold\""),
                "4" => result.push_str(" underline=\"single\""),
                "38" if codes.next() == Some("2") => {
                    let mut rgb = codes.by_ref().take(3).map(str::parse::<u8>);
                    if let (Some(Ok(r)), Some(Ok(g)), Some(Ok(b))) =
                        (rgb.next(), rgb.next(), rgb.next())
                    {
                        let _ = write!(result, " foreground=\"#{r:02x}{g:02x}{b:02x}\"");
                    }
                }
                _ => {}
            }
        }
        result.push('>');
        styled = true;
    }
    escape_markup(&mut result, rest);
    if styled {
        result.push_str("</span>");
    }
    result.push_str("</span>");
    result
}

fn escape_markup(output: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            ch if !ch.is_control() => output.push(ch),
            _ => {}
        }
    }
}

pub struct Graphics {
    protocol: Protocol,
    image: Option<u32>,
    last: Option<Frame>,
}

impl Graphics {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            image: None,
            last: None,
        }
    }

    pub fn paint(
        &mut self,
        grid: Size,
        cell: Size,
        rows: Vec<String>,
        shift: usize,
    ) -> io::Result<()> {
        let frame = Frame {
            grid,
            cell,
            rows,
            shift,
        };
        if self.last.as_ref() == Some(&frame) {
            return Ok(());
        }
        let visible = if matches!(self.protocol, Protocol::Sixel) {
            // Some emulators move the cursor below a Sixel image and scroll at the bottom.
            &frame.rows[..frame.grid.1.saturating_sub(1)]
        } else {
            &frame.rows
        };
        if let Some(first) = visible.iter().position(|row| !row.is_empty()) {
            let last = visible.iter().rposition(|row| !row.is_empty()).unwrap();
            let start_row = first.saturating_sub(usize::from(shift != 0));
            self.present(&frame, first, last, start_row)?;
        } else {
            let mut out = io::stdout().lock();
            if let Some(id) = self.image.take() {
                write!(out, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\")?;
            } else if matches!(self.protocol, Protocol::Sixel) && self.last.is_some() {
                out.write_all(b"\x1b[?2026h\x1b[2J\x1b[?2026l")?;
            }
            out.flush()?;
        }
        self.last = Some(frame);
        Ok(())
    }

    fn present(
        &mut self,
        frame: &Frame,
        first: usize,
        last: usize,
        start_row: usize,
    ) -> io::Result<()> {
        let (surface, size) = frame.rasterize(first, last, start_row)?;
        if matches!(self.protocol, Protocol::Sixel) {
            return sixel(&surface, size, start_row);
        }
        let png = png(&surface)?;
        let first_id = std::process::id().wrapping_mul(2).max(2);
        let id = if self.image == Some(first_id) {
            first_id + 1
        } else {
            first_id
        };
        let mut command = format!("\x1b[?2026h\x1b[{};1H", start_row + 1);
        let mut chunks = png.chunks(3072).peekable();
        let _ = write!(command, "\x1b_Ga=T,f=100,C=1,q=2,i={id},");
        while let Some(chunk) = chunks.next() {
            let more = chunks.peek().is_some();
            let _ = write!(command, "m={};", u8::from(more));
            base64(&mut command, chunk);
            command.push_str("\x1b\\");
            if more {
                command.push_str("\x1b_G");
            }
        }
        if let Some(old) = self.image.replace(id) {
            let _ = write!(command, "\x1b_Ga=d,d=I,i={old},q=2;\x1b\\");
        }
        command.push_str("\x1b[?2026l");
        let mut out = io::stdout().lock();
        out.write_all(command.as_bytes())?;
        out.flush()
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(b"\x1b[?2026l");
        if let Some(id) = self.image {
            let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\");
        }
        let _ = out.flush();
    }
}

fn base64(output: &mut String, input: &[u8]) {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in input.chunks(3) {
        let a = chunk[0] as usize;
        let b = usize::from(*chunk.get(1).unwrap_or(&0));
        let c = usize::from(*chunk.get(2).unwrap_or(&0));
        output.push(ALPHABET[a >> 2].into());
        output.push(ALPHABET[((a & 3) << 4) | (b >> 4)].into());
        output.push(if chunk.len() >= 2 {
            ALPHABET[((b & 15) << 2) | (c >> 6)].into()
        } else {
            '='
        });
        output.push(if chunk.len() == 3 {
            ALPHABET[c & 63].into()
        } else {
            '='
        });
    }
}
