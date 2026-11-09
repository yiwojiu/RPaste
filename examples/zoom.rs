//! 临时工具：裁一块区域放大，或扫一段列看暗像素落在哪几行。
//!
//! 用法：
//!   zoom in.png out.png x0 y0 x1 y1 scale   裁矩形并放大
//!   zoom in.png scan x0 x1                  逐行统计该列区间内的暗像素，用来量控件上下沿

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: zoom in.png out.png x0 y0 x1 y1 scale | zoom in.png scan x0 x1");
        std::process::exit(2);
    }
    let img = image::open(&args[0]).expect("open");

    if args[1] == "scan" {
        let x0: u32 = args[2].parse().unwrap();
        let x1: u32 = args[3].parse().unwrap();
        let gray = img.to_luma8();
        for y in 0..gray.height() {
            let mut dark = 0;
            let mut min = 255u8;
            for x in x0..x1.min(gray.width()) {
                let v = gray.get_pixel(x, y).0[0];
                if v < 205 {
                    dark += 1;
                    min = min.min(v);
                }
            }
            if dark > 0 {
                println!("y={y:<4} dark={dark:<4} min={min}");
            }
        }
        return;
    }

    let out = &args[1];
    let x0: u32 = args[2].parse().unwrap();
    let y0: u32 = args[3].parse().unwrap();
    let x1: u32 = args[4].parse().unwrap();
    let y1: u32 = args[5].parse().unwrap();
    let scale: u32 = args[6].parse().unwrap();
    let w = (x1 - x0).min(img.width() - x0);
    let h = (y1 - y0).min(img.height() - y0);
    let crop = img.crop_imm(x0, y0, w, h);
    let big = crop.resize_exact(w * scale, h * scale, image::imageops::FilterType::Nearest);
    big.save(out).expect("save");
    println!("saved {out}");
}
