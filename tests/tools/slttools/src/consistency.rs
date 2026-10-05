//! `slttools consistency`: tests/slt/_include/consistency.slt.part を、z_final/consistency.slt と
//! 再起動テストの各シナリオの最後のフェーズへコピーする（slt の include の相対パスが未検証のため）。

use std::path::{Path, PathBuf};

pub const BEGIN: &str = "# >>> consistency (slttools consistency が tests/slt/_include/consistency.slt.part からコピー。直接編集しない)";
pub const END: &str = "# <<< consistency";
const FINAL_HEADER: &str = "# 07 §8.3 の SQL 版（slttools consistency が tests/slt/_include/consistency.slt.part からコピーする。直接編集しない）";

fn block(part: &str) -> String {
    format!("{BEGIN}\n{}\n\n{END}\n", part.trim_end())
}

/// マーカーで囲まれた部分を差し替える。マーカーがなく add なら末尾に足す。None = 変更なし / 対象外
fn apply(text: &str, part: &str, add: bool) -> Option<String> {
    let b = block(part);
    if let (Some(s), Some(e)) = (text.find("# >>> consistency"), text.find(END)) {
        let end = e + END.len();
        let end = if text[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        let new = format!("{}{}{}", &text[..s], b, &text[end..]);
        return (new != text).then_some(new);
    }
    if add {
        let mut t = text.trim_end().to_string();
        t.push_str("\n\n");
        t.push_str(&b);
        return Some(t);
    }
    None
}

fn last_phases(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in crate::lint::collect_slt(paths) {
        let Some(dir) = p.parent() else { continue };
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut slts: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|f| f.extension().is_some_and(|e| e == "slt"))
            .collect();
        slts.sort();
        if slts.last() == Some(&p) {
            out.push(p);
        }
    }
    out
}

/// 戻り値: 差分のあるファイルの一覧。check でなければ書き込む。
pub fn run(root: &Path, paths: &[PathBuf], add: bool, check: bool) -> Result<Vec<PathBuf>, String> {
    let part_path = root.join("tests/slt/_include/consistency.slt.part");
    let part =
        std::fs::read_to_string(&part_path).map_err(|e| format!("{}: {e}", part_path.display()))?;
    let mut changed = Vec::new();
    let mut write = |p: &Path, new: String| -> Result<(), String> {
        changed.push(p.to_path_buf());
        if !check {
            std::fs::write(p, new).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        Ok(())
    };

    // z_final/consistency.slt は全体がコピー
    let fin = root.join("tests/slt/m4/z_final/consistency.slt");
    let want = format!("{FINAL_HEADER}\n\n{}", part.trim_end().to_string() + "\n");
    if std::fs::read_to_string(&fin).ok().as_deref() != Some(want.as_str()) {
        write(&fin, want)?;
    }

    let targets = if paths.is_empty() {
        vec![root.join("tests/restart/m4")]
    } else {
        paths.to_vec()
    };
    for p in last_phases(&targets) {
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        if let Some(new) = apply(&text, &part, add) {
            write(&p, new)?;
        }
    }
    Ok(changed)
}
