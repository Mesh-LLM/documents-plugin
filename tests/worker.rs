//! The native Tika worker, run as the plugin runs it: PDF on stdin, XHTML out.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_worker(input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mesh-pdf-plugin"))
        .args(["--extract-worker", "--deadline-secs", "60"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// A minimal PDF with one Helvetica text line per page.
fn pdf(pages: &[&str]) -> Vec<u8> {
    let n = pages.len();
    let font = 3 + 2 * n;
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
    ];
    for (i, text) in pages.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 {font} 0 R >> >> /Contents {} 0 R >>",
            4 + 2 * i
        ));
        let body = format!("BT /F1 14 Tf 72 720 Td ({text}) Tj ET");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{body}\nendstream",
            body.len()
        ));
    }
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
    for offset in offsets {
        out.extend(format!("{offset:010} 00000 n \n").bytes());
    }
    out.extend(
        format!(
            "trailer << /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .bytes(),
    );
    out
}

#[test]
fn extracts_pages_without_a_jvm() {
    let output = run_worker(&pdf(&["First page here.", "The code word is PELICAN."]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let xhtml = String::from_utf8(output.stdout).unwrap();
    assert_eq!(xhtml.matches("<div class=\"page\">").count(), 2, "{xhtml}");
    assert!(xhtml.contains("PELICAN"), "{xhtml}");
}

#[test]
fn rejects_a_damaged_pdf() {
    let output = run_worker(b"%PDF-1.7\nthis is not really a pdf");
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
