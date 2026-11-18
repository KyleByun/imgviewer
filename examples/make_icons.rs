//! `src/icon.png` → OS별 아이콘 파일 (`assets/icons/`)
//!
//!     cargo run --release --example make_icons
//!
//! - 둥근 사각형 테두리 바깥의 흰 배경을 투명하게 만든다.
//!   이미지 가장자리에서 시작해 밝은 배경을 따라가다 진한 테두리에서 멈추므로
//!   테두리 모양이 정확한 원호가 아니어도 그대로 따라간다.
//!   테두리 바깥쪽의 부드러운 경계(안티에일리어싱)는 흰색이 섞인 만큼 투명도로 바꿔 흰 테두리가 남지 않게 한다.
//! - Windows: `imgview.ico` (16~256px)
//! - macOS:   `imgview.icns` (16~1024px, Apple 아이콘 격자에 맞춰 1024 캔버스에 824px로 여백을 둠)
//! - Linux / 창 아이콘: `imgview_256.png`, `imgview_512.png`

use std::collections::VecDeque;
use std::fs;
use std::io::Cursor;

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::imageops::{self, FilterType};
use image::{ExtendedColorType, ImageBuffer, ImageFormat, Rgba, RgbaImage};

const SOURCE: &str = "src/icon.png";
const OUT_DIR: &str = "assets/icons";

/// 이보다 밝으면 바깥 배경으로 본다 (흰 배경은 약 253)
const BACKGROUND_LUMA: f32 = 200.0;
/// 바깥 배경에서 이만큼 안쪽까지를 테두리의 부드러운 경계로 보고 투명도를 다시 계산한다
const FRINGE_PX: u32 = 4;

const ICO_SIZES: [u32; 10] = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256];

/// (종류, 크기). @2x 항목은 같은 크기를 고해상도 화면용으로 한 번 더 넣는다.
const ICNS_ENTRIES: [(&[u8; 4], u32); 11] = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"ic11", 32), // 16@2x
    (b"icp6", 64),
    (b"ic12", 64), // 32@2x
    (b"ic07", 128),
    (b"ic13", 256), // 128@2x
    (b"ic08", 256),
    (b"ic14", 512), // 256@2x
    (b"ic09", 512),
    (b"ic10", 1024), // 512@2x
];

type Rgba32F = ImageBuffer<Rgba<f32>, Vec<f32>>;

fn main() {
    let source = image::open(SOURCE)
        .unwrap_or_else(|e| panic!("{SOURCE}을(를) 읽을 수 없습니다: {e}"))
        .to_rgba8();
    let cut = remove_outer_background(&source);
    let icon = crop_to_square(&cut);
    println!(
        "원본 {}x{} → 바깥 배경을 투명하게 만들고 {}x{}로 자름",
        source.width(),
        source.height(),
        icon.width(),
        icon.height()
    );

    fs::create_dir_all(OUT_DIR).expect("assets/icons 폴더를 만들 수 없습니다");

    for size in [256, 512] {
        let path = format!("{OUT_DIR}/imgview_{size}.png");
        fs::write(&path, encode_png(&resize(&icon, size))).expect("PNG를 쓰지 못했습니다");
        println!("{path}");
    }

    let ico_path = format!("{OUT_DIR}/imgview.ico");
    write_ico(&icon, &ico_path);
    println!("{ico_path} ({}개 크기)", ICO_SIZES.len());

    let icns_path = format!("{OUT_DIR}/imgview.icns");
    write_icns(&with_margin(&icon, 1024, 824), &icns_path);
    println!("{icns_path} ({}개 항목)", ICNS_ENTRIES.len());
}

fn luma(p: &Rgba<u8>) -> f32 {
    0.299 * f32::from(p[0]) + 0.587 * f32::from(p[1]) + 0.114 * f32::from(p[2])
}

/// 이미지 가장자리와 이어진 밝은 배경을 투명하게 만든다.
fn remove_outer_background(src: &RgbaImage) -> RgbaImage {
    let (w, h) = src.dimensions();
    let index = |x: u32, y: u32| (y * w + x) as usize;
    // 바깥 배경까지의 거리: 0 = 바깥 배경, 1..=FRINGE_PX = 경계 부근, MAX = 안쪽
    let mut dist = vec![u32::MAX; (w * h) as usize];

    // 1) 가장자리의 밝은 픽셀에서 시작해 밝은 픽셀만 따라 채운다 (진한 테두리에서 멈춤)
    let mut queue = VecDeque::new();
    let seed = |x: u32, y: u32, dist: &mut Vec<u32>, queue: &mut VecDeque<(u32, u32)>| {
        if dist[index(x, y)] == u32::MAX && luma(src.get_pixel(x, y)) > BACKGROUND_LUMA {
            dist[index(x, y)] = 0;
            queue.push_back((x, y));
        }
    };
    for x in 0..w {
        seed(x, 0, &mut dist, &mut queue);
        seed(x, h - 1, &mut dist, &mut queue);
    }
    for y in 0..h {
        seed(0, y, &mut dist, &mut queue);
        seed(w - 1, y, &mut dist, &mut queue);
    }
    while let Some((x, y)) = queue.pop_front() {
        for (nx, ny) in neighbors(x, y, w, h) {
            let i = index(nx, ny);
            if dist[i] == u32::MAX && luma(src.get_pixel(nx, ny)) > BACKGROUND_LUMA {
                dist[i] = 0;
                queue.push_back((nx, ny));
            }
        }
    }

    // 2) 바깥 배경에서 FRINGE_PX 안쪽까지의 거리
    let mut queue: VecDeque<(u32, u32)> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .filter(|&(x, y)| dist[index(x, y)] == 0)
        .collect();
    while let Some((x, y)) = queue.pop_front() {
        let d = dist[index(x, y)];
        if d >= FRINGE_PX {
            continue;
        }
        for (nx, ny) in neighbors(x, y, w, h) {
            let i = index(nx, ny);
            if dist[i] == u32::MAX {
                dist[i] = d + 1;
                queue.push_back((nx, ny));
            }
        }
    }

    // 3) 테두리 색: 경계 부근에서 가장 진한 픽셀들의 평균
    let fringe: Vec<&Rgba<u8>> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .filter(|&(x, y)| dist[index(x, y)] == FRINGE_PX)
        .map(|(x, y)| src.get_pixel(x, y))
        .filter(|p| luma(p) < 128.0)
        .collect();
    let border = if fringe.is_empty() {
        Rgba([0, 0, 0, 255])
    } else {
        let n = fringe.len() as f32;
        let avg = |c: usize| (fringe.iter().map(|p| f32::from(p[c])).sum::<f32>() / n).round() as u8;
        Rgba([avg(0), avg(1), avg(2), 255])
    };
    let border_luma = luma(&border);

    // 4) 바깥은 투명, 경계는 흰색이 섞인 만큼 투명하게 (색은 흰색을 뺀 원래 색으로)
    RgbaImage::from_fn(w, h, |x, y| {
        let p = *src.get_pixel(x, y);
        match dist[index(x, y)] {
            0 => Rgba([border[0], border[1], border[2], 0]),
            d if d <= FRINGE_PX && luma(&p) > border_luma => {
                let alpha = ((255.0 - luma(&p)) / (255.0 - border_luma)).clamp(0.0, 1.0);
                if alpha <= 0.0 {
                    return Rgba([border[0], border[1], border[2], 0]);
                }
                let unblend = |c: u8| {
                    ((f32::from(c) - (1.0 - alpha) * 255.0) / alpha)
                        .round()
                        .clamp(0.0, 255.0) as u8
                };
                Rgba([
                    unblend(p[0]),
                    unblend(p[1]),
                    unblend(p[2]),
                    (alpha * f32::from(p[3])).round() as u8,
                ])
            }
            _ => p,
        }
    })
}

fn neighbors(x: u32, y: u32, w: u32, h: u32) -> impl Iterator<Item = (u32, u32)> {
    let candidates = [
        (x.wrapping_sub(1), y),
        (x + 1, y),
        (x, y.wrapping_sub(1)),
        (x, y + 1),
    ];
    candidates.into_iter().filter(move |&(nx, ny)| nx < w && ny < h)
}

/// 투명하지 않은 부분에 딱 맞게 자르고, 정사각형이 되도록 투명한 여백을 둔다.
fn crop_to_square(img: &RgbaImage) -> RgbaImage {
    let (mut left, mut top, mut right, mut bottom) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 0 {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    assert!(left <= right, "아이콘에 보이는 부분이 없습니다");
    let (w, h) = (right - left + 1, bottom - top + 1);
    let cropped = imageops::crop_imm(img, left, top, w, h).to_image();
    let side = w.max(h);
    let mut square = RgbaImage::new(side, side);
    imageops::overlay(
        &mut square,
        &cropped,
        i64::from((side - w) / 2),
        i64::from((side - h) / 2),
    );
    square
}

/// 투명한 부분의 색이 섞여 가장자리가 하얘지거나 어두워지지 않도록, 알파를 곱한 상태에서 줄인다.
fn resize(img: &RgbaImage, size: u32) -> RgbaImage {
    if img.width() == size && img.height() == size {
        return img.clone();
    }
    let premultiplied = Rgba32F::from_fn(img.width(), img.height(), |x, y| {
        let p = img.get_pixel(x, y);
        let a = f32::from(p[3]) / 255.0;
        Rgba([
            f32::from(p[0]) / 255.0 * a,
            f32::from(p[1]) / 255.0 * a,
            f32::from(p[2]) / 255.0 * a,
            a,
        ])
    });
    let small = imageops::resize(&premultiplied, size, size, FilterType::Lanczos3);
    RgbaImage::from_fn(size, size, |x, y| {
        let p = small.get_pixel(x, y);
        let a = p[3].clamp(0.0, 1.0);
        if a <= 0.0 {
            return Rgba([0, 0, 0, 0]);
        }
        let channel = |c: f32| ((c / a).clamp(0.0, 1.0) * 255.0).round() as u8;
        Rgba([channel(p[0]), channel(p[1]), channel(p[2]), (a * 255.0).round() as u8])
    })
}

/// `canvas`×`canvas` 투명 캔버스 가운데에 `content` 크기로 놓는다.
fn with_margin(img: &RgbaImage, canvas: u32, content: u32) -> RgbaImage {
    let mut out = RgbaImage::new(canvas, canvas);
    let offset = i64::from((canvas - content) / 2);
    imageops::overlay(&mut out, &resize(img, content), offset, offset);
    out
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)
        .expect("PNG로 바꾸지 못했습니다");
    out.into_inner()
}

fn write_ico(icon: &RgbaImage, path: &str) {
    let images: Vec<RgbaImage> = ICO_SIZES.iter().map(|&s| resize(icon, s)).collect();
    let frames: Vec<IcoFrame> = images
        .iter()
        .map(|img| {
            IcoFrame::as_png(img.as_raw(), img.width(), img.height(), ExtendedColorType::Rgba8)
                .expect("아이콘 프레임을 만들지 못했습니다")
        })
        .collect();
    let file = fs::File::create(path).expect("ico 파일을 만들 수 없습니다");
    IcoEncoder::new(file)
        .encode_images(&frames)
        .expect("ico 파일을 쓰지 못했습니다");
}

/// .icns: "icns" + 전체 길이, 그 뒤에 (종류 4바이트 + 길이 4바이트 + PNG) 항목들 (길이는 빅엔디언)
fn write_icns(icon_1024: &RgbaImage, path: &str) {
    let mut body = Vec::new();
    for (kind, size) in ICNS_ENTRIES {
        let png = encode_png(&resize(icon_1024, size));
        body.extend_from_slice(kind);
        body.extend_from_slice(&(png.len() as u32 + 8).to_be_bytes());
        body.extend_from_slice(&png);
    }
    let mut file = b"icns".to_vec();
    file.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    file.extend_from_slice(&body);
    fs::write(path, file).expect("icns 파일을 쓰지 못했습니다");
}
