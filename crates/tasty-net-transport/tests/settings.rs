//! The settings of the connection table and the redialer (docs/design/crates.md "설정값
//! 규칙", docs/design/testing.md "설정값 시험").

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use tasty_net_transport::settings::{DEFAULTS, Setting, SettingSource, SettingValue};
use tasty_net_transport::{ConnectionTable, ConnectionTableConfig, Redialer, RedialerConfig};

/// Every setting the connection table and a redialer show.
fn shown(table: &ConnectionTable, redialer: &Redialer) -> Vec<Setting> {
    table
        .settings()
        .into_iter()
        .chain(redialer.settings())
        .collect()
}

#[test]
fn settings_list_matches_query() {
    // Untouched: every key of the defaults, with its default value and source.
    let table = ConnectionTable::default();
    let redialer = Redialer::new(&table, RedialerConfig::new()).expect("defaults");
    let listed: Vec<_> = DEFAULTS.iter().map(|d| (d.key, d.value)).collect();
    let queried: Vec<_> = shown(&table, &redialer)
        .iter()
        .map(|s| (s.key, s.value))
        .collect();
    assert_eq!(queried, listed);
    assert!(
        shown(&table, &redialer)
            .iter()
            .all(|s| s.source == SettingSource::Default)
    );

    // Each value given: that key is configured with the value, the others stay default.
    let given: [(&str, SettingValue); 4] = [
        ("connections.history_limit", SettingValue::Count(4)),
        ("connections.event_queue_limit", SettingValue::Count(9)),
        (
            "redial.initial",
            SettingValue::Duration(Duration::from_millis(250)),
        ),
        ("redial.jitter", SettingValue::Ratio(0.5)),
    ];
    let table = ConnectionTable::new(
        ConnectionTableConfig::new()
            .with_history_limit(4)
            .with_event_queue_limit(9),
    );
    let config = RedialerConfig::new()
        .with_initial(Duration::from_millis(250))
        .with_jitter(0.5);
    assert_eq!(config.initial(), Duration::from_millis(250));
    assert_eq!(config.multiplier(), 2.0, "not given: the default");
    let redialer = Redialer::new(&table, config).expect("valid");
    let queried = shown(&table, &redialer);
    assert_eq!(
        queried.iter().map(|s| s.key).collect::<Vec<_>>(),
        DEFAULTS.iter().map(|d| d.key).collect::<Vec<_>>()
    );
    for setting in &queried {
        match given.iter().find(|(key, _)| *key == setting.key) {
            Some((_, value)) => {
                assert_eq!(setting.value, *value, "{}", setting.key);
                assert_eq!(setting.source, SettingSource::Configured, "{}", setting.key);
            }
            None => {
                let default = DEFAULTS
                    .iter()
                    .find(|d| d.key == setting.key)
                    .expect("listed");
                assert_eq!(setting.value, default.value, "{}", setting.key);
                assert_eq!(setting.source, SettingSource::Default, "{}", setting.key);
            }
        }
    }
    assert_eq!(SettingSource::Configured.as_str(), "configured");
    assert_eq!(SettingSource::Default.as_str(), "default");
}

/// A value cell of the design table: `32`, `500 ms`, `30 s`, `0.2`, or a number followed by
/// a note in parentheses.
fn parse_cell(cell: &str) -> SettingValue {
    let number: String = cell
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let unit = cell[number.len()..].trim_start();
    if unit.starts_with("ms") {
        SettingValue::Duration(Duration::from_millis(number.parse().expect("ms")))
    } else if unit.starts_with('s') && !unit.starts_with("s(") {
        SettingValue::Duration(Duration::from_secs(number.parse().expect("s")))
    } else if number.contains('.') {
        SettingValue::Ratio(number.parse().expect("ratio"))
    } else {
        SettingValue::Count(number.parse().expect("count"))
    }
}

/// The rows of the table under the heading `heading` in `doc`: the key in backticks and
/// the value cell.
fn table_rows(doc: &str, heading: &str) -> Vec<(String, String)> {
    let section = doc
        .split_once(heading)
        .map(|(_, rest)| rest)
        .expect("the heading is in the document");
    let section = section.split("\n#").next().unwrap_or(section);
    section
        .lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let key = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
            Some((key.to_string(), cells.get(2)?.to_string()))
        })
        .collect()
}

#[test]
fn defaults_match_design_tables() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/transport.md");
    let doc = std::fs::read_to_string(&path).expect("docs/design/transport.md");
    let rows = table_rows(&doc, "### 보관 상한과 재연결 도우미");
    assert_eq!(
        rows.iter()
            .map(|(k, _)| k.as_str())
            .collect::<BTreeSet<_>>(),
        DEFAULTS.iter().map(|d| d.key).collect::<BTreeSet<_>>(),
        "the design table and settings::DEFAULTS name the same keys"
    );
    for (key, cell) in &rows {
        let default = DEFAULTS.iter().find(|d| d.key == key).expect("listed");
        let documented = parse_cell(cell);
        let same = match (documented, default.value) {
            (SettingValue::Count(a), SettingValue::Ratio(b)) => a as f64 == b,
            (a, b) => a == b,
        };
        assert!(
            same,
            "{key}: the design table says {cell}, the code {}",
            default.value
        );
    }
}
