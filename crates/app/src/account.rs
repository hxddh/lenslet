//! An account as a file: the calls that made it, in order, to send again to
//! a new engine. Saving the account keeps it past quitting; opening one
//! builds it again.

use crate::session::Sent;

/// What a saved account says it is.
const KIND: &str = "lenslet-account";
const VERSION: u64 = 1;

/// The account that `log` made, as JSON.
pub fn to_json(log: &[Sent]) -> String {
    let calls: Vec<serde_json::Value> = log
        .iter()
        .map(|s| {
            // A value that is not text (bytes past ASCII) goes as hex.
            let headers: Vec<serde_json::Value> = s
                .headers
                .iter()
                .map(|(n, v)| match v.to_str() {
                    Ok(text) => serde_json::json!([n.as_str(), text]),
                    Err(_) => serde_json::json!([n.as_str(), "", hex(v.as_bytes())]),
                })
                .collect();
            let mut call = serde_json::json!({
                "at": s.at_ms,
                "method": s.method.as_str(),
                "uri": s.uri.to_string(),
                "headers": headers,
            });
            match std::str::from_utf8(&s.body) {
                Ok(text) => call["body"] = text.into(),
                Err(_) => call["bodyHex"] = hex(&s.body).into(),
            }
            call
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "kind": KIND,
        "version": VERSION,
        "calls": calls,
    }))
    .unwrap_or_default()
}

/// The calls a saved account holds, in order.
pub fn from_json(text: &str) -> Result<Vec<Sent>, String> {
    let not_one = || "This file is not a saved Lenslet account.".to_string();
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| not_one())?;
    if value["kind"] != KIND {
        return Err(not_one());
    }
    if value["version"].as_u64().is_none_or(|v| v > VERSION) {
        return Err("This account was saved by a newer Lenslet.".into());
    }
    let calls = value["calls"].as_array().ok_or_else(not_one)?;
    calls
        .iter()
        .map(|c| {
            let method = c["method"]
                .as_str()
                .and_then(|m| http::Method::from_bytes(m.as_bytes()).ok())
                .ok_or_else(not_one)?;
            let uri = c["uri"]
                .as_str()
                .and_then(|u| u.parse::<http::Uri>().ok())
                .ok_or_else(not_one)?;
            let mut headers = http::HeaderMap::new();
            for pair in c["headers"].as_array().into_iter().flatten() {
                let Some(name) = pair[0]
                    .as_str()
                    .and_then(|n| http::HeaderName::from_bytes(n.as_bytes()).ok())
                else {
                    continue;
                };
                let value = match pair.get(2).and_then(|h| h.as_str()) {
                    Some(h) => unhex(h).and_then(|b| http::HeaderValue::from_bytes(&b).ok()),
                    None => pair[1]
                        .as_str()
                        .and_then(|v| http::HeaderValue::from_str(v).ok()),
                };
                if let Some(value) = value {
                    headers.append(name, value);
                }
            }
            let body = match (c["body"].as_str(), c["bodyHex"].as_str()) {
                (Some(text), _) => bytes::Bytes::from(text.to_string()),
                (None, Some(h)) => bytes::Bytes::from(unhex(h).ok_or_else(not_one)?),
                (None, None) => bytes::Bytes::new(),
            };
            Ok(Sent {
                at_ms: c["at"].as_u64().unwrap_or_default(),
                method,
                uri,
                headers,
                body,
            })
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = text.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_comes_back_as_it_was_saved() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-amz-target",
            "DynamoDB_20120810.CreateTable".parse().unwrap(),
        );
        headers.insert(
            "x-amz-meta-name",
            http::HeaderValue::from_bytes("café".as_bytes()).unwrap(),
        );
        let log = vec![
            Sent {
                at_ms: 1,
                method: http::Method::POST,
                uri: "/".parse().unwrap(),
                headers,
                body: bytes::Bytes::from_static(b"{\"TableName\":\"orders\"}"),
            },
            Sent {
                at_ms: 2,
                method: http::Method::PUT,
                uri: "/shop/logo.png".parse().unwrap(),
                headers: http::HeaderMap::new(),
                body: bytes::Bytes::from_static(&[0x89, 0x50, 0xff, 0x00]),
            },
        ];
        let back = from_json(&to_json(&log)).expect("reads back");
        assert_eq!(back.len(), 2);
        assert_eq!(
            back[0].headers.get("x-amz-target").unwrap(),
            "DynamoDB_20120810.CreateTable"
        );
        assert_eq!(back[0].body, log[0].body);
        assert_eq!(
            back[0].headers.get("x-amz-meta-name").unwrap().as_bytes(),
            "café".as_bytes()
        );
        assert_eq!(back[1].body, log[1].body);
        assert_eq!(back[1].uri.path(), "/shop/logo.png");
        assert!(from_json("{\"kind\":\"something else\"}").is_err());
    }
}
