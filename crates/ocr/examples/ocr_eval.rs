//! Measure recognition on page images: `cargo run -p pdfcraft-ocr --release --example ocr_eval
//! -- OUT_DIR page.png…` writes `<stem>.txt` (the lines read, in reading order) and `<stem>.time`
//! (seconds) for each image. `PDFCRAFT_OCR_ARABIC=0` measures without the Arabic model.

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, pages) = args.split_first().ok_or("usage: ocr_eval OUT_DIR page.png…")?;
    std::fs::create_dir_all(out)?;
    let t = std::time::Instant::now();
    let ocr = pdfcraft_ocr::Ocr::find()?;
    eprintln!("loaded in {:.2}s (Arabic: {})", t.elapsed().as_secs_f32(), ocr.reads_arabic());
    for page in pages {
        let mut r = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(page)?)).read_info()?;
        let mut buf = vec![0; r.output_buffer_size().ok_or("image too large")?];
        let info = r.next_frame(&mut buf)?;
        let px: Vec<u8> = match info.color_type {
            png::ColorType::Grayscale => buf[..info.buffer_size()].to_vec(),
            png::ColorType::Rgb => buf[..info.buffer_size()].to_vec(),
            png::ColorType::Rgba => buf[..info.buffer_size()].chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
            other => return Err(format!("{page}: {other:?} images are not supported").into()),
        };
        let t = std::time::Instant::now();
        let lines = ocr.recognize(&px, info.width, info.height)?;
        let secs = t.elapsed().as_secs_f64();
        let stem = Path::new(page).file_stem().and_then(|s| s.to_str()).ok_or("bad file name")?;
        let text: Vec<String> = lines.iter().map(pdfcraft_ocr::Line::text).collect();
        std::fs::write(Path::new(out).join(format!("{stem}.txt")), text.join("\n") + "\n")?;
        std::fs::write(Path::new(out).join(format!("{stem}.time")), format!("{secs:.3}\n"))?;
        eprintln!("{stem}: {} lines, {secs:.2}s", lines.len());
    }
    Ok(())
}
