//! F7 pictures in the window. The worker downloads into the disk cache
//! (`Cmd::Media`); a thread decodes the file once, shrunk to `MAX_SIDE`, and
//! makes the texture. A frame only asks [`Media::show`], which never blocks:
//! it answers Loading, Failed or the texture, and queues a request once.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use eframe::egui::{
    self, Color32, ColorImage, Context, Rect, RichText, TextureHandle, TextureOptions, Ui,
};
use notion_core::media::{self, Owner};
use notion_core::store::Store;

/// Downloads out at once (the worker answers one at a time anyway).
pub(crate) const IN_FLIGHT: usize = 3;
/// Inline images are shrunk to this many pixels on the long side.
const MAX_SIDE: u32 = 2048;
/// Covers and gallery thumbnails (page pictures) are shrunk to this.
const THUMB_SIDE: u32 = 512;
/// Textures kept; the least recently shown is dropped past this.
const MAX_TEX: usize = 32;
/// Decodes at once.
const MAX_DECODE: usize = 2;
/// Failed pictures are retried this many times in all.
const MAX_TRIES: u8 = 2;

enum Slot {
    /// Wanted (page cover?), not yet asked.
    Want(bool),
    /// Asked the worker (page cover?).
    Sent(bool),
    Decoding,
    /// The texture and when it was last shown.
    Ready(TextureHandle, u64),
    Failed(String),
}

#[derive(Default)]
struct Inner {
    /// owner id → (source key the slot is for, state).
    slots: HashMap<String, (String, Slot)>,
    /// Ids that became `Want`, oldest first (entries may be stale).
    wants: VecDeque<String>,
    /// Worker answers not yet taken by a frame.
    done: Vec<(String, Result<PathBuf, String>)>,
    /// Decodes waiting for a free thread: (id, file, longest side).
    queue: VecDeque<(String, PathBuf, u32)>,
    running: usize,
    #[cfg(test)]
    peak: usize,
    /// Failures per id since its last success.
    tries: HashMap<String, u8>,
    /// Ids whose cached file didn't decode: the shell drops their cache entry.
    bad: Vec<String>,
    /// The egui pass of the latest `show`: the LRU clock.
    pass: u64,
    /// (id, page?) → source key; cleared on `refresh`.
    srcs: HashMap<(String, bool), Option<String>>,
}

impl Inner {
    fn fail(&mut self, id: &str, e: String) {
        *self.tries.entry(id.to_string()).or_default() += 1;
        if let Some((_, slot)) = self.slots.get_mut(id) {
            *slot = Slot::Failed(e);
        }
    }

    /// Drops the least recently shown textures past [`MAX_TEX`], but never one
    /// shown this pass or the last: more than that on screen goes over the cap
    /// instead of evicting and re-decoding visible pictures every frame.
    fn evict(&mut self) {
        // ponytail: scan per new texture; fine for a few hundred slots
        let now = self.pass;
        while self
            .slots
            .values()
            .filter(|(_, s)| matches!(s, Slot::Ready(..)))
            .count()
            > MAX_TEX
        {
            let old = self
                .slots
                .iter()
                .filter_map(|(id, (_, s))| match s {
                    Slot::Ready(_, t) if t + 1 < now => Some((*t, id.clone())),
                    _ => None,
                })
                .min();
            match old {
                Some((_, id)) => self.slots.remove(&id),
                None => return,
            };
        }
    }
}

/// What to draw.
pub(crate) enum Shown {
    Loading,
    Failed(String),
    Tex(TextureHandle),
}

/// Shared handle: cheap to clone, usable from draw code and decode threads.
#[derive(Clone, Default)]
pub(crate) struct Media(Arc<Mutex<Inner>>);

fn decode(path: &Path, max: u32) -> Result<ColorImage, String> {
    #[cfg(test)]
    if path.ends_with("panic.png") {
        panic!("test: decoder panicked");
    }
    let mut reader = image::ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?;
    let mut lim = image::Limits::default();
    lim.max_image_width = Some(16384);
    lim.max_image_height = Some(16384);
    lim.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(lim);
    let img = reader
        .decode()
        .map_err(|e| format!("can't show this picture: {e}"))?;
    let img = if img.width().max(img.height()) > max {
        img.thumbnail(max, max)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

impl Media {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts queued decodes while fewer than [`MAX_DECODE`] threads run.
    fn spawn_next(&self, g: &mut Inner, ctx: &Context) {
        while g.running < MAX_DECODE {
            let Some((id, path, max)) = g.queue.pop_front() else {
                break;
            };
            g.running += 1;
            #[cfg(test)]
            {
                g.peak = g.peak.max(g.running);
            }
            let (me, ctx) = (self.clone(), ctx.clone());
            std::thread::spawn(move || {
                // a panic in the image crate must still free this decode thread's place
                let r =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(&path, max)))
                        .unwrap_or_else(|_| Err("can't show this picture".into()))
                        .map(|img| {
                            ctx.load_texture(format!("media-{id}"), img, TextureOptions::LINEAR)
                        });
                let mut g = me.lock();
                g.running -= 1;
                match r {
                    Ok(t) => {
                        g.tries.remove(&id);
                        let now = g.pass;
                        if let Some((_, slot)) = g.slots.get_mut(&id) {
                            *slot = Slot::Ready(t, now);
                        }
                        g.evict();
                    }
                    Err(e) => {
                        g.bad.push(id.clone());
                        g.fail(&id, e);
                    }
                }
                me.spawn_next(&mut g, &ctx);
                ctx.request_repaint();
            });
        }
    }

    /// The source key of block/page `id`'s picture (None: it has none). Cached
    /// until [`forget_srcs`](Self::forget_srcs).
    pub fn src(&self, store: &Store, id: &str, page: bool) -> Option<String> {
        let k = (id.to_string(), page);
        if let Some(s) = self.lock().srcs.get(&k) {
            return s.clone();
        }
        let s = media::current(store, id, if page { Owner::Page } else { Owner::Block })
            .ok()
            .map(|s| s.key);
        self.lock().srcs.insert(k, s.clone());
        s
    }

    pub fn forget_srcs(&self) {
        self.lock().srcs.clear();
    }

    /// A pull or push may have fresh URLs: failed pictures get another try
    /// (each at most [`MAX_TRIES`] failures in all).
    pub fn retry_failed(&self) {
        let mut g = self.lock();
        let tries = std::mem::take(&mut g.tries);
        g.slots.retain(|id, (_, s)| {
            !matches!(s, Slot::Failed(_)) || tries.get(id).copied().unwrap_or(0) >= MAX_TRIES
        });
        g.tries = tries;
    }

    /// The worker is gone: what was asked will never answer.
    pub fn worker_stopped(&self) {
        let mut g = self.lock();
        let ids: Vec<String> = g
            .slots
            .iter()
            .filter(|(_, (_, s))| matches!(s, Slot::Sent(_)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            g.fail(&id, "sync worker stopped".into());
        }
    }

    /// Ids whose cached file didn't decode since the last call.
    pub fn take_bad(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().bad)
    }

    /// The worker's answer for `id`.
    pub fn downloaded(&self, id: String, path: Result<PathBuf, String>) {
        self.lock().done.push((id, path));
    }

    /// Up to `n` pictures to ask the worker for, marked as asked.
    pub fn take_wants(&self, n: usize) -> Vec<(String, bool)> {
        let mut g = self.lock();
        let mut out = Vec::new();
        while out.len() < n {
            let Some(id) = g.wants.pop_front() else { break };
            if let Some((_, s)) = g.slots.get_mut(&id) {
                if let Slot::Want(page) = *s {
                    *s = Slot::Sent(page);
                    out.push((id, page));
                }
            }
        }
        out
    }

    /// The picture of `id` whose source is `src`: a texture once decoded.
    /// Wants a download the first time (or when `src` changed).
    pub fn show(&self, ctx: &Context, id: &str, page: bool, src: &str) -> Shown {
        let mut g = self.lock();
        let tick = ctx.cumulative_pass_nr();
        g.pass = tick;
        for (done_id, res) in std::mem::take(&mut g.done) {
            let Some((_, slot)) = g.slots.get_mut(&done_id) else {
                continue;
            };
            match res {
                Err(e) => g.fail(&done_id, e),
                Ok(path) => {
                    let max = if matches!(slot, Slot::Sent(true)) {
                        THUMB_SIDE
                    } else {
                        MAX_SIDE
                    };
                    *slot = Slot::Decoding;
                    g.queue.push_back((done_id, path, max));
                }
            }
        }
        self.spawn_next(&mut g, ctx);
        if let Some((_, slot)) = g.slots.get_mut(id).filter(|(k, _)| k == src) {
            return match slot {
                Slot::Ready(t, last) => {
                    *last = tick;
                    Shown::Tex(t.clone())
                }
                Slot::Failed(e) => Shown::Failed(e.clone()),
                _ => Shown::Loading,
            };
        }
        g.slots
            .insert(id.to_string(), (src.to_string(), Slot::Want(page)));
        g.wants.push_back(id.to_string());
        // `media_pump` runs next frame: make sure there is one
        ctx.request_repaint();
        Shown::Loading
    }
}

/// An inline image block: scaled down to the width (never up), a note while
/// loading, the error if it failed.
pub(crate) fn block_image(ui: &mut Ui, shown: &Shown, grey: Color32) {
    match shown {
        Shown::Loading => {
            ui.label(RichText::new("loading image…").color(grey));
        }
        Shown::Failed(e) => {
            ui.label(RichText::new(format!("image: {e}")).color(ui.visuals().error_fg_color));
        }
        Shown::Tex(t) => {
            let s = t.size_vec2();
            let k = (ui.available_width() / s.x).min(600.0 / s.y).min(1.0);
            ui.add_space(2.0);
            ui.add(egui::Image::new((t.id(), s * k)).corner_radius(6.0));
        }
    }
}

/// `tex` filling `rect`, cropped to its aspect from the centre.
pub(crate) fn paint_cover(ui: &Ui, tex: &TextureHandle, rect: Rect, rounding: f32) {
    let (ra, ia) = (rect.aspect_ratio(), tex.aspect_ratio());
    let (fx, fy) = if ia > ra {
        (ra / ia, 1.0)
    } else {
        (1.0, ia / ra)
    };
    let uv = Rect::from_min_max(
        egui::pos2((1.0 - fx) / 2.0, (1.0 - fy) / 2.0),
        egui::pos2((1.0 + fx) / 2.0, (1.0 + fy) / 2.0),
    );
    egui::Image::new((tex.id(), rect.size()))
        .uv(uv)
        .corner_radius(rounding)
        .paint_at(ui, rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_request_per_picture_then_one_decode() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.png");
        image::RgbaImage::from_pixel(4, 2, image::Rgba([255, 0, 0, 255]))
            .save(&f)
            .unwrap();
        let (m, ctx) = (Media::default(), Context::default());
        assert!(matches!(m.show(&ctx, "b", false, "k"), Shown::Loading));
        assert!(matches!(m.show(&ctx, "b", false, "k"), Shown::Loading));
        assert_eq!(m.take_wants(5), [("b".to_string(), false)], "asked once");
        assert!(m.take_wants(5).is_empty());
        m.downloaded("b".into(), Ok(f));
        let t = std::time::Instant::now();
        let tex = loop {
            match m.show(&ctx, "b", false, "k") {
                Shown::Tex(t) => break t,
                _ if t.elapsed().as_secs() < 5 => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                _ => panic!("never decoded"),
            }
        };
        assert_eq!(tex.size(), [4, 2]);
        // another source = another picture: asked again
        assert!(matches!(m.show(&ctx, "b", false, "k2"), Shown::Loading));
        assert_eq!(m.take_wants(5).len(), 1);
    }

    #[test]
    fn a_failed_download_shows_why_until_retried() {
        let (m, ctx) = (Media::default(), Context::default());
        m.show(&ctx, "b", true, "k");
        m.take_wants(1);
        m.downloaded("b".into(), Err("offline".into()));
        assert!(matches!(m.show(&ctx, "b", true, "k"), Shown::Failed(e) if e == "offline"));
        m.retry_failed();
        assert!(matches!(m.show(&ctx, "b", true, "k"), Shown::Loading));
        assert_eq!(m.take_wants(5), [("b".to_string(), true)]);
    }

    fn wait_tex(m: &Media, ctx: &Context, id: &str, key: &str) -> TextureHandle {
        let t = std::time::Instant::now();
        loop {
            match m.show(ctx, id, false, key) {
                Shown::Tex(t) => return t,
                Shown::Failed(e) => panic!("{e}"),
                _ if t.elapsed().as_secs() < 5 => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                _ => panic!("never decoded"),
            }
        }
    }

    #[test]
    fn decodes_png_jpeg_and_webp_whatever_the_file_name() {
        use image::ImageEncoder;
        let d = tempfile::tempdir().unwrap();
        let px = image::RgbImage::from_pixel(6, 3, image::Rgb([10, 200, 30]));
        let jpg = d.path().join("j.bin"); // no telling extension: sniffed
        image::codecs::jpeg::JpegEncoder::new(std::fs::File::create(&jpg).unwrap())
            .encode_image(&px)
            .unwrap();
        let webp = d.path().join("w");
        image::codecs::webp::WebPEncoder::new_lossless(std::fs::File::create(&webp).unwrap())
            .write_image(px.as_raw(), 6, 3, image::ExtendedColorType::Rgb8)
            .unwrap();
        for f in [jpg, webp] {
            assert_eq!(
                decode(&f, MAX_SIDE).unwrap().size,
                [6, 3],
                "{}",
                f.display()
            );
        }
    }

    #[test]
    fn big_pictures_shrink_to_2048_and_garbage_fails_politely() {
        let d = tempfile::tempdir().unwrap();
        let big = d.path().join("big.png");
        image::RgbaImage::from_pixel(4096, 8, image::Rgba([1, 2, 3, 255]))
            .save(&big)
            .unwrap();
        let s = decode(&big, MAX_SIDE).unwrap().size;
        assert_eq!(s[0], 2048);
        assert!(s[1] >= 1);
        let bad = d.path().join("bad.png");
        std::fs::write(&bad, b"not a picture").unwrap();
        assert!(decode(&bad, MAX_SIDE).unwrap_err().contains("can't show"));
        assert!(decode(&d.path().join("missing.png"), MAX_SIDE).is_err());
    }

    #[test]
    fn a_decoded_picture_is_reused_for_the_same_key_and_downloads_are_bounded() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]))
            .save(&f)
            .unwrap();
        let (m, ctx) = (Media::default(), Context::default());
        for i in 0..5 {
            m.show(&ctx, &format!("b{i}"), false, "k");
        }
        assert_eq!(m.take_wants(IN_FLIGHT).len(), IN_FLIGHT);
        assert_eq!(m.take_wants(IN_FLIGHT).len(), 2, "the rest, once");
        m.downloaded("b0".into(), Ok(f));
        let t1 = wait_tex(&m, &ctx, "b0", "k");
        let t2 = wait_tex(&m, &ctx, "b0", "k");
        assert_eq!(t1.id(), t2.id(), "one texture per picture");
        assert!(m.take_wants(5).is_empty(), "same key: no new request");
    }

    #[test]
    fn a_undecodable_download_ends_failed_not_loading_forever() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.png");
        std::fs::write(&f, b"junk").unwrap();
        let (m, ctx) = (Media::default(), Context::default());
        m.show(&ctx, "b", false, "k");
        m.take_wants(1);
        m.downloaded("b".into(), Ok(f));
        let t = std::time::Instant::now();
        loop {
            if let Shown::Failed(e) = m.show(&ctx, "b", false, "k") {
                assert!(e.contains("can't show"));
                break;
            }
            assert!(t.elapsed().as_secs() < 5, "never failed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_want_requests_a_repaint() {
        let (m, ctx) = (Media::default(), Context::default());
        let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = hit.clone();
        ctx.set_request_repaint_callback(move |_| {
            h.store(true, std::sync::atomic::Ordering::Relaxed)
        });
        m.show(&ctx, "b", false, "k");
        assert!(hit.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn covers_decode_small_and_oversized_headers_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("c.png");
        image::RgbaImage::from_pixel(2000, 10, image::Rgba([1, 2, 3, 255]))
            .save(&f)
            .unwrap();
        assert_eq!(decode(&f, THUMB_SIDE).unwrap().size[0], 512);
        let wide = d.path().join("w.png");
        image::RgbaImage::from_pixel(20000, 1, image::Rgba([1, 2, 3, 255]))
            .save(&wide)
            .unwrap();
        assert!(decode(&wide, MAX_SIDE).unwrap_err().contains("can't show"));
    }

    #[test]
    fn textures_are_capped_least_recently_shown_first_and_decodes_bounded() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]))
            .save(&f)
            .unwrap();
        let (m, ctx) = (Media::default(), Context::default());
        for i in 0..=MAX_TEX {
            next_pass(&ctx);
            next_pass(&ctx);
            let id = format!("b{i}");
            m.show(&ctx, &id, false, "k");
            m.take_wants(1);
            m.downloaded(id.clone(), Ok(f.clone()));
            let _ = wait_tex(&m, &ctx, &id, "k");
        }
        let g = m.lock();
        let ready = g
            .slots
            .values()
            .filter(|(_, s)| matches!(s, Slot::Ready(..)))
            .count();
        assert_eq!(ready, MAX_TEX);
        assert!(!g.slots.contains_key("b0") && g.slots.contains_key(&format!("b{MAX_TEX}")));
        assert!(g.peak <= MAX_DECODE);
        drop(g);
        // five at once: never more than MAX_DECODE running
        for i in 0..5 {
            let id = format!("c{i}");
            m.show(&ctx, &id, false, "k");
            m.take_wants(1);
            m.downloaded(id, Ok(f.clone()));
        }
        m.show(&ctx, "c0", false, "k");
        for i in 0..5 {
            let _ = wait_tex(&m, &ctx, &format!("c{i}"), "k");
        }
        assert!(m.lock().peak <= MAX_DECODE);
    }

    #[test]
    fn more_pictures_on_screen_than_the_cap_are_all_kept_and_settle() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]))
            .save(&f)
            .unwrap();
        let (m, ctx) = (Media::default(), Context::default());
        let ids: Vec<String> = (0..MAX_TEX + 5).map(|i| format!("g{i}")).collect();
        for id in &ids {
            m.show(&ctx, id, true, "k");
        }
        for (id, _) in m.take_wants(usize::MAX) {
            m.downloaded(id, Ok(f.clone()));
        }
        for id in &ids {
            let _ = wait_tex_page(&m, &ctx, id);
        }
        // every frame shows all of them: nothing is evicted, nothing is wanted again
        for _ in 0..3 {
            next_pass(&ctx);
            for id in &ids {
                assert!(
                    matches!(m.show(&ctx, id, true, "k"), Shown::Tex(_)),
                    "{id} was evicted while on screen"
                );
            }
            assert!(m.take_wants(usize::MAX).is_empty());
        }
    }

    /// Ends one egui pass (the LRU clock), discarding its texture changes.
    fn next_pass(ctx: &Context) {
        ctx.run_ui(Default::default(), |_| {})
            .textures_delta
            .clear();
    }

    fn wait_tex_page(m: &Media, ctx: &Context, id: &str) -> TextureHandle {
        let t = std::time::Instant::now();
        loop {
            match m.show(ctx, id, true, "k") {
                Shown::Tex(t) => return t,
                Shown::Failed(e) => panic!("{e}"),
                _ if t.elapsed().as_secs() < 5 => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                _ => panic!("never decoded"),
            }
        }
    }

    #[test]
    fn a_panicking_decode_fails_its_picture_and_frees_its_thread() {
        let d = tempfile::tempdir().unwrap();
        let bad = d.path().join("panic.png");
        std::fs::write(&bad, b"x").unwrap();
        let good = d.path().join("ok.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]))
            .save(&good)
            .unwrap();
        let (m, ctx) = (Media::default(), Context::default());
        for i in 0..MAX_DECODE + 1 {
            let id = format!("p{i}");
            m.show(&ctx, &id, false, "k");
            m.take_wants(1);
            m.downloaded(id, Ok(bad.clone()));
        }
        m.show(&ctx, "ok", false, "k");
        m.take_wants(1);
        m.downloaded("ok".into(), Ok(good));
        let _ = wait_tex(&m, &ctx, "ok", "k");
        assert!(
            matches!(m.show(&ctx, "p0", false, "k"), Shown::Failed(e) if e.contains("can't show"))
        );
        assert_eq!(m.lock().running, 0);
    }

    #[test]
    fn failures_retry_twice_then_stay_and_bad_decodes_are_reported() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.png");
        std::fs::write(&f, b"junk").unwrap();
        let (m, ctx) = (Media::default(), Context::default());
        m.show(&ctx, "b", false, "k");
        m.take_wants(1);
        m.downloaded("b".into(), Ok(f));
        while !matches!(m.show(&ctx, "b", false, "k"), Shown::Failed(_)) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(m.take_bad(), ["b"]);
        m.retry_failed();
        m.show(&ctx, "b", false, "k");
        m.take_wants(1);
        m.downloaded("b".into(), Err("offline".into()));
        assert!(matches!(m.show(&ctx, "b", false, "k"), Shown::Failed(_)));
        m.retry_failed();
        assert!(
            matches!(m.show(&ctx, "b", false, "k"), Shown::Failed(_)),
            "two failures: no third try"
        );
    }

    #[test]
    fn a_stopped_worker_fails_what_was_asked() {
        let (m, ctx) = (Media::default(), Context::default());
        m.show(&ctx, "b", false, "k");
        m.take_wants(1);
        m.worker_stopped();
        assert!(
            matches!(m.show(&ctx, "b", false, "k"), Shown::Failed(e) if e == "sync worker stopped")
        );
    }
}
