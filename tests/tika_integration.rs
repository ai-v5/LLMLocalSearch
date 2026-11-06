use anyhow::{Context, Result};
use std::path::PathBuf;

#[test]
#[ignore = "requires verified Tika and Java runtime; set LLMLOCALSEARCH_TIKA_RUNTIME"]
fn extracts_synthetic_office_and_pdf_text() -> Result<()> {
    let runtime = PathBuf::from(
        std::env::var_os("LLMLOCALSEARCH_TIKA_RUNTIME")
            .context("missing LLMLOCALSEARCH_TIKA_RUNTIME")?,
    );
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let cases = [
        (
            "synthetic.docx",
            vec!["合成Word文档", "表格内容香蕉", "合成文本框火星"],
        ),
        (
            "synthetic.pptx",
            vec!["合成PPT", "搜索葡萄", "合成幻灯备注海王星"],
        ),
        ("synthetic.xlsx", vec!["搜索橘子", "937241"]),
        ("synthetic.pdf", vec!["PDF合成文本", "搜索西瓜"]),
    ];
    for (name, expected) in cases {
        let text = llmlocalsearch::tika::extract(&runtime, &fixtures.join(name))?;
        for value in expected {
            assert!(text.contains(value), "{name}: missing synthetic marker");
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires verified Tika and Java runtime; set LLMLOCALSEARCH_TIKA_RUNTIME"]
fn reports_extracted_text_above_the_limit() -> Result<()> {
    let runtime = PathBuf::from(
        std::env::var_os("LLMLOCALSEARCH_TIKA_RUNTIME")
            .context("missing LLMLOCALSEARCH_TIKA_RUNTIME")?,
    );
    let temporary = tempfile::tempdir()?;
    let file = temporary.path().join("synthetic-large.html");
    let mut document = b"<html><body><p>".to_vec();
    document.extend(std::iter::repeat_n(
        b'x',
        llmlocalsearch::text::MAX_TEXT_BYTES + 4096,
    ));
    document.extend_from_slice(b"</p></body></html>");
    std::fs::write(&file, document)?;
    let error = llmlocalsearch::tika::extract(&runtime, &file).unwrap_err();
    assert!(
        error.to_string().contains("提取文本超过大小限制"),
        "{error:#}"
    );
    Ok(())
}
