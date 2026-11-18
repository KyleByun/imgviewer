//! 백그라운드 이미지 로더.
//!
//! UI 스레드가 멈추지 않도록 디코딩/축소는 작업 스레드에서 하고,
//! 결과(GPU 텍스처 핸들)를 채널로 UI 스레드에 넘긴다.
//! 폴더를 바꾸면 세대(generation) 번호가 올라가서 이전 폴더의 작업/결과는 버려진다.

use std::collections::VecDeque;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use eframe::egui;
use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage};

/// 썸네일의 최대 가로/세로 픽셀
pub const THUMB_SIZE: u32 = 256;
/// 애니메이션 GIF 한 개가 쓸 수 있는 최대 메모리
const MAX_ANIMATION_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ANIMATION_FRAMES: usize = 1000;

pub struct ThumbImage {
    pub texture: egui::TextureHandle,
    /// 원본 이미지 크기 (EXIF 회전 적용 후)
    pub orig_size: [u32; 2],
    /// 탐색기로 끌 때 커서 옆에 보여줄 작은 PNG
    pub preview_png: Vec<u8>,
}

/// 끌기 미리보기 이미지의 최대 크기
const PREVIEW_SIZE: u32 = 96;

pub struct AnimFrame {
    pub texture: egui::TextureHandle,
    /// 이 프레임을 보여줄 시간(초). 정지 이미지는 0.
    pub delay: f64,
}

pub struct FullImage {
    pub frames: Vec<AnimFrame>,
    pub orig_size: [u32; 2],
}

impl FullImage {
    /// 애니메이션 시작 후 `elapsed`초가 지났을 때 보여줄 프레임과, 다음 프레임까지 남은 시간.
    pub fn frame_at(&self, elapsed: f64) -> (usize, f64) {
        if self.frames.len() <= 1 {
            return (0, f64::INFINITY);
        }
        let total: f64 = self.frames.iter().map(|f| f.delay).sum();
        if total <= 0.0 {
            return (0, f64::INFINITY);
        }
        let mut t = elapsed.max(0.0) % total;
        for (i, frame) in self.frames.iter().enumerate() {
            if t < frame.delay {
                return (i, frame.delay - t);
            }
            t -= frame.delay;
        }
        (self.frames.len() - 1, 0.01)
    }
}

pub enum LoadResult {
    Thumb {
        gen: u64,
        index: usize,
        result: Result<ThumbImage, String>,
    },
    Full {
        gen: u64,
        index: usize,
        result: Result<FullImage, String>,
    },
}

enum JobKind {
    Thumb,
    Full { max_side: u32 },
}

struct Job {
    gen: u64,
    index: usize,
    path: PathBuf,
    kind: JobKind,
}

#[derive(Default)]
struct Queues {
    /// 크게 보기용 원본 이미지 작업 (항상 썸네일보다 먼저 처리)
    full: VecDeque<Job>,
    /// 썸네일 작업 (가장 최근에 요청된 = 지금 화면에 보이는 것부터 처리)
    thumbs: VecDeque<Job>,
    shutdown: bool,
}

struct Shared {
    queues: Mutex<Queues>,
    wake: Condvar,
    gen: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Loader {
    shared: Arc<Shared>,
    results: Receiver<LoadResult>,
}

impl Loader {
    pub fn new(ctx: &egui::Context) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            wake: Condvar::new(),
            gen: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::channel();
        let threads = thread::available_parallelism()
            .map_or(4, |n| n.get())
            .clamp(2, 8);
        for i in 0..threads {
            let shared = Arc::clone(&shared);
            let tx = tx.clone();
            let ctx = ctx.clone();
            // 0번 스레드는 크게 보기 전용: 썸네일이 잔뜩 밀려 있어도 클릭한 이미지가 바로 열리도록.
            let full_only = i == 0;
            thread::Builder::new()
                .name(format!("imgview-loader-{i}"))
                .spawn(move || worker(shared, tx, ctx, full_only))
                .expect("이미지 로더 스레드를 만들 수 없습니다");
        }
        Self {
            shared,
            results: rx,
        }
    }

    /// 대기 중인 작업을 모두 버리고 새 세대 번호를 돌려준다.
    pub fn reset(&self) -> u64 {
        let mut q = self.shared.lock();
        q.full.clear();
        q.thumbs.clear();
        self.shared.gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn request_thumb(&self, gen: u64, index: usize, path: PathBuf) {
        self.shared.lock().thumbs.push_front(Job {
            gen,
            index,
            path,
            kind: JobKind::Thumb,
        });
        self.shared.wake.notify_all();
    }

    /// `urgent`이면 지금 보려는 이미지, 아니면 미리 읽어 두는 이웃 이미지.
    pub fn request_full(&self, gen: u64, index: usize, path: PathBuf, max_side: u32, urgent: bool) {
        let job = Job {
            gen,
            index,
            path,
            kind: JobKind::Full { max_side },
        };
        {
            let mut q = self.shared.lock();
            if urgent {
                q.full.push_front(job);
            } else {
                q.full.push_back(job);
            }
        }
        self.shared.wake.notify_all();
    }

    /// `keep`이 false인 썸네일 작업을 큐에서 빼고, 뺀 작업의 인덱스를 돌려준다.
    pub fn retain_thumbs(&self, keep: impl FnMut(usize) -> bool) -> Vec<usize> {
        retain_jobs(&mut self.shared.lock().thumbs, keep)
    }

    /// `keep`이 false인 원본 이미지 작업을 큐에서 빼고, 뺀 작업의 인덱스를 돌려준다.
    pub fn retain_full(&self, keep: impl FnMut(usize) -> bool) -> Vec<usize> {
        retain_jobs(&mut self.shared.lock().full, keep)
    }

    pub fn try_recv(&self) -> Option<LoadResult> {
        self.results.try_recv().ok()
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.wake.notify_all();
    }
}

fn retain_jobs(jobs: &mut VecDeque<Job>, mut keep: impl FnMut(usize) -> bool) -> Vec<usize> {
    let mut dropped = Vec::new();
    jobs.retain(|job| {
        if keep(job.index) {
            true
        } else {
            dropped.push(job.index);
            false
        }
    });
    dropped
}

fn worker(shared: Arc<Shared>, tx: Sender<LoadResult>, ctx: egui::Context, full_only: bool) {
    loop {
        let job = {
            let mut q = shared.lock();
            loop {
                if q.shutdown {
                    return;
                }
                let next = match q.full.pop_front() {
                    Some(job) => Some(job),
                    None if !full_only => q.thumbs.pop_front(),
                    None => None,
                };
                if let Some(job) = next {
                    break job;
                }
                q = shared.wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };

        if job.gen != shared.gen.load(Ordering::SeqCst) {
            continue; // 이미 다른 폴더로 이동함
        }

        let Job {
            gen,
            index,
            path,
            kind,
        } = job;
        let message = match kind {
            JobKind::Thumb => LoadResult::Thumb {
                gen,
                index,
                result: catch_panic(|| load_thumb(&ctx, &path)),
            },
            JobKind::Full { max_side } => LoadResult::Full {
                gen,
                index,
                result: catch_panic(|| load_full(&ctx, &path, max_side)),
            },
        };
        if tx.send(message).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// 깨진 파일 때문에 디코더가 패닉해도 작업 스레드가 죽지 않게 한다.
fn catch_panic<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err("이미지를 해석하는 중 오류가 발생했습니다".to_owned()))
}

/// 파일을 디코딩한다. 확장자가 틀려도 파일 내용으로 형식을 추정하고, EXIF 회전 정보를 적용한다.
pub fn decode_image(path: &Path) -> Result<DynamicImage, String> {
    let reader = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if let Some(orientation) = orientation {
        img.apply_orientation(orientation);
    }
    Ok(img)
}

/// 가로/세로 모두 `max` 이하가 되도록 비율을 유지하며 줄인다 (작은 이미지는 그대로).
pub fn make_thumbnail(img: DynamicImage, max: u32) -> RgbaImage {
    if img.width() > max || img.height() > max {
        img.thumbnail(max, max).into_rgba8()
    } else {
        img.into_rgba8()
    }
}

/// GPU 텍스처 한계를 넘는 큰 이미지만 줄인다.
fn fit_within(img: DynamicImage, max_side: u32) -> DynamicImage {
    if img.width() > max_side || img.height() > max_side {
        img.resize(max_side, max_side, FilterType::Triangle)
    } else {
        img
    }
}

/// 애니메이션 GIF의 모든 프레임(전체 캔버스 크기로 합성된 것)과 각 프레임의 표시 시간(초).
pub fn decode_gif_frames(
    path: &Path,
    max_side: u32,
) -> Result<(Vec<(RgbaImage, f64)>, [u32; 2]), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let decoder = GifDecoder::new(BufReader::new(file)).map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions();
    let frame_bytes = (u64::from(width) * u64::from(height) * 4).max(1);
    let limit = ((MAX_ANIMATION_BYTES / frame_bytes) as usize).clamp(1, MAX_ANIMATION_FRAMES);

    let mut frames = Vec::new();
    for frame in decoder.into_frames().take(limit) {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) if frames.is_empty() => return Err(e.to_string()),
            Err(_) => break, // 뒤쪽 프레임이 깨졌으면 앞부분만 재생
        };
        let (numer, denom) = frame.delay().numer_denom_ms();
        let mut delay = if denom == 0 {
            0.0
        } else {
            f64::from(numer) / f64::from(denom) / 1000.0
        };
        // 브라우저처럼 지연시간이 거의 0인 프레임은 0.1초로 취급
        if delay < 0.02 {
            delay = 0.1;
        }
        let buffer = frame.into_buffer();
        let buffer = if buffer.width() > max_side || buffer.height() > max_side {
            fit_within(DynamicImage::ImageRgba8(buffer), max_side).into_rgba8()
        } else {
            buffer
        };
        frames.push((buffer, delay));
    }
    Ok((frames, [width, height]))
}

fn is_gif(path: &Path) -> bool {
    ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map(|r| r.format() == Some(ImageFormat::Gif))
        .unwrap_or(false)
}

fn texture_options() -> egui::TextureOptions {
    // 밉맵을 켜서 큰 사진을 많이 축소해도 지글거리지 않게 한다.
    egui::TextureOptions {
        mipmap_mode: Some(egui::TextureFilter::Linear),
        ..egui::TextureOptions::LINEAR
    }
}

fn upload(ctx: &egui::Context, name: String, rgba: &RgbaImage) -> egui::TextureHandle {
    let size = [rgba.width() as usize, rgba.height() as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    ctx.load_texture(name, image, texture_options())
}

fn load_thumb(ctx: &egui::Context, path: &Path) -> Result<ThumbImage, String> {
    let img = decode_image(path)?;
    let orig_size = [img.width(), img.height()];
    let rgba = make_thumbnail(img, THUMB_SIZE);
    let preview = make_thumbnail(DynamicImage::ImageRgba8(rgba.clone()), PREVIEW_SIZE);
    Ok(ThumbImage {
        texture: upload(ctx, format!("thumb:{}", path.display()), &rgba),
        orig_size,
        preview_png: encode_png(&preview),
    })
}

pub fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    match img.write_to(&mut out, ImageFormat::Png) {
        Ok(()) => out.into_inner(),
        Err(_) => Vec::new(),
    }
}

/// 한 가지 색으로 채운 PNG (폴더를 끌 때의 미리보기용)
pub fn solid_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    encode_png(&RgbaImage::from_pixel(width, height, image::Rgba(rgba)))
}

fn load_full(ctx: &egui::Context, path: &Path, max_side: u32) -> Result<FullImage, String> {
    if is_gif(path) {
        if let Ok((frames, orig_size)) = decode_gif_frames(path, max_side) {
            if !frames.is_empty() {
                let frames = frames
                    .into_iter()
                    .enumerate()
                    .map(|(i, (rgba, delay))| AnimFrame {
                        texture: upload(ctx, format!("full:{}#{i}", path.display()), &rgba),
                        delay,
                    })
                    .collect();
                return Ok(FullImage { frames, orig_size });
            }
        }
        // 애니메이션 해석에 실패하면 아래의 일반 디코딩으로 첫 프레임이라도 보여준다.
    }

    let img = decode_image(path)?;
    let orig_size = [img.width(), img.height()];
    let rgba = fit_within(img, max_side).into_rgba8();
    Ok(FullImage {
        frames: vec![AnimFrame {
            texture: upload(ctx, format!("full:{}", path.display()), &rgba),
            delay: 0.0,
        }],
        orig_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgb, RgbImage, Rgba};

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("imgview-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_rgba(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]))
    }

    #[test]
    fn decodes_common_formats_and_makes_thumbnails() {
        let dir = test_dir("formats");
        for ext in ["png", "jpg", "gif", "bmp", "tiff", "webp", "tga", "qoi", "ppm"] {
            let path = dir.join(format!("sample.{ext}"));
            let img = if matches!(ext, "jpg" | "ppm") {
                DynamicImage::ImageRgb8(RgbImage::from_fn(240, 180, |x, y| {
                    Rgb([(x % 256) as u8, (y % 256) as u8, 64])
                }))
            } else {
                DynamicImage::ImageRgba8(sample_rgba(240, 180))
            };
            img.save(&path)
                .unwrap_or_else(|e| panic!("{ext} 저장 실패: {e}"));

            let decoded = decode_image(&path).unwrap_or_else(|e| panic!("{ext} 디코딩 실패: {e}"));
            assert_eq!((decoded.width(), decoded.height()), (240, 180), "{ext}");

            let thumb = make_thumbnail(decoded, 64);
            assert_eq!(thumb.width(), 64, "{ext}");
            assert_eq!(thumb.height(), 48, "{ext}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn small_images_are_not_enlarged() {
        let thumb = make_thumbnail(DynamicImage::ImageRgba8(sample_rgba(20, 10)), 256);
        assert_eq!((thumb.width(), thumb.height()), (20, 10));
    }

    #[test]
    fn wrong_extension_is_detected_from_content() {
        let dir = test_dir("mislabelled");
        let png_path = dir.join("real.png");
        sample_rgba(30, 20).save(&png_path).unwrap();
        let fake = dir.join("actually_png.jpg");
        std::fs::copy(&png_path, &fake).unwrap();

        let decoded = decode_image(&fake).expect("내용으로 PNG를 알아내야 함");
        assert_eq!((decoded.width(), decoded.height()), (30, 20));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_file_returns_error_instead_of_panicking() {
        let dir = test_dir("broken");
        let path = dir.join("broken.png");
        std::fs::write(&path, b"this is not an image").unwrap();
        assert!(decode_image(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn animated_gif_frames_and_timing() {
        let dir = test_dir("anim");
        let path = dir.join("anim.gif");
        {
            let file = File::create(&path).unwrap();
            let mut encoder = GifEncoder::new(file);
            let frames = (0..3u8).map(|i| {
                Frame::from_parts(
                    RgbaImage::from_pixel(20, 10, Rgba([i * 80, 0, 0, 255])),
                    0,
                    0,
                    Delay::from_numer_denom_ms(50, 1),
                )
            });
            encoder.encode_frames(frames).unwrap();
        }

        let (frames, size) = decode_gif_frames(&path, 8192).unwrap();
        assert_eq!(size, [20, 10]);
        assert_eq!(frames.len(), 3);
        assert!((frames[0].1 - 0.05).abs() < 1e-6, "delay = {}", frames[0].1);

        // 텍스처 업로드 경로까지 확인 (렌더러 없는 egui 컨텍스트로)
        let ctx = egui::Context::default();
        let full = load_full(&ctx, &path, 8192).unwrap();
        assert_eq!(full.frames.len(), 3);
        assert_eq!(full.frame_at(0.0).0, 0);
        assert_eq!(full.frame_at(0.06).0, 1);
        assert_eq!(full.frame_at(0.16).0, 0); // 한 바퀴 돌아서 처음으로

        let thumb = load_thumb(&ctx, &path).unwrap();
        assert_eq!(thumb.orig_size, [20, 10]);
        let preview = image::load_from_memory(&thumb.preview_png).expect("미리보기는 PNG여야 함");
        assert_eq!((preview.width(), preview.height()), (20, 10));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn drag_preview_is_small_png() {
        let preview = make_thumbnail(DynamicImage::ImageRgba8(sample_rgba(400, 200)), PREVIEW_SIZE);
        let png = encode_png(&preview);
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (96, 48));
        assert!(image::load_from_memory(&solid_png(64, 48, [1, 2, 3, 255])).is_ok());
    }

    #[test]
    fn huge_images_are_shrunk_to_texture_limit() {
        let img = DynamicImage::ImageRgba8(sample_rgba(1000, 400));
        let fitted = fit_within(img, 500);
        assert_eq!((fitted.width(), fitted.height()), (500, 200));
    }
}
