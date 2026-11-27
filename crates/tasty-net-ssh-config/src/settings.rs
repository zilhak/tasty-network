//! Fixed values of this crate (docs/design/ssh-backends.md "설정값", "고정값", docs/design/crates.md
//! "설정값 규칙").
//!
//! The crate has no settable values: the two values below fix how a configuration is read, and
//! changing them would read the same files differently from OpenSSH. [`FIXED`] lists them for
//! the settings query (source `fixed`), and the tests compare it with the design table.

/// `READCONF_MAX_DEPTH` (`readconf.c:2629`): the deepest `Include` nesting.
pub(crate) const INCLUDE_MAX_DEPTH: usize = 16;

/// `SSH_RELEASE` of the base version (`version.h`): the value `Match version` compares.
pub const OPENSSH_VERSION: &str = "OpenSSH_10.6p2";

/// A fixed value: its key, its value as the design table writes it, and what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fixed {
    /// The key.
    pub key: &'static str,
    /// The value as text.
    pub value: &'static str,
    /// What it is.
    pub doc: &'static str,
}

const fn fixed(key: &'static str, value: &'static str, doc: &'static str) -> Fixed {
    Fixed { key, value, doc }
}

/// `16`, the text of [`INCLUDE_MAX_DEPTH`] (a test checks they agree).
const INCLUDE_MAX_DEPTH_TEXT: &str = "16";

/// Every fixed value, in the order of the design table.
pub static FIXED: &[Fixed] = &[
    fixed(
        "config.include_max_depth",
        INCLUDE_MAX_DEPTH_TEXT,
        "The deepest Include nesting (OpenSSH READCONF_MAX_DEPTH)",
    ),
    fixed(
        "config.openssh_version",
        OPENSSH_VERSION,
        "The OpenSSH release this crate reads configurations as (Match version)",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows of the first table under `### 고정값` of the `## 설정값` section.
    fn design_rows() -> Vec<(String, String)> {
        let doc = include_str!("../../../docs/design/ssh-backends.md");
        let section = doc
            .split("\n## 설정값\n")
            .nth(1)
            .expect("ssh-backends.md has ## 설정값");
        let fixed = section
            .split("\n### 고정값\n")
            .nth(1)
            .expect("## 설정값 has ### 고정값");
        fixed
            .lines()
            .skip_while(|l| !l.starts_with('|'))
            .take_while(|l| l.starts_with('|'))
            .skip(2)
            .map(|l| {
                let cells: Vec<String> = l
                    .split('|')
                    .map(|c| c.trim().trim_matches('`').to_string())
                    .collect();
                (cells[1].clone(), cells[2].clone())
            })
            .collect()
    }

    #[test]
    fn fixed_values_match_design_table() {
        let rows = design_rows();
        let code: Vec<(String, String)> = FIXED
            .iter()
            .map(|f| (f.key.to_string(), f.value.to_string()))
            .collect();
        assert_eq!(code, rows);
        assert_eq!(INCLUDE_MAX_DEPTH.to_string(), INCLUDE_MAX_DEPTH_TEXT);
    }
}
