//! The keyword table against the original `keywords[]` (`tests/data/readconf-keywords.txt`)
//! and the treatment of every name against the tables of docs/design/ssh-backends.md
//! "지시어 분류".

use std::collections::{BTreeMap, BTreeSet};

use tasty_net_ssh_config::keywords::{Treatment, known_keywords};

const TABLE: &str = include_str!("data/readconf-keywords.txt");
const DESIGN: &str = include_str!("../../../docs/design/ssh-backends.md");

/// `(name, opcode, in the enabled branch)` for every entry of the original table.
fn original_entries() -> Vec<(String, String, bool)> {
    let body = TABLE
        .split_once("---\n")
        .expect("the data file has its header line")
        .1;
    let mut enabled = vec![true];
    let mut out = Vec::new();
    for line in body.lines() {
        let l = line.trim();
        if l.starts_with("#if") {
            // `#if defined(GSSAPI)` and `#ifdef ENABLE_PKCS11`: the build the port follows.
            enabled.push(true);
        } else if l.starts_with("# else") || l.starts_with("#else") {
            let last = enabled.last_mut().expect("#else inside #if");
            *last = !*last;
        } else if l.starts_with("#endif") {
            enabled.pop();
        } else if let Some(rest) = l.strip_prefix("{ \"") {
            let (name, rest) = rest.split_once('"').expect("a quoted name");
            let opcode = rest
                .trim_start_matches([',', ' '])
                .split([' ', '}'])
                .next()
                .unwrap_or("");
            if name.is_empty() {
                continue;
            }
            out.push((
                name.to_string(),
                opcode.to_string(),
                enabled.iter().all(|e| *e),
            ));
        }
    }
    out
}

#[test]
fn table_matches_original() {
    let entries = original_entries();
    let enabled: Vec<&(String, String, bool)> = entries.iter().filter(|e| e.2).collect();
    println!(
        "keywords[]: {} entries, {} in the enabled branch",
        entries.len(),
        enabled.len()
    );
    assert_eq!(entries.len(), 132);
    assert_eq!(enabled.len(), 128);
    let names: BTreeSet<&str> = entries.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(names.len(), 128);

    let ours: Vec<(String, String)> = known_keywords()
        .map(|(n, op, _)| (n.to_string(), format!("{op:?}").to_ascii_lowercase()))
        .collect();
    let theirs: Vec<(String, String)> = enabled
        .iter()
        .map(|(n, o, _)| {
            (
                n.clone(),
                o.strip_prefix('o').unwrap_or(o).to_ascii_lowercase(),
            )
        })
        .collect();
    assert_eq!(ours, theirs);
}

fn section<'a>(heading: &str) -> &'a str {
    let start = DESIGN
        .split_once(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("ssh-backends.md has {heading}"))
        .1;
    match start.find("\n#") {
        Some(end) => &start[..end],
        None => start,
    }
}

/// The first word of every `code` span, lower-cased.
fn code_words(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter_map(|t| t.split_whitespace().next())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The code words of the first cell of every table row.
fn first_cells(sec: &str) -> Vec<String> {
    sec.lines()
        .filter(|l| l.starts_with('|'))
        .skip(2)
        .flat_map(|l| code_words(l.split('|').nth(1).unwrap_or("")))
        .collect()
}

#[test]
fn treatment_matches_design() {
    let known: BTreeMap<&str, Treatment> = known_keywords().map(|(n, _, t)| (n, t)).collect();
    let pick = |words: Vec<String>| -> BTreeSet<String> {
        words
            .into_iter()
            .filter(|w| known.contains_key(w.as_str()))
            .collect()
    };
    let supported = pick(first_cells(section("### 지원")));
    // A name in the supported table is supported; the other tables name some of its values.
    let refused: BTreeSet<String> =
        pick(first_cells(section("### 보안 관련 미구현 (적용되면 거부)")))
            .difference(&supported)
            .cloned()
            .collect();
    let shown: BTreeSet<String> = pick(first_cells(section("### 읽었지만 미지원 (표시하고 계속)")))
        .difference(&supported)
        .cloned()
        .collect();
    let dropped = pick(code_words(section("### OpenSSH가 버린 지시어")));

    let mut expected: BTreeMap<String, Treatment> = BTreeMap::new();
    for (set, t) in [
        (&supported, Treatment::Supported),
        (&refused, Treatment::SecurityRefused),
        (&shown, Treatment::ReadNotSupported),
        (&dropped, Treatment::Dropped),
    ] {
        for n in set {
            if let Some(prev) = expected.insert(n.clone(), t) {
                panic!("{n} is in two design tables ({prev:?}, {t:?})");
            }
        }
    }
    let missing: Vec<&str> = known
        .keys()
        .filter(|n| !expected.contains_key(**n))
        .copied()
        .collect();
    assert!(missing.is_empty(), "not in a design table: {missing:?}");

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (name, t) in &known {
        assert_eq!(Some(t), expected.get(*name), "treatment of {name}");
        *counts.entry(format!("{t:?}")).or_default() += 1;
    }
    println!("treatments of {} names: {counts:?}", known.len());
    assert_eq!(counts.values().sum::<usize>(), 128);
}
