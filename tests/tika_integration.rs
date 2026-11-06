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
        ("synthetic.docx", vec!["合成Word文档", "表格内容香蕉"]),
        ("synthetic.pptx", vec!["合成PPT", "搜索葡萄"]),
        ("synthetic.xlsx", vec!["搜索橘子"]),
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
