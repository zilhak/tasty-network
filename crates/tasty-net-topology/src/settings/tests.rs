use std::time::Instant;

use tasty_net_identity::{DeviceId, DeviceKey};

use super::*;
use crate::model::{Side, TransportKind};
use crate::{
    Agent, Done, ExploreRequest, LinkId, LinkKind, Local, LocalLink, MAX_MESSAGE_JSON, Message,
    MessageError, Now, Outbox, Query, ResultHeader, RouteError, SelfInfo, SendError, Sharing,
    TopologyError, split_done, split_result, validate_route,
};

/// The rows of the first table after the heading `heading` of a design document, as
/// (key, cells by column name).
fn table_rows(doc: &str, heading: &str) -> Vec<(String, BTreeMap<String, String>)> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/design")
        .join(doc);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let mut lines = text
        .lines()
        .skip_while(|l| !(l.starts_with('#') && l.trim_start_matches('#').trim() == heading));
    assert!(lines.next().is_some(), "{doc}: no heading {heading}");
    let mut table = lines
        .skip_while(|l| !l.starts_with('|'))
        .take_while(|l| l.starts_with('|'));
    let cells = |l: &str| -> Vec<String> {
        l.trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_owned())
            .collect()
    };
    let header = cells(table.next().expect("table header"));
    table.next(); // the |---| line
    table
        .map(|l| {
            let row = cells(l);
            let key = row[0].trim_matches('`').to_owned();
            (key, header.iter().cloned().zip(row).collect())
        })
        .collect()
}

/// The comparable form of a design cell: the text before a parenthesis, without backticks and
/// without the spaces that group digits.
fn normalize(cell: &str) -> String {
    let head = cell.split('(').next().unwrap_or(cell).replace('`', "");
    let chars: Vec<char> = head.trim().chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        let between_digits = *c == ' '
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
        if !between_digits {
            out.push(*c);
        }
    }
    out
}

#[test]
fn defaults_match_design_tables() {
    let rows = table_rows("topology.md", "설정값");
    let doc_keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
    let code_keys: Vec<&str> = DEFAULTS.iter().map(|s| s.key).collect();
    assert_eq!(code_keys, doc_keys, "the keys and their order");
    for ((key, cells), s) in rows.iter().zip(DEFAULTS) {
        assert_eq!(
            normalize(&s.default.to_string()),
            normalize(&cells["기본"]),
            "{key}: default"
        );
        assert_eq!(s.range_text(), normalize(&cells["범위"]), "{key}: range");
    }
}

fn id(n: u8) -> DeviceId {
    DeviceKey::from_seed([n; 32]).id()
}

fn me(n: u8) -> SelfInfo {
    SelfInfo {
        device_id: id(n),
        display_name: None,
    }
}

fn ms(n: u64) -> Value {
    Value::Duration(Duration::from_millis(n))
}

#[test]
fn settings_list_matches_query() {
    let mut settings = TopologySettings::new();
    settings.set("hop_timeout", ms(2000)).unwrap();
    // The same value as the default given explicitly is still `configured`.
    settings.set("max_parts", Value::Count(1024)).unwrap();
    let agent = Agent::new(me(1), settings.clone());
    let entries = agent.settings().entries();
    let keys: Vec<&str> = entries.iter().map(|e| e.key).collect();
    let listed: Vec<&str> = DEFAULTS.iter().map(|s| s.key).collect();
    assert_eq!(keys, listed);
    for e in &entries {
        let want = match e.key {
            "hop_timeout" | "max_parts" => Source::Configured,
            _ => Source::Default,
        };
        assert_eq!(e.source, want, "{e}");
        assert_eq!(agent.settings().get(e.key), Some(*e));
    }
    assert_eq!(
        agent.settings().get("hop_timeout").unwrap().to_string(),
        "hop_timeout = 2 s (configured)"
    );
    assert_eq!(
        agent.settings().get("max_query_time").unwrap().to_string(),
        "max_query_time = 30 s (default)"
    );
    // The values the agent runs with are the values the query shows.
    let s = agent.settings();
    assert_eq!(s.hop_timeout(), Duration::from_secs(2));
    assert_eq!(s.hop_reserve(), Duration::from_millis(250));
    assert_eq!(s.max_query_time(), Duration::from_secs(30));
    assert_eq!(s.max_query_ms(), 30_000);
    assert_eq!(s.max_route_hops(), 16);
    assert_eq!(s.max_parts(), 1024);
    // Unknown keys, wrong types and values out of range are refused and change nothing.
    let before = settings.clone();
    assert_eq!(
        settings.set("depth", Value::Count(1)).unwrap_err(),
        SettingsError::UnknownKey("depth".to_owned())
    );
    assert!(matches!(
        settings.set("max_parts", ms(1)),
        Err(SettingsError::WrongType { .. })
    ));
    for (key, value) in [
        ("max_query_time", ms(0)),
        ("max_query_time", ms(3_600_001)),
        ("max_route_hops", Value::Count(1025)),
        ("max_parts", Value::Count(0)),
        ("max_parts", Value::Count(65_537)),
    ] {
        assert!(
            matches!(
                settings.set(key, value),
                Err(SettingsError::OutOfRange { .. })
            ),
            "{key} = {value}"
        );
    }
    assert_eq!(settings, before);
    // Both ends of a range are accepted; a time without an upper end takes any value.
    for s in DEFAULTS {
        let mut one = TopologySettings::new();
        one.set(s.key, s.min).unwrap();
        if let Some(max) = s.max {
            one.set(s.key, max).unwrap();
        }
    }
    let mut long = TopologySettings::new();
    long.set("hop_timeout", Value::Duration(Duration::MAX))
        .unwrap();
    assert_eq!(long.hop_timeout(), Duration::MAX);
}

fn header() -> ResultHeader {
    ResultHeader {
        request_id: "r".into(),
        branch: "b".into(),
        from: id(1),
        return_path: vec![id(2)],
    }
}

#[test]
fn the_limits_apply_to_messages_and_routes() {
    let defaults = TopologySettings::default();
    let mut small = TopologySettings::new();
    small.set("max_parts", Value::Count(2)).unwrap();
    small.set("max_route_hops", Value::Count(1)).unwrap();

    // Reading: a part number of max_parts or more is malformed.
    let done = |part: u32| {
        Message::Done(Done {
            request_id: "r".into(),
            branch: "b".into(),
            from: id(1),
            return_path: vec![id(2)],
            timed_out: Vec::new(),
            is_final: false,
            more: false,
            part,
            incomplete: Vec::new(),
        })
    };
    let json = |m: Message| m.to_json().unwrap();
    assert!(Message::from_json(&json(done(1)), &small).is_ok());
    assert_eq!(
        Message::from_json(&json(done(2)), &small),
        Err(MessageError::Malformed)
    );
    assert!(Message::from_json(&json(done(2)), &defaults).is_ok());

    // Sending: an answer that needs more parts than max_parts is not split. The limit is the
    // smallest message size at which the default settings need exactly three.
    let ids: Vec<DeviceId> = (10..13).map(id).collect();
    let dones = |limit: usize, settings: &TopologySettings| {
        split_done(&header(), ids.clone(), Vec::new(), true, limit, settings)
    };
    let limit = (1..MAX_MESSAGE_JSON)
        .find(|&l| dones(l, &defaults).is_ok_and(|d| d.len() == 3))
        .expect("a size that needs three dones");
    assert_eq!(dones(limit, &small), Err(MessageError::TooLarge));
    let node = |n: u8| crate::Node {
        device_id: Some(id(n)),
        display_name: None,
        status: crate::NodeStatus::Online,
        reason: None,
        reported_by: None,
        peer_hint: None,
        explored_at: 0,
    };
    let nodes: Vec<crate::Node> = (10..13).map(node).collect();
    let parts = |limit: usize, settings: &TopologySettings| {
        split_result(&header(), nodes.clone(), Vec::new(), limit, settings)
    };
    let limit = (1..MAX_MESSAGE_JSON)
        .find(|&l| parts(l, &defaults).is_ok_and(|p| p.len() == 3))
        .expect("a size that needs three parts");
    assert_eq!(parts(limit, &small), Err(MessageError::TooLarge));

    // Routes: longer than max_route_hops is refused.
    let route = [id(3), id(4)];
    assert_eq!(
        validate_route(&id(1), &id(2), &route, &small),
        Err(RouteError::TooLong)
    );
    assert_eq!(validate_route(&id(1), &id(2), &route, &defaults), Ok(()));
    assert_eq!(validate_route(&id(1), &id(2), &route[..1], &small), Ok(()));
}

/// Records every message sent.
#[derive(Default)]
struct Recorder(Vec<(DeviceId, Message)>);

impl Outbox for Recorder {
    fn send(&mut self, to: &DeviceId, message: &Message) -> Result<(), SendError> {
        self.0.push((*to, message.clone()));
        Ok(())
    }
}

fn attach(peer: u8) -> LocalLink {
    LocalLink {
        link_id: LinkId::from_bytes([peer; 16]),
        peer: id(peer),
        local_side: Side::Dialer,
        holder: Side::Dialer,
        transport: TransportKind::SshForward,
        rtt: None,
        connected_at: 0,
        workspace: None,
        kind: LinkKind::Attach,
    }
}

fn sent_query(out: &Recorder, to: u8) -> Query {
    out.0
        .iter()
        .find_map(|(t, m)| match m {
            Message::Query(q) if *t == id(to) => Some(q.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no query to {to}"))
}

#[test]
fn the_agent_applies_its_settings_to_queries_it_sends_and_receives() {
    let mut short = TopologySettings::new();
    short.set("max_query_time", ms(1000)).unwrap();
    short.set("hop_reserve", ms(100)).unwrap();
    short.set("max_route_hops", Value::Count(0)).unwrap();
    let now = Now {
        mono: Instant::now(),
        unix_ms: 0,
    };

    // The requester caps the time it asks for and keeps its reserve back.
    let links = [attach(2)];
    let local = Local {
        links: &links,
        failures: &[],
        sharing: Sharing::default(),
    };
    let request = |route: Vec<DeviceId>| ExploreRequest {
        origin: id(2),
        route,
        depth: 1,
        timeout: Duration::from_secs(10),
    };
    for (settings, want_ms) in [(&short, 900), (&TopologySettings::default(), 9_750)] {
        let mut a = Agent::new(me(1), settings.clone());
        let mut out = Recorder::default();
        a.explore(request(Vec::new()), local, &mut out, now)
            .unwrap();
        assert_eq!(sent_query(&out, 2).remaining_ms, want_ms);
    }
    // ... and refuses a route over its max_route_hops before sending anything.
    let mut a = Agent::new(me(1), short.clone());
    let mut out = Recorder::default();
    assert_eq!(
        a.explore(request(vec![id(3)]), local, &mut out, now),
        Err(TopologyError::InvalidRoute(RouteError::TooLong))
    );
    assert!(out.0.is_empty());

    // The origin caps the remaining time it receives at its own max_query_time.
    let links = [attach(1), attach(3)];
    let local = Local {
        links: &links,
        failures: &[],
        sharing: Sharing::default(),
    };
    let query = Query {
        request_id: "r".into(),
        requester: id(1),
        origin: id(2),
        route: Vec::new(),
        path: Vec::new(),
        depth: 1,
        remaining_ms: 5_000,
        branch: "b".into(),
    };
    for (settings, want_ms) in [(&short, 900), (&TopologySettings::default(), 4_750)] {
        let mut b = Agent::new(me(2), settings.clone());
        let mut out = Recorder::default();
        b.on_message(&id(1), Message::Query(query.clone()), local, &mut out, now);
        assert_eq!(sent_query(&out, 3).remaining_ms, want_ms);
    }
}
