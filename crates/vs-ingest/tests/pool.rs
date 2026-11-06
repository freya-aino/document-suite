//! Render pool against the real child binary and against misbehaving stand-ins.

use std::io::Cursor;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use vs_ingest::{ChildCommand, Format, IngestError, PoolConfig, RenderPool};

fn child() -> ChildCommand {
    ChildCommand { program: env!("CARGO_BIN_EXE_vs-render-child").into(), args: vec![], env: vec![] }
}

fn cfg(size: usize, page_timeout: Duration) -> PoolConfig {
    PoolConfig { size, page_timeout, open_timeout: page_timeout }
}

fn tiff_file(name: &str, frames: u32) -> PathBuf {
    use tiff::encoder::{colortype::Gray8, TiffEncoder};
    let mut out = Cursor::new(Vec::new());
    let mut enc = TiffEncoder::new(&mut out).unwrap();
    for i in 0..frames {
        enc.write_image::<Gray8>(64, 80, &vec![(i * 40) as u8; 64 * 80]).unwrap();
    }
    let path = std::env::temp_dir().join(format!("vs-pool-{}-{name}.tiff", std::process::id()));
    std::fs::write(&path, out.into_inner()).unwrap();
    path
}

#[tokio::test]
async fn renders_pages_in_parallel_children() {
    let path = tiff_file("ok", 6);
    let pool = std::sync::Arc::new(RenderPool::new(child(), cfg(3, Duration::from_secs(30))));
    assert_eq!(pool.page_count(&path, Format::Tiff, 100).await.unwrap(), 6);

    let mut handles = Vec::new();
    for n in 1..=6 {
        let (pool, path) = (pool.clone(), path.clone());
        handles.push(tokio::spawn(async move { pool.page(&path, Format::Tiff, 100, n, 300, 40).await }));
    }
    for h in handles {
        let a = h.await.unwrap().unwrap();
        assert_eq!((a.image.width_px, a.image.height_px), (64, 80));
        assert_eq!(&a.image.bytes[1..4], b"PNG");
        assert_eq!(a.thumbnail.height_px, 40);
        assert_eq!(a.signals.image_count, 1);
    }

    // Child-side errors come back as errors and keep the child usable.
    let missing = std::env::temp_dir().join("vs-pool-does-not-exist.tiff");
    assert!(matches!(pool.page_count(&missing, Format::Tiff, 100).await, Err(IngestError::Io(_))));
    assert!(matches!(pool.page(&path, Format::Tiff, 100, 7, 300, 40).await, Err(IngestError::Render { page: 7, .. })));
    assert_eq!(pool.page_count(&path, Format::Tiff, 100).await.unwrap(), 6);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn hanging_child_times_out_permanently() {
    let hang = ChildCommand { program: "sleep".into(), args: vec!["1000".into()], env: vec![] };
    let pool = RenderPool::new(hang, cfg(1, Duration::from_millis(300)));
    let t = Instant::now();
    let err = pool.page(&PathBuf::from("/x.pdf"), Format::Pdf, 10, 4, 200, 768).await.unwrap_err();
    assert!(matches!(err, IngestError::Timeout { page: 4 }), "{err:?}");
    assert!(err.is_permanent());
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn crashing_child_is_retried_once_then_permanent() {
    let crash = ChildCommand { program: "false".into(), args: vec![], env: vec![] };
    let pool = RenderPool::new(crash, cfg(1, Duration::from_secs(5)));
    let err = pool.page(&PathBuf::from("/x.pdf"), Format::Pdf, 10, 2, 200, 768).await.unwrap_err();
    match &err {
        IngestError::Crashed { page: 2, reason } => assert!(reason.contains("exit"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert!(err.is_permanent());
}

#[tokio::test]
async fn renders_pdf_in_child() {
    use pdfium_render::prelude::*;
    let Ok(p) = vs_ingest::pdfium() else {
        assert!(std::env::var_os("PDFIUM_DYNAMIC_LIB_PATH").is_none(), "pdfium binding failed");
        eprintln!("skipping: no pdfium");
        return;
    };
    let bytes = {
        let mut doc = p.create_new_pdf().unwrap();
        let font = doc.fonts_mut().helvetica();
        for i in 0..2 {
            let mut page = doc.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            page.objects_mut()
                .create_text_object(PdfPoints::new(72.0), PdfPoints::new(750.0), format!("page {}", i + 1), font, PdfPoints::new(12.0))
                .unwrap();
        }
        doc.save_to_bytes().unwrap()
    };
    drop(p);
    let path = std::env::temp_dir().join(format!("vs-pool-{}.pdf", std::process::id()));
    std::fs::write(&path, bytes).unwrap();

    let pool = RenderPool::new(child(), cfg(2, Duration::from_secs(30)));
    assert_eq!(pool.page_count(&path, Format::Pdf, 10).await.unwrap(), 2);
    let a = pool.page(&path, Format::Pdf, 10, 2, 72, 200).await.unwrap();
    assert_eq!((a.image.width_px, a.image.dpi), (595, 72));
    assert!(a.text_layer.spans.iter().any(|s| s.text.contains("page 2")));
    let _ = std::fs::remove_file(path);
}
